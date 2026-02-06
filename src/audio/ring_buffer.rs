//! Lock-free Single-Producer Single-Consumer Ring Buffer
//!
//! 设计目标：
//! - 零锁：生产者和消费者完全无锁操作
//! - 零分配：所有内存在初始化时预分配
//! - 缓存友好：使用 cache line 对齐避免 false sharing
//! - 内存锁定：可选 mlock 防止 page fault
//!
//! 用于解码线程（生产者）和音频输出线程（消费者）之间的数据传递

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Cache line 大小
///
/// - Intel x86_64: 64 bytes
/// - Apple Silicon (M1/M2/M3): 128 bytes (P-core)
///
/// 使用 128 bytes 以兼容 Apple Silicon，在 Intel 上略有浪费但无害
pub const CACHE_LINE_SIZE: usize = 128;

/// Cache line 对齐包装器
///
/// 使用 #[repr(align(128))] 确保包装的值独占一个 cache line，
/// 避免 false sharing。128 字节对齐兼容 Apple Silicon P-core。
#[repr(C, align(128))]
pub struct CacheLine<T>(pub T);

impl<T> CacheLine<T> {
    pub fn new(val: T) -> Self {
        Self(val)
    }
}

impl<T: Default> Default for CacheLine<T> {
    fn default() -> Self {
        Self(T::default())
    }
}

/// SPSC 无锁环形缓冲区
///
/// 内存布局保证：
/// - write_pos 和 read_pos 各自独占一个 64 字节 cache line
/// - 避免 false sharing
/// - 可选 mlock 防止 page fault
pub struct RingBuffer<T: Copy + Default> {
    buffer: Box<[UnsafeCell<T>]>,
    capacity: usize,
    mask: usize,

    // 使用 CacheLine 包装，真正对齐到 cache line 边界
    write_pos: CacheLine<AtomicUsize>,
    read_pos: CacheLine<AtomicUsize>,

    // 是否已锁定内存
    memory_locked: AtomicBool,
}

unsafe impl<T: Copy + Default + Send> Send for RingBuffer<T> {}
unsafe impl<T: Copy + Default + Send> Sync for RingBuffer<T> {}

