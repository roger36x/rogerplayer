use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::{
    model::{App, DialogState},
    view,
};

/// TUI 绘制间隔（播放中）
///
/// 降低到 500ms (2 FPS)，最大程度减少对音频线程的干扰：
/// - 减少 stdout 写入 syscall（每次 draw 会 flush）
/// - 减少内存分配（ratatui 每次 draw 分配 widget tree）
/// - 减少 CPU cache pollution（layout 计算、diff 计算）
const DRAW_INTERVAL_PLAYING_MS: u64 = 500;

/// TUI 绘制间隔（空闲/暂停/停止）
///
/// 空闲时可以更频繁地绘制，提升 UI 响应性
const DRAW_INTERVAL_IDLE_MS: u64 = 100;

/// 输入轮询间隔
///
/// 保持较快的轮询频率以确保键盘响应性。
/// poll 本身是极轻量级 syscall（select/kqueue），不会影响音频
const INPUT_POLL_MS: u64 = 50;

/// TUI 运行入口
pub fn run(mut app: App) -> io::Result<()> {
    // =======================================================
    // 隔离措施 0: TUI 线程堆内存隔离
    // =======================================================
    // 创建独立的 macOS malloc zone，将 TUI 线程的堆分配路由到独立内存区域
    // 消除 TUI 的 malloc/free 对音频线程的 cache pollution 和 allocator contention
    crate::alloc::platform::init_tui_zone();
    crate::alloc::platform::mark_tui_thread();

    // =======================================================
    // 隔离措施 1: 降低 TUI 线程优先级
    // =======================================================
    // 将 TUI 线程（主线程）设为系统最低优先级
    // 确保 UI 渲染和输入处理永远不会抢占音频线程的 CPU 时间
    set_tui_thread_low_priority();

    // =======================================================
    // 隔离措施 2: 设置线程亲和性标签
    // =======================================================
    // TUI 使用 tag 2，音频线程（解码器 + IO 回调）使用 tag 1
    // macOS 调度器会尽量将不同 tag 的线程调度到不同核心组
    // 减少跨核心缓存争用和 cache pollution
    set_thread_affinity_tag(2);

    // 1. Setup Terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // 2. Run Loop
    // =======================================================
    // 隔离措施 3: 输入轮询与绘制解耦
    // =======================================================
    // - 输入轮询：每 50ms（保持键盘响应性，poll 是轻量 syscall）
    // - 绘制：播放中 500ms / 空闲 100ms（减少 stdout I/O 和内存分配）
    // - 统计读取：仅在绘制前（减少对音频线程 cache line 的访问）
    let mut last_draw = Instant::now();
    let mut needs_redraw = true;

    // 自动播放第一首
    if !app.playlist.is_empty() {
        app.play_current();
    } else {
        app.log("Drop a file or folder to start playing".to_string());
    }

    loop {
        // === 确定当前绘制间隔 ===
        let is_active = app.engine.is_playing();
        let draw_interval = if is_active {
            Duration::from_millis(DRAW_INTERVAL_PLAYING_MS)
        } else {
            Duration::from_millis(DRAW_INTERVAL_IDLE_MS)
        };

        // === 输入处理（高频率，保持响应性）===
        // poll timeout = min(输入轮询间隔, 距下次绘制的剩余时间)
        let time_to_draw = draw_interval.saturating_sub(last_draw.elapsed());
        let poll_timeout = Duration::from_millis(INPUT_POLL_MS).min(time_to_draw);

        if crossterm::event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key_event(&mut app, key.code);
                    needs_redraw = true;
                }
            }
        }

        // === 轻量级曲目结束检测 ===
        // is_track_finished() 仅读取一个 AtomicBool (eof_reached)
        // 只有 eof_reached=true 时才会进一步检查 ring_buffer.available()
        // 所以播放过程中几乎零开销
        if app.check_track_end() {
            needs_redraw = true;
        }

        // === 选曲光标超时检查（纯本地状态，无原子操作）===
        app.check_cursor_timeout();

        // === 绘制（低频率，减少对音频的干扰）===
        let draw_elapsed = last_draw.elapsed();
        let should_draw = (needs_redraw && draw_elapsed >= Duration::from_millis(16))
            || draw_elapsed >= draw_interval;

        if should_draw {
            // 仅在绘制前读取统计信息（减少 cache line 访问频率）
            app.update_stats();
            terminal.draw(|f| view::draw(f, &mut app))?;
            last_draw = Instant::now();
            needs_redraw = false;
        }

        if app.should_quit {
            break;
        }
    }

    // 3. Restore Terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // 停止播放引擎
    let _ = app.engine.stop();

    Ok(())
}

