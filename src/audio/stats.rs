//! 播放统计模块
//!
//! 在音频回调中收集统计信息，采用降频采样策略减少开销

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::ring_buffer::RingBuffer;
use super::timing::{mach_ticks_to_ns, now_ticks};

/// 统计采样间隔：每 N 次 callback 才采样一次
const SAMPLE_INTERVAL: u64 = 16;

/// 时间戳缓冲区大小
const TIMESTAMP_BUFFER_SIZE: usize = 256;

/// 播放统计收集器
///
/// 所有操作都是 lock-free 的，适合在音频回调中使用
pub struct PlaybackStats {
    callback_count: AtomicU64,
    last_sampled_ticks: AtomicU64,

    // 存储 interval（单位：mach ticks，后处理时转换）
    interval_buffer: Box<[AtomicU64; TIMESTAMP_BUFFER_SIZE]>,
    interval_write_idx: AtomicUsize,

    // 水位（也降频采样）
    water_level_buffer: Box<[AtomicUsize; TIMESTAMP_BUFFER_SIZE]>,
    water_level_write_idx: AtomicUsize,

    underrun_count: AtomicU64,

    // 已播放样本数
    samples_played: AtomicU64,
}

impl PlaybackStats {
    pub fn new() -> Self {
        Self {
            callback_count: AtomicU64::new(0),
            last_sampled_ticks: AtomicU64::new(0),
            interval_buffer: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
            interval_write_idx: AtomicUsize::new(0),
            water_level_buffer: Box::new(std::array::from_fn(|_| AtomicUsize::new(0))),
            water_level_write_idx: AtomicUsize::new(0),
            underrun_count: AtomicU64::new(0),
            samples_played: AtomicU64::new(0),
        }
    }

    /// 在 render callback 内调用（使用硬件时间戳）
    ///
    /// `host_time`: 来自 AudioTimeStamp 的 host_time（mach ticks），
    ///              如果为 0 则回退到 now_ticks()
    ///
    /// 只在采样点才读 now + water_level，减少开销
    #[inline]
    pub fn on_callback_with_timestamp(&self, ring_buffer: &RingBuffer<i32>, host_time: u64) {
        let count = self.callback_count.fetch_add(1, Ordering::Relaxed);

        // 只在采样点才做额外工作
        if count.is_multiple_of(SAMPLE_INTERVAL) {
            // 使用硬件时间戳（更精确）或回退到 now_ticks()
            let now = if host_time > 0 { host_time } else { now_ticks() };
            let last = self.last_sampled_ticks.swap(now, Ordering::Relaxed);

            if last > 0 {
                let interval = now.saturating_sub(last);
                let idx = self.interval_write_idx.fetch_add(1, Ordering::Relaxed)
                    % TIMESTAMP_BUFFER_SIZE;
                self.interval_buffer[idx].store(interval, Ordering::Relaxed);
            }

            // 水位也降频读取
            let water_level = ring_buffer.available();
            let idx = self.water_level_write_idx.fetch_add(1, Ordering::Relaxed)
                % TIMESTAMP_BUFFER_SIZE;
            self.water_level_buffer[idx].store(water_level, Ordering::Relaxed);
        }
    }

    /// 在 render callback 内调用（不使用硬件时间戳）
    ///
    /// 只在采样点才读 now + water_level，减少开销
    #[inline]
    pub fn on_callback(&self, ring_buffer: &RingBuffer<i32>) {
        self.on_callback_with_timestamp(ring_buffer, 0);
    }

