use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;

use crate::audio::AudioOutput;
use crate::engine::{Engine, EngineConfig, EngineStats};

/// 支持的音频文件扩展名
const AUDIO_EXTENSIONS: &[&str] = &["flac", "wav", "aiff", "aif", "mp3", "pcm"];

/// 循环播放模式
#[derive(Clone, Copy, PartialEq, Default)]
pub enum RepeatMode {
    #[default]
    Off,   // 播放完列表后停止
    All,   // 列表循环
    Track, // 单曲循环
}

/// 输出模式选择
#[derive(Clone, Copy, PartialEq, Default)]
pub enum OutputModeChoice {
    #[default]
    HalExclusive,  // HAL 独占模式（最高音质）
    SystemMixer,   // 系统混音器（兼容性好）
}

/// 弹窗状态
#[derive(Clone, Default)]
pub enum DialogState {
    #[default]
    None,
    /// 输出模式选择弹窗，包含待加载的路径
    OutputModeSelect {
        pending_path: String,
        selected: OutputModeChoice,
    },
}

/// TUI 应用状态
pub struct App {
    /// 音频引擎（负责核心播放逻辑）
    pub engine: Engine,

    /// 引擎配置（保存以便重新创建）
    config: EngineConfig,

    /// 播放列表文件
    pub playlist: Vec<PathBuf>,

    /// 当前播放索引
    pub current_index: usize,

    /// 播放列表滚动状态（Ratatui ListState）
    pub playlist_state: ratatui::widgets::ListState,

    /// 日志消息队列
    pub logs: Vec<String>,

    /// 是否应该退出
    pub should_quit: bool,

    /// 缓存的统计信息（避免过度刷新）
    pub cached_stats: EngineStats,

    /// 是否处于路径输入模式
    pub input_mode: bool,

    /// 路径输入缓冲区
    pub path_input: String,

    /// 是否启用随机播放
    pub shuffle: bool,

    /// 循环播放模式
    pub repeat_mode: RepeatMode,

    /// 随机播放顺序（shuffle 模式下使用）
    shuffle_order: Vec<usize>,

    /// 上次切歌时间（防抖用，防止快速切歌导致 AudioUnit 错误）
    last_switch_time: Option<Instant>,

    /// 弹窗状态
    pub dialog: DialogState,

    /// 上次选曲操作时间（用于自动回位）
    pub last_selection_time: Option<Instant>,

    /// 是否显示选曲光标
    pub show_cursor: bool,

    /// 是否处于搜索模式
    pub search_mode: bool,

    /// 搜索输入
    pub search_input: String,

    /// 搜索结果索引列表
    pub search_results: Vec<usize>,

    /// 当前搜索结果中的选中索引
    pub search_result_index: usize,

    /// 是否显示帮助页面
    pub show_help: bool,
}

/// 切歌防抖间隔（毫秒）
const TRACK_SWITCH_DEBOUNCE_MS: u64 = 200;

impl App {
    pub fn new(config: EngineConfig, playlist: Vec<PathBuf>) -> Self {
        let engine = Engine::new(config.clone());
        let mut playlist_state = ratatui::widgets::ListState::default();
        let input_mode = playlist.is_empty();
        if !playlist.is_empty() {
            playlist_state.select(Some(0));
        }

        let shuffle_order = (0..playlist.len()).collect();

        Self {
            engine,
            config,
            playlist,
            current_index: 0,
            playlist_state,
            logs: Vec::new(),
            should_quit: false,
            cached_stats: EngineStats {
                buffer_fill_ratio: 0.0,
                underrun_count: 0,
                samples_played: 0,
                position_secs: 0.0,
            },
            input_mode,
            path_input: String::new(),
            shuffle: false,
            repeat_mode: RepeatMode::default(),
            shuffle_order,
            last_switch_time: None,
            dialog: DialogState::None,
            last_selection_time: None,
            show_cursor: false,
            search_mode: false,
            search_input: String::new(),
            search_results: Vec::new(),
            search_result_index: 0,
            show_help: false,
        }
    }

    /// 创建空播放列表的 App（用于无参数启动）
    pub fn new_empty(config: EngineConfig) -> Self {
        Self::new(config, Vec::new())
    }