impl<T: Copy + Default> RingBuffer<T> {
    /// 创建指定容量的 Ring Buffer
    ///
    /// capacity 必须是 2 的幂
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "capacity must be power of two");

        let buffer: Vec<UnsafeCell<T>> = (0..capacity)
            .map(|_| UnsafeCell::new(T::default()))
            .collect();

        Self {
            buffer: buffer.into_boxed_slice(),
            capacity,
            mask: capacity - 1,
            write_pos: CacheLine::new(AtomicUsize::new(0)),
            read_pos: CacheLine::new(AtomicUsize::new(0)),
            memory_locked: AtomicBool::new(false),
        }
    }

    /// 锁定缓冲区内存，防止被换页
    ///
    /// 在实时音频场景下，page fault 会导致严重的时序抖动。
    /// 调用此函数后，缓冲区内存将被锁定在物理内存中，不会被换出。
    ///
    /// 返回是否成功锁定
    pub fn lock_memory(&self) -> bool {
        if self.memory_locked.load(Ordering::Acquire) {
            return true; // 已经锁定
        }

        let ptr = self.buffer.as_ptr() as *const libc::c_void;
        let len = self.capacity * std::mem::size_of::<UnsafeCell<T>>();

        let result = unsafe { libc::mlock(ptr, len) };

        if result == 0 {
            self.memory_locked.store(true, Ordering::Release);
            log::debug!("Ring buffer memory locked: {} bytes", len);
            true
        } else {
            log::warn!("Failed to lock ring buffer memory (errno: {})", unsafe {
                *libc::__error()
            });
            false
        }
    }

    /// 解锁缓冲区内存
    pub fn unlock_memory(&self) {
        if !self.memory_locked.load(Ordering::Acquire) {
            return;
        }

        let ptr = self.buffer.as_ptr() as *const libc::c_void;
        let len = self.capacity * std::mem::size_of::<UnsafeCell<T>>();

        unsafe {
            libc::munlock(ptr, len);
        }

        self.memory_locked.store(false, Ordering::Release);
        log::debug!("Ring buffer memory unlocked");
    }

    /// 检查内存是否已锁定
    pub fn is_memory_locked(&self) -> bool {
        self.memory_locked.load(Ordering::Acquire)
    }

    /// 创建指定最小容量的 Ring Buffer（自动向上取整到 2 的幂）
    pub fn with_min_capacity(min_capacity: usize) -> Self {
        Self::new(min_capacity.next_power_of_two())
    }

    /// 写入样本（生产者调用）
    ///
    /// 返回实际写入的样本数
    /// 此函数是 wait-free 的，绝不阻塞
    ///
    /// 优化：使用批量拷贝代替逐元素循环，利用 SIMD memcpy
    #[inline]
    pub fn write(&self, data: &[T]) -> usize {
        let write = self.write_pos.0.load(Ordering::Relaxed);
        let read = self.read_pos.0.load(Ordering::Acquire);

        let used = write.wrapping_sub(read);
        debug_assert!(used <= self.capacity, "ring buffer invariant violated: used > capacity");

        let free = self.capacity - used;
        let to_write = data.len().min(free);

        if to_write == 0 {
            return 0;
        }

        // 计算物理写入位置
        let write_idx = write & self.mask;
        // 到缓冲区末尾的连续空间
        let first_part = (self.capacity - write_idx).min(to_write);

        // 批量拷贝第一段（到缓冲区末尾）
        unsafe {
            let dst = self.buffer[write_idx].get() as *mut T;
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, first_part);
        }

        // 如果需要环绕，拷贝第二段（从缓冲区开头）
        let second_part = to_write - first_part;
        if second_part > 0 {
            unsafe {
                let dst = self.buffer[0].get() as *mut T;
                std::ptr::copy_nonoverlapping(data.as_ptr().add(first_part), dst, second_part);
            }
        }

        self.write_pos.0.store(write.wrapping_add(to_write), Ordering::Release);
        to_write
    }

    /// 读取样本（消费者调用）
    ///
    /// 返回实际读取的样本数
    /// 此函数是 wait-free 的，绝不阻塞
    ///
    /// 优化：使用批量拷贝 + prefetch 隐藏 L2→L1 延迟
    #[inline]
    pub fn read(&self, output: &mut [T]) -> usize {
        let read = self.read_pos.0.load(Ordering::Relaxed);
        let write = self.write_pos.0.load(Ordering::Acquire);

        let available = write.wrapping_sub(read);
        let to_read = output.len().min(available);

        if to_read == 0 {
            return 0;
        }

        // 计算物理读取位置
        let read_idx = read & self.mask;
        // 到缓冲区末尾的连续数据
        let first_part = (self.capacity - read_idx).min(to_read);

        // Prefetch 即将读取的数据到 L1 cache（PRFM PLDL1KEEP）
        // 预取前 2 条 cache line（256 字节 ≈ 64 个 i32 样本）
        #[cfg(target_arch = "aarch64")]
        unsafe {
            let src = self.buffer[read_idx].get() as *const u8;
            std::arch::asm!("prfm pldl1keep, [{addr}]", addr = in(reg) src, options(nostack, preserves_flags));
            if first_part * std::mem::size_of::<T>() > 128 {
                std::arch::asm!("prfm pldl1keep, [{addr}]", addr = in(reg) src.add(128), options(nostack, preserves_flags));
            }
        }

        // 批量拷贝第一段（到缓冲区末尾）
        unsafe {
            let src = self.buffer[read_idx].get() as *const T;
            std::ptr::copy_nonoverlapping(src, output.as_mut_ptr(), first_part);
        }

        // 如果需要环绕，拷贝第二段（从缓冲区开头）
        let second_part = to_read - first_part;
        if second_part > 0 {
            unsafe {
                let src = self.buffer[0].get() as *const T;
                std::ptr::copy_nonoverlapping(src, output.as_mut_ptr().add(first_part), second_part);
            }
        }

        self.read_pos.0.store(read.wrapping_add(to_read), Ordering::Release);
        to_read
    }

    /// 获取当前可读样本数
    #[inline]
    pub fn available(&self) -> usize {
        let write = self.write_pos.0.load(Ordering::Acquire);
        let read = self.read_pos.0.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }

    /// 获取当前可写空间
    ///
    /// 直接计算，避免通过 available() 间接做两次 Acquire load
    #[inline]
    pub fn free_space(&self) -> usize {
        let write = self.write_pos.0.load(Ordering::Relaxed);
        let read = self.read_pos.0.load(Ordering::Acquire);
        self.capacity - write.wrapping_sub(read)
    }

    /// 获取容量
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 获取缓冲区填充百分比（用于监控）
    #[inline]
    pub fn fill_ratio(&self) -> f64 {
        self.available() as f64 / self.capacity as f64
    }

    /// 清空缓冲区
    pub fn clear(&self) {
        let write = self.write_pos.0.load(Ordering::Acquire);
        self.read_pos.0.store(write, Ordering::Release);
    }
}

