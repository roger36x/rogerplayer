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

use symphonia::core::audio::{AudioBuffer, AudioBufferRef, SampleBuffer, Signal, SignalSpec};
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
    sample_buffer: Option<SampleBuffer<f64>>,
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
        let format_name = reader
            .metadata()
            .current()
            .map(|m| format!("{:?}", m))
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

        Ok(Self {
            reader,
            decoder,
            track_id,
            info,
            sample_buffer: None,
            i32_buffer: Vec::new(),
            spec,
        })
    }

    /// 获取音频信息
    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    /// 解码下一块数据
    ///
    /// 返回交错格式的 f64 样本
    /// 返回空 Vec 表示文件结束
    pub fn decode_next(&mut self) -> Result<Vec<f64>, DecodeError> {
        loop {
            // 读取下一个 packet
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(Vec::new()); // EOF
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

            // 转换为 f64 样本
            let spec = *decoded.spec();
            let duration = decoded.capacity();

            // 确保 sample buffer 容量足够
            if self.sample_buffer.is_none()
                || self.sample_buffer.as_ref().unwrap().capacity() < duration
            {
                self.sample_buffer = Some(SampleBuffer::new(duration as u64, spec));
                self.spec = spec;
            }

            let sample_buffer = self.sample_buffer.as_mut().unwrap();
            sample_buffer.copy_interleaved_ref(decoded);

            return Ok(sample_buffer.samples().to_vec());
        }
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

            // 确保缓冲区容量足够
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

/// 简单的解码器迭代器，用于流式解码
pub struct DecoderIterator {
    decoder: AudioDecoder,
    buffer: Vec<f64>,
    position: usize,
    /// i32 缓冲区（整数直通路径）
    i32_buffer: Vec<i32>,
    i32_position: usize,
}

impl DecoderIterator {
    pub fn new(decoder: AudioDecoder) -> Self {
        Self {
            decoder,
            buffer: Vec::new(),
            position: 0,
            i32_buffer: Vec::new(),
            i32_position: 0,
        }
    }

    /// 获取解码器引用
    pub fn decoder(&self) -> &AudioDecoder {
        &self.decoder
    }

    /// 读取指定数量的样本
    pub fn read(&mut self, count: usize) -> Result<Vec<f64>, DecodeError> {
        let mut result = Vec::with_capacity(count);

        while result.len() < count {
            // 先从缓冲区读取
            if self.position < self.buffer.len() {
                let available = self.buffer.len() - self.position;
                let to_copy = available.min(count - result.len());
                result.extend_from_slice(&self.buffer[self.position..self.position + to_copy]);
                self.position += to_copy;
            } else {
                // 解码更多数据
                self.buffer = self.decoder.decode_next()?;
                self.position = 0;

                if self.buffer.is_empty() {
                    break; // EOF
                }
            }
        }

        Ok(result)
    }

    /// 读取指定数量的 i32 样本（整数直通路径）
    ///
    /// 对于整数源格式，避免 f64 中间转换
    /// 返回的样本已左对齐到 i32 高位
    pub fn read_i32(&mut self, count: usize) -> Result<&[i32], DecodeError> {
        // 如果当前缓冲区有足够数据，直接返回切片
        let available = self.i32_buffer.len() - self.i32_position;
        if available >= count {
            let start = self.i32_position;
            self.i32_position += count;
            return Ok(&self.i32_buffer[start..start + count]);
        }

        // 需要解码更多数据
        // 先将剩余数据移到开头
        if self.i32_position > 0 && available > 0 {
            self.i32_buffer.copy_within(self.i32_position.., 0);
            self.i32_buffer.truncate(available);
        } else {
            self.i32_buffer.clear();
        }
        self.i32_position = 0;

        // 解码直到有足够数据
        while self.i32_buffer.len() < count {
            let samples = self.decoder.decode_next_i32()?;
            if samples.is_empty() {
                break; // EOF
            }
            self.i32_buffer.extend_from_slice(samples);
        }

        // 返回可用数据
        let to_return = self.i32_buffer.len().min(count);
        self.i32_position = to_return;
        Ok(&self.i32_buffer[..to_return])
    }

    /// 检查是否到达文件末尾
    pub fn is_eof(&self) -> bool {
        self.position >= self.buffer.len() && self.buffer.is_empty()
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
            let out_idx = i * 2;
            let l_arr: [i32; 4] = std::mem::transmute(left_shifted);
            let r_arr: [i32; 4] = std::mem::transmute(right_shifted);

            output[out_idx] = l_arr[0];
            output[out_idx + 1] = r_arr[0];
            output[out_idx + 2] = l_arr[1];
            output[out_idx + 3] = r_arr[1];
            output[out_idx + 4] = l_arr[2];
            output[out_idx + 5] = r_arr[2];
            output[out_idx + 6] = l_arr[3];
            output[out_idx + 7] = r_arr[3];
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

            // 交织写入输出
            let out_idx = i * 2;
            let l_arr: [i32; 4] = std::mem::transmute(left_shifted);
            let r_arr: [i32; 4] = std::mem::transmute(right_shifted);

            output[out_idx] = l_arr[0];
            output[out_idx + 1] = r_arr[0];
            output[out_idx + 2] = l_arr[1];
            output[out_idx + 3] = r_arr[1];
            output[out_idx + 4] = l_arr[2];
            output[out_idx + 5] = r_arr[2];
            output[out_idx + 6] = l_arr[3];
            output[out_idx + 7] = r_arr[3];
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
#[inline]
fn convert_s32_to_i32(buf: &AudioBuffer<i32>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            output[frame * channels + ch] = buf.chan(ch)[frame];
        }
    }
}

/// 转换 f32 样本到 i32 左对齐
#[inline]
fn convert_f32_to_i32(buf: &AudioBuffer<f32>, output: &mut [i32]) {
    let channels = buf.spec().channels.count();
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels {
            let sample = buf.chan(ch)[frame];
            let clamped = sample.clamp(-1.0, 1.0);
            output[frame * channels + ch] = (clamped * i32::MAX as f32) as i32;
        }
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