    /// 从路径加载播放列表
    ///
    /// 如果不是蓝牙设备，会先显示输出模式选择弹窗
    pub fn load_path(&mut self, path_str: &str) {
        // 清理路径字符串
        let path_str = path_str.trim().trim_matches(|c| c == '\'' || c == '"');
        let path_str = Self::unescape_shell_path(path_str);
        let path = PathBuf::from(&path_str);

        // 先验证路径是否有效
        if !path.exists() {
            self.log(format!("Path not found: {}", path_str));
            return;
        }

        // 检查是否是支持的音频文件或目录
        if !path.is_dir() && !Self::is_audio_file(&path) {
            self.log(format!("Not a supported audio file: {}", path_str));
            return;
        }

        // 退出输入模式
        self.input_mode = false;
        self.path_input.clear();

        // 检测是否是蓝牙设备
        if AudioOutput::is_default_device_bluetooth() {
            // 蓝牙设备：直接使用系统混音器，不显示弹窗
            self.config.output.use_hal = false;
            self.config.output.exclusive_mode = false;
            self.log("Bluetooth device detected, using System Mixer".to_string());
            self.do_load_path(&path_str);
        } else {
            // 非蓝牙设备：显示输出模式选择弹窗
            self.dialog = DialogState::OutputModeSelect {
                pending_path: path_str,
                selected: OutputModeChoice::HalExclusive, // 默认选中 HAL
            };
        }
    }

    /// 实际执行路径加载（弹窗确认后调用）
    fn do_load_path(&mut self, path_str: &str) {
        let path = PathBuf::from(path_str);

        let files = if path.is_dir() {
            match Self::scan_audio_files(&path) {
                Ok(f) => f,
                Err(e) => {
                    self.log(format!("Error scanning directory: {}", e));
                    return;
                }
            }
        } else if Self::is_audio_file(&path) {
            vec![path]
        } else {
            self.log(format!("Not a supported audio file: {}", path_str));
            return;
        };

        if files.is_empty() {
            self.log("No audio files found".to_string());
            return;
        }

        self.log(format!("Loaded {} files", files.len()));
        self.playlist = files;
        self.current_index = 0;
        self.playlist_state.select(Some(0));

        // 重新生成 shuffle 顺序
        if self.shuffle {
            self.generate_shuffle_order();
        } else {
            self.shuffle_order = (0..self.playlist.len()).collect();
        }

        // 自动播放第一首
        self.play_current();
    }

