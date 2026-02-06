//! 播放统计模块
//!
//! IO callback 内仅记录 samples_played 和 underrun_count，
//! 不做任何诊断性采样（interval timing、water level 等），
//! 确保信号路径上只有必要的计算。

use std::sync::atomic::{AtomicU64, Ordering};

use super::ring_buffer::CacheLine;

/// 播放统计收集器
///
/// IO callback 内仅调用 `add_samples_played()` 和 `record_underrun()`，
/// 各自只需一次 `fetch_add(Relaxed)` 原子操作。
///
/// 内存布局：两个字段独占缓存行，避免 false sharing。
pub struct PlaybackStats {
    samples_played: CacheLine<AtomicU64>,
    underrun_count: CacheLine<AtomicU64>,
}

impl PlaybackStats {
    pub fn new() -> Self {
        Self {
            samples_played: CacheLine::new(AtomicU64::new(0)),
            underrun_count: CacheLine::new(AtomicU64::new(0)),
        }
    }

    /// 记录 underrun（IO callback 内调用）
    #[inline]
    pub fn record_underrun(&self) {
        self.underrun_count.0.fetch_add(1, Ordering::Relaxed);
    }

    /// 更新已播放样本数（IO callback 内调用）
    #[inline]
    pub fn add_samples_played(&self, samples: u64) {
        self.samples_played.0.fetch_add(samples, Ordering::Relaxed);
    }

    /// 获取 underrun 计数
    #[inline]
    pub fn underrun_count(&self) -> u64 {
        self.underrun_count.0.load(Ordering::Relaxed)
    }

    /// 获取已播放样本数
    #[inline]
    pub fn samples_played(&self) -> u64 {
        self.samples_played.0.load(Ordering::Relaxed)
    }

    /// 重置统计
    pub fn reset(&self) {
        self.underrun_count.0.store(0, Ordering::Relaxed);
        self.samples_played.0.store(0, Ordering::Relaxed);
    }
}

impl Default for PlaybackStats {
    fn default() -> Self {
        Self::new()
    }
}
