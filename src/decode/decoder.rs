//! 音频文件解码器
//!
//! 使用 symphonia 库解码无损音频格式
//! 支持：FLAC, WAV, AIFF, MP3
//!
//! 设计目标：
//! - 整数直通：PCM 整数格式直接转换到 i32，避免 f64 中间表示
//! - 精度保持：16/24/32-bit 源文件无精度损失

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Signal, SignalSpec};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// 解码错误
#[derive(Debug)]
pub enum DecodeError {
    /// 文件打开失败
    FileOpen(std::io::Error),
    /// 格式不支持
    UnsupportedFormat,
    /// 没有找到音频轨道
    NoAudioTrack,
    /// 解码器创建失败
    DecoderCreation(String),
    /// 解码失败
    DecodeFailed(String),
    /// Seek 失败
    SeekFailed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileOpen(e) => write!(f, "Failed to open file: {}", e),
            Self::UnsupportedFormat => write!(f, "Unsupported audio format"),
            Self::NoAudioTrack => write!(f, "No audio track found"),
            Self::DecoderCreation(s) => write!(f, "Failed to create decoder: {}", s),
            Self::DecodeFailed(s) => write!(f, "Decode failed: {}", s),
            Self::SeekFailed(s) => write!(f, "Seek failed: {}", s),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 音频文件信息
#[derive(Debug, Clone)]
pub struct AudioInfo {
    /// 采样率
    pub sample_rate: u32,
    /// 声道数
    pub channels: u32,
    /// 位深度（原始格式）
    pub bit_depth: Option<u32>,
    /// 总帧数（如果已知）
    pub total_frames: Option<u64>,
    /// 总时长（秒）
    pub duration_secs: Option<f64>,
    /// 格式名称
    pub format: String,
    /// 编解码器名称
    pub codec: String,
}

/// 音频文件解码器
pub struct AudioDecoder {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    info: AudioInfo,
    /// i32 样本缓冲区（整数直通路径）
    i32_buffer: Vec<i32>,
    spec: SignalSpec,
}

impl AudioDecoder {
    /// 打开音频文件
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DecodeError> {
        let path = path.as_ref();

        // 打开文件
        let file = File::open(path).map_err(DecodeError::FileOpen)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        // 提示文件扩展名
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        // 探测格式
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let metadata_opts = MetadataOptions::default();

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &metadata_opts)
            .map_err(|_| DecodeError::UnsupportedFormat)?;

        let mut reader = probed.format;
        // 简单起见，直接使用文件扩展名作为格式名称
        // symphonia 的 metadata debug 输出对用户不友好
        let format_name = path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_uppercase())
            .unwrap_or_else(|| "Unknown".to_string());

        // 查找第一个音频轨道
        let track = reader
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or(DecodeError::NoAudioTrack)?;

        let track_id = track.id;
        let codec_params = &track.codec_params;

        // 提取信息
        let sample_rate = codec_params.sample_rate.ok_or(DecodeError::NoAudioTrack)?;
        let channels = codec_params
            .channels
            .map(|c| c.count() as u32)
            .unwrap_or(2);
        let bit_depth = codec_params.bits_per_sample;
        let total_frames = codec_params.n_frames;
        let duration_secs = total_frames.map(|f| f as f64 / sample_rate as f64);

        let codec_name = symphonia::default::get_codecs()
            .get_codec(codec_params.codec)
            .map(|c| c.short_name.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        let info = AudioInfo {
            sample_rate,
            channels,
            bit_depth,
            total_frames,
            duration_secs,
            format: format_name,
            codec: codec_name,
        };

        // 创建解码器
        let decoder_opts = DecoderOptions::default();
        let decoder = symphonia::default::get_codecs()
            .make(codec_params, &decoder_opts)
            .map_err(|e| DecodeError::DecoderCreation(e.to_string()))?;

        let spec = SignalSpec::new(sample_rate, codec_params.channels.unwrap_or_default());

        // 预分配 i32 缓冲区，避免播放时动态分配
        // 8192 frames * 8 channels = 65536 samples（覆盖所有常见格式）
        let i32_buffer = Vec::with_capacity(65536);

        Ok(Self {
            reader,
            decoder,
            track_id,
            info,
            i32_buffer,
            spec,
        })
    }

