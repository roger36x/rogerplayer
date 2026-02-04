//! Core Audio AUHAL 输出
//!
//! 使用 AudioUnit HAL (AUHAL) 实现音频输出
//! 支持：
//! - 独占模式 (Hog Mode)
//! - 整数模式 (避免浮点转换)
//! - 动态采样率切换
//! - Interleaved/NonInterleaved 输出布局

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;


use super::format::{AudioFormat, OutputLayout};
use super::ring_buffer::RingBuffer;
use super::stats::PlaybackStats;

/// Core Audio 类型定义
type AudioDeviceID = u32;
type AudioObjectID = u32;
type AudioObjectPropertySelector = u32;
type AudioObjectPropertyScope = u32;
type AudioObjectPropertyElement = u32;
type OSStatus = i32;
type AudioUnit = *mut c_void;
type AudioComponentInstance = AudioUnit;
type AudioDeviceIOProcID = *mut c_void;

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_AUDIO_HARDWARE_PROPERTY_DEVICES: AudioObjectPropertySelector = 0x64657623; // 'dev#'
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: AudioObjectPropertySelector = 0x644F7574; // 'dOut'
const K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE: AudioObjectPropertySelector = 0x6E737274; // 'nsrt'
const K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES: AudioObjectPropertySelector =
    0x6E737223; // 'nsr#'
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: AudioObjectPropertySelector = 0x6673697A; // 'fsiz'
const K_AUDIO_DEVICE_PROPERTY_HOG_MODE: AudioObjectPropertySelector = 0x6F696E6B; // 'oink'
const K_AUDIO_DEVICE_PROPERTY_STREAMS: AudioObjectPropertySelector = 0x73746D23; // 'stm#'
const K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION: AudioObjectPropertySelector = 0x736C6179; // 'slay'
const K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT: AudioObjectPropertySelector = 0x70667420; // 'pft '
const K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE: AudioObjectPropertySelector = 0x7472616E; // 'tran'
const K_AUDIO_OBJECT_PROPERTY_NAME: AudioObjectPropertySelector = 0x6E616D65; // 'name'

// 设备能力查询属性
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE: AudioObjectPropertySelector = 0x66737223; // 'fsr#'
const K_AUDIO_DEVICE_PROPERTY_LATENCY: AudioObjectPropertySelector = 0x6C746E63; // 'ltnc'
const K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: AudioObjectPropertySelector = 0x73616674; // 'saft'
const K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS: AudioObjectPropertySelector = 0x6F706672; // 'opfr'

// 设备传输类型
const K_AUDIO_DEVICE_TRANSPORT_TYPE_BLUETOOTH: u32 = 0x626C7565; // 'blue'
const K_AUDIO_DEVICE_TRANSPORT_TYPE_BLUETOOTH_LE: u32 = 0x62746C65; // 'btle'

const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: AudioObjectPropertyScope = 0x6F757470; // 'outp'
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: AudioObjectPropertyScope = 0x676C6F62; // 'glob'
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: AudioObjectPropertyElement = 0;

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6C70636D; // 'lpcm'
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;

const K_AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE: u32 = 2000;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO: u32 = 2003;

const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = 0x61756F75; // 'auou'
const K_AUDIO_UNIT_SUB_TYPE_HAL_OUTPUT: u32 = 0x6168616C; // 'ahal'
const K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT: u32 = 0x64656620; // 'def '
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = 0x6170706C; // 'appl'

const NO_ERR: OSStatus = 0;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct AudioObjectPropertyAddress {
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

impl AudioStreamBasicDescription {
    fn is_non_interleaved(&self) -> bool {
        (self.format_flags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED) != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AudioValueRange {
    minimum: f64,
    maximum: f64,
}

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

type AudioComponent = *mut c_void;

#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: RenderCallback,
    input_proc_ref_con: *mut c_void,
}

type RenderCallback = extern "C" fn(
    in_ref_con: *mut c_void,
    io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus;

/// AudioTimeStamp flags
const K_AUDIO_TIME_STAMP_SAMPLE_TIME_VALID: u32 = 1;
const K_AUDIO_TIME_STAMP_HOST_TIME_VALID: u32 = 2;

#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte_time: SMPTETime,
    flags: u32,
    reserved: u32,
}

impl AudioTimeStamp {
    /// 获取有效的 host_time，如果无效返回 0
    #[inline]
    fn valid_host_time(&self) -> u64 {
        if (self.flags & K_AUDIO_TIME_STAMP_HOST_TIME_VALID) != 0 {
            self.host_time
        } else {
            0
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct SMPTETime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    smpte_type: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 2], // 支持最多 2 个 buffer（立体声非交织）
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        out_data_size: *mut u32,
    ) -> OSStatus;

    fn AudioObjectGetPropertyData(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;

    fn AudioObjectSetPropertyData(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> OSStatus;

    // HAL IOProc API - 直接硬件访问，绕过 AudioUnit 层
    fn AudioDeviceCreateIOProcID(
        in_device: AudioDeviceID,
        in_proc: Option<
            unsafe extern "C" fn(
                in_device: AudioObjectID,
                in_now: *const AudioTimeStamp,
                in_input_data: *const AudioBufferList,
                in_input_time: *const AudioTimeStamp,
                out_output_data: *mut AudioBufferList,
                in_output_time: *const AudioTimeStamp,
                in_client_data: *mut c_void,
            ) -> OSStatus,
        >,
        in_client_data: *mut c_void,
        out_io_proc_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;

    fn AudioDeviceDestroyIOProcID(
        in_device: AudioDeviceID,
        in_io_proc_id: AudioDeviceIOProcID,
    ) -> OSStatus;

    fn AudioDeviceStart(
        in_device: AudioDeviceID,
        in_proc_id: AudioDeviceIOProcID,
    ) -> OSStatus;

    fn AudioDeviceStop(
        in_device: AudioDeviceID,
        in_proc_id: AudioDeviceIOProcID,
    ) -> OSStatus;
}

/// IOKit Power Management 相关类型和函数
///
/// 用于防止系统在播放期间进入节能模式（CPU 降频、睡眠等）
mod power_management {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use std::ffi::c_void;

    pub type IOPMAssertionID = u32;

    /// 断言级别
    pub const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        /// 创建电源管理断言
        pub fn IOPMAssertionCreateWithName(
            assertion_type: *const c_void,  // CFStringRef
            assertion_level: u32,
            assertion_name: *const c_void,  // CFStringRef
            assertion_id: *mut IOPMAssertionID,
        ) -> i32;

        /// 释放电源管理断言
        pub fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> i32;
    }

    /// 电源断言包装器
    ///
    /// RAII 风格：创建时获取断言，Drop 时自动释放
    pub struct PowerAssertion {
        assertion_id: IOPMAssertionID,
    }

    impl PowerAssertion {
        /// 创建电源断言，防止系统节能
        ///
        /// 使用 "PreventUserIdleSystemSleep" 类型：
        /// - 防止系统空闲睡眠
        /// - 防止 CPU 降频到低功耗状态
        /// - 保持音频处理的时序稳定性
        pub fn new(name: &str) -> Option<Self> {
            // 断言类型：防止用户空闲时系统睡眠
            let assertion_type = CFString::new("PreventUserIdleSystemSleep");
            let assertion_name = CFString::new(name);

            let mut assertion_id: IOPMAssertionID = 0;

            let result = unsafe {
                IOPMAssertionCreateWithName(
                    assertion_type.as_concrete_TypeRef() as *const c_void,
                    K_IOPM_ASSERTION_LEVEL_ON,
                    assertion_name.as_concrete_TypeRef() as *const c_void,
                    &mut assertion_id,
                )
            };

            if result == 0 {
                log::info!("Power assertion created: {} (ID: {})", name, assertion_id);
                Some(Self { assertion_id })
            } else {
                log::warn!("Failed to create power assertion (error: {})", result);
                None
            }
        }
    }

    impl Drop for PowerAssertion {
        fn drop(&mut self) {
            let result = unsafe { IOPMAssertionRelease(self.assertion_id) };
            if result == 0 {
                log::debug!("Power assertion released (ID: {})", self.assertion_id);
            } else {
                log::warn!(
                    "Failed to release power assertion {} (error: {})",
                    self.assertion_id,
                    result
                );
            }
        }
    }
}

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioComponentFindNext(
        component: AudioComponent,
        desc: *const AudioComponentDescription,
    ) -> AudioComponent;

    fn AudioComponentInstanceNew(
        component: AudioComponent,
        out_instance: *mut AudioComponentInstance,
    ) -> OSStatus;

    fn AudioComponentInstanceDispose(instance: AudioComponentInstance) -> OSStatus;

    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;

    fn AudioUnitSetProperty(
        unit: AudioUnit,
        property_id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        data_size: u32,
    ) -> OSStatus;

    fn AudioUnitGetProperty(
        unit: AudioUnit,
        property_id: u32,
        scope: u32,
        element: u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> OSStatus;
}


/// 音频输出设备信息
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: AudioDeviceID,
    pub name: String,
    pub supported_sample_rates: Vec<f64>,
    pub current_sample_rate: f64,
    pub is_bluetooth: bool,
}

/// 输出配置
#[derive(Clone, Debug)]
pub struct OutputConfig {
    /// 目标采样率
    pub sample_rate: u32,
    /// 缓冲区帧数
    pub buffer_frames: u32,
    /// 是否尝试独占模式
    pub exclusive_mode: bool,
    /// 是否尝试整数模式
    pub integer_mode: bool,
    /// 是否使用 HALOutput（直接硬件访问）
    /// 设为 false 时强制使用 DefaultOutput（通过系统混音器）
    /// 蓝牙设备建议设为 false
    pub use_hal: bool,
    /// 指定输出设备 ID（None 表示使用系统默认设备）
    pub device_id: Option<u32>,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            buffer_frames: 512,
            exclusive_mode: true,
            integer_mode: true,
            use_hal: true, // 默认使用 HALOutput（有线设备最佳）
            device_id: None, // 默认使用系统默认设备
        }
    }
}

/// 音频输出错误
#[derive(Debug)]
pub enum OutputError {
    NoDefaultDevice,
    GetPropertyFailed(OSStatus),
    SetPropertyFailed(OSStatus),
    AudioUnitFailed(OSStatus),
    SampleRateNotSupported(u32),
    InvalidState(&'static str),
    NoAudioComponent,
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDefaultDevice => write!(f, "No default audio output device"),
            Self::GetPropertyFailed(s) => write!(f, "Failed to get property: OSStatus {}", s),
            Self::SetPropertyFailed(s) => write!(f, "Failed to set property: OSStatus {}", s),
            Self::AudioUnitFailed(s) => write!(f, "AudioUnit error: OSStatus {}", s),
            Self::SampleRateNotSupported(r) => write!(f, "Sample rate {} not supported", r),
            Self::InvalidState(s) => write!(f, "Invalid state: {}", s),
            Self::NoAudioComponent => write!(f, "No audio component found"),
        }
    }
}