impl<T: Copy + Default> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        self.unlock_memory();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let rb = RingBuffer::<i32>::new(16);

        let data = [1, 2, 3, 4];
        assert_eq!(rb.write(&data), 4);
        assert_eq!(rb.available(), 4);

        let mut output = [0i32; 4];
        assert_eq!(rb.read(&mut output), 4);
        assert_eq!(output, data);
    }

    #[test]
    fn test_ring_buffer_wrap() {
        let rb = RingBuffer::<i32>::new(4);

        // 填满
        let data = [1, 2, 3, 4];
        assert_eq!(rb.write(&data), 4);

        // 读一半
        let mut output = [0i32; 2];
        assert_eq!(rb.read(&mut output), 2);
        assert_eq!(output, [1, 2]);

        // 再写入，测试环绕
        let more = [5, 6];
        assert_eq!(rb.write(&more), 2);

        // 读取全部
        let mut all = [0i32; 4];
        assert_eq!(rb.read(&mut all), 4);
        assert_eq!(all, [3, 4, 5, 6]);
    }

    #[test]
    fn test_ring_buffer_full() {
        let rb = RingBuffer::<i32>::new(4);

        // 写满
        let data = [1, 2, 3, 4];
        assert_eq!(rb.write(&data), 4);
        assert_eq!(rb.free_space(), 0);

        // 再写应该返回 0
        let more = [5, 6];
        assert_eq!(rb.write(&more), 0);
    }

    #[test]
    fn test_ring_buffer_empty() {
        let rb = RingBuffer::<i32>::new(4);

        // 空的时候读应该返回 0
        let mut output = [0i32; 4];
        assert_eq!(rb.read(&mut output), 0);
    }

    #[test]
    fn test_cache_line_alignment() {
        // 验证 CacheLine 确实是 128 字节对齐（Apple Silicon 兼容）
        assert_eq!(std::mem::align_of::<CacheLine<AtomicUsize>>(), 128);
        // 验证大小也是 128 字节（完整占用一个 cache line）
        assert_eq!(std::mem::size_of::<CacheLine<AtomicUsize>>(), 128);
    }

    #[test]
    fn test_no_false_sharing() {
        // 验证 write_pos 和 read_pos 在不同的 cache line
        let rb = RingBuffer::<i32>::new(16);

        let write_addr = &rb.write_pos as *const _ as usize;
        let read_addr = &rb.read_pos as *const _ as usize;

        // 两个位置的差距应该 >= 128 字节
        let distance = if write_addr > read_addr {
            write_addr - read_addr
        } else {
            read_addr - write_addr
        };

        assert!(
            distance >= 128,
            "write_pos and read_pos should be on different cache lines, distance: {}",
            distance
        );
    }
}