    /// 获取音频信息
    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    /// 解码下一块数据（整数直通路径）
    ///
    /// 返回交错格式的 i32 样本（左对齐到高位）
    /// 对于整数源格式，避免 f64 中间转换，实现 bit-perfect 路径
    /// 返回空切片表示文件结束
    pub fn decode_next_i32(&mut self) -> Result<&[i32], DecodeError> {
        loop {
            // 读取下一个 packet
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.i32_buffer.clear();
                    return Ok(&self.i32_buffer); // EOF
                }
                Err(e) => return Err(DecodeError::DecodeFailed(e.to_string())),
            };

            // 跳过非目标轨道
            if packet.track_id() != self.track_id {
                continue;
            }

            // 解码
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymphoniaError::DecodeError(_)) => continue, // 跳过损坏的帧
                Err(e) => return Err(DecodeError::DecodeFailed(e.to_string())),
            };

            // 获取帧数和声道数
            let frames = decoded.frames();
            let channels = decoded.spec().channels.count();
            let total_samples = frames * channels;

            // 确保缓冲区长度足够（已预分配 65536 容量，正常情况不会触发分配）
            // resize 在容量足够时只调整长度，不分配内存
            debug_assert!(
                self.i32_buffer.capacity() >= total_samples,
                "Decoded packet ({} samples) exceeds pre-allocated capacity ({})",
                total_samples,
                self.i32_buffer.capacity()
            );
            if self.i32_buffer.len() < total_samples {
                self.i32_buffer.resize(total_samples, 0);
            }

            // 根据源格式直接转换到 i32 左对齐
            // 整数格式：直接位移，无浮点转换（bit-perfect）
            // 浮点格式：转换到 i32
            let i32_buffer = &mut self.i32_buffer;
            match decoded {
                AudioBufferRef::S16(buf) => {
                    // 16-bit → i32: 左移 16 位
                    convert_s16_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::S24(buf) => {
                    // 24-bit → i32: 左移 8 位
                    convert_s24_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::S32(buf) => {
                    // 32-bit → i32: 直接复制
                    convert_s32_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::F32(buf) => {
                    // f32 → i32: 浮点转换
                    convert_f32_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::F64(buf) => {
                    // f64 → i32: 浮点转换
                    convert_f64_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::U8(buf) => {
                    // u8 → i32: 转换为有符号并左移 24 位
                    convert_u8_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::S8(buf) => {
                    // s8 → i32: 左移 24 位
                    convert_s8_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::U16(buf) => {
                    // u16 → i32: 转换为有符号并左移 16 位
                    convert_u16_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::U24(buf) => {
                    // u24 → i32: 转换为有符号并左移 8 位
                    convert_u24_to_i32(&buf, i32_buffer);
                }
                AudioBufferRef::U32(buf) => {
                    // u32 → i32: 转换为有符号
                    convert_u32_to_i32(&buf, i32_buffer);
                }
            }

            return Ok(&self.i32_buffer[..total_samples]);
        }
    }

    /// Seek 到指定时间（秒）
    pub fn seek(&mut self, time_secs: f64) -> Result<(), DecodeError> {
        let seek_to = SeekTo::Time {
            time: Time::new(time_secs as u64, time_secs.fract()),
            track_id: Some(self.track_id),
        };

        self.reader
            .seek(SeekMode::Accurate, seek_to)
            .map_err(|e| DecodeError::SeekFailed(e.to_string()))?;

        // 重置解码器状态
        self.decoder.reset();

        Ok(())
    }

    /// 获取当前位置（帧数）
    pub fn position_frames(&self) -> u64 {
        // 这需要从 reader 获取，但 symphonia 的 API 有限
        // 简化处理
        0
    }
}

/// 最大单次解码样本数（覆盖所有常见格式）
/// 8192 frames * 8 channels = 65536 samples
const MAX_SAMPLES_PER_DECODE: usize = 65536;

/// 双缓冲结构，避免 copy_within
struct DoubleBuffer {
    buffers: [Vec<i32>; 2],
    active: usize,
    /// 当前缓冲区中的有效样本数
    len: usize,
    /// 当前读取位置
    position: usize,
}

impl DoubleBuffer {
    fn new() -> Self {
        Self {
            buffers: [
                Vec::with_capacity(MAX_SAMPLES_PER_DECODE * 2),
                Vec::with_capacity(MAX_SAMPLES_PER_DECODE * 2),
            ],
            active: 0,
            len: 0,
            position: 0,
        }
    }

    /// 获取可读取的样本数
    #[inline]
    fn available(&self) -> usize {
        self.len - self.position
    }

    /// 读取指定数量的样本（返回切片）
    #[inline]
    fn read(&mut self, count: usize) -> &[i32] {
        let available = self.available();
        let to_read = count.min(available);
        let start = self.position;
        self.position += to_read;
        &self.buffers[self.active][start..start + to_read]
    }

    /// 切换到另一个缓冲区并追加数据
    /// 将当前缓冲区剩余数据复制到新缓冲区，然后追加新数据
    fn swap_and_append(&mut self, new_data: &[i32]) {
        let remaining = self.available();
        let next = 1 - self.active;
        let current = self.active;
        let pos = self.position;

        // 确保目标缓冲区容量足够
        let needed = remaining + new_data.len();
        if self.buffers[next].capacity() < needed {
            self.buffers[next].reserve(needed - self.buffers[next].capacity());
        }

        // 使用 split_at_mut 来同时获取两个缓冲区的可变引用
        let (first, second) = self.buffers.split_at_mut(1);
        let (src_buf, dst_buf) = if current == 0 {
            (&first[0], &mut second[0])
        } else {
            (&second[0], &mut first[0])
        };

        // 批量拷贝：剩余数据 + 新数据，用 copy_nonoverlapping 代替 extend_from_slice
        let total = remaining + new_data.len();
        if dst_buf.capacity() < total {
            dst_buf.reserve(total - dst_buf.len());
        }
        unsafe {
            let dst = dst_buf.as_mut_ptr();
            if remaining > 0 {
                std::ptr::copy_nonoverlapping(src_buf.as_ptr().add(pos), dst, remaining);
            }
            std::ptr::copy_nonoverlapping(new_data.as_ptr(), dst.add(remaining), new_data.len());
            dst_buf.set_len(total);
        }

        // 切换
        self.active = next;
        self.len = self.buffers[next].len();
        self.position = 0;
    }

    /// 直接设置数据源（当缓冲区为空时使用）
    ///
    /// 使用 unsafe 的 set_len + copy_nonoverlapping 代替 extend_from_slice，
    /// 避免逐元素 Clone（虽然 i32 是 Copy，编译器通常会优化，但显式 memcpy 更确定）
    fn append(&mut self, data: &[i32]) {
        let buf = &mut self.buffers[self.active];
        let len = data.len();
        // 容量已预分配（MAX_SAMPLES_PER_DECODE * 2），正常情况不触发分配
        if buf.capacity() < len {
            buf.reserve(len - buf.len());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf.as_mut_ptr(), len);
            buf.set_len(len);
        }
        self.len = len;
        self.position = 0;
    }
}

/// 解码器迭代器，用于流式解码
///
/// 使用双缓冲避免 copy_within，减少热路径开销
pub struct DecoderIterator {
    decoder: AudioDecoder,
    /// 双缓冲（i32 直通路径）
    double_buffer: DoubleBuffer,
}

impl DecoderIterator {
    pub fn new(decoder: AudioDecoder) -> Self {
        Self {
            decoder,
            double_buffer: DoubleBuffer::new(),
        }
    }

    /// 获取解码器引用
    pub fn decoder(&self) -> &AudioDecoder {
        &self.decoder
    }

    /// 读取指定数量的 i32 样本
    ///
    /// 返回的样本已左对齐到 i32 高位
    pub fn read_i32(&mut self, count: usize) -> Result<&[i32], DecodeError> {
        // 如果当前缓冲区有足够数据，直接返回切片（零拷贝快速路径）
        if self.double_buffer.available() >= count {
            return Ok(self.double_buffer.read(count));
        }

        // 需要解码更多数据
        // 解码一批数据
        let samples = self.decoder.decode_next_i32()?;
        if samples.is_empty() {
            // EOF - 返回剩余数据
            let remaining = self.double_buffer.available();
            if remaining > 0 {
                return Ok(self.double_buffer.read(remaining));
            }
            return Ok(&[]);
        }

        // 使用双缓冲策略
        if self.double_buffer.available() == 0 {
            // 缓冲区已空，直接使用新数据
            self.double_buffer.append(samples);
        } else {
            // 还有剩余数据，交换缓冲区并合并
            self.double_buffer.swap_and_append(samples);
        }

        // 返回请求的数据量（或全部可用数据）
        let to_return = self.double_buffer.available().min(count);
        Ok(self.double_buffer.read(to_return))
    }

    /// 检查是否到达文件末尾
    pub fn is_eof(&self) -> bool {
        self.double_buffer.available() == 0
    }
}

// ============================================================================
// 独立转换函数（避免借用冲突）
// ============================================================================

/// 转换 i8 样本到 i32 左对齐
#[inline]
fn convert_s8_to_i32(buf: &AudioBuffer<i8>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame] as i32;
            output[frame * channels + ch] = sample << 24;
        }
    }
}

/// 转换 i16 样本到 i32 左对齐
///
/// 使用 SIMD 加速（ARM NEON）实现向量化转换
#[inline]
fn convert_s16_to_i32(buf: &AudioBuffer<i16>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();

    // 立体声 + ARM64 SIMD 优化路径
    #[cfg(target_arch = "aarch64")]
    if channels == 2 {
        convert_s16_to_i32_stereo_neon(buf, output, frames);
        return;
    }

    // 标量回退路径
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame] as i32;
            output[frame * channels + ch] = sample << 16;
        }
    }
}

