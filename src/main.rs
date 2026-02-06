//! Roger Player - 极致音质音频播放器
//!
//! 设计目标：
//! - 时序绝对稳定：lock-free 架构 + 实时线程
//! - 数据流最干净：bit-perfect 直通路径
//! - 软件层极限优化：不依赖 DAC 端补偿

#![allow(dead_code, unused_mut)]

mod alloc;
mod audio;
mod decode;
mod engine;
mod tui;

#[global_allocator]
static GLOBAL: alloc::TuiIsolatedAllocator = alloc::TuiIsolatedAllocator;

use std::ffi::OsStr;
use std::io::{self, Read as IoRead, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use rand::seq::SliceRandom;

use crate::audio::AudioOutput;
use crate::engine::{Engine, EngineConfig, PlaybackState};

/// 曲目跳转命令
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipCommand {
    /// 继续当前曲目 / 正常结束
    None,
    /// 跳到下一首
    Next,
    /// 跳到上一首
    Previous,
}

/// 终端原始模式 RAII 守卫
struct RawModeGuard {
    original: libc::termios,
}

impl RawModeGuard {
    /// 进入原始模式，返回守卫（离开作用域自动恢复）
    fn enter() -> Option<Self> {
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return None;
            }

            let mut raw = original;
            // 关闭 canonical 模式和回显
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            // 非阻塞读取：VMIN=0, VTIME=0
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;

            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }

            Some(Self { original })
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original);
        }
    }
}

/// 按键类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyPress {
    Space,
    Left,
    Right,
    Other(u8),
}

/// 非阻塞读取按键（支持方向键转义序列）
fn read_key_nonblocking() -> Option<KeyPress> {
    let mut buf = [0u8; 1];
    let stdin = io::stdin();
    let mut handle = stdin.lock();

    match handle.read(&mut buf) {
        Ok(1) => {
            match buf[0] {
                b' ' => Some(KeyPress::Space),
                0x1B => {
                    // ESC - 可能是方向键转义序列
                    // 尝试读取后续字符: ESC [ <code>
                    let mut seq = [0u8; 2];
                    if handle.read(&mut seq).unwrap_or(0) == 2 && seq[0] == b'[' {
                        match seq[1] {
                            b'C' => Some(KeyPress::Right), // 右箭头
                            b'D' => Some(KeyPress::Left),  // 左箭头
                            _ => Some(KeyPress::Other(0x1B)),
                        }
                    } else {
                        Some(KeyPress::Other(0x1B))
                    }
                }
                ch => Some(KeyPress::Other(ch)),
            }
        }
        _ => None,
    }
}

/// Roger Player - High-fidelity audio player
#[derive(Parser)]
#[command(name = "roger-player")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Audio file or directory to play
    #[arg(value_name = "PATH")]
    file: Option<PathBuf>,

    /// Buffer size in milliseconds
    #[arg(short, long, default_value = "2000")]
    buffer_ms: u32,

    /// Disable exclusive (hog) mode
    #[arg(long)]
    no_exclusive: bool,

    /// Enable HAL direct hardware access (default, best quality)
    #[arg(long, conflicts_with = "hal_off")]
    hal_on: bool,

    /// Disable HAL, use system mixer (recommended for Bluetooth)
    #[arg(long, conflicts_with = "hal_on")]
    hal_off: bool,

    /// Select output device by name or ID (use 'info' command to list devices)
    #[arg(short, long)]
    device: Option<String>,

    /// Show verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Shuffle playback order (for directory mode)
    #[arg(short, long)]
    shuffle: bool,

    /// Repeat playback (loop directory or single track)
    #[arg(short, long)]
    repeat: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Show audio device information
    Info,

    /// Interactive playback mode
    Interactive {
        /// Audio file to play
        file: PathBuf,
    },

    /// Play file and exit
    Play {
        /// Audio file to play
        file: PathBuf,
    },

    /// Terminal UI mode
    Tui {
        /// Audio file or directory
        file: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    if cli.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }

    match cli.command {
        Some(Commands::Info) => {
            show_device_info()?;
        }
        Some(Commands::Interactive { ref file }) => {
            interactive_play(file, &cli)?;
        }
        Some(Commands::Play { ref file }) => {
            simple_play(file, &cli)?;
        }
        Some(Commands::Tui { ref file }) => {
            // TUI 模式下禁用日志输出到 stderr，避免干扰界面
            log::set_max_level(log::LevelFilter::Off);

            let path = file.as_ref().or(cli.file.as_ref());
            if let Some(p) = path {
                tui_play(p, &cli)?;
            } else {
                // 无参数启动，显示空界面等待拖拽
                tui_play_empty(&cli)?;
            }
        }
        None => {
            if let Some(ref file) = cli.file {
                simple_play(file, &cli)?;
            } else {
                // 没有参数，显示帮助
                println!("Roger Player - Extreme quality audio player\n");
                println!("Usage: roger-player [OPTIONS] <FILE|DIR>");
                println!("       roger-player info");
                println!("       roger-player tui <FILE|DIR>");
                println!("       roger-player interactive <FILE>");
                println!("\nOptions:");
                println!("  -b, --buffer-ms <MS>   Buffer size in milliseconds [default: 2000]");
                println!("  -d, --device <ID|NAME> Select output device (use 'info' to list)");
                println!("  -s, --shuffle          Shuffle playback order (directory mode)");
                println!("  -r, --repeat           Loop playback (directory or single track)");
                println!("  --no-exclusive         Disable exclusive mode");
                println!("  --no-hal               Use system mixer (recommended for Bluetooth)");
                println!("  -v, --verbose          Show verbose output");
                println!("\nSupported formats: {}", AUDIO_EXTENSIONS.join(", "));
                println!("If PATH is a directory, all audio files will be played in order.");
                println!("\nPress Ctrl+C to stop playback");
            }
        }
    }

    Ok(())
}

