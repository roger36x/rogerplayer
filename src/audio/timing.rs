//! Mach 时间相关函数
//!
//! 提供正确的 mach ticks 到纳秒转换

use std::sync::OnceLock;

#[cfg(target_os = "macos")]
mod mach {
    #[repr(C)]
    pub struct mach_timebase_info_t {
        pub numer: u32,
        pub denom: u32,
    }

    extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut mach_timebase_info_t) -> i32;
    }
}

/// Mach timebase 信息（全局缓存，只初始化一次）
static TIMEBASE: OnceLock<TimebaseInfo> = OnceLock::new();

#[derive(Clone, Copy)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

impl TimebaseInfo {
    #[cfg(target_os = "macos")]
    fn get() -> Self {
        *TIMEBASE.get_or_init(|| {
            let mut info = mach::mach_timebase_info_t { numer: 0, denom: 0 };
            unsafe { mach::mach_timebase_info(&mut info) };
            TimebaseInfo {
                numer: info.numer,
                denom: info.denom,
            }
        })
    }

    #[cfg(not(target_os = "macos"))]
    fn get() -> Self {
        *TIMEBASE.get_or_init(|| TimebaseInfo { numer: 1, denom: 1 })
    }
}

/// 将 mach ticks 转换为纳秒
///
/// 注意：Intel Mac 上 timebase 通常是 1/1
/// Apple Silicon 上通常是 125/3 (约 41.67ns/tick)
#[inline]
pub fn mach_ticks_to_ns(ticks: u64) -> u64 {
    let info = TimebaseInfo::get();
    // 注意：先乘后除可能溢出，但对于典型的 timebase (1/1 或 125/3) 和
    // 合理的 interval (< 1秒)，不会溢出
    ticks * info.numer as u64 / info.denom as u64
}

/// 获取当前时间（mach ticks）
#[cfg(target_os = "macos")]
#[inline]
pub fn now_ticks() -> u64 {
    unsafe { mach::mach_absolute_time() }
}

#[cfg(not(target_os = "macos"))]
#[inline]
pub fn now_ticks() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// 获取当前时间（纳秒）
#[inline]
pub fn now_ns() -> u64 {
    mach_ticks_to_ns(now_ticks())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timebase() {
        let info = TimebaseInfo::get();
        println!("Mach timebase: {}/{}", info.numer, info.denom);

        // 典型值：Intel 是 1/1，Apple Silicon 是 125/3
        assert!(info.numer > 0);
        assert!(info.denom > 0);

        // 测试转换
        let ticks = 1_000_000; // 1M ticks
        let ns = mach_ticks_to_ns(ticks);
        println!("{} ticks = {} ns", ticks, ns);

        // 对于 1/1 timebase，应该相等
        // 对于 125/3 timebase，ns ≈ ticks * 41.67
        assert!(ns > 0);
    }

    #[test]
    fn test_now() {
        let t1 = now_ticks();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = now_ticks();

        assert!(t2 > t1, "time should advance");

        let ns1 = now_ns();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let ns2 = now_ns();

        let diff = ns2 - ns1;
        // 至少 10ms (10_000_000 ns)
        assert!(
            diff >= 8_000_000,
            "expected at least 8ms, got {}ns",
            diff
        );
    }
}
