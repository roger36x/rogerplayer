use std::ffi::OsStr;
use std::path::PathBuf;

use rand::seq::SliceRandom;

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
}

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
        }
    }

    /// 创建空播放列表的 App（用于无参数启动）
    pub fn new_empty(config: EngineConfig) -> Self {
        Self::new(config, Vec::new())
    }

    /// 从路径加载播放列表
    pub fn load_path(&mut self, path_str: &str) {
        // 清理路径字符串
        // 1. 去除首尾空白
        // 2. 去除首尾引号
        // 3. 处理 shell 转义字符（macOS Terminal 拖拽文件时会转义空格等）
        let path_str = path_str.trim().trim_matches(|c| c == '\'' || c == '"');
        let path_str = Self::unescape_shell_path(path_str);
        let path = PathBuf::from(&path_str);

        if !path.exists() {
            self.log(format!("Path not found: {}", path_str));
            return;
        }

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
        self.input_mode = false;
        self.path_input.clear();

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

    /// 更新统计信息
    pub fn on_tick(&mut self) {
        self.cached_stats = self.engine.stats();

        // 自动切歌检测
        if self.engine.is_track_finished() {
            self.log("Track finished".to_string());
            self.go_to_next(true); // 自动切歌
        }
    }
}