/// 显示设备信息
fn show_device_info() -> anyhow::Result<()> {
    println!("=== Audio Output Devices ===\n");

    let default_device = AudioOutput::get_default_device()?;
    let all_devices = AudioOutput::get_all_output_devices()?;

    for device in &all_devices {
        let is_default = device.id == default_device.id;
        let default_mark = if is_default { " *" } else { "" };
        let type_str = if device.is_bluetooth { "BT" } else { "USB" };

        println!("[{:>3}] {} ({}){}", device.id, device.name, type_str, default_mark);
    }

    println!();
    println!("* = system default");
    println!("BT = Bluetooth (auto system mixer), USB = Wired/USB\n");
    println!("Select device: roger-player -d <ID> <file>");
    println!("Example: roger-player -d {} <file>", default_device.id);

    Ok(())
}

/// 支持的音频文件扩展名
const AUDIO_EXTENSIONS: &[&str] = &["flac", "wav", "aiff", "aif", "mp3", "pcm"];

/// 检查文件是否为支持的音频格式
fn is_audio_file(path: &PathBuf) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// 扫描目录中的音频文件（按文件名排序）
fn scan_audio_files(dir: &PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_audio_file(path))
        .collect();

    // 按文件名排序
    files.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .cmp(b.file_name().unwrap_or_default())
    });

    Ok(files)
}

/// 简单播放模式
fn simple_play(path: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    // 检查是文件还是目录
    if path.is_dir() {
        return play_directory(path, cli);
    }

    // 单曲循环模式
    if cli.repeat {
        return play_single_file_repeat(path, cli);
    }

    play_single_file(path, cli, None)
}