/// NEON 优化的立体声 i16→i32 转换
#[cfg(target_arch = "aarch64")]
#[inline]
fn convert_s16_to_i32_stereo_neon(buf: &AudioBuffer<i16>, output: &mut [i32], frames: usize) {
    use std::arch::aarch64::*;

    let left = buf.chan(0);
    let right = buf.chan(1);

    // 每次处理 4 帧（8 个样本）
    let chunks = frames / 4;

    for chunk in 0..chunks {
        let i = chunk * 4;
        unsafe {
            // 加载 4 个左声道和 4 个右声道样本
            let left_s16 = vld1_s16(left.as_ptr().add(i));   // 4 x i16
            let right_s16 = vld1_s16(right.as_ptr().add(i)); // 4 x i16

            // 扩展到 i32
            let left_s32 = vmovl_s16(left_s16);   // 4 x i32
            let right_s32 = vmovl_s16(right_s16); // 4 x i32

            // 左移 16 位（左对齐）
            let left_shifted = vshlq_n_s32(left_s32, 16);
            let right_shifted = vshlq_n_s32(right_s32, 16);

            // 交织写入输出（L0, R0, L1, R1, L2, R2, L3, R3）
            // vst2q_s32 一条指令完成交织 + 存储，替代 8 次标量 store
            let pair = int32x4x2_t(left_shifted, right_shifted);
            vst2q_s32(output.as_mut_ptr().add(i * 2), pair);
        }
    }

    // 处理剩余帧
    for frame in (chunks * 4)..frames {
        let out_idx = frame * 2;
        output[out_idx] = (left[frame] as i32) << 16;
        output[out_idx + 1] = (right[frame] as i32) << 16;
    }
}

