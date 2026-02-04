//! 音频核心模块
//!
//! 包含：
//! - Ring Buffer: Lock-free 数据传递
//! - Format: 音频格式和样本编解码
//! - Timing: Mach 时间相关函数
//! - Stats: 播放统计
//! - Output: Core Audio AUHAL 输出

pub mod format;
pub mod output;
pub mod ring_buffer;
pub mod stats;
pub mod timing;

pub use format::AudioFormat;
pub use output::{AudioOutput, OutputConfig, OutputError};
pub use ring_buffer::RingBuffer;
pub use stats::{PlaybackStats, StatsReport};
