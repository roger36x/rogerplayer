//! 音频格式和样本编解码
//!
//! 内部表示：所有位深统一左对齐到 i32 的高位
//! - 16-bit: 占据 bit[31:16]，bit[15:0] = 0
//! - 24-bit: 占据 bit[31:8]，bit[7:0] = 0
//! - 32-bit: 占据 bit[31:0]

/// 输出布局
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputLayout {
    /// 交织：LRLRLR...，所有样本在 mBuffers[0]
    Interleaved,
    /// 非交织：每声道独立 buffer，mBuffers[0]=L, mBuffers[1]=R
    NonInterleaved,
}

impl Default for OutputLayout {
    fn default() -> Self {
        Self::Interleaved
    }
}

/// 音频格式
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub layout: OutputLayout,
}

impl AudioFormat {
    /// 创建新的音频格式
    pub fn new(sample_rate: u32, channels: u16, bits_per_sample: u16) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_sample,
            layout: OutputLayout::default(),
        }
    }

    /// 每帧的样本数（= 声道数）
    #[inline]
    pub fn samples_per_frame(&self) -> usize {
        self.channels as usize
    }

    /// 每帧的字节数
    #[inline]
    pub fn bytes_per_frame(&self) -> usize {
        (self.bits_per_sample as usize / 8) * self.channels as usize
    }

    /// 每样本的字节数
    #[inline]
    pub fn bytes_per_sample(&self) -> usize {
        self.bits_per_sample as usize / 8
    }

    /// 将原始字节解码为 i32 样本（左对齐到 32-bit）
    ///
    /// 内部表示：所有位深统一左对齐到 i32 的高位
    /// - 16-bit: 占据 bit[31:16]，bit[15:0] = 0
    /// - 24-bit: 占据 bit[31:8]，bit[7:0] = 0
    /// - 32-bit: 占据 bit[31:0]
    pub fn bytes_to_samples(&self, bytes: &[u8], output: &mut [i32]) -> usize {
        match self.bits_per_sample {
            16 => {
                for (i, chunk) in bytes.chunks_exact(2).enumerate() {
                    if i >= output.len() {
                        break;
                    }
                    // little-endian 16-bit signed
                    let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    // 左对齐：16-bit → 占据 i32 高 16 位
                    output[i] = (sample as i32) << 16;
                }
                (bytes.len() / 2).min(output.len())
            }
            24 => {
                for (i, chunk) in bytes.chunks_exact(3).enumerate() {
                    if i >= output.len() {
                        break;
                    }

                    // little-endian 24-bit 解码
                    // chunk[0] = LSB, chunk[2] = MSB (含符号位)
                    let raw = (chunk[0] as i32)
                        | ((chunk[1] as i32) << 8)
                        | ((chunk[2] as i32) << 16);

                    // 符号扩展 24-bit → 32-bit
                    // 先左移把符号位移到 bit31，再算术右移恢复
                    let signed = (raw << 8) >> 8;

                    // 左对齐：24-bit → 占据 i32 高 24 位
                    output[i] = signed << 8;
                }
                (bytes.len() / 3).min(output.len())
            }
            32 => {
                for (i, chunk) in bytes.chunks_exact(4).enumerate() {
                    if i >= output.len() {
                        break;
                    }
                    // little-endian 32-bit signed，已经是完整 i32
                    output[i] = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                (bytes.len() / 4).min(output.len())
            }
            _ => 0,
        }
    }

    /// 将 i32 样本（左对齐）打包为输出字节
    pub fn samples_to_bytes(&self, samples: &[i32], output: &mut [u8]) {
        match self.bits_per_sample {
            16 => {
                for (i, &sample) in samples.iter().enumerate() {
                    if i * 2 + 1 >= output.len() {
                        break;
                    }
                    // 右移 16 位取回 16-bit
                    let val = (sample >> 16) as i16;
                    let bytes = val.to_le_bytes();
                    output[i * 2] = bytes[0];
                    output[i * 2 + 1] = bytes[1];
                }
            }
            24 => {
                for (i, &sample) in samples.iter().enumerate() {
                    if i * 3 + 2 >= output.len() {
                        break;
                    }
                    // 右移 8 位取回 24-bit（带符号）
                    let v = sample >> 8;
                    // little-endian 输出
                    output[i * 3] = (v & 0xFF) as u8;
                    output[i * 3 + 1] = ((v >> 8) & 0xFF) as u8;
                    output[i * 3 + 2] = ((v >> 16) & 0xFF) as u8;
                }
            }
            32 => {
                for (i, &sample) in samples.iter().enumerate() {
                    if i * 4 + 3 >= output.len() {
                        break;
                    }
                    let bytes = sample.to_le_bytes();
                    output[i * 4..i * 4 + 4].copy_from_slice(&bytes);
                }
            }
            _ => {}
        }
    }

    /// 提取单个声道的样本并转换为字节
    ///
    /// 用于 NonInterleaved 输出
    pub fn extract_channel_to_bytes(
        &self,
        samples: &[i32],
        channel: usize,
        channels: usize,
        output: &mut [u8],
    ) {
        let bytes_per_sample = self.bytes_per_sample();
        let mut frame_idx = 0;

        for sample_idx in (channel..samples.len()).step_by(channels) {
            let sample = samples[sample_idx];
            let offset = frame_idx * bytes_per_sample;

            if offset + bytes_per_sample > output.len() {
                break;
            }

            match self.bits_per_sample {
                16 => {
                    let val = (sample >> 16) as i16;
                    let bytes = val.to_le_bytes();
                    output[offset] = bytes[0];
                    output[offset + 1] = bytes[1];
                }
                24 => {
                    let v = sample >> 8;
                    output[offset] = (v & 0xFF) as u8;
                    output[offset + 1] = ((v >> 8) & 0xFF) as u8;
                    output[offset + 2] = ((v >> 16) & 0xFF) as u8;
                }
                32 => {
                    let bytes = sample.to_le_bytes();
                    output[offset..offset + 4].copy_from_slice(&bytes);
                }
                _ => {}
            }

            frame_idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_16bit_roundtrip() {
        let format = AudioFormat::new(48000, 1, 16);

        // 正数
        let input_bytes = [0x00, 0x40]; // +16384
        let mut samples = [0i32; 1];
        format.bytes_to_samples(&input_bytes, &mut samples);

        let mut output_bytes = [0u8; 2];
        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(input_bytes, output_bytes);

        // 负数
        let input_bytes = [0x00, 0xC0]; // -16384
        format.bytes_to_samples(&input_bytes, &mut samples);
        assert!(samples[0] < 0, "negative sample should be negative after decode");

        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(input_bytes, output_bytes);
    }

    #[test]
    fn test_24bit_roundtrip() {
        let format = AudioFormat::new(96000, 1, 24);

        // 测试正数
        let input_bytes = [0x00, 0x00, 0x40]; // +0x400000 (正数)
        let mut samples = [0i32; 1];
        format.bytes_to_samples(&input_bytes, &mut samples);

        let mut output_bytes = [0u8; 3];
        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(input_bytes, output_bytes);

        // 测试负数（关键！）
        let input_bytes = [0x00, 0x00, 0xC0]; // -0x400000 (负数，MSB=0xC0)
        format.bytes_to_samples(&input_bytes, &mut samples);

        // 验证符号扩展正确：samples[0] 应该是负数
        assert!(
            samples[0] < 0,
            "negative sample should be negative after decode"
        );

        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(
            input_bytes, output_bytes,
            "24-bit roundtrip failed for negative"
        );
    }

    #[test]
    fn test_24bit_sign_extend() {
        let format = AudioFormat::new(96000, 1, 24);

        // 最大正值: 0x7FFFFF
        let max_pos = [0xFF, 0xFF, 0x7F];
        let mut samples = [0i32; 1];
        format.bytes_to_samples(&max_pos, &mut samples);
        assert!(samples[0] > 0);
        assert_eq!(samples[0], 0x7FFFFF << 8);

        // 最小负值: 0x800000 = -8388608
        let min_neg = [0x00, 0x00, 0x80];
        format.bytes_to_samples(&min_neg, &mut samples);
        assert!(samples[0] < 0);
        assert_eq!(samples[0], (-8388608i32) << 8);

        // -1: 0xFFFFFF
        let neg_one = [0xFF, 0xFF, 0xFF];
        format.bytes_to_samples(&neg_one, &mut samples);
        assert_eq!(samples[0], (-1i32) << 8);
    }

    #[test]
    fn test_32bit_roundtrip() {
        let format = AudioFormat::new(192000, 1, 32);

        // 正数
        let input_bytes = [0x00, 0x00, 0x00, 0x40];
        let mut samples = [0i32; 1];
        format.bytes_to_samples(&input_bytes, &mut samples);

        let mut output_bytes = [0u8; 4];
        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(input_bytes, output_bytes);

        // 负数
        let input_bytes = [0x00, 0x00, 0x00, 0xC0];
        format.bytes_to_samples(&input_bytes, &mut samples);
        assert!(samples[0] < 0);

        format.samples_to_bytes(&samples, &mut output_bytes);
        assert_eq!(input_bytes, output_bytes);
    }
}