/// 单曲循环播放
fn play_single_file_repeat(file: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    println!("Roger Player - Single Track Repeat Mode");
    println!("Press Ctrl+C to stop.\n");

    let mut play_count = 0u64;

    loop {
        if !running.load(Ordering::SeqCst) {
            println!("\nPlayback interrupted.");
            break;
        }

        play_count += 1;
        let track_info = Some((play_count as usize, 0)); // 0 表示无限循环

        match play_single_file_with_running(file, cli, track_info, running.clone(), false) {
            Ok(SkipCommand::None) => {
                // 正常结束，继续循环
                println!("\n--- Repeating track ---\n");
            }
            Ok(_) => {
                // 用户跳过（单曲模式下忽略跳转命令）
            }
            Err(e) => {
                eprintln!("Error playing: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// 播放目录中的所有音频文件
fn play_directory(dir: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    let mut files = scan_audio_files(dir)?;

    if files.is_empty() {
        println!("No audio files found in: {}", dir.display());
        println!("Supported formats: {}", AUDIO_EXTENSIONS.join(", "));
        return Ok(());
    }

    // 如果启用 shuffle，随机打乱播放顺序
    if cli.shuffle {
        let mut rng = rand::thread_rng();
        files.shuffle(&mut rng);
    }

    // 构建模式描述
    let mode_flags: Vec<&str> = [
        if cli.shuffle { Some("shuffle") } else { None },
        if cli.repeat { Some("repeat") } else { None },
    ].into_iter().flatten().collect();
    let mode_str = if mode_flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", mode_flags.join(", "))
    };

    println!("Roger Player - Directory Mode{}", mode_str);
    println!("Found {} audio files in: {}\n", files.len(), dir.display());

    for (i, file) in files.iter().enumerate() {
        println!(
            "  [{}] {}",
            i + 1,
            file.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    println!();
    println!("Controls: [Space] pause/play | [→] next | [←] previous | [Ctrl+C] quit\n");

    // 设置 Ctrl+C 处理（在播放开始前设置一次）
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // 进入终端原始模式（用于键盘控制）
    let _raw_guard = RawModeGuard::enter();

    // 使用索引循环，支持前后跳转
    let mut current_index: usize = 0;

    loop {
        // 检查是否已播放完所有曲目
        if current_index >= files.len() {
            if cli.repeat {
                // 循环模式：重新开始
                current_index = 0;
                println!("\n--- Playlist restarting ---\n");
            } else {
                // 非循环模式：结束
                break;
            }
        }

        if !running.load(Ordering::SeqCst) {
            println!("\nPlayback interrupted.");
            break;
        }

        let file = &files[current_index];
        let track_info = Some((current_index + 1, files.len()));

        match play_single_file_with_running(file, cli, track_info, running.clone(), true) {
            Ok(skip_command) => {
                match skip_command {
                    SkipCommand::Next => {
                        // 下一首
                        current_index += 1;
                    }
                    SkipCommand::Previous => {
                        // 上一首（如果已经是第一首则跳到最后一首，在循环模式下）
                        if current_index == 0 {
                            if cli.repeat {
                                current_index = files.len() - 1;
                            }
                            // 非循环模式下保持在第一首
                        } else {
                            current_index -= 1;
                        }
                    }
                    SkipCommand::None => {
                        // 正常结束，继续下一首
                        current_index += 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("Error playing {}: {}", file.display(), e);
                // 出错时继续下一首
                current_index += 1;
            }
        }
    }

    println!("Playlist finished.");
    Ok(())
}

/// 播放单个文件（带可选的曲目信息）
fn play_single_file(
    file: &PathBuf,
    cli: &Cli,
    track_info: Option<(usize, usize)>,
) -> anyhow::Result<()> {
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    // 进入终端原始模式（用于键盘控制）
    let _raw_guard = RawModeGuard::enter();

    play_single_file_with_running(file, cli, track_info, running, false)?;
    Ok(())
}

/// 播放单个文件（使用已存在的 running 标志）
///
/// 参数 `keyboard_control` 为 true 时启用键盘控制（空格切换曲目）
/// 返回 SkipCommand 指示是否需要跳转
fn play_single_file_with_running(
    file: &PathBuf,
    cli: &Cli,
    track_info: Option<(usize, usize)>,
    running: Arc<std::sync::atomic::AtomicBool>,
    keyboard_control: bool,
) -> anyhow::Result<SkipCommand> {
    let config = create_engine_config(cli);
    let mut engine = Engine::new(config);

    // 显示播放信息
    let file_name = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    if let Some((current, total)) = track_info {
        // 换曲时加空行分隔（第一首除外）
        // 需要两个换行：一个结束状态行（\r 覆盖的行），一个创建空行
        if current > 1 {
            println!("\n");
        }
        if total == 0 {
            // 单曲循环模式：显示播放次数
            println!("[Play #{}] Loading: {}", current, file_name);
        } else {
            println!("[{}/{}] Loading: {}", current, total, file_name);
        }
    } else {
        println!("Roger Player - Loading: {}", file.display());
    }

    engine.play(file)?;

    // 等待预缓冲完成
    print!("Buffering...");
    io::stdout().flush()?;

    while engine.state() == PlaybackState::Buffering {
        if !running.load(Ordering::SeqCst) {
            engine.stop()?;
            return Ok(SkipCommand::None);
        }
        let stats = engine.stats();
        print!("\rBuffering... {:.0}%", stats.buffer_fill_ratio * 100.0);
        io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(100));
    }

    // 显示输出模式状态
    if let Some((is_hal, is_exclusive)) = engine.output_mode() {
        let mode = if is_hal { "HALOutput (bit-perfect)" } else { "DefaultOutput (mixer)" };
        let exclusive = if is_exclusive { " | exclusive" } else { "" };
        print!("\rOutput: {}{}", mode, exclusive);
        // 补齐空格清除 Buffering 残留
        println!("                    ");
    } else {
        println!("\rBuffering complete.     ");
    }

    // 播放循环
    if track_info.is_none() {
        println!("Playing. [Space] pause/play | [Ctrl+C] quit\n");
    }

    let mut skip_command = SkipCommand::None;

    loop {
        // 检查用户中断
        if !running.load(Ordering::SeqCst) {
            break;
        }

        // 检查音轨是否播放完毕
        if engine.is_track_finished() {
            break;
        }

        // 键盘控制
        // Space = 暂停/播放, → = 下一首, ← = 上一首
        if let Some(key) = read_key_nonblocking() {
            match key {
                KeyPress::Space => {
                    // 空格：暂停/播放
                    let _ = engine.toggle_pause();
                }
                KeyPress::Right if keyboard_control => {
                    // →：下一首
                    skip_command = SkipCommand::Next;
                    break;
                }
                KeyPress::Left if keyboard_control => {
                    // ←：上一首
                    skip_command = SkipCommand::Previous;
                    break;
                }
                _ => {}
            }
        }

        let stats = engine.stats();

        // 格式化时间
        let pos_mins = (stats.position_secs / 60.0) as u32;
        let pos_secs = stats.position_secs % 60.0;

        let total_secs = engine
            .current_info()
            .and_then(|i| i.duration_secs)
            .unwrap_or(0.0);
        let total_mins = (total_secs / 60.0) as u32;
        let total_secs_rem = total_secs % 60.0;

        print!(
            "\r  {:02}:{:05.2} / {:02}:{:05.2}  |  Buffer: {:5.1}%  |  Underruns: {}  ",
            pos_mins,
            pos_secs,
            total_mins,
            total_secs_rem,
            stats.buffer_fill_ratio * 100.0,
            stats.underrun_count
        );
        io::stdout().flush()?;

        std::thread::sleep(Duration::from_millis(50)); // 更快响应键盘
    }

    println!();
    engine.stop()?;

    Ok(skip_command)
}

/// 交互式播放模式
fn interactive_play(file: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    let config = create_engine_config(cli);
    let mut engine = Engine::new(config);

    println!("Roger Player - Interactive Mode");
    println!("Loading: {}", file.display());

    engine.play(file)?;

    // 等待预缓冲
    while engine.state() == PlaybackState::Buffering {
        std::thread::sleep(Duration::from_millis(50));
    }

    println!("\nCommands: [space]=pause/resume  [q]=quit  [i]=info\n");

    // 简单的命令行交互
    // 注意：这需要 terminal raw mode，这里简化为轮询
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::SeqCst);
    })?;

    while running.load(std::sync::atomic::Ordering::SeqCst) && engine.is_playing() {
        let stats = engine.stats();
        let state = engine.state();

        let state_str = match state {
            PlaybackState::Playing => "▶",
            PlaybackState::Paused => "⏸",
            PlaybackState::Buffering => "⏳",
            PlaybackState::Stopped => "⏹",
        };

        print!(
            "\r{} {:.1}s | Buffer: {:.0}% | Underruns: {}    ",
            state_str,
            stats.position_secs,
            stats.buffer_fill_ratio * 100.0,
            stats.underrun_count
        );
        io::stdout().flush()?;

        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\n");
    engine.stop()?;

    Ok(())
}

/// TUI 播放模式
fn tui_play(path: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    // 扫描文件
    let mut files = if path.is_dir() {
        scan_audio_files(path)?
    } else {
        if is_audio_file(path) {
            vec![path.clone()]
        } else {
            return Err(anyhow::anyhow!("Not a supported audio file: {}", path.display()));
        }
    };

    if files.is_empty() {
        return Err(anyhow::anyhow!("No audio files found in: {}", path.display()));
    }

    // Shuffle
    if cli.shuffle {
        let mut rng = rand::thread_rng();
        files.shuffle(&mut rng);
    }

    let config = create_engine_config(cli);
    let app = crate::tui::model::App::new(config, files);

    crate::tui::controller::run(app)?;

    Ok(())
}

/// TUI 空启动模式（无参数，等待拖拽文件）
fn tui_play_empty(cli: &Cli) -> anyhow::Result<()> {
    let config = create_engine_config(cli);
    let app = crate::tui::model::App::new_empty(config);

    crate::tui::controller::run(app)?;

    Ok(())
}

/// 创建引擎配置
fn create_engine_config(cli: &Cli) -> EngineConfig {
    let buffer_frames = (cli.buffer_ms as usize * 48) + 1000; // 近似，实际会根据采样率调整

    // 解析设备选择
    let device_id = cli.device.as_ref().and_then(|d| {
        // 先尝试解析为设备 ID
        if let Ok(id) = d.parse::<u32>() {
            println!("Using device ID: {}", id);
            return Some(id);
        }

        // 否则按名称查找
        if let Some(device) = AudioOutput::find_device_by_name(d) {
            println!("Found device: {} (ID: {})", device.name, device.id);
            return Some(device.id);
        }

        eprintln!("Warning: Device '{}' not found, using system default", d);
        None
    });

    EngineConfig {
        output: crate::audio::OutputConfig {
            sample_rate: 48000, // 会被文件采样率覆盖
            buffer_frames: 512,
            exclusive_mode: !cli.no_exclusive,
            integer_mode: true,
            use_hal: !cli.hal_off,
            device_id,
        },
        buffer_frames,
        prebuffer_ratio: 0.5,
    }
}
