//! TUI 线程内存隔离分配器
//!
//! 使用 macOS malloc zone 将 TUI 线程的堆分配路由到独立的内存区域，
//! 彻底消除 TUI 的内存活动对音频线程的 cache 干扰。
//!
//! # 原理
//!
//! macOS malloc zone 是独立的堆管理器，拥有自己的内存池、空闲链表和元数据页面。
//! TUI zone 分配的内存与音频线程使用的内存在物理页面上完全分离，
//! 消除了以下干扰源：
//!
//! - **Cache pollution**: TUI 分配/释放不会驱逐音频线程的 L1/L2 cache line
//! - **Allocator contention**: TUI 的 malloc/free 不会与音频线程竞争全局堆锁
//! - **TLB 压力**: TUI 的页面访问模式不会影响音频线程的 TLB 命中率
//!
//! # 安全性
//!
//! macOS 的 `free()` 和 `realloc()` 自动识别指针所属的 zone，
//! 因此即使对象跨 zone 边界传递，释放操作也是安全的。

use std::alloc::{GlobalAlloc, Layout, System};

/// TUI 内存隔离分配器
///
/// 作为 `#[global_allocator]` 使用，透明地将 TUI 线程的堆分配
/// 路由到独立的 macOS malloc zone。
///
/// - **TUI 线程（标记后）**: 新分配 → TUI zone（独立内存区域）
/// - **其他线程**: 新分配 → System allocator（默认行为）
/// - **释放/重分配**: macOS 自动识别指针所属 zone，无需手动路由
pub struct TuiIsolatedAllocator;

unsafe impl GlobalAlloc for TuiIsolatedAllocator {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(target_os = "macos")]
        {
            if platform::is_tui_thread() {
                if let Some(ptr) = unsafe { platform::zone_alloc(layout) } {
                    return ptr;
                }
            }
        }
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // macOS free() 内部自动识别指针所属的 zone 并调用对应 zone 的 free。
        // 无论指针来自 TUI zone 还是 System zone，都能正确释放。
        // 非 macOS 平台等价于 System.dealloc()。
        unsafe { libc::free(ptr as *mut libc::c_void) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // 常见情况：alignment ≤ 16（ratatui 的 Vec/String 均满足）
        // macOS realloc() 自动识别 zone 并在原 zone 内重分配
        if layout.align() <= 16 && new_size >= layout.align() {
            return unsafe { libc::realloc(ptr as *mut libc::c_void, new_size) as *mut u8 };
        }

        // 超对齐情况（极罕见）：alloc + copy + free
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    ptr,
                    new_ptr,
                    layout.size().min(new_size),
                );
                self.dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}

// =============================================================================
// macOS 平台实现
// =============================================================================

#[cfg(target_os = "macos")]
pub(crate) mod platform {
    use std::alloc::Layout;
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicPtr, Ordering};

    // macOS malloc zone API
    extern "C" {
        fn malloc_create_zone(start_size: usize, flags: u32) -> *mut c_void;
        fn malloc_zone_malloc(zone: *mut c_void, size: usize) -> *mut c_void;
        fn malloc_zone_memalign(
            zone: *mut c_void,
            alignment: usize,
            size: usize,
        ) -> *mut c_void;
    }

    /// TUI 专用 malloc zone
    static TUI_ZONE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    // 线程本地标记：当前线程是否为 TUI 线程
    // 使用 `const { }` 初始化确保 TLS 访问不触发堆分配（避免递归）
    thread_local! {
        static IS_TUI_THREAD: Cell<bool> = const { Cell::new(false) };
    }

    /// 创建 TUI 专用 malloc zone
    ///
    /// 调用后，TUI 线程的新分配将路由到此 zone 的独立内存池。
    /// 必须在 `mark_tui_thread()` 之前调用。
    pub fn init_tui_zone() {
        let zone = unsafe { malloc_create_zone(0, 0) };
        if !zone.is_null() {
            TUI_ZONE.store(zone, Ordering::Release);
        }
    }

    /// 标记当前线程为 TUI 线程
    ///
    /// 调用后，当前线程的所有新堆分配将路由到 TUI zone。
    pub fn mark_tui_thread() {
        IS_TUI_THREAD.with(|f| f.set(true));
    }

    /// 检查当前线程是否为 TUI 线程
    ///
    /// 开销：~2-5ns（TLS 寄存器读取 + Cell 读取）
    #[inline]
    pub fn is_tui_thread() -> bool {
        IS_TUI_THREAD.try_with(|f| f.get()).unwrap_or(false)
    }

    /// 从 TUI zone 分配内存
    ///
    /// 返回 None 表示 zone 未初始化或分配失败（调用方应回退到 System）
    #[inline]
    pub unsafe fn zone_alloc(layout: Layout) -> Option<*mut u8> {
        let zone = TUI_ZONE.load(Ordering::Relaxed);
        if zone.is_null() {
            return None;
        }

        let ptr = if layout.align() <= 16 {
            // macOS malloc 保证 16 字节对齐（64 位系统）
            unsafe { malloc_zone_malloc(zone, layout.size()) }
        } else {
            // 需要更大对齐时使用 memalign
            unsafe { malloc_zone_memalign(zone, layout.align(), layout.size()) }
        };

        if ptr.is_null() {
            None
        } else {
            Some(ptr as *mut u8)
        }
    }
}

// =============================================================================
// 非 macOS 平台：透传到 System allocator
// =============================================================================

#[cfg(not(target_os = "macos"))]
pub(crate) mod platform {
    pub fn init_tui_zone() {}
    pub fn mark_tui_thread() {}
}
