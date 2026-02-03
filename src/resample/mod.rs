//! 重采样模块（预留）
//!
//! 当前版本优先 bit-perfect 直通，自动匹配 DAC 采样率
//! 将来支持：
//! - 可插拔重采样算法
//! - A/B 对比切换
//! - 自定义 Sinc 滤波器

/// 重采样器特征（预留接口）
pub trait Resampler: Send {
    /// 处理样本
    ///
    /// input: 输入样本（交错格式）
    /// output: 输出缓冲区
    /// 返回: 实际输出的样本数
    fn process(&mut self, input: &[f64], output: &mut [f64]) -> usize;

    /// 获取延迟（样本数）
    fn latency(&self) -> usize;

    /// 重置状态
    fn reset(&mut self);

    /// 获取输入/输出采样率比
    fn ratio(&self) -> f64;
}

/// 直通重采样器（不做任何处理）
pub struct PassthroughResampler;

impl Resampler for PassthroughResampler {
    fn process(&mut self, input: &[f64], output: &mut [f64]) -> usize {
        let len = input.len().min(output.len());
        output[..len].copy_from_slice(&input[..len]);
        len
    }

    fn latency(&self) -> usize {
        0
    }

    fn reset(&mut self) {}

    fn ratio(&self) -> f64 {
        1.0
    }
}

/// 重采样策略
#[derive(Clone, Debug)]
pub enum ResamplePolicy {
    /// 自动匹配：尽量切换 DAC 采样率，避免重采样
    MatchSource,

    /// 固定输出：使用指定采样率，必要时重采样
    Fixed {
        target_rate: u32,
        // resampler: Box<dyn Resampler>, // 将来实现
    },
}

impl Default for ResamplePolicy {
    fn default() -> Self {
        Self::MatchSource
    }
}

// === 将来实现的模块 ===

// pub mod sinc;      // Sinc 重采样器
// pub mod window;    // 窗函数
// pub mod polyphase; // 多相实现
// pub mod external;  // 外部库封装 (libsoxr, rubato)