/// 转换 i24 样本到 i32 左对齐
///
/// 使用 SIMD 加速（ARM NEON）实现向量化转换
#[inline]
fn convert_s24_to_i32(buf: &AudioBuffer<symphonia::core::sample::i24>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();

    // 立体声 + ARM64 SIMD 优化路径
    #[cfg(target_arch = "aarch64")]
    if channels == 2 {
        convert_s24_to_i32_stereo_neon(buf, output, frames);
        return;
    }

    // 标量回退路径
    for frame in 0..frames {
        for ch in 0..channels {
            // i24 内部是 i32，需要左移 8 位对齐到高位
            let sample = buf.chan(ch)[frame].inner();
            output[frame * channels + ch] = sample << 8;
        }
    }
}

/// NEON 优化的立体声 i24→i32 转换
#[cfg(target_arch = "aarch64")]
#[inline]
fn convert_s24_to_i32_stereo_neon(
    buf: &AudioBuffer<symphonia::core::sample::i24>,
    output: &mut [i32],
    frames: usize,
) {
    use std::arch::aarch64::*;

    let left = buf.chan(0);
    let right = buf.chan(1);

    // 每次处理 4 帧（8 个样本）
    let chunks = frames / 4;

    for chunk in 0..chunks {
        let i = chunk * 4;
        unsafe {
            // i24 的内部存储是 i32，可以直接加载
            // 加载 4 个左声道样本（i24 → i32）
            let left_raw = [
                left[i].inner(),
                left[i + 1].inner(),
                left[i + 2].inner(),
                left[i + 3].inner(),
            ];
            let right_raw = [
                right[i].inner(),
                right[i + 1].inner(),
                right[i + 2].inner(),
                right[i + 3].inner(),
            ];

            // 加载到 NEON 寄存器
            let left_s32 = vld1q_s32(left_raw.as_ptr());
            let right_s32 = vld1q_s32(right_raw.as_ptr());

            // 左移 8 位（左对齐）
            let left_shifted = vshlq_n_s32(left_s32, 8);
            let right_shifted = vshlq_n_s32(right_s32, 8);

            // 交织写入输出（L0, R0, L1, R1, L2, R2, L3, R3）
            let pair = int32x4x2_t(left_shifted, right_shifted);
            vst2q_s32(output.as_mut_ptr().add(i * 2), pair);
        }
    }

    // 处理剩余帧
    for frame in (chunks * 4)..frames {
        let out_idx = frame * 2;
        output[out_idx] = left[frame].inner() << 8;
        output[out_idx + 1] = right[frame].inner() << 8;
    }
}

