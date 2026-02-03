//! HiFi Replayer - 极致音质音频播放器
//!
//! 设计目标：
//! - 时序绝对稳定：lock-free 架构 + 实时线程
//! - 数据流最干净：bit-perfect 直通路径
//! - 软件层极限优化：不依赖 DAC 端补偿

#![allow(dead_code, unused_imports, unused_mut)]

mod audio;
mod decode;
mod engine;
mod resample;

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::audio::AudioOutput;
use crate::engine::{Engine, EngineConfig, PlaybackState};

/// HiFi Replayer - High-fidelity audio player
#[derive(Parser)]
#[command(name = "hifi-replayer")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Audio file to play
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Buffer size in milliseconds
    #[arg(short, long, default_value = "2000")]
    buffer_ms: u32,

    /// Disable exclusive (hog) mode
    #[arg(long)]
    no_exclusive: bool,

    /// Show verbose output
    #[arg(short, long)]
    verbose: bool,
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
        None => {
            if let Some(ref file) = cli.file {
                simple_play(file, &cli)?;
            } else {
                // 没有参数，显示帮助
                println!("HiFi Replayer - Extreme quality audio player\n");
                println!("Usage: hifi-replayer [OPTIONS] <FILE>");
                println!("       hifi-replayer info");
                println!("       hifi-replayer interactive <FILE>");
                println!("\nOptions:");
                println!("  -b, --buffer-ms <MS>   Buffer size in milliseconds [default: 2000]");
                println!("  --no-exclusive         Disable exclusive mode");
                println!("  -v, --verbose          Show verbose output");
                println!("\nPress Ctrl+C to stop playback");
            }
        }
    }

    Ok(())
}

/// 显示设备信息
fn show_device_info() -> anyhow::Result<()> {
    println!("=== Audio Device Information ===\n");

    let device = AudioOutput::get_default_device()?;

    println!("Default Output Device:");
    println!("  ID: {}", device.id);
    println!("  Current Sample Rate: {} Hz", device.current_sample_rate);
    println!("\nSupported Sample Rates:");
    for rate in &device.supported_sample_rates {
        let mark = if (*rate - device.current_sample_rate).abs() < 1.0 {
            " (current)"
        } else {
            ""
        };
        println!("  {} Hz{}", rate, mark);
    }

    Ok(())
}

/// 简单播放模式
fn simple_play(file: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    let config = create_engine_config(cli);
    let mut engine = Engine::new(config);

    println!("HiFi Replayer - Loading: {}", file.display());

    engine.play(file)?;

    // 等待预缓冲完成
    print!("Buffering...");
    io::stdout().flush()?;

    while engine.state() == PlaybackState::Buffering {
        let stats = engine.stats();
        print!("\rBuffering... {:.0}%", stats.buffer_fill_ratio * 100.0);
        io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("\rBuffering complete.     ");

    // 播放循环
    println!("Playing. Press Ctrl+C to stop.\n");

    // 设置 Ctrl+C 处理
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::SeqCst);
    })?;

    while running.load(std::sync::atomic::Ordering::SeqCst) && engine.is_playing() {
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

        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\n\nStopping...");
    engine.stop()?;
    println!("Done.");

    Ok(())
}

/// 交互式播放模式
fn interactive_play(file: &PathBuf, cli: &Cli) -> anyhow::Result<()> {
    let config = create_engine_config(cli);
    let mut engine = Engine::new(config);

    println!("HiFi Replayer - Interactive Mode");
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

/// 创建引擎配置
fn create_engine_config(cli: &Cli) -> EngineConfig {
    let buffer_frames = (cli.buffer_ms as usize * 48) + 1000; // 近似，实际会根据采样率调整

    EngineConfig {
        output: crate::audio::OutputConfig {
            sample_rate: 48000, // 会被文件采样率覆盖
            buffer_frames: 512,
            exclusive_mode: !cli.no_exclusive,
            integer_mode: true,
        },
        buffer_frames,
        prebuffer_ratio: 0.5,
    }
}