/// 处理按键事件（从主循环中提取，减少主循环复杂度）
fn handle_key_event(app: &mut App, code: KeyCode) {
    // 弹窗模式优先处理
    if !matches!(app.dialog, DialogState::None) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.dialog_select_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.dialog_select_down();
            }
            KeyCode::Char('1') => {
                app.dialog_select_option(0);
                app.dialog_confirm();
            }
            KeyCode::Char('2') => {
                app.dialog_select_option(1);
                app.dialog_confirm();
            }
            KeyCode::Enter => {
                app.dialog_confirm();
            }
            KeyCode::Esc => {
                app.dialog_cancel();
            }
            _ => {}
        }
        return;
    }

    // 帮助页面：任意键关闭
    if app.show_help {
        app.show_help = false;
        return;
    }

    // 搜索模式下的按键处理
    if app.search_mode {
        match code {
            KeyCode::Enter => {
                if let Some(i) = app.playlist_state.selected() {
                    app.current_index = i;
                    app.play_current();
                }
                app.exit_search();
            }
            KeyCode::Esc => {
                app.exit_search();
            }
            KeyCode::Down => {
                app.search_next();
            }
            KeyCode::Up => {
                app.search_prev();
            }
            KeyCode::Backspace => {
                app.search_input.pop();
                app.do_search();
            }
            KeyCode::Char(c) => {
                app.search_input.push(c);
                app.do_search();
            }
            _ => {}
        }
        return;
    }

    // 输入模式下的按键处理
    if app.input_mode {
        match code {
            KeyCode::Enter => {
                if !app.path_input.is_empty() {
                    let path = app.path_input.clone();
                    app.load_path(&path);
                }
            }
            KeyCode::Esc => {
                if app.playlist.is_empty() {
                    app.should_quit = true;
                } else {
                    app.input_mode = false;
                    app.path_input.clear();
                }
            }
            KeyCode::Char('q') if app.path_input.is_empty() => {
                app.should_quit = true;
            }
            KeyCode::Backspace => {
                app.path_input.pop();
            }
            KeyCode::Char(c) => {
                app.path_input.push(c);
            }
            _ => {}
        }
        return;
    }

    // 正常模式下的按键处理
    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Char(' ') => {
            if let Err(e) = app.engine.toggle_pause() {
                app.log(format!("Error: {}", e));
            }
        }
        KeyCode::Char('n') => app.next_track(),
        KeyCode::Char('p') => app.prev_track(),
        KeyCode::Char('o') => {
            app.input_mode = true;
            app.path_input.clear();
        }
        KeyCode::Char('s') => app.toggle_shuffle(),
        KeyCode::Char('r') => app.cycle_repeat(),
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.playlist.is_empty() {
                app.last_selection_time = Some(Instant::now());
                app.show_cursor = true;

                let len = app.playlist.len();
                let current = app.playlist_state.selected().unwrap_or(0);
                let new_index = (current + 1) % len;
                app.playlist_state.select(Some(new_index));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !app.playlist.is_empty() {
                app.last_selection_time = Some(Instant::now());
                app.show_cursor = true;

                let len = app.playlist.len();
                let current = app.playlist_state.selected().unwrap_or(0);
                let new_index = if current > 0 { current - 1 } else { len - 1 };
                app.playlist_state.select(Some(new_index));
            }
        }
        KeyCode::Enter => {
            if let Some(i) = app.playlist_state.selected() {
                app.current_index = i;
                app.play_current();
            }
        }
        KeyCode::Char('/') => {
            app.search_mode = true;
            app.search_input.clear();
        }
        KeyCode::Char('h') => {
            app.show_help = true;
        }
        _ => {}
    }
}

// =============================================================================
// 线程隔离措施（macOS 特化）
// =============================================================================

/// 降低 TUI 线程优先级至系统最低
///
/// 两级优先级降低：
/// 1. QoS 类设为 BACKGROUND — macOS 调度器最低优先级类别
///    - 系统会将此线程视为后台任务
///    - 不会抢占 USER_INTERACTIVE（音频线程）的 CPU 时间
///    - 在 CPU 繁忙时会被大幅降低调度频率
/// 2. Nice 值设为 20 — Unix 调度最低优先级
///    - 作为 QoS 的补充，进一步降低调度优先权
///    - 使用 PRIO_DARWIN_THREAD 仅影响当前线程
fn set_tui_thread_low_priority() {
    #[cfg(target_os = "macos")]
    {
        // QOS_CLASS_BACKGROUND = 0x09
        const QOS_CLASS_BACKGROUND: u32 = 0x09;
        // PRIO_DARWIN_THREAD = 3 (macOS 专有，仅设置当前线程)
        const PRIO_DARWIN_THREAD: libc::c_int = 3;

        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }

        unsafe {
            // 1. 设置 QoS 类为 BACKGROUND（最低优先级）
            pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);

            // 2. 设置 nice 值为 20（Unix 最低优先级），仅当前线程
            libc::setpriority(PRIO_DARWIN_THREAD, 0, 20);
        }
    }
}

/// 设置 macOS 线程亲和性标签
///
/// macOS 调度器使用 affinity tag 进行核心分组调度：
/// - 相同 tag 的线程倾向于调度到相邻核心（共享 L2 cache）
/// - 不同 tag 的线程倾向于调度到不同核心组
///
/// 标签分配策略：
/// - Tag 1: 音频线程（解码器 + CoreAudio IO 回调）— 共享音频数据
/// - Tag 2: TUI 线程 — 独立的 UI 渲染，不与音频共享热数据
///
/// 效果：
/// - 减少 TUI 的 cache pollution 对音频线程的影响
/// - 音频线程间共享 ring buffer 数据，亲和性有利于 cache 命中
fn set_thread_affinity_tag(tag: i32) {
    #[cfg(target_os = "macos")]
    {
        const THREAD_AFFINITY_POLICY: u32 = 4;

        #[repr(C)]
        struct ThreadAffinityPolicy {
            affinity_tag: i32,
        }

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
            let policy = ThreadAffinityPolicy { affinity_tag: tag };
            thread_policy_set(
                thread,
                THREAD_AFFINITY_POLICY,
                &policy as *const _ as *const std::ffi::c_void,
                1, // policy count = 1 (struct 中有 1 个 integer_t)
            );
        }
    }
}