/// 转换 i32 样本（直接复制）
///
/// 立体声 NEON 优化：vld1q + vst2q 交织写入
#[inline]
fn convert_s32_to_i32(buf: &AudioBuffer<i32>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();

    #[cfg(target_arch = "aarch64")]
    if channels == 2 {
        convert_s32_to_i32_stereo_neon(buf, output, frames);
        return;
    }

    for frame in 0..frames {
        for ch in 0..channels {
            output[frame * channels + ch] = buf.chan(ch)[frame];
        }
    }
}

/// NEON 优化的立体声 i32→i32 交织拷贝
#[cfg(target_arch = "aarch64")]
#[inline]
fn convert_s32_to_i32_stereo_neon(buf: &AudioBuffer<i32>, output: &mut [i32], frames: usize) {
    use std::arch::aarch64::*;

    let left = buf.chan(0);
    let right = buf.chan(1);
    let chunks = frames / 4;

    for chunk in 0..chunks {
        let i = chunk * 4;
        unsafe {
            let left_s32 = vld1q_s32(left.as_ptr().add(i));
            let right_s32 = vld1q_s32(right.as_ptr().add(i));
            let pair = int32x4x2_t(left_s32, right_s32);
            vst2q_s32(output.as_mut_ptr().add(i * 2), pair);
        }
    }

    for frame in (chunks * 4)..frames {
        let out_idx = frame * 2;
        output[out_idx] = left[frame];
        output[out_idx + 1] = right[frame];
    }
}