    /// 记录 underrun
    #[inline]
    pub fn record_underrun(&self) {
        self.underrun_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 更新已播放样本数
    #[inline]
    pub fn add_samples_played(&self, samples: u64) {
        self.samples_played.fetch_add(samples, Ordering::Relaxed);
    }

    /// 获取 underrun 计数
    #[inline]
    pub fn underrun_count(&self) -> u64 {
        self.underrun_count.load(Ordering::Relaxed)
    }

    /// 获取 callback 计数
    #[inline]
    pub fn callback_count(&self) -> u64 {
        self.callback_count.load(Ordering::Relaxed)
    }

    /// 获取已播放样本数
    #[inline]
    pub fn samples_played(&self) -> u64 {
        self.samples_played.load(Ordering::Relaxed)
    }

    /// 生成报告
    pub fn report(&self, frames_per_callback: u32, sample_rate: u32) -> StatsReport {
        // 期望的单次 callback 间隔（纳秒）
        let expected_interval_ns =
            (frames_per_callback as u64 * 1_000_000_000) / sample_rate as u64;
        // 由于我们每 SAMPLE_INTERVAL 次才采样，期望的采样间隔
        let expected_sampled_interval_ns = expected_interval_ns * SAMPLE_INTERVAL;

        // 收集 interval 数据
        let mut intervals_ns: Vec<u64> = Vec::with_capacity(TIMESTAMP_BUFFER_SIZE);
        for i in 0..TIMESTAMP_BUFFER_SIZE {
            let ticks = self.interval_buffer[i].load(Ordering::Relaxed);
            if ticks > 0 {
                intervals_ns.push(mach_ticks_to_ns(ticks));
            }
        }

        // 收集水位数据
        let mut water_levels: Vec<usize> = Vec::with_capacity(TIMESTAMP_BUFFER_SIZE);
        for i in 0..TIMESTAMP_BUFFER_SIZE {
            let level = self.water_level_buffer[i].load(Ordering::Relaxed);
            // 只收集非零值
            water_levels.push(level);
        }
        water_levels.retain(|&l| l > 0);

        let interval_stats = if intervals_ns.is_empty() {
            IntervalStats {
                min_ns: 0,
                max_ns: 0,
                avg_ns: 0,
            }
        } else {
            IntervalStats {
                min_ns: *intervals_ns.iter().min().unwrap(),
                max_ns: *intervals_ns.iter().max().unwrap(),
                avg_ns: intervals_ns.iter().sum::<u64>() / intervals_ns.len() as u64,
            }
        };

        let water_stats = if water_levels.is_empty() {
            WaterLevelStats { min: 0, max: 0 }
        } else {
            WaterLevelStats {
                min: *water_levels.iter().min().unwrap(),
                max: *water_levels.iter().max().unwrap(),
            }
        };

        StatsReport {
            callback_count: self.callback_count.load(Ordering::Relaxed),
            sample_interval: SAMPLE_INTERVAL,
            expected_sampled_interval_ns,
            interval_stats,
            water_stats,
            underrun_count: self.underrun_count.load(Ordering::Relaxed),
            samples_played: self.samples_played.load(Ordering::Relaxed),
        }
    }

    /// 重置统计
    pub fn reset(&self) {
        self.callback_count.store(0, Ordering::Relaxed);
        self.last_sampled_ticks.store(0, Ordering::Relaxed);
        self.interval_write_idx.store(0, Ordering::Relaxed);
        self.water_level_write_idx.store(0, Ordering::Relaxed);
        self.underrun_count.store(0, Ordering::Relaxed);
        self.samples_played.store(0, Ordering::Relaxed);

        for i in 0..TIMESTAMP_BUFFER_SIZE {
            self.interval_buffer[i].store(0, Ordering::Relaxed);
            self.water_level_buffer[i].store(0, Ordering::Relaxed);
        }
    }
}

impl Default for PlaybackStats {
    fn default() -> Self {
        Self::new()
    }
}

/// 统计报告
#[derive(Debug)]
pub struct StatsReport {
    pub callback_count: u64,
    pub sample_interval: u64,
    pub expected_sampled_interval_ns: u64,
    pub interval_stats: IntervalStats,
    pub water_stats: WaterLevelStats,
    pub underrun_count: u64,
    pub samples_played: u64,
}

#[derive(Debug)]
pub struct IntervalStats {
    pub min_ns: u64,
    pub max_ns: u64,
    pub avg_ns: u64,
}

#[derive(Debug)]
pub struct WaterLevelStats {
    pub min: usize,
    pub max: usize,
}

impl std::fmt::Display for StatsReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Playback Statistics")?;
        writeln!(f, "===================")?;
        writeln!(f, "Total callbacks: {}", self.callback_count)?;
        writeln!(
            f,
            "Stats sample interval: every {} callbacks",
            self.sample_interval
        )?;
        writeln!(f)?;

        writeln!(
            f,
            "Callback Timing (per {} callbacks):",
            self.sample_interval
        )?;
        writeln!(
            f,
            "  Expected: {:.2} ms",
            self.expected_sampled_interval_ns as f64 / 1_000_000.0
        )?;
        writeln!(f, "  Measured:")?;
        writeln!(
            f,
            "    Min: {:.2} ms",
            self.interval_stats.min_ns as f64 / 1_000_000.0
        )?;
        writeln!(
            f,
            "    Max: {:.2} ms",
            self.interval_stats.max_ns as f64 / 1_000_000.0
        )?;
        writeln!(
            f,
            "    Avg: {:.2} ms",
            self.interval_stats.avg_ns as f64 / 1_000_000.0
        )?;

        let jitter_ns = self
            .interval_stats
            .max_ns
            .saturating_sub(self.interval_stats.min_ns);
        let jitter_pct = if self.expected_sampled_interval_ns > 0 {
            jitter_ns as f64 / self.expected_sampled_interval_ns as f64 * 100.0
        } else {
            0.0
        };
        writeln!(
            f,
            "  Jitter: {:.2} ms ({:.1}%)",
            jitter_ns as f64 / 1_000_000.0,
            jitter_pct
        )?;
        writeln!(f)?;

        writeln!(f, "Ring Buffer Water Level:")?;
        writeln!(f, "  Min: {} samples", self.water_stats.min)?;
        writeln!(f, "  Max: {} samples", self.water_stats.max)?;
        writeln!(f)?;

        writeln!(f, "Underruns: {}", self.underrun_count)?;
        writeln!(f, "Samples played: {}", self.samples_played)?;

        Ok(())
    }
}
