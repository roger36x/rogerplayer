//! 播放引擎
//!
//! 整合解码、缓冲、输出各模块
//! 核心设计：解码线程和输出回调完全解耦，通过 lock-free ring buffer 连接

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::audio::{AudioFormat, AudioOutput, OutputConfig, PlaybackStats, RingBuffer};
use crate::decode::{AudioDecoder, AudioInfo, DecoderIterator};

/// 播放状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
    Buffering,
}

/// 引擎配置
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// 输出配置
    pub output: OutputConfig,
    /// Ring buffer 大小（样本数，会被向上取整到 2 的幂）
    /// 越大越稳定，但延迟也越高
    pub buffer_frames: usize,
    /// 预缓冲比例（0.0-1.0）
    /// 开始播放前需要填充到这个比例
    pub prebuffer_ratio: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            output: OutputConfig::default(),
            // 2秒缓冲 @ 48kHz 立体声
            buffer_frames: 48000 * 2 * 2,
            // 50% 预缓冲
            prebuffer_ratio: 0.5,
        }
    }
}

/// 引擎错误
#[derive(Debug)]
pub enum EngineError {
    DecodeError(crate::decode::DecodeError),
    OutputError(crate::audio::OutputError),
    InvalidState(&'static str),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecodeError(e) => write!(f, "Decode error: {}", e),
            Self::OutputError(e) => write!(f, "Output error: {}", e),
            Self::InvalidState(s) => write!(f, "Invalid state: {}", s),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<crate::decode::DecodeError> for EngineError {
    fn from(e: crate::decode::DecodeError) -> Self {
        Self::DecodeError(e)
    }
}

impl From<crate::audio::OutputError> for EngineError {
    fn from(e: crate::audio::OutputError) -> Self {
        Self::OutputError(e)
    }
}

/// 播放引擎统计
#[derive(Debug, Clone)]
pub struct EngineStats {
    /// 缓冲区填充比例
    pub buffer_fill_ratio: f64,
    /// Underrun 次数
    pub underrun_count: u64,
    /// 已播放样本数
    pub samples_played: u64,
    /// 当前播放时间（秒）
    pub position_secs: f64,
}

/// 解码线程共享状态
///
/// 完全基于原子操作，无锁设计
struct DecoderState {
    /// 是否应该继续运行
    running: AtomicBool,
    /// 是否暂停解码
    paused: AtomicBool,
    /// 解码是否已到达 EOF
    eof_reached: AtomicBool,
    /// 已解码样本数
    samples_decoded: AtomicU64,
}

/// 播放引擎
pub struct Engine {
    config: EngineConfig,
    state: PlaybackState,
    ring_buffer: Arc<RingBuffer<i32>>,
    stats: Arc<PlaybackStats>,
    output: Option<AudioOutput>,
    decoder_thread: Option<JoinHandle<()>>,
    decoder_state: Arc<DecoderState>,
    current_info: Option<AudioInfo>,
    current_format: Option<AudioFormat>,
}

impl Engine {
    /// 创建新引擎
    pub fn new(config: EngineConfig) -> Self {
        // 向上取整到 2 的幂
        let buffer_capacity = config.buffer_frames.next_power_of_two();
        let ring_buffer = Arc::new(RingBuffer::new(buffer_capacity));
        let stats = Arc::new(PlaybackStats::new());
        let decoder_state = Arc::new(DecoderState {
            running: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            eof_reached: AtomicBool::new(false),
            samples_decoded: AtomicU64::new(0),
        });

        Self {
            config,
            state: PlaybackState::Stopped,
            ring_buffer,
            stats,
            output: None,
            decoder_thread: None,
            decoder_state,
            current_info: None,
            current_format: None,
        }
    }

    /// 加载并播放文件
    pub fn play<P: AsRef<Path>>(&mut self, path: P) -> Result<(), EngineError> {
        // 如果正在播放，先停止
        if self.state != PlaybackState::Stopped {
            self.stop()?;
        }

        let path = path.as_ref();
        log::info!("Loading: {}", path.display());

        // 打开解码器
        let decoder = AudioDecoder::open(path)?;
        let info = decoder.info().clone();

        log::info!(
            "Format: {} | Codec: {} | {}Hz {}ch {}bit | Duration: {:.1}s",
            info.format,
            info.codec,
            info.sample_rate,
            info.channels,
            info.bit_depth.unwrap_or(0),
            info.duration_secs.unwrap_or(0.0)
        );

        // 创建音频格式
        let bit_depth = info.bit_depth.unwrap_or(24) as u16;
        let source_sample_rate = info.sample_rate;

        // 配置输出采样率为源文件采样率（作为请求）
        let mut output_config = self.config.output.clone();
        output_config.sample_rate = source_sample_rate;

        // 创建输出
        let mut output = AudioOutput::new(output_config)?;

        // 查询设备实际采样率
        let device_sample_rate = output.target_sample_rate(source_sample_rate);
        let needs_src = source_sample_rate != device_sample_rate;

        // 使用 CoreAudio 内置 SRC
        // ring buffer 中的数据是 source rate，CoreAudio 会自动转换到 device rate
        if needs_src {
            log::info!(
                "CoreAudio SRC: {}Hz → {}Hz",
                source_sample_rate, device_sample_rate
            );
        }
        let format = AudioFormat::new(source_sample_rate, info.channels as u16, bit_depth);

        // 清空缓冲区
        self.ring_buffer.clear();
        self.stats.reset();

        // 启动输出
        output.start(
            format,
            Arc::clone(&self.ring_buffer),
            Arc::clone(&self.stats),
        )?;

        // 启动解码线程
        self.decoder_state.running.store(true, Ordering::Release);
        self.decoder_state.paused.store(false, Ordering::Release);
        self.decoder_state.eof_reached.store(false, Ordering::Release);
        self.decoder_state
            .samples_decoded
            .store(0, Ordering::Release);

        let decoder_state = Arc::clone(&self.decoder_state);
        let ring_buffer = Arc::clone(&self.ring_buffer);
        let prebuffer_ratio = self.config.prebuffer_ratio;
        let channels = info.channels as usize;
        let sample_rate = source_sample_rate;

        let decoder_thread = thread::Builder::new()
            .name("decoder".to_string())
            .spawn(move || {
                Self::decoder_thread_main(
                    decoder,
                    ring_buffer,
                    decoder_state,
                    prebuffer_ratio,
                    channels,
                    sample_rate,
                );
            })
            .expect("Failed to spawn decoder thread");

        self.output = Some(output);
        self.decoder_thread = Some(decoder_thread);
        self.current_info = Some(info);
        self.current_format = Some(format);
        self.state = PlaybackState::Buffering;

        Ok(())
    }

    /// 解码线程主函数
    ///
    /// 使用整数直通路径：对于整数源格式，避免 f64 中间转换
    /// SRC 由 CoreAudio 内部处理
    fn decoder_thread_main(
        decoder: AudioDecoder,
        ring_buffer: Arc<RingBuffer<i32>>,
        state: Arc<DecoderState>,
        prebuffer_ratio: f64,
        channels: usize,
        sample_rate: u32,
    ) {
        // 设置较高的线程优先级（但不是实时，避免影响 CoreAudio IO 线程）
        Self::set_decoder_thread_priority();

        let mut iter = DecoderIterator::new(decoder);

        // 预缓冲目标
        let prebuffer_samples = (ring_buffer.capacity() as f64 * prebuffer_ratio) as usize;
        let mut prebuffered = false;

        // 读取块大小
        let read_chunk_size = 4096 * channels;

        // 自适应等待参数
        // 计算消耗速率：每微秒消耗多少样本
        // samples_per_us = sample_rate * channels / 1_000_000
        let samples_per_us = (sample_rate as u64 * channels as u64) as f64 / 1_000_000.0;
        let min_free_threshold = 1024 * channels;

        log::info!(
            "Decoder thread started, prebuffer target: {} samples, consumption rate: {:.2} samples/µs",
            prebuffer_samples,
            samples_per_us
        );

        while state.running.load(Ordering::Acquire) {
            // 检查暂停 - 使用 spin + yield 等待，完全无锁
            // 比 Condvar 更简单，避免 Mutex 的优先级反转风险
            if state.paused.load(Ordering::Acquire) {
                // 短暂自旋（适合极短暂停）
                for _ in 0..16 {
                    std::hint::spin_loop();
                    if !state.paused.load(Ordering::Relaxed) {
                        break;
                    }
                }
                // 仍在暂停则让出 CPU 并短暂睡眠
                if state.paused.load(Ordering::Acquire) {
                    thread::yield_now();
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                continue;
            }

            // 检查缓冲区是否有空间
            let available_write = ring_buffer.free_space();

            if available_write < min_free_threshold {
                // 缓冲区快满了 - 使用自适应等待策略
                // 计算需要等待多久才能有足够空间
                let samples_needed = min_free_threshold - available_write;
                let wait_us = (samples_needed as f64 / samples_per_us) as u64;

                // 根据等待时间选择策略：
                // - < 50µs: 仅自旋（避免 syscall 开销）
                // - 50-500µs: yield + 短自旋
                // - > 500µs: 睡眠（节省 CPU）
                if wait_us < 50 {
                    // 短等待：纯自旋
                    for _ in 0..64 {
                        std::hint::spin_loop();
                    }
                } else if wait_us < 500 {
                    // 中等等待：yield 后短自旋
                    thread::yield_now();
                    for _ in 0..32 {
                        std::hint::spin_loop();
                    }
                } else {
                    // 长等待：睡眠 70% 的预计时间（留出余量）
                    let sleep_us = (wait_us * 7 / 10).max(100).min(2000);
                    thread::sleep(std::time::Duration::from_micros(sleep_us));
                }
                continue;
            }

            // 解码（整数直通路径）
            // 对于 PCM 整数源，直接转换到 i32，避免 f64 中间表示
            let samples_to_read = available_write.min(read_chunk_size);
            match iter.read_i32(samples_to_read) {
                Ok(samples) => {
                    if samples.is_empty() {
                        // EOF - 设置标志，让上层知道解码已完成
                        state.eof_reached.store(true, Ordering::Release);
                        log::info!("Decoder reached end of file");
                        break;
                    }

                    // 直接写入 ring buffer（SRC 由 CoreAudio 处理）
                    let samples_written = ring_buffer.write(samples);

                    state
                        .samples_decoded
                        .fetch_add(samples_written as u64, Ordering::Relaxed);

                    // 检查预缓冲是否完成
                    if !prebuffered && ring_buffer.available() >= prebuffer_samples {
                        prebuffered = true;
                        log::info!("Prebuffer complete");
                    }
                }
                Err(e) => {
                    log::error!("Decode error: {}", e);
                    break;
                }
            }
        }

        log::info!("Decoder thread finished");
    }

    /// 设置解码线程优先级
    ///
    /// 优化策略（按优先级顺序）：
    /// 1. QoS 标记 - 告诉系统这是用户交互敏感任务（总是成功）
    /// 2. 实时调度策略 - 使用 Mach THREAD_TIME_CONSTRAINT_POLICY（不需要 root）
    /// 3. 后备方案 - nice 值
    fn set_decoder_thread_priority() {
        #[cfg(target_os = "macos")]
        {
            // === 1. 设置 QoS 类（总是成功，无需权限）===
            Self::set_qos_class();

            // === 2. 设置实时调度策略（不需要 root）===
            Self::set_realtime_priority();
        }
    }

    /// 设置 QoS 类为 User Interactive
    ///
    /// 告诉系统调度器这是用户交互敏感任务，减少被抢占概率
    #[cfg(target_os = "macos")]
    fn set_qos_class() {
        // QOS_CLASS_USER_INTERACTIVE = 0x21
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }

        unsafe {
            let result = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
            if result == 0 {
                log::debug!("QoS class set to USER_INTERACTIVE");
            } else {
                log::debug!("Failed to set QoS class (errno: {})", result);
            }
        }
    }

    /// 设置 Mach 实时调度策略
    ///
    /// 使用 THREAD_TIME_CONSTRAINT_POLICY 告诉调度器：
    /// - period: 解码周期（5ms）
    /// - computation: 每周期需要的计算时间（2ms）
    /// - constraint: 最大延迟容忍（4ms）
    ///
    /// 这不需要 root 权限，系统会尽量满足请求
    #[cfg(target_os = "macos")]
    fn set_realtime_priority() {
        #[repr(C)]
        struct ThreadTimeConstraintPolicy {
            period: u32,
            computation: u32,
            constraint: u32,
            preemptible: u32,
        }

        const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;

        extern "C" {
            fn pthread_mach_thread_np(thread: libc::pthread_t) -> u32;
            fn thread_policy_set(
                thread: u32,
                flavor: u32,
                policy_info: *const std::ffi::c_void,
                count: u32,
            ) -> i32;
        }

        unsafe {
            let thread = pthread_mach_thread_np(libc::pthread_self());

            // 时间约束策略（单位：Mach 绝对时间，约等于纳秒）
            // period: 5ms - 解码周期
            // computation: 2ms - 每周期需要的计算时间
            // constraint: 4ms - 必须在这个时间内完成
            let policy = ThreadTimeConstraintPolicy {
                period: 5_000_000,       // 5ms
                computation: 2_000_000,  // 2ms
                constraint: 4_000_000,   // 4ms
                preemptible: 1,          // 可被更高优先级抢占
            };

            // count = struct 中 u32 的数量
            let result = thread_policy_set(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                &policy as *const _ as *const std::ffi::c_void,
                4,
            );

            if result == 0 {
                log::debug!(
                    "Realtime priority set: period=5ms, computation=2ms, constraint=4ms"
                );
            } else {
                log::debug!(
                    "Failed to set realtime priority (kern_return: {}), using default scheduling",
                    result
                );
                // 后备方案：设置 nice 值
                libc::setpriority(libc::PRIO_PROCESS, 0, -10);
            }
        }
    }

    /// 停止播放
    pub fn stop(&mut self) -> Result<(), EngineError> {
        // 停止解码线程
        self.decoder_state.running.store(false, Ordering::Release);
        // 解除暂停状态（如果有），确保解码线程能退出
        self.decoder_state.paused.store(false, Ordering::Release);

        if let Some(thread) = self.decoder_thread.take() {
            let _ = thread.join();
        }

        // 停止输出
        if let Some(mut output) = self.output.take() {
            output.stop()?;
        }

        self.ring_buffer.clear();
        self.state = PlaybackState::Stopped;
        self.current_info = None;
        self.current_format = None;

        log::info!("Playback stopped");

        Ok(())
    }

    /// 暂停/恢复
    pub fn toggle_pause(&mut self) -> Result<(), EngineError> {
        // 先同步状态：如果缓冲已完成但内部状态仍是 Buffering，更新为 Playing
        if self.state == PlaybackState::Buffering {
            let fill_ratio = self.ring_buffer.fill_ratio();
            if fill_ratio >= self.config.prebuffer_ratio {
                self.state = PlaybackState::Playing;
            }
        }

        match self.state {
            PlaybackState::Playing => {
                // 暂停解码线程
                self.decoder_state.paused.store(true, Ordering::Release);
                // 暂停音频输出（立即静音）
                if let Some(ref mut output) = self.output {
                    output.pause()?;
                }
                self.state = PlaybackState::Paused;
                log::info!("Paused");
            }
            PlaybackState::Paused | PlaybackState::Buffering => {
                // 恢复音频输出
                if let Some(ref mut output) = self.output {
                    output.resume()?;
                }
                // 恢复解码线程（原子写入，解码线程会在下次循环检测到）
                self.decoder_state.paused.store(false, Ordering::Release);
                self.state = PlaybackState::Playing;
                log::info!("Resumed");
            }
            PlaybackState::Stopped => {
                return Err(EngineError::InvalidState("Cannot pause when stopped"));
            }
        }
        Ok(())
    }

    /// 获取当前状态
    pub fn state(&self) -> PlaybackState {
        // 检查是否从 Buffering 转为 Playing
        if self.state == PlaybackState::Buffering {
            let fill_ratio = self.ring_buffer.fill_ratio();
            if fill_ratio >= self.config.prebuffer_ratio {
                return PlaybackState::Playing;
            }
        }
        self.state
    }

    /// 获取统计信息
    pub fn stats(&self) -> EngineStats {
        let buffer_fill_ratio = self.ring_buffer.fill_ratio();
        let underrun_count = self.stats.underrun_count();
        let samples_played = self.stats.samples_played();
        let sample_rate = self
            .current_info
            .as_ref()
            .map(|i| i.sample_rate)
            .unwrap_or(48000);
        let channels = self.current_info.as_ref().map(|i| i.channels).unwrap_or(2);
        let frames_played = samples_played / channels as u64;
        let position_secs = frames_played as f64 / sample_rate as f64;

        EngineStats {
            buffer_fill_ratio,
            underrun_count,
            samples_played,
            position_secs,
        }
    }

    /// 获取详细统计报告
    pub fn stats_report(&self) -> Option<crate::audio::StatsReport> {
        let info = self.current_info.as_ref()?;
        let buffer_frames = self.config.output.buffer_frames;
        Some(self.stats.report(buffer_frames, info.sample_rate))
    }

    /// 获取当前文件信息
    pub fn current_info(&self) -> Option<&AudioInfo> {
        self.current_info.as_ref()
    }

    /// 检查是否正在播放
    pub fn is_playing(&self) -> bool {
        matches!(
            self.state(),
            PlaybackState::Playing | PlaybackState::Buffering
        )
    }

    /// 检查当前音轨是否已播放完毕
    ///
    /// 条件：解码到达 EOF 且缓冲区已被消费完
    pub fn is_track_finished(&self) -> bool {
        self.decoder_state.eof_reached.load(Ordering::Acquire)
            && self.ring_buffer.available() == 0
    }

    /// 获取输出模式信息
    ///
    /// 返回 (是否为HAL直接输出, 是否为独占模式)
    pub fn output_mode(&self) -> Option<(bool, bool)> {
        self.output.as_ref().map(|o| (o.is_hal_output(), o.is_exclusive_mode()))
    }

    /// 检查是否为 bit-perfect 输出
    ///
    /// Bit-perfect 意味着：
    /// - HAL 直接输出（绕过系统混音器）
    /// - 独占模式
    /// - 整数格式（无浮点转换）
    /// - 无采样率转换（SRC）
    pub fn is_bit_perfect(&self) -> bool {
        let source_rate = self.current_info
            .as_ref()
            .map(|i| i.sample_rate)
            .unwrap_or(0);

        self.output
            .as_ref()
            .map(|o| o.is_bit_perfect(source_rate))
            .unwrap_or(false)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_config_default() {
        let config = EngineConfig::default();
        assert_eq!(config.buffer_frames, 48000 * 2 * 2);
        assert_eq!(config.prebuffer_ratio, 0.5);
    }
}