/// 转换 f32 样本到 i32 左对齐
///
/// 立体声 NEON 优化：向量化 clamp + f32→i32 转换
#[inline]
fn convert_f32_to_i32(buf: &AudioBuffer<f32>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();

    #[cfg(target_arch = "aarch64")]
    if channels == 2 {
        convert_f32_to_i32_stereo_neon(buf, output, frames);
        return;
    }

    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame];
            let clamped = sample.clamp(-1.0, 1.0);
            output[frame * channels + ch] = (clamped * i32::MAX as f32) as i32;
        }
    }
}

/// NEON 优化的立体声 f32→i32 转换
#[cfg(target_arch = "aarch64")]
#[inline]
fn convert_f32_to_i32_stereo_neon(buf: &AudioBuffer<f32>, output: &mut [i32], frames: usize) {
    use std::arch::aarch64::*;

    let left = buf.chan(0);
    let right = buf.chan(1);
    let chunks = frames / 4;

    unsafe {
        let min_val = vdupq_n_f32(-1.0);
        let max_val = vdupq_n_f32(1.0);
        let scale = vdupq_n_f32(i32::MAX as f32);

        for chunk in 0..chunks {
            let i = chunk * 4;

            // 加载 4 个左/右声道样本
            let left_f32 = vld1q_f32(left.as_ptr().add(i));
            let right_f32 = vld1q_f32(right.as_ptr().add(i));

            // clamp [-1.0, 1.0]
            let left_clamped = vminq_f32(vmaxq_f32(left_f32, min_val), max_val);
            let right_clamped = vminq_f32(vmaxq_f32(right_f32, min_val), max_val);

            // 乘以 i32::MAX
            let left_scaled = vmulq_f32(left_clamped, scale);
            let right_scaled = vmulq_f32(right_clamped, scale);

            // f32 → i32
            let left_i32 = vcvtq_s32_f32(left_scaled);
            let right_i32 = vcvtq_s32_f32(right_scaled);

            // 交织写入
            let pair = int32x4x2_t(left_i32, right_i32);
            vst2q_s32(output.as_mut_ptr().add(i * 2), pair);
        }
    }

    // 剩余帧标量处理
    for frame in (chunks * 4)..frames {
        let out_idx = frame * 2;
        let l = left[frame].clamp(-1.0, 1.0);
        let r = right[frame].clamp(-1.0, 1.0);
        output[out_idx] = (l * i32::MAX as f32) as i32;
        output[out_idx + 1] = (r * i32::MAX as f32) as i32;
    }
}

/// 转换 f64 样本到 i32 左对齐
#[inline]
fn convert_f64_to_i32(buf: &AudioBuffer<f64>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame];
            let clamped = sample.clamp(-1.0, 1.0);
            output[frame * channels + ch] = (clamped * i32::MAX as f64) as i32;
        }
    }
}

/// 转换 u8 样本到 i32 左对齐
#[inline]
fn convert_u8_to_i32(buf: &AudioBuffer<u8>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            // u8 [0, 255] → 有符号 [-128, 127] → i32 左对齐
            let sample = buf.chan(ch)[frame] as i32 - 128;
            output[frame * channels + ch] = sample << 24;
        }
    }
}

/// 转换 u16 样本到 i32 左对齐
#[inline]
fn convert_u16_to_i32(buf: &AudioBuffer<u16>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame] as i32 - 32768;
            output[frame * channels + ch] = sample << 16;
        }
    }
}

/// 转换 u24 样本到 i32 左对齐
#[inline]
fn convert_u24_to_i32(buf: &AudioBuffer<symphonia::core::sample::u24>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame].inner() as i32 - 8388608;
            output[frame * channels + ch] = sample << 8;
        }
    }
}

/// 转换 u32 样本到 i32 左对齐
#[inline]
fn convert_u32_to_i32(buf: &AudioBuffer<u32>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            // u32 → i32: 减去 2^31
            let sample = buf.chan(ch)[frame].wrapping_sub(2147483648) as i32;
            output[frame * channels + ch] = sample;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // 需要实际音频文件
    fn test_decode_flac() {
        let decoder = AudioDecoder::open("test.flac").unwrap();
        let info = decoder.info();
        println!("Info: {:?}", info);
    }
}
