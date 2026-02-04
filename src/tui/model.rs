use std::path::PathBuf;

use crate::engine::{Engine, EngineConfig, EngineStats};

/// TUI 应用状态
pub struct App {
    /// 音频引擎（负责核心播放逻辑）
    pub engine: Engine,
    
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
}

impl App {
    pub fn new(config: EngineConfig, playlist: Vec<PathBuf>) -> Self {
        let engine = Engine::new(config);
        let mut playlist_state = ratatui::widgets::ListState::default();
        if !playlist.is_empty() {
            playlist_state.select(Some(0));
        }

        Self {
            engine,
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
        }
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

    /// 下一首
    pub fn next_track(&mut self) {
        if self.playlist.is_empty() {
            return;
        }
        
        if self.current_index + 1 < self.playlist.len() {
            self.current_index += 1;
        } else {
            self.current_index = 0; // 循环
        }
        self.playlist_state.select(Some(self.current_index));
        self.play_current();
    }

    /// 上一首
    pub fn prev_track(&mut self) {
        if self.playlist.is_empty() {
            return;
        }

        if self.current_index > 0 {
            self.current_index -= 1;
        } else {
            self.current_index = self.playlist.len() - 1;
        }
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

    /// 更新统计信息
    pub fn on_tick(&mut self) {
        self.cached_stats = self.engine.stats();
        
        // 自动切歌检测
        if self.engine.is_track_finished() {
            self.log("Track finished. Playing next...".to_string());
            self.next_track();
        }
    }
}