impl std::error::Error for OutputError {}

/// TPDF Dither 状态
///
/// 使用 xorshift32 PRNG，realtime-safe（无分配、无锁）
/// TPDF = 两个均匀随机数相加，产生三角形概率分布
pub struct DitherState {
    /// xorshift32 状态
    state: u32,
}

impl DitherState {
    pub fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { 0xDEADBEEF } else { seed } }
    }

    /// 生成下一个随机 u32（xorshift32 算法）
    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// 生成 TPDF dither 值，范围约 [-1, 1]
    ///
    /// TPDF = rand1 + rand2 - 1.0，其中 rand1, rand2 ∈ [0, 1]
    /// 结果是三角形分布，峰值在 0
    #[inline(always)]
    pub fn next_tpdf(&mut self) -> f32 {
        // 生成两个 [0, 1) 范围的随机数
        let r1 = (self.next_u32() >> 8) as f32 / 16777216.0; // 24-bit precision
        let r2 = (self.next_u32() >> 8) as f32 / 16777216.0;
        // TPDF: 范围 [-1, 1]，三角形分布
        r1 + r2 - 1.0
    }
}

/// 输出格式模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormatMode {
    /// Float32 格式（通过系统混音器或 DefaultOutput）
    Float32,
    /// Int32 格式（直接整数输出，bit-perfect）
    Int32,
    /// Int24 格式（24-bit packed）
    Int24,
}

/// Render 回调上下文
///
/// 所有字段在 callback 启动前预分配，callback 内不做任何分配
/// 内存通过 mlock 锁定，防止 page fault
pub struct CallbackContext {
    pub ring_buffer: Arc<RingBuffer<i32>>,
    pub stats: Arc<PlaybackStats>,
    pub format: AudioFormat,
    pub output_layout: OutputLayout,

    /// 预分配的样本缓冲区（i32，保证对齐）
    pub sample_buffer: Vec<i32>,

    /// TPDF dither 状态
    pub dither: DitherState,

    /// 输出格式模式
    pub output_mode: OutputFormatMode,

    /// 源文件位深（用于判断是否需要 dither）
    /// 当输出位深 >= 源位深时，无需 dither（bit-perfect）
    pub source_bits: u16,

    /// 是否正在运行
    pub running: AtomicBool,

    /// IO 线程是否已设置时间约束策略
    pub thread_policy_set: AtomicBool,
}

/// Mach 线程策略相关类型和常量
#[cfg(target_os = "macos")]
mod thread_policy {
    use std::ffi::c_void;

    pub const THREAD_TIME_CONSTRAINT_POLICY: u32 = 2;
    pub const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

    #[repr(C)]
    pub struct ThreadTimeConstraintPolicy {
        pub period: u32,        // 周期（Mach ticks）
        pub computation: u32,   // 计算时间（Mach ticks）
        pub constraint: u32,    // 约束时间（Mach ticks）
        pub preemptible: i32,   // 是否可抢占
    }

    #[link(name = "System")]
    extern "C" {
        pub fn mach_thread_self() -> u32;
        pub fn thread_policy_set(
            thread: u32,
            flavor: u32,
            policy_info: *const c_void,
            count: u32,
        ) -> i32;
    }

    #[repr(C)]
    pub struct MachTimebaseInfo {
        pub numer: u32,
        pub denom: u32,
    }

    #[link(name = "System")]
    extern "C" {
        pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }

    /// 获取 Mach timebase 信息
    pub fn get_timebase_info() -> (u32, u32) {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe {
            mach_timebase_info(&mut info);
        }
        (info.numer, info.denom)
    }

    /// 将纳秒转换为 Mach ticks
    pub fn ns_to_ticks(ns: u64) -> u32 {
        let (numer, denom) = get_timebase_info();
        // ticks = ns * denom / numer
        ((ns * denom as u64) / numer as u64) as u32
    }
}

impl CallbackContext {
    /// 设置 IO 线程的时间约束策略
    ///
    /// 使用 THREAD_TIME_CONSTRAINT_POLICY 为 CoreAudio IO 线程设置实时调度。
    /// 这告诉调度器此线程有严格的实时需求。
    ///
    /// 参数基于音频缓冲区大小和采样率计算：
    /// - period: 回调周期（通常是 buffer_frames / sample_rate 秒）
    /// - computation: 预计计算时间（通常是周期的 50%）
    /// - constraint: 必须完成的截止时间（通常等于周期）
    #[cfg(target_os = "macos")]
    pub fn set_realtime_thread_policy(&self) -> bool {
        use thread_policy::*;

        // 计算回调周期（纳秒）
        // 假设 512 frames @ 48kHz = ~10.67ms
        let buffer_frames = 512u64;
        let sample_rate = self.format.sample_rate as u64;
        let period_ns = buffer_frames * 1_000_000_000 / sample_rate;

        // 转换为 Mach ticks
        let period_ticks = ns_to_ticks(period_ns);
        let computation_ticks = ns_to_ticks(period_ns / 2);  // 50% 计算时间
        let constraint_ticks = period_ticks;

        let policy = ThreadTimeConstraintPolicy {
            period: period_ticks,
            computation: computation_ticks,
            constraint: constraint_ticks,
            preemptible: 1,  // 允许抢占
        };

        let thread = unsafe { mach_thread_self() };
        let result = unsafe {
            thread_policy_set(
                thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                &policy as *const _ as *const std::ffi::c_void,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            )
        };

        if result == 0 {
            // 使用 eprintln 而不是 log，因为在回调中不能使用 log
            // 实际上这个函数只会在第一次回调时被调用一次
            true
        } else {
            false
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set_realtime_thread_policy(&self) -> bool {
        false
    }

    /// 锁定上下文内存，防止 page fault
    ///
    /// 在实时音频回调中，page fault 会导致严重的时序问题。
    /// 此函数锁定 CallbackContext 结构体和 sample_buffer 的内存。
    pub fn lock_memory(&self) -> bool {
        // 锁定 sample_buffer
        let sample_ptr = self.sample_buffer.as_ptr() as *const libc::c_void;
        let sample_len = self.sample_buffer.len() * std::mem::size_of::<i32>();

        let result = unsafe { libc::mlock(sample_ptr, sample_len) };

        if result == 0 {
            log::debug!("CallbackContext sample_buffer locked: {} bytes", sample_len);
            true
        } else {
            log::warn!(
                "Failed to lock sample_buffer memory (errno: {})",
                unsafe { *libc::__error() }
            );
            false
        }
    }

    /// 解锁上下文内存
    pub fn unlock_memory(&self) {
        let sample_ptr = self.sample_buffer.as_ptr() as *const libc::c_void;
        let sample_len = self.sample_buffer.len() * std::mem::size_of::<i32>();
        unsafe {
            libc::munlock(sample_ptr, sample_len);
        }
    }
}

/// 音频后端类型
///
/// 支持两种模式：
/// - IOProc: 直接 HAL 层访问，绕过 AudioUnit，延迟更低
/// - AudioUnit: 通过 AudioUnit 层，兼容性更好（蓝牙等）
enum AudioBackend {
    /// 直接 HAL IOProc（首选，最短信号路径）
    HalIOProc {
        io_proc_id: AudioDeviceIOProcID,
    },
    /// AudioUnit 输出（回退路径）
    AudioUnit {
        audio_unit: AudioUnit,
    },
}

/// Core Audio AUHAL 输出
pub struct AudioOutput {
    device_id: AudioDeviceID,
    /// 音频后端（IOProc 或 AudioUnit）
    backend: AudioBackend,
    config: OutputConfig,
    context: Option<Box<CallbackContext>>,
    original_sample_rate: f64,
    hog_mode_acquired: bool,
    actual_format: AudioFormat,
    /// 设备支持的采样率列表
    supported_sample_rates: Vec<f64>,
    /// 是否使用 HALOutput（直接硬件访问）
    is_hal_output: bool,
    /// 是否使用直接 IOProc（绕过 AudioUnit）
    is_direct_ioproc: bool,
    /// 是否已暂停
    paused: bool,
    /// 电源管理断言（防止 CPU 降频）
    power_assertion: Option<power_management::PowerAssertion>,
    /// 设备最小缓冲帧数
    min_buffer_frames: u32,
    /// 设备延迟（帧数）
    device_latency_frames: u32,
    /// 安全偏移（帧数）
    safety_offset_frames: u32,
}

impl AudioOutput {
    /// 获取默认输出设备
    pub fn get_default_device() -> Result<DeviceInfo, OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut device_id: AudioDeviceID = 0;
        let mut size = std::mem::size_of::<AudioDeviceID>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut device_id as *mut _ as *mut c_void,
            )
        };