    /// 处理 shell 转义的路径
    /// macOS Terminal 拖拽文件时会转义空格和特殊字符：
    /// - `\ ` -> ` ` (空格)
    /// - `\\` -> `\` (反斜杠)
    /// - `\'` -> `'` (单引号)
    /// - `\(`, `\)` 等特殊字符
    fn unescape_shell_path(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\\' {
                // 反斜杠：检查下一个字符
                if let Some(&next) = chars.peek() {
                    // 转义字符，取消转义
                    result.push(next);
                    chars.next();
                } else {
                    // 末尾的反斜杠，保留
                    result.push(c);
                }
            } else {
                result.push(c);
            }
        }

        result
    }

    /// 检查文件是否为支持的音频格式
    fn is_audio_file(path: &PathBuf) -> bool {
        path.extension()
            .and_then(OsStr::to_str)
            .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// 扫描目录中的音频文件（按文件名排序）
    fn scan_audio_files(dir: &PathBuf) -> std::io::Result<Vec<PathBuf>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && Self::is_audio_file(path))
            .collect();

        // 按文件名排序
        files.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .cmp(b.file_name().unwrap_or_default())
        });

        Ok(files)
    }

    /// 添加日志
    pub fn log(&mut self, message: String) {
        // 保留最近 50 条日志
        if self.logs.len() >= 50 {
            self.logs.remove(0);
        }
        let timestamp = chrono::Local::now().format("%H:%M:%S");
        self.logs.push(format!("[{}] {}", timestamp, message));
    }

    /// 获取当前播放的文件名
    pub fn current_track_name(&self) -> String {
        if self.current_index < self.playlist.len() {
            self.playlist[self.current_index]
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        } else {
            "No Track".to_string()
        }
    }

    /// 下一首（用户手动触发）
    pub fn next_track(&mut self) {
        if self.playlist.is_empty() {
            return;
        }

        // 防抖：防止快速切歌导致 AudioUnit 状态错误
        if let Some(last_time) = self.last_switch_time {
            if last_time.elapsed() < Duration::from_millis(TRACK_SWITCH_DEBOUNCE_MS) {
                return;
            }
        }

        // 单曲循环模式下，手动按 next 仍然切到下一首
        self.go_to_next(false);
    }

    /// 内部方法：切换到下一首
    /// auto_advance: 是否是自动切歌（播放完毕时触发）
    fn go_to_next(&mut self, auto_advance: bool) {
        if self.playlist.is_empty() {
            return;
        }

        // 单曲循环模式且是自动切歌时，重播当前曲目
        if auto_advance && self.repeat_mode == RepeatMode::Track {
            self.play_current();
            return;
        }

        let next_index = if self.shuffle {
            // Shuffle 模式：找到当前在 shuffle_order 中的位置，然后取下一个
            if let Some(pos) = self.current_shuffle_position() {
                let next_pos = pos + 1;
                if next_pos < self.shuffle_order.len() {
                    Some(self.shuffle_order[next_pos])
                } else if self.repeat_mode == RepeatMode::All {
                    // 循环：回到 shuffle_order 开头
                    Some(self.shuffle_order[0])
                } else {
                    None // 播放完毕
                }
            } else {
                // 当前位置不在 shuffle_order 中，从头开始
                self.shuffle_order.first().copied()
            }
        } else {
            // 顺序播放模式
            if self.current_index + 1 < self.playlist.len() {
                Some(self.current_index + 1)
            } else if self.repeat_mode == RepeatMode::All {
                Some(0) // 循环
            } else {
                None // 播放完毕
            }
        };

        if let Some(idx) = next_index {
            self.current_index = idx;
            self.playlist_state.select(Some(self.current_index));
            self.play_current();
        } else {
            // 播放结束，停止
            let _ = self.engine.stop();
            self.log("Playlist finished".to_string());
        }
    }

    /// 上一首
    pub fn prev_track(&mut self) {
        if self.playlist.is_empty() {
            return;
        }

        // 防抖：防止快速切歌导致 AudioUnit 状态错误
        if let Some(last_time) = self.last_switch_time {
            if last_time.elapsed() < Duration::from_millis(TRACK_SWITCH_DEBOUNCE_MS) {
                return;
            }
        }

        let prev_index = if self.shuffle {
            // Shuffle 模式：找到当前在 shuffle_order 中的位置，然后取上一个
            if let Some(pos) = self.current_shuffle_position() {
                if pos > 0 {
                    self.shuffle_order[pos - 1]
                } else {
                    // 已经是第一首，循环到最后
                    *self.shuffle_order.last().unwrap_or(&0)
                }
            } else {
                // 当前位置不在 shuffle_order 中，取第一个
                *self.shuffle_order.first().unwrap_or(&0)
            }
        } else {
            // 顺序播放模式
            if self.current_index > 0 {
                self.current_index - 1
            } else {
                self.playlist.len() - 1
            }
        };

        self.current_index = prev_index;
        self.playlist_state.select(Some(self.current_index));
        self.play_current();
    }

    /// 播放当前选中的曲目
    pub fn play_current(&mut self) {
        if self.current_index < self.playlist.len() {
            // 更新切歌时间戳（用于防抖）
            self.last_switch_time = Some(Instant::now());

            let path = &self.playlist[self.current_index];
            if let Err(e) = self.engine.play(path) {
                self.log(format!("Error playing: {}", e));
            } else {
                self.log(format!("Playing: {}", path.display()));
            }
        }
    }

    /// 切换随机播放模式
    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.generate_shuffle_order();
            self.log("Shuffle: ON".to_string());
        } else {
            self.log("Shuffle: OFF".to_string());
        }
    }

    /// 循环切换重复模式 (Off -> All -> Track -> Off)
    pub fn cycle_repeat(&mut self) {
        self.repeat_mode = match self.repeat_mode {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::Track,
            RepeatMode::Track => RepeatMode::Off,
        };
        let mode_str = match self.repeat_mode {
            RepeatMode::Off => "OFF",
            RepeatMode::All => "ALL",
            RepeatMode::Track => "TRACK",
        };
        self.log(format!("Repeat: {}", mode_str));
    }

    // ========== 弹窗相关方法 ==========

    /// 弹窗选择向上
    pub fn dialog_select_up(&mut self) {
        if let DialogState::OutputModeSelect { selected, .. } = &mut self.dialog {
            *selected = OutputModeChoice::HalExclusive;
        }
    }

    /// 弹窗选择向下
    pub fn dialog_select_down(&mut self) {
        if let DialogState::OutputModeSelect { selected, .. } = &mut self.dialog {
            *selected = OutputModeChoice::SystemMixer;
        }
    }

    /// 弹窗选择指定选项（0: HAL, 1: Mixer）
    pub fn dialog_select_option(&mut self, index: usize) {
        if let DialogState::OutputModeSelect { selected, .. } = &mut self.dialog {
            *selected = if index == 0 {
                OutputModeChoice::HalExclusive
            } else {
                OutputModeChoice::SystemMixer
            };
        }
    }

    /// 确认弹窗选择
    pub fn dialog_confirm(&mut self) {
        if let DialogState::OutputModeSelect { pending_path, selected } = &self.dialog {
            let path = pending_path.clone();
            let use_hal = *selected == OutputModeChoice::HalExclusive;

            // 更新配置
            self.config.output.use_hal = use_hal;
            self.config.output.exclusive_mode = use_hal;

            // 重新创建引擎（使用新配置）
            self.engine = Engine::new(self.config.clone());

            let mode_str = if use_hal { "HAL (Exclusive)" } else { "System Mixer" };
            self.log(format!("Output mode: {}", mode_str));

            // 关闭弹窗
            self.dialog = DialogState::None;

            // 执行实际加载
            self.do_load_path(&path);
        }
    }

    /// 取消弹窗
    pub fn dialog_cancel(&mut self) {
        self.dialog = DialogState::None;
        self.log("Cancelled".to_string());
    }

    /// 生成随机播放顺序
    fn generate_shuffle_order(&mut self) {
        self.shuffle_order = (0..self.playlist.len()).collect();
        let mut rng = rand::thread_rng();
        self.shuffle_order.shuffle(&mut rng);
    }

    /// 获取当前曲目在 shuffle_order 中的位置
    fn current_shuffle_position(&self) -> Option<usize> {
        self.shuffle_order.iter().position(|&i| i == self.current_index)
    }

    /// 轻量级曲目结束检测
    ///
    /// 仅读取 eof_reached (AtomicBool)，播放中几乎零开销。
    /// 只有 eof_reached=true 时才进一步检查 ring_buffer.available()。
    /// 从主循环高频调用（每次输入轮询），不读取统计信息。
    pub fn check_track_end(&mut self) -> bool {
        if self.engine.is_track_finished() {
            self.log("Track finished".to_string());
            self.go_to_next(true);
            true
        } else {
            false
        }
    }

    /// 选曲光标超时检查（纯本地状态，无原子操作）
    pub fn check_cursor_timeout(&mut self) {
        if let Some(last_time) = self.last_selection_time {
            if last_time.elapsed() > Duration::from_secs(10) {
                self.playlist_state.select(Some(self.current_index));
                self.last_selection_time = None;
                self.show_cursor = false;
            }
        }
    }

    /// 更新统计信息（仅在绘制前调用）
    ///
    /// 读取引擎统计会访问 ring_buffer 的 write_pos/read_pos 原子变量，
    /// 导致音频线程的 cache line 失效。因此仅在即将绘制时才读取，
    /// 播放中约每 500ms 一次（而非之前的 50ms），减少 10 倍 cache 干扰。
    pub fn update_stats(&mut self) {
        self.cached_stats = self.engine.stats();
    }

    /// 执行搜索
    pub fn do_search(&mut self) {
        let query = self.search_input.to_lowercase();
        self.search_results = self
            .playlist
            .iter()
            .enumerate()
            .filter(|(_, path)| {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(i, _)| i)
            .collect();
        self.search_result_index = 0;

        // 如果有结果，跳转到第一个
        if let Some(&idx) = self.search_results.first() {
            self.playlist_state.select(Some(idx));
            self.show_cursor = true;
            self.last_selection_time = Some(Instant::now());
        }
    }

    /// 跳转到下一个搜索结果
    pub fn search_next(&mut self) {
        if self.search_results.is_empty() {
            return;
        }
        self.search_result_index = (self.search_result_index + 1) % self.search_results.len();
        let idx = self.search_results[self.search_result_index];
        self.playlist_state.select(Some(idx));
        self.show_cursor = true;
        self.last_selection_time = Some(Instant::now());
    }

    /// 跳转到上一个搜索结果
    pub fn search_prev(&mut self) {
        if self.search_results.is_empty() {
            return;
        }
        if self.search_result_index == 0 {
            self.search_result_index = self.search_results.len() - 1;
        } else {
            self.search_result_index -= 1;
        }
        let idx = self.search_results[self.search_result_index];
        self.playlist_state.select(Some(idx));
        self.show_cursor = true;
        self.last_selection_time = Some(Instant::now());
    }

    /// 退出搜索模式
    pub fn exit_search(&mut self) {
        self.search_mode = false;
        self.search_input.clear();
        self.search_results.clear();
        self.search_result_index = 0;
    }
}