        if status != NO_ERR {
            return Err(OutputError::GetPropertyFailed(status));
        }

        if device_id == 0 {
            return Err(OutputError::NoDefaultDevice);
        }

        let sample_rates = Self::get_supported_sample_rates(device_id)?;
        let current_rate = Self::get_current_sample_rate(device_id)?;
        let device_name = Self::get_device_name(device_id);
        let is_bluetooth = Self::is_bluetooth_device(device_id);

        log::info!("Default device: {} (ID: {})", device_name, device_id);
        log::info!("Device type: {}", if is_bluetooth { "Bluetooth" } else { "Wired/USB" });
        log::info!("Supported sample rates: {:?}", sample_rates);
        log::info!("Current sample rate: {} Hz", current_rate);

        Ok(DeviceInfo {
            id: device_id,
            name: device_name,
            supported_sample_rates: sample_rates,
            current_sample_rate: current_rate,
            is_bluetooth,
        })
    }

    /// 获取所有输出设备
    pub fn get_all_output_devices() -> Result<Vec<DeviceInfo>, OutputError> {
        // 获取设备列表大小
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_HARDWARE_PROPERTY_DEVICES,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut size: u32 = 0;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                ptr::null(),
                &mut size,
            )
        };

        if status != NO_ERR {
            return Err(OutputError::GetPropertyFailed(status));
        }

        let device_count = size as usize / std::mem::size_of::<AudioDeviceID>();
        if device_count == 0 {
            return Ok(vec![]);
        }

        // 获取所有设备 ID
        let mut device_ids = vec![0u32; device_count];
        let status = unsafe {
            AudioObjectGetPropertyData(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &address,
                0,
                ptr::null(),
                &mut size,
                device_ids.as_mut_ptr() as *mut c_void,
            )
        };

        if status != NO_ERR {
            return Err(OutputError::GetPropertyFailed(status));
        }

        // 过滤出有输出通道的设备
        let mut output_devices = Vec::new();
        for device_id in device_ids {
            if Self::has_output_channels(device_id) {
                if let Ok(info) = Self::get_device_info(device_id) {
                    output_devices.push(info);
                }
            }
        }

        Ok(output_devices)
    }

    /// 根据设备 ID 获取设备信息
    pub fn get_device_info(device_id: AudioDeviceID) -> Result<DeviceInfo, OutputError> {
        let device_name = Self::get_device_name(device_id);

        // 获取采样率（某些设备可能不支持）
        let sample_rates = Self::get_supported_sample_rates(device_id)
            .unwrap_or_else(|_| vec![44100.0, 48000.0]);
        let current_rate = Self::get_current_sample_rate(device_id)
            .unwrap_or(48000.0);
        let is_bluetooth = Self::is_bluetooth_device(device_id);

        Ok(DeviceInfo {
            id: device_id,
            name: device_name,
            supported_sample_rates: sample_rates,
            current_sample_rate: current_rate,
            is_bluetooth,
        })
    }

    /// 按名称查找设备（支持部分匹配）
    pub fn find_device_by_name(name: &str) -> Option<DeviceInfo> {
        let devices = Self::get_all_output_devices().ok()?;
        let name_lower = name.to_lowercase();

        // 先尝试精确匹配
        for device in &devices {
            if device.name.to_lowercase() == name_lower {
                return Some(device.clone());
            }
        }

        // 再尝试部分匹配
        for device in &devices {
            if device.name.to_lowercase().contains(&name_lower) {
                return Some(device.clone());
            }
        }

        None
    }

    /// 检查设备是否有输出通道
    fn has_output_channels(device_id: AudioDeviceID) -> bool {
        // 使用 kAudioDevicePropertyStreams 检查是否有输出流
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut size: u32 = 0;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(device_id, &address, 0, ptr::null(), &mut size)
        };

        // 如果有输出流，size > 0
        status == NO_ERR && size > 0
    }

    /// 检测设备是否是蓝牙设备
    fn is_bluetooth_device(device_id: AudioDeviceID) -> bool {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut transport_type: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut transport_type as *mut _ as *mut c_void,
            )
        };

        if status != NO_ERR {
            return false;
        }

        transport_type == K_AUDIO_DEVICE_TRANSPORT_TYPE_BLUETOOTH
            || transport_type == K_AUDIO_DEVICE_TRANSPORT_TYPE_BLUETOOTH_LE
    }

    /// 获取设备名称
    fn get_device_name(device_id: AudioDeviceID) -> String {
        // 使用 coreaudio_sys 的 CFString API
        use coreaudio_sys::{
            AudioObjectGetPropertyData as sysGetPropertyData,
            kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
            AudioObjectPropertyAddress as SysPropertyAddress,
        };

        let address = SysPropertyAddress {
            mSelector: kAudioObjectPropertyName,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        // 获取属性大小（应该是一个 CFStringRef）
        let mut size: u32 = std::mem::size_of::<*const c_void>() as u32;
        let mut cf_string_ref: *const c_void = ptr::null();

        let status = unsafe {
            sysGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut cf_string_ref as *mut _ as *mut c_void,
            )
        };

        if status != 0 || cf_string_ref.is_null() {
            return format!("Device {}", device_id);
        }

        // 使用 core-foundation crate 安全处理 CFString
        use core_foundation::base::TCFType;
        use core_foundation::string::CFString;

        let cf_string = unsafe {
            // wrap_under_create_rule 表示我们拥有这个引用（需要 release）
            CFString::wrap_under_create_rule(cf_string_ref as *const _)
        };

        cf_string.to_string()
    }

    /// 查询缓冲区帧数范围 (最小/最大)
    ///
    /// 用于 IOProc 模式下选择最优 buffer size
    fn get_buffer_size_range(device_id: AudioDeviceID) -> Option<(u32, u32)> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut range = AudioValueRange::default();
        let mut size = std::mem::size_of::<AudioValueRange>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut range as *mut _ as *mut c_void,
            )
        };

        if status == NO_ERR {
            Some((range.minimum as u32, range.maximum as u32))
        } else {
            log::debug!("Failed to query buffer size range (status {})", status);
            None
        }
    }

    /// 查询设备输出延迟 (帧数)
    fn get_device_latency(device_id: AudioDeviceID) -> u32 {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_LATENCY,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut latency: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut latency as *mut _ as *mut c_void,
            )
        };

        if status == NO_ERR {
            latency
        } else {
            log::debug!("Failed to query device latency (status {})", status);
            0
        }
    }

    /// 查询安全偏移 (帧数)
    ///
    /// 安全偏移是系统推荐的额外缓冲，用于避免 underrun
    fn get_safety_offset(device_id: AudioDeviceID) -> u32 {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut offset: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut offset as *mut _ as *mut c_void,
            )
        };

        if status == NO_ERR {
            offset
        } else {
            log::debug!("Failed to query safety offset (status {})", status);
            0
        }
    }

    /// 获取设备支持的采样率
    fn get_supported_sample_rates(device_id: AudioDeviceID) -> Result<Vec<f64>, OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut size: u32 = 0;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(device_id, &address, 0, ptr::null(), &mut size)
        };

        // 蓝牙设备（如 AirPods）可能不支持此属性，返回常见采样率
        if status != NO_ERR {
            log::warn!(
                "Failed to query sample rates (status {}), using defaults for Bluetooth device",
                status
            );
            return Ok(vec![44100.0, 48000.0]);
        }

        let count = size as usize / std::mem::size_of::<AudioValueRange>();
        let mut ranges: Vec<AudioValueRange> = vec![AudioValueRange::default(); count];

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                ranges.as_mut_ptr() as *mut c_void,
            )
        };

        if status != NO_ERR {
            log::warn!(
                "Failed to get sample rates (status {}), using defaults",
                status
            );
            return Ok(vec![44100.0, 48000.0]);
        }

        let mut rates: Vec<f64> = ranges
            .iter()
            .flat_map(|r| {
                if (r.minimum - r.maximum).abs() < 0.1 {
                    vec![r.minimum]
                } else {
                    vec![44100.0, 48000.0, 88200.0, 96000.0, 176400.0, 192000.0]
                        .into_iter()
                        .filter(|&rate| rate >= r.minimum && rate <= r.maximum)
                        .collect()
                }
            })
            .collect();

        rates.sort_by(|a, b| a.partial_cmp(b).unwrap());
        rates.dedup();

        Ok(rates)
    }

    /// 获取当前采样率
    fn get_current_sample_rate(device_id: AudioDeviceID) -> Result<f64, OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut rate: f64 = 0.0;
        let mut size = std::mem::size_of::<f64>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut rate as *mut _ as *mut c_void,
            )
        };

        if status != NO_ERR {
            // 蓝牙设备可能不支持此属性，尝试 GLOBAL scope
            let address_global = AudioObjectPropertyAddress {
                selector: K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
                scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
            };

            let status = unsafe {
                AudioObjectGetPropertyData(
                    device_id,
                    &address_global,
                    0,
                    ptr::null(),
                    &mut size,
                    &mut rate as *mut _ as *mut c_void,
                )
            };

            if status != NO_ERR {
                log::warn!(
                    "Failed to get current sample rate (status {}), using 48000 Hz",
                    status
                );
                return Ok(48000.0);
            }
        }

        Ok(rate)
    }

    /// 选择最优采样率
    ///
    /// 优先级：
    /// 1. 精确匹配
    /// 2. 整数倍关系（96→48, 88.2→44.1）
    /// 3. 最接近的高采样率
    fn select_optimal_sample_rate(requested: f64, supported: &[f64]) -> f64 {
        if supported.is_empty() {
            return requested;
        }

        // 1. 精确匹配
        for &rate in supported {
            if (rate - requested).abs() < 1.0 {
                return rate;
            }
        }

        // 2. 整数倍关系 - 优先下采样（96→48）
        // 44100 系列：44100, 88200, 176400
        // 48000 系列：48000, 96000, 192000
        let rate_families: [(f64, &[f64]); 2] = [
            (44100.0, &[44100.0, 88200.0, 176400.0]),
            (48000.0, &[48000.0, 96000.0, 192000.0]),
        ];

        // 确定请求的采样率属于哪个系列
        let requested_family = if (requested / 44100.0).fract().abs() < 0.01 {
            Some(44100.0)
        } else if (requested / 48000.0).fract().abs() < 0.01 {
            Some(48000.0)
        } else {
            None
        };

        if let Some(base) = requested_family {
            // 找同系列中设备支持的整数分频采样率
            let family = rate_families.iter().find(|(b, _)| (*b - base).abs() < 1.0);
            if let Some((_, rates)) = family {
                // 从请求的采样率开始向下找
                for &rate in rates.iter().rev() {
                    if rate <= requested + 1.0 {
                        for &supported_rate in supported {
                            if (supported_rate - rate).abs() < 1.0 {
                                log::info!(
                                    "Sample rate fallback: {} → {} Hz (integer division)",
                                    requested, supported_rate
                                );
                                return supported_rate;
                            }
                        }
                    }
                }
            }
        }

        // 3. 最接近的采样率（优先选择大于等于请求的）
        let mut best = supported[0];
        let mut best_diff = (best - requested).abs();

        for &rate in supported {
            let diff = (rate - requested).abs();
            // 优先选择大于等于请求的采样率，否则选最接近的
            if diff < best_diff && (rate >= requested || best < requested) {
                best = rate;
                best_diff = diff;
            }
        }

        if (best - requested).abs() > 1.0 {
            log::info!(
                "Sample rate fallback: {} → {} Hz (nearest)",
                requested, best
            );
        }
        best
    }

    /// 设置采样率（带智能选择和验证）
    ///
    /// 先检查设备支持的采样率，选择最优值，然后设置并验证
    fn set_sample_rate_smart(
        device_id: AudioDeviceID,
        requested_rate: f64,
        supported_rates: &[f64],
    ) -> Result<f64, OutputError> {
        // 选择最优采样率
        let rate = Self::select_optimal_sample_rate(requested_rate, supported_rates);

        // 如果选择的采样率与请求不同，记录日志
        if (rate - requested_rate).abs() > 1.0 {
            log::info!(
                "Sample rate {} Hz not supported, using {} Hz instead",
                requested_rate, rate
            );
        }

        // 设置采样率
        Self::set_sample_rate(device_id, rate)?;

        Ok(rate)
    }

    /// 设置采样率（带验证）
    ///
    /// 设置后验证采样率是否正确切换，最多重试 3 次
    fn set_sample_rate(device_id: AudioDeviceID, rate: f64) -> Result<(), OutputError> {
        const TOLERANCE: f64 = 1.0; // 允许 1Hz 误差

        // 先检查当前采样率是否已经正确，避免不必要的设置操作
        if let Ok(current_rate) = Self::get_current_sample_rate(device_id) {
            if (current_rate - rate).abs() < TOLERANCE {
                log::debug!("Sample rate already at {} Hz, skipping set", current_rate);
                return Ok(());
            }
        }

        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                std::mem::size_of::<f64>() as u32,
                &rate as *const _ as *const c_void,
            )
        };

        if status != NO_ERR {
            // 蓝牙设备可能不支持设置采样率，尝试 GLOBAL scope
            let address_global = AudioObjectPropertyAddress {
                selector: K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
                scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
            };

            let status = unsafe {
                AudioObjectSetPropertyData(
                    device_id,
                    &address_global,
                    0,
                    ptr::null(),
                    std::mem::size_of::<f64>() as u32,
                    &rate as *const _ as *const c_void,
                )
            };

            if status != NO_ERR {
                // 蓝牙设备通常不支持更改采样率，继续使用设备默认采样率
                log::warn!(
                    "Cannot set sample rate to {} Hz (status {}), using device default",
                    rate,
                    status
                );
                return Ok(());
            }
        }

        // 验证采样率切换是否成功（带重试）
        const MAX_RETRIES: u32 = 10;
        const RETRY_DELAY_MS: u64 = 20;

        for attempt in 0..MAX_RETRIES {
            std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));

            if let Ok(actual_rate) = Self::get_current_sample_rate(device_id) {
                if (actual_rate - rate).abs() < TOLERANCE {
                    log::info!("Sample rate verified: {} Hz (attempt {})", actual_rate, attempt + 1);
                    return Ok(());
                }
            }
        }

        // 验证失败但不阻止播放，记录警告
        log::warn!(
            "Sample rate verification failed after {} attempts, requested {} Hz",
            MAX_RETRIES, rate
        );
        Ok(())
    }

    /// 设置缓冲区大小
    fn set_buffer_size(device_id: AudioDeviceID, frames: u32) -> Result<(), OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                std::mem::size_of::<u32>() as u32,
                &frames as *const _ as *const c_void,
            )
        };

        if status != NO_ERR {
            // 蓝牙设备可能不支持设置缓冲区大小
            log::warn!(
                "Cannot set buffer size to {} frames (status {}), using device default",
                frames,
                status
            );
        }

        Ok(())
    }

    /// 获取缓冲区大小
    fn get_buffer_size(device_id: AudioDeviceID) -> Result<u32, OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut frames: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut frames as *mut _ as *mut c_void,
            )
        };

        if status != NO_ERR {
            // 蓝牙设备可能不支持查询，返回默认值
            log::warn!(
                "Cannot get buffer size (status {}), using default 512 frames",
                status
            );
            return Ok(512);
        }

        Ok(frames)
    }

    /// 尝试获取独占模式
    fn acquire_hog_mode(device_id: AudioDeviceID) -> Result<bool, OutputError> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_HOG_MODE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let pid = unsafe { libc::getpid() };

        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                std::mem::size_of::<i32>() as u32,
                &pid as *const _ as *const c_void,
            )
        };

        Ok(status == NO_ERR)
    }

    /// 释放独占模式
    fn release_hog_mode(device_id: AudioDeviceID) {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_HOG_MODE,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let pid: i32 = -1;

        let _ = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                std::mem::size_of::<i32>() as u32,
                &pid as *const _ as *const c_void,
            )
        };
    }

    /// 创建音频输出
    ///
    /// 优先级：
    /// 1. IOProc（直接 HAL，最低延迟）
    /// 2. HALOutput AudioUnit（绕过系统混音器）
    /// 3. DefaultOutput（通过系统混音器，蓝牙设备）
    pub fn new(config: OutputConfig) -> Result<Self, OutputError> {
        // 获取目标设备（指定的或默认的）
        let target_device = if let Some(device_id) = config.device_id {
            Self::get_device_info(device_id)?
        } else {
            Self::get_default_device()?
        };

        log::info!("Target device: {} (ID: {})", target_device.name, target_device.id);

        // 检测目标设备是否是蓝牙
        let is_bluetooth = target_device.is_bluetooth;
        if is_bluetooth {
            log::info!("Detected Bluetooth device, using system mixer");
        }

        // 根据配置选择输出模式（蓝牙设备自动使用系统混音器）
        if config.use_hal && !is_bluetooth {
            // 1. 首先尝试 IOProc（直接 HAL，最短信号路径）
            match Self::new_hal_ioproc(config.clone(), &target_device) {
                Ok(output) => {
                    log::info!("Using IOProc (direct HAL, lowest latency)");
                    return Ok(output);
                }
                Err(e) => {
                    log::info!("IOProc failed: {:?}, trying HALOutput AudioUnit", e);
                }
            }

            // 2. 回退到 HALOutput AudioUnit
            let desc_hal = AudioComponentDescription {
                component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
                component_sub_type: K_AUDIO_UNIT_SUB_TYPE_HAL_OUTPUT,
                component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
                component_flags: 0,
                component_flags_mask: 0,
            };

            let component_hal = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc_hal) };
            if !component_hal.is_null() {
                log::info!("Found HALOutput component, using AudioUnit");
                match Self::new_hal_output(component_hal, config.clone(), &target_device) {
                    Ok(output) => return Ok(output),
                    Err(e) => {
                        log::info!("HALOutput failed: {:?}, falling back to DefaultOutput", e);
                    }
                }
            }
        } else if !config.use_hal {
            log::info!("HALOutput disabled by config, using system mixer");
        }

        // 3. 回退到 DefaultOutput（通过系统混音器）
        log::info!("Using DefaultOutput (via system mixer)");
        let desc = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT,
            component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };

        let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
        if component.is_null() {
            return Err(OutputError::NoAudioComponent);
        }

        Self::new_default_output(component, config)
    }

    /// 使用直接 HAL IOProc 创建输出（最短信号路径）
    fn new_hal_ioproc(config: OutputConfig, device: &DeviceInfo) -> Result<Self, OutputError> {
        // 查询设备能力
        let (min_buffer, max_buffer) = Self::get_buffer_size_range(device.id)
            .unwrap_or((64, 4096));
        let device_latency = Self::get_device_latency(device.id);
        let safety_offset = Self::get_safety_offset(device.id);

        log::info!(
            "IOProc device capabilities: buffer range [{}-{}], latency {} frames, safety offset {} frames",
            min_buffer, max_buffer, device_latency, safety_offset
        );

        // 验证 buffer 大小在有效范围内
        let buffer_frames = config.buffer_frames.max(min_buffer).min(max_buffer);

        Ok(Self {
            device_id: device.id,
            backend: AudioBackend::HalIOProc {
                io_proc_id: ptr::null_mut(), // 在 start() 中创建
            },
            config: OutputConfig { buffer_frames, ..config },
            context: None,
            original_sample_rate: device.current_sample_rate,
            hog_mode_acquired: false,
            actual_format: AudioFormat::new(48000, 2, 32),
            supported_sample_rates: device.supported_sample_rates.clone(),
            is_hal_output: true,
            is_direct_ioproc: true,
            paused: false,
            power_assertion: None,
            min_buffer_frames: min_buffer,
            device_latency_frames: device_latency,
            safety_offset_frames: safety_offset,
        })
    }

    /// 使用 HALOutput 创建输出（绕过系统混音器）
    fn new_hal_output(component: AudioComponent, config: OutputConfig, device: &DeviceInfo) -> Result<Self, OutputError> {
        let mut audio_unit: AudioUnit = ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &mut audio_unit) };
        if status != NO_ERR {
            return Err(OutputError::AudioUnitFailed(status));
        }

        log::info!("HALOutput: using device {} (ID: {}, {}Hz)", device.name, device.id, device.current_sample_rate);

        // 设置输出设备
        let status = unsafe {
            AudioUnitSetProperty(
                audio_unit,
                K_AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &device.id as *const _ as *const c_void,
                std::mem::size_of::<AudioDeviceID>() as u32,
            )
        };
        if status != NO_ERR {
            unsafe { AudioComponentInstanceDispose(audio_unit) };
            return Err(OutputError::AudioUnitFailed(status));
        }

        // 查询设备能力
        let (min_buffer, _max_buffer) = Self::get_buffer_size_range(device.id)
            .unwrap_or((64, 4096));
        let device_latency = Self::get_device_latency(device.id);
        let safety_offset = Self::get_safety_offset(device.id);

        Ok(Self {
            device_id: device.id,
            backend: AudioBackend::AudioUnit { audio_unit },
            config,
            context: None,
            original_sample_rate: device.current_sample_rate,
            hog_mode_acquired: false,
            actual_format: AudioFormat::new(48000, 2, 32),
            supported_sample_rates: device.supported_sample_rates.clone(),
            is_hal_output: true,
            is_direct_ioproc: false,
            paused: false,
            power_assertion: None,
            min_buffer_frames: min_buffer,
            device_latency_frames: device_latency,
            safety_offset_frames: safety_offset,
        })
    }

    /// 使用 DefaultOutput 创建输出（通过系统混音器）
    fn new_default_output(component: AudioComponent, config: OutputConfig) -> Result<Self, OutputError> {
        let mut audio_unit: AudioUnit = ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &mut audio_unit) };
        if status != NO_ERR {
            return Err(OutputError::AudioUnitFailed(status));
        }

        // DefaultOutput 不需要手动设置设备
        Ok(Self {
            device_id: 0,  // DefaultOutput 不使用具体设备 ID
            backend: AudioBackend::AudioUnit { audio_unit },
            config,
            context: None,
            original_sample_rate: 48000.0,
            hog_mode_acquired: false,
            actual_format: AudioFormat::new(48000, 2, 32),
            supported_sample_rates: vec![44100.0, 48000.0],  // DefaultOutput 常见支持率
            is_hal_output: false,
            is_direct_ioproc: false,
            paused: false,
            power_assertion: None,
            min_buffer_frames: 512,
            device_latency_frames: 0,
            safety_offset_frames: 0,
        })
    }

    /// 获取 AudioUnit（如果是 AudioUnit 后端）
    fn get_audio_unit(&self) -> Option<AudioUnit> {
        match &self.backend {
            AudioBackend::AudioUnit { audio_unit } => Some(*audio_unit),
            AudioBackend::HalIOProc { .. } => None,
        }
    }

    /// 查询输出布局
    fn query_output_layout(&self) -> Result<OutputLayout, OutputError> {
        // IOProc 模式下使用 Interleaved
        let audio_unit = match self.get_audio_unit() {
            Some(au) => au,
            None => return Ok(OutputLayout::Interleaved),
        };

        let mut asbd = AudioStreamBasicDescription::default();
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;

        let status = unsafe {
            AudioUnitGetProperty(
                audio_unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &mut asbd as *mut _ as *mut c_void,
                &mut size,
            )
        };

        if status != NO_ERR {
            // 默认为 Interleaved
            return Ok(OutputLayout::Interleaved);
        }

        if asbd.is_non_interleaved() {
            Ok(OutputLayout::NonInterleaved)
        } else {
            Ok(OutputLayout::Interleaved)
        }
    }

    /// 获取设备的输出流 ID
    fn get_output_stream_id(device_id: AudioDeviceID) -> Option<u32> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut size: u32 = 0;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(device_id, &address, 0, ptr::null(), &mut size)
        };

        if status != NO_ERR || size == 0 {
            return None;
        }

        let count = size as usize / std::mem::size_of::<u32>();
        let mut streams: Vec<u32> = vec![0; count];

        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                streams.as_mut_ptr() as *mut c_void,
            )
        };

        if status != NO_ERR || streams.is_empty() {
            return None;
        }

        Some(streams[0])
    }

    /// 获取流的物理格式
    fn get_physical_format(stream_id: u32) -> Option<AudioStreamBasicDescription> {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let mut asbd = AudioStreamBasicDescription::default();
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;

        let status = unsafe {
            AudioObjectGetPropertyData(
                stream_id,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut asbd as *mut _ as *mut c_void,
            )
        };

        if status != NO_ERR {
            return None;
        }

        Some(asbd)
    }

    /// 设置流的物理格式
    fn set_physical_format(stream_id: u32, format: &AudioStreamBasicDescription) -> bool {
        let address = AudioObjectPropertyAddress {
            selector: K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT,
            scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };

        let status = unsafe {
            AudioObjectSetPropertyData(
                stream_id,
                &address,
                0,
                ptr::null(),
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                format as *const _ as *const c_void,
            )
        };

        status == NO_ERR
    }

    /// 尝试设置物理流格式（直接硬件访问）
    ///
    /// 这是最直接的信号路径，绕过所有格式转换。
    /// 需要设备支持，返回成功与否和实际使用的格式。
    ///
    /// # Arguments
    /// * `format` - 音频格式（声道数等）
    /// * `device_sample_rate` - 设备实际采样率（由 set_sample_rate_smart 确定）
    fn try_set_physical_format(&self, format: &AudioFormat, device_sample_rate: u32) -> Option<(AudioStreamBasicDescription, OutputFormatMode)> {
        // 获取输出流 ID
        let stream_id = Self::get_output_stream_id(self.device_id)?;
        log::info!("Output stream ID: {}", stream_id);

        // 获取当前物理格式
        if let Some(current) = Self::get_physical_format(stream_id) {
            log::info!(
                "Current physical format: {}Hz, {} channels, {} bits, flags=0x{:x}",
                current.sample_rate,
                current.channels_per_frame,
                current.bits_per_channel,
                current.format_flags
            );
        }

        // 尝试设置 32-bit 整数物理格式（使用设备实际采样率）
        let asbd_int32 = AudioStreamBasicDescription {
            sample_rate: device_sample_rate as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 4 * format.channels as u32,
            frames_per_packet: 1,
            bytes_per_frame: 4 * format.channels as u32,
            channels_per_frame: format.channels as u32,
            bits_per_channel: 32,
            reserved: 0,
        };

        if Self::set_physical_format(stream_id, &asbd_int32) {
            // 验证设置成功
            if let Some(actual) = Self::get_physical_format(stream_id) {
                if actual.bits_per_channel == 32
                    && (actual.format_flags & K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER) != 0
                {
                    log::info!("Physical format set to Int32 (direct hardware path)");
                    return Some((actual, OutputFormatMode::Int32));
                }
            }
        }

        // 尝试 24-bit 整数（使用设备实际采样率）
        let asbd_int24 = AudioStreamBasicDescription {
            sample_rate: device_sample_rate as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 3 * format.channels as u32,
            frames_per_packet: 1,
            bytes_per_frame: 3 * format.channels as u32,
            channels_per_frame: format.channels as u32,
            bits_per_channel: 24,
            reserved: 0,
        };

        if Self::set_physical_format(stream_id, &asbd_int24) {
            if let Some(actual) = Self::get_physical_format(stream_id) {
                if actual.bits_per_channel == 24 {
                    log::info!("Physical format set to Int24 (direct hardware path)");
                    return Some((actual, OutputFormatMode::Int24));
                }
            }
        }

        log::info!("Physical format setting failed, using ASBD format");
        None
    }

    /// 尝试设置整数输出格式
    ///
    /// 整数格式避免了 i32 → f32 的转换，信号路径更直接。
    /// 返回 (成功与否, 输出格式模式)
    ///
    /// # Arguments
    /// * `format` - 音频格式（包含源文件采样率）
    ///
    /// 注意：Input scope 使用源文件采样率，CoreAudio 会自动做 SRC 到设备采样率
    fn try_set_integer_format(&self, format: &AudioFormat) -> (bool, OutputFormatMode) {
        // IOProc 模式下不使用此方法，直接使用物理格式
        let audio_unit = match self.get_audio_unit() {
            Some(au) => au,
            None => return (false, OutputFormatMode::Float32),
        };

        // 优先尝试 32-bit Integer（使用源文件采样率，CoreAudio 会做 SRC）
        let asbd_int32 = AudioStreamBasicDescription {
            sample_rate: format.sample_rate as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 4 * format.channels as u32,
            frames_per_packet: 1,
            bytes_per_frame: 4 * format.channels as u32,
            channels_per_frame: format.channels as u32,
            bits_per_channel: 32,
            reserved: 0,
        };

        let status = unsafe {
            AudioUnitSetProperty(
                audio_unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &asbd_int32 as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };

        if status == NO_ERR {
            log::info!("Integer 32-bit output mode enabled (bit-perfect path)");
            return (true, OutputFormatMode::Int32);
        }

        // 尝试 24-bit Integer (packed)（使用源文件采样率）
        let asbd_int24 = AudioStreamBasicDescription {
            sample_rate: format.sample_rate as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 3 * format.channels as u32,
            frames_per_packet: 1,
            bytes_per_frame: 3 * format.channels as u32,
            channels_per_frame: format.channels as u32,
            bits_per_channel: 24,
            reserved: 0,
        };

        let status = unsafe {
            AudioUnitSetProperty(
                audio_unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &asbd_int24 as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            )
        };

        if status == NO_ERR {
            log::info!("Integer 24-bit output mode enabled");
            return (true, OutputFormatMode::Int24);
        }

        log::info!("Integer formats not supported, using Float32");
        (false, OutputFormatMode::Float32)
    }

    /// 启动输出
    pub fn start(
        &mut self,
        format: AudioFormat,
        ring_buffer: Arc<RingBuffer<i32>>,
        stats: Arc<PlaybackStats>,
    ) -> Result<(), OutputError> {
        // 显示输出模式
        if self.is_hal_output {
            log::info!("Output mode: HALOutput (direct hardware access, bit-perfect)");
        } else {
            log::info!("Output mode: DefaultOutput (via system mixer)");
        }

        // 如果有有效的 device_id，尝试设备相关操作
        if self.device_id != 0 {
            // 尝试独占模式
            if self.config.exclusive_mode {
                self.hog_mode_acquired = Self::acquire_hog_mode(self.device_id)?;
                if self.hog_mode_acquired {
                    log::info!("Acquired exclusive (hog) mode");
                } else {
                    log::warn!("Failed to acquire exclusive mode, continuing in shared mode");
                }
            }

            // 智能选择并设置采样率
            let actual_rate = Self::set_sample_rate_smart(
                self.device_id,
                self.config.sample_rate as f64,
                &self.supported_sample_rates,
            )?;
            // 更新 config 中的采样率为实际使用的值
            self.config.sample_rate = actual_rate as u32;

            // 设置缓冲区大小
            Self::set_buffer_size(self.device_id, self.config.buffer_frames)?;

            // 设置输出设备（仅 AudioUnit 后端）
            if let Some(audio_unit) = self.get_audio_unit() {
                let status = unsafe {
                    AudioUnitSetProperty(
                        audio_unit,
                        K_AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
                        K_AUDIO_UNIT_SCOPE_GLOBAL,
                        0,
                        &self.device_id as *const _ as *const c_void,
                        std::mem::size_of::<AudioDeviceID>() as u32,
                    )
                };
                // Ignore error - will use DefaultOutput
                let _ = status;
            }
        }

        // 启用输出（仅 AudioUnit 后端，DefaultOutput 可能不支持此属性）
        if let Some(audio_unit) = self.get_audio_unit() {
            let enable_io: u32 = 1;
            let status = unsafe {
                AudioUnitSetProperty(
                    audio_unit,
                    K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
                    K_AUDIO_UNIT_SCOPE_OUTPUT,
                    0,
                    &enable_io as *const _ as *const c_void,
                    std::mem::size_of::<u32>() as u32,
                )
            };
            // EnableIO may not be supported on all devices
            let _ = status;
        }

        // 尝试设置流格式
        // 优先级：Physical Format (直接硬件，仅当不需要 SRC 时) > ASBD Integer > Float32
        // Input scope 使用源文件采样率，CoreAudio 会自动做 SRC 到设备采样率
        let device_sample_rate = self.config.sample_rate;
        let needs_src = format.sample_rate != device_sample_rate;

        // 辅助函数：设置 Float32 格式（仅 AudioUnit 后端）
        let set_float32_format = |audio_unit: AudioUnit, format: &AudioFormat| {
            let asbd = AudioStreamBasicDescription {
                sample_rate: format.sample_rate as f64,
                format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
                bytes_per_packet: 4 * format.channels as u32,
                frames_per_packet: 1,
                bytes_per_frame: 4 * format.channels as u32,
                channels_per_frame: format.channels as u32,
                bits_per_channel: 32,
                reserved: 0,
            };
            unsafe {
                AudioUnitSetProperty(
                    audio_unit,
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    0,
                    &asbd as *const _ as *const c_void,
                    std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                )
            }
        };

        // 确定输出模式
        let output_mode = if self.is_direct_ioproc {
            // IOProc 模式：优先物理格式，否则 Float32
            if !needs_src {
                self.try_set_physical_format(&format, device_sample_rate)
                    .map(|(_, mode)| mode)
                    .unwrap_or(OutputFormatMode::Float32)
            } else {
                log::info!("IOProc with SRC: {}Hz → {}Hz, using Float32", format.sample_rate, device_sample_rate);
                OutputFormatMode::Float32
            }
        } else if self.config.integer_mode && self.device_id != 0 {
            // AudioUnit 模式：物理格式 > Integer > Float32
            let physical_mode = if !needs_src {
                self.try_set_physical_format(&format, device_sample_rate).map(|(_, mode)| mode)
            } else {
                log::info!("SRC required ({}Hz → {}Hz), skipping physical format", format.sample_rate, device_sample_rate);
                None
            };

            if let Some(mode) = physical_mode {
                mode
            } else {
                // 回退到 ASBD 格式（Integer 或 Float32）
                let (success, mode) = self.try_set_integer_format(&format);
                if success {
                    mode
                } else {
                    if let Some(au) = self.get_audio_unit() {
                        let _ = set_float32_format(au, &format);
                    }
                    OutputFormatMode::Float32
                }
            }
        } else {
            // DefaultOutput 使用 Float32
            if let Some(au) = self.get_audio_unit() {
                let _ = set_float32_format(au, &format);
            }
            OutputFormatMode::Float32
        };

        // 查询实际的 buffer size（如果失败使用较大默认值）
        let buffer_frames = if self.device_id != 0 {
            Self::get_buffer_size(self.device_id).unwrap_or(4096)
        } else {
            4096  // DefaultOutput 使用较大缓冲区
        };
        // 使用更大的缓冲区以处理可变的 callback 大小
        let max_samples_per_callback = buffer_frames.max(8192) as usize * format.channels as usize;
        log::info!("Buffer frames: {}, max samples: {}", buffer_frames, max_samples_per_callback);

        // 查询输出布局
        let output_layout = self.query_output_layout()?;

        // 预分配 sample_buffer（足够大以处理任何 callback）
        let sample_buffer = vec![0i32; max_samples_per_callback];

        // 保存实际格式（使用设备实际采样率，而非源文件采样率）
        self.actual_format = AudioFormat {
            sample_rate: device_sample_rate,
            channels: format.channels,
            bits_per_sample: format.bits_per_sample,
            layout: output_layout,
        };

        // 创建上下文（使用当前时间戳作为 dither 种子）
        let dither_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0xCAFEBABE);

        let context = Box::new(CallbackContext {
            ring_buffer: Arc::clone(&ring_buffer),
            stats,
            format: self.actual_format,
            output_layout,
            sample_buffer,
            dither: DitherState::new(dither_seed),
            output_mode,
            source_bits: format.bits_per_sample,
            running: AtomicBool::new(true),
            thread_policy_set: AtomicBool::new(false),
        });

        // 锁定关键内存，防止 page fault
        ring_buffer.lock_memory();
        context.lock_memory();
        log::info!("Memory locked for realtime safety");

        let context_ptr = Box::into_raw(context);

        // 根据后端类型设置回调并启动
        match &mut self.backend {
            AudioBackend::HalIOProc { io_proc_id } => {
                // IOProc 模式：直接 HAL 层回调
                let status = unsafe {
                    AudioDeviceCreateIOProcID(
                        self.device_id,
                        Some(hal_io_proc),
                        context_ptr as *mut c_void,
                        io_proc_id,
                    )
                };
                if status != NO_ERR {
                    unsafe { let _ = Box::from_raw(context_ptr); }
                    return Err(OutputError::AudioUnitFailed(status));
                }

                self.context = Some(unsafe { Box::from_raw(context_ptr) });

                // 启动设备
                let status = unsafe { AudioDeviceStart(self.device_id, *io_proc_id) };
                if status != NO_ERR {
                    unsafe { AudioDeviceDestroyIOProcID(self.device_id, *io_proc_id); }
                    *io_proc_id = ptr::null_mut();
                    return Err(OutputError::AudioUnitFailed(status));
                }

                log::info!("IOProc started: direct HAL callback (lowest latency path)");
            }
            AudioBackend::AudioUnit { audio_unit } => {
                // AudioUnit 模式：通过 AudioUnit 层回调
                let callback_struct = AURenderCallbackStruct {
                    input_proc: render_callback,
                    input_proc_ref_con: context_ptr as *mut c_void,
                };

                let status = unsafe {
                    AudioUnitSetProperty(
                        *audio_unit,
                        K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                        K_AUDIO_UNIT_SCOPE_INPUT,
                        0,
                        &callback_struct as *const _ as *const c_void,
                        std::mem::size_of::<AURenderCallbackStruct>() as u32,
                    )
                };
                if status != NO_ERR {
                    unsafe { let _ = Box::from_raw(context_ptr); }
                    return Err(OutputError::AudioUnitFailed(status));
                }

                self.context = Some(unsafe { Box::from_raw(context_ptr) });

                // 初始化 AudioUnit
                let status = unsafe { AudioUnitInitialize(*audio_unit) };
                if status != NO_ERR {
                    return Err(OutputError::AudioUnitFailed(status));
                }

                // 启动
                let status = unsafe { AudioOutputUnitStart(*audio_unit) };
                if status != NO_ERR {
                    return Err(OutputError::AudioUnitFailed(status));
                }
            }
        }

        // 如果源采样率与设备采样率不同，记录警告
        if format.sample_rate != device_sample_rate {
            log::warn!(
                "Sample rate conversion: source {}Hz → device {}Hz (CoreAudio SRC)",
                format.sample_rate,
                device_sample_rate
            );
        }

        // 创建电源管理断言，防止 CPU 降频
        // 这对于保持音频处理的时序稳定性非常重要
        self.power_assertion = power_management::PowerAssertion::new("HiFi Replayer Audio Playback");

        log::info!(
            "Audio output started: {}Hz (device), {} channels, {}bit, {:?}, mode={:?}",
            self.actual_format.sample_rate,
            self.actual_format.channels,
            self.actual_format.bits_per_sample,
            self.actual_format.layout,
            output_mode
        );

        Ok(())
    }

    /// 暂停输出
    pub fn pause(&mut self) -> Result<(), OutputError> {
        if self.paused {
            return Ok(());
        }

        let status = match &self.backend {
            AudioBackend::HalIOProc { io_proc_id } => {
                if io_proc_id.is_null() {
                    return Ok(());
                }
                unsafe { AudioDeviceStop(self.device_id, *io_proc_id) }
            }
            AudioBackend::AudioUnit { audio_unit } => {
                if audio_unit.is_null() {
                    return Ok(());
                }
                unsafe { AudioOutputUnitStop(*audio_unit) }
            }
        };

        if status != NO_ERR {
            return Err(OutputError::AudioUnitFailed(status));
        }

        self.paused = true;
        log::info!("Audio output paused");
        Ok(())
    }

    /// 恢复输出
    pub fn resume(&mut self) -> Result<(), OutputError> {
        if !self.paused {
            return Ok(());
        }

        let status = match &self.backend {
            AudioBackend::HalIOProc { io_proc_id } => {
                if io_proc_id.is_null() {
                    return Ok(());
                }
                unsafe { AudioDeviceStart(self.device_id, *io_proc_id) }
            }
            AudioBackend::AudioUnit { audio_unit } => {
                if audio_unit.is_null() {
                    return Ok(());
                }
                unsafe { AudioOutputUnitStart(*audio_unit) }
            }
        };

        if status != NO_ERR {
            return Err(OutputError::AudioUnitFailed(status));
        }

        self.paused = false;
        log::info!("Audio output resumed");
        Ok(())
    }

    /// 是否已暂停
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 停止输出
    pub fn stop(&mut self) -> Result<(), OutputError> {
        if let Some(ref context) = self.context {
            context.running.store(false, Ordering::Release);
        }

        match &mut self.backend {
            AudioBackend::HalIOProc { io_proc_id } => {
                if !io_proc_id.is_null() {
                    let _ = unsafe { AudioDeviceStop(self.device_id, *io_proc_id) };
                    let _ = unsafe { AudioDeviceDestroyIOProcID(self.device_id, *io_proc_id) };
                    *io_proc_id = ptr::null_mut();
                }
            }
            AudioBackend::AudioUnit { audio_unit } => {
                if !audio_unit.is_null() {
                    let _ = unsafe { AudioOutputUnitStop(*audio_unit) };
                    let _ = unsafe { AudioUnitUninitialize(*audio_unit) };
                }
            }
        }

        // 释放独占模式
        if self.hog_mode_acquired {
            Self::release_hog_mode(self.device_id);
            self.hog_mode_acquired = false;
        }

        // 恢复原始采样率（仅 HALOutput 需要，DefaultOutput 的 device_id 为 0）
        if self.device_id != 0 {
            let _ = Self::set_sample_rate(self.device_id, self.original_sample_rate);
        }

        // 释放电源管理断言（允许系统恢复节能模式）
        self.power_assertion = None;

        self.context = None;

        log::info!("Audio output stopped");
        Ok(())
    }

    /// 检查是否正在运行
    pub fn is_running(&self) -> bool {
        self.context
            .as_ref()
            .map(|c| c.running.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// 获取实际格式
    pub fn actual_format(&self) -> AudioFormat {
        self.actual_format
    }

    /// 是否使用 HALOutput（直接硬件访问）
    pub fn is_hal_output(&self) -> bool {
        self.is_hal_output
    }

    /// 是否已获取独占模式
    pub fn is_exclusive_mode(&self) -> bool {
        self.hog_mode_acquired
    }

    /// 获取设备 ID
    pub fn device_id(&self) -> u32 {
        self.device_id
    }

    /// 获取目标采样率
    ///
    /// 根据请求的采样率和设备支持的采样率，返回实际会使用的采样率。
    /// 用于在 start() 之前决定是否需要外部 SRC。
    pub fn target_sample_rate(&self, requested_rate: u32) -> u32 {
        if self.supported_sample_rates.is_empty() {
            // DefaultOutput 或无法查询的设备，假设支持请求的采样率
            return requested_rate;
        }
        Self::select_optimal_sample_rate(requested_rate as f64, &self.supported_sample_rates) as u32
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.stop();

        // 清理 AudioUnit（IOProc 在 stop 中已清理）
        if let AudioBackend::AudioUnit { audio_unit } = &self.backend {
            if !audio_unit.is_null() {
                let _ = unsafe { AudioComponentInstanceDispose(*audio_unit) };
            }
        }
    }
}

/// 共享的音频输出处理逻辑
///
/// 供 hal_io_proc 和 render_callback 共用，避免代码重复。
/// 处理 Int32/Int24/Float32 三种输出格式。
///
/// **绝对禁止：**
/// - 锁
/// - 分配
/// - I/O
#[inline(always)]
unsafe fn process_audio_output(
    ctx: &mut CallbackContext,
    buffer_list: &mut AudioBufferList,
    samples_needed: usize,
) {
    if buffer_list.number_buffers == 0 {
        return;
    }

    match ctx.output_mode {
        OutputFormatMode::Int32 => {
            // 零拷贝路径：直接从 ring buffer 读取到输出缓冲区
            let output_ptr = buffer_list.buffers[0].data as *mut i32;
            let output_samples = buffer_list.buffers[0].data_byte_size as usize / 4;
            let output_slice = std::slice::from_raw_parts_mut(output_ptr, output_samples);

            let count = samples_needed.min(output_slice.len());
            let samples_read = ctx.ring_buffer.read(&mut output_slice[..count]);
            ctx.stats.add_samples_played(samples_read as u64);

            // 填零
            for i in samples_read..output_slice.len() {
                output_slice[i] = 0;
            }

            if samples_read < count {
                ctx.stats.record_underrun();
            }
        }
        OutputFormatMode::Int24 => {
            let actual_samples = samples_needed.min(ctx.sample_buffer.len());
            let sample_buffer = &mut ctx.sample_buffer[..actual_samples];
            let samples_read = ctx.ring_buffer.read(sample_buffer);
            ctx.stats.add_samples_played(samples_read as u64);

            if samples_read < actual_samples {
                ctx.stats.record_underrun();
                for i in samples_read..actual_samples {
                    sample_buffer[i] = 0;
                }
            }

            let output_ptr = buffer_list.buffers[0].data as *mut u8;
            let output_bytes = buffer_list.buffers[0].data_byte_size as usize;
            let output_slice = std::slice::from_raw_parts_mut(output_ptr, output_bytes);

            let count = actual_samples.min(output_bytes / 3);

            if ctx.source_bits <= 24 {
                for i in 0..count {
                    let bytes = sample_buffer[i].to_le_bytes();
                    output_slice[i * 3] = bytes[1];
                    output_slice[i * 3 + 1] = bytes[2];
                    output_slice[i * 3 + 2] = bytes[3];
                }
            } else {
                for i in 0..count {
                    let sample = sample_buffer[i];
                    let r1 = (ctx.dither.next_u32() & 0xFF) as i32;
                    let r2 = (ctx.dither.next_u32() & 0xFF) as i32;
                    let dither = (r1 + r2 - 256) << 8;
                    let dithered = sample.saturating_add(dither);

                    let bytes = dithered.to_le_bytes();
                    output_slice[i * 3] = bytes[1];
                    output_slice[i * 3 + 1] = bytes[2];
                    output_slice[i * 3 + 2] = bytes[3];
                }
            }

            for i in (count * 3)..output_bytes {
                output_slice[i] = 0;
            }
        }
        OutputFormatMode::Float32 => {
            let actual_samples = samples_needed.min(ctx.sample_buffer.len());
            let sample_buffer = &mut ctx.sample_buffer[..actual_samples];
            let samples_read = ctx.ring_buffer.read(sample_buffer);
            ctx.stats.add_samples_played(samples_read as u64);

            if samples_read < actual_samples {
                ctx.stats.record_underrun();
                for i in samples_read..actual_samples {
                    sample_buffer[i] = 0;
                }
            }

            let output_ptr = buffer_list.buffers[0].data as *mut f32;
            let output_samples = buffer_list.buffers[0].data_byte_size as usize / 4;
            let output_slice = std::slice::from_raw_parts_mut(output_ptr, output_samples);

            const DITHER_SCALE: f32 = 1.0 / 8388608.0;
            const I32_TO_FLOAT: f32 = 1.0 / 2147483648.0;

            let count = actual_samples.min(output_slice.len());

            #[cfg(target_arch = "aarch64")]
            {
                use std::arch::aarch64::*;

                let scale_vec = vdupq_n_f32(I32_TO_FLOAT);
                let dither_scale_vec = vdupq_n_f32(DITHER_SCALE);

                let chunks8 = count / 8;
                for chunk_idx in 0..chunks8 {
                    let i = chunk_idx * 8;

                    let i32x4_a = vld1q_s32(sample_buffer.as_ptr().add(i));
                    let i32x4_b = vld1q_s32(sample_buffer.as_ptr().add(i + 4));

                    let scaled_a = vmulq_f32(vcvtq_f32_s32(i32x4_a), scale_vec);
                    let scaled_b = vmulq_f32(vcvtq_f32_s32(i32x4_b), scale_vec);

                    let dither_vals: [f32; 8] = [
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                        ctx.dither.next_tpdf(),
                    ];

                    let dither_a = vmulq_f32(vld1q_f32(dither_vals.as_ptr()), dither_scale_vec);
                    let dither_b = vmulq_f32(vld1q_f32(dither_vals.as_ptr().add(4)), dither_scale_vec);

                    let result_a = vaddq_f32(scaled_a, dither_a);
                    let result_b = vaddq_f32(scaled_b, dither_b);

                    vst1q_f32(output_slice.as_mut_ptr().add(i), result_a);
                    vst1q_f32(output_slice.as_mut_ptr().add(i + 4), result_b);
                }

                for i in (chunks8 * 8)..count {
                    let sample = sample_buffer[i] as f32 * I32_TO_FLOAT;
                    let dither = ctx.dither.next_tpdf() * DITHER_SCALE;
                    output_slice[i] = sample + dither;
                }
            }

            #[cfg(not(target_arch = "aarch64"))]
            {
                for i in 0..count {
                    let sample = sample_buffer[i] as f32 * I32_TO_FLOAT;
                    let dither = ctx.dither.next_tpdf() * DITHER_SCALE;
                    output_slice[i] = sample + dither;
                }
            }

            for i in count..output_slice.len() {
                output_slice[i] = 0.0;
            }
        }
    }
}

/// HAL IOProc 回调
///
/// 直接 HAL 层回调，绕过 AudioUnit 层。
/// 与 render_callback 相比，延迟更低、时序更可预测。
///
/// **绝对禁止：**
/// - 锁
/// - 分配
/// - I/O
/// - println!
unsafe extern "C" fn hal_io_proc(
    _in_device: AudioObjectID,
    in_now: *const AudioTimeStamp,
    _in_input_data: *const AudioBufferList,
    _in_input_time: *const AudioTimeStamp,
    out_output_data: *mut AudioBufferList,
    in_output_time: *const AudioTimeStamp,
    in_client_data: *mut c_void,
) -> OSStatus {
    let ctx = &mut *(in_client_data as *mut CallbackContext);

    // 检查是否停止
    if !ctx.running.load(Ordering::Acquire) {
        // 填充静音
        let buffer_list = &mut *out_output_data;
        if buffer_list.number_buffers > 0 {
            let buf = &mut buffer_list.buffers[0];
            ptr::write_bytes(buf.data as *mut u8, 0, buf.data_byte_size as usize);
        }
        return NO_ERR;
    }

    // 首次调用时设置实时线程策略
    if ctx.thread_policy_set
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        ctx.set_realtime_thread_policy();
    }

    // 使用 output_time 获取更精确的时间戳（音频实际输出时间）
    let host_time = if !in_output_time.is_null() {
        (*in_output_time).valid_host_time()
    } else if !in_now.is_null() {
        (*in_now).valid_host_time()
    } else {
        0
    };
    ctx.stats.on_callback_with_timestamp(&ctx.ring_buffer, host_time);

    let buffer_list = &mut *out_output_data;
    if buffer_list.number_buffers == 0 {
        return NO_ERR;
    }

    // 从 buffer 大小计算帧数
    let buf = &buffer_list.buffers[0];
    let bytes_per_sample = match ctx.output_mode {
        OutputFormatMode::Int32 | OutputFormatMode::Float32 => 4,
        OutputFormatMode::Int24 => 3,
    };
    let channels = ctx.format.channels as usize;
    let frames = buf.data_byte_size as usize / (bytes_per_sample * channels);
    let samples_needed = frames * channels;

    // 调用共享的音频处理逻辑
    process_audio_output(ctx, buffer_list, samples_needed);

    NO_ERR
}

/// Render Callback (AudioUnit)
///
/// **绝对禁止：**
/// - 锁
/// - 分配
/// - I/O
/// - println!
extern "C" fn render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    let ctx = unsafe { &mut *(in_ref_con as *mut CallbackContext) };

    if !ctx.running.load(Ordering::Acquire) {
        return NO_ERR;
    }

    // 首次调用时设置 IO 线程的实时调度策略
    if ctx.thread_policy_set
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        ctx.set_realtime_thread_policy();
    }

    let frames = in_number_frames as usize;
    let channels = ctx.format.channels as usize;
    let samples_needed = frames * channels;

    // 统计
    let host_time = unsafe { (*in_time_stamp).valid_host_time() };
    ctx.stats.on_callback_with_timestamp(&ctx.ring_buffer, host_time);

    // 调用共享的音频处理逻辑
    let buffer_list = unsafe { &mut *io_data };
    unsafe { process_audio_output(ctx, buffer_list, samples_needed); }

    NO_ERR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // 需要音频设备
    fn test_get_default_device() {
        let device = AudioOutput::get_default_device().unwrap();
        println!("Device: {:?}", device);
        assert!(!device.supported_sample_rates.is_empty());
    }
}
