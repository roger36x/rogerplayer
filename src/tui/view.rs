use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use super::model::{App, RepeatMode};
use crate::engine::PlaybackState;

pub fn draw(f: &mut Frame, app: &mut App) {
    // 垂直布局：Header, Main (Playlist + Info), Logs, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Header (with border)
            Constraint::Min(10),    // Main
            Constraint::Length(4),  // Logs
            Constraint::Length(3),  // Footer
        ])
        .split(f.size());

    draw_header(f, app, chunks[0]);
    draw_main(f, app, chunks[1]);
    draw_logs(f, app, chunks[2]);
    draw_footer(f, app, chunks[3]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let state_str = match app.engine.state() {
        PlaybackState::Playing => "[RUNNING]",
        PlaybackState::Paused => "[PAUSED]",
        PlaybackState::Stopped => "[STOPPED]",
        PlaybackState::Buffering => "[BUFFERING]",
    };

    // 构建模式标签
    let mut mode_tags = Vec::new();
    if app.shuffle {
        mode_tags.push("[SHUFFLE]");
    }
    match app.repeat_mode {
        RepeatMode::All => mode_tags.push("[REPEAT:ALL]"),
        RepeatMode::Track => mode_tags.push("[REPEAT:1]"),
        RepeatMode::Off => {}
    }
    let modes_str = mode_tags.join(" ");

    // 单行显示：Roger Player v0.1.0    [SHUFFLE] [REPEAT:ALL]    [RUNNING]
    let title = "Roger Player v0.1.0";
    let right_part = if modes_str.is_empty() {
        state_str.to_string()
    } else {
        format!("{} {}", modes_str, state_str)
    };
    let spaces = " ".repeat(area.width.saturating_sub(title.len() as u16 + right_part.len() as u16 + 2) as usize);
    let header_line = format!("{}{}{}", title, spaces, right_part);

    let block = Block::default().borders(Borders::ALL);
    let paragraph = Paragraph::new(header_line).block(block);
    f.render_widget(paragraph, area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    // 水平分割：左边播放列表，右边详细信息
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(65),
            Constraint::Percentage(35),
        ])
        .split(area);

    draw_playlist(f, app, chunks[0]);
    draw_now_playing(f, app, chunks[1]);
}

fn draw_playlist(f: &mut Frame, app: &mut App, area: Rect) {
    // 如果在输入模式，显示路径输入界面
    if app.input_mode {
        draw_path_input(f, app, area);
        return;
    }

    let items: Vec<ListItem> = app
        .playlist
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            // 添加曲目编号
            let num = format!("{:02}. ", i + 1);
            let prefix = if i == app.current_index { "> " } else { "  " };
            let content = format!("{}{}{}", prefix, num, name);

            let style = if i == app.current_index {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            ListItem::new(content).style(style)
        })
        .collect();

    let playlist = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Playlist"));

    f.render_stateful_widget(playlist, area, &mut app.playlist_state);
}

fn draw_path_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Drop Path Here");

    let inner_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    f.render_widget(block, area);

    let mut lines = Vec::new();

    // 说明文字
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Drag and drop a file or folder here",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "or type/paste the path manually:",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));

    // 输入框
    let input_display = if app.path_input.is_empty() {
        Span::styled("_", Style::default().fg(Color::Cyan).add_modifier(Modifier::SLOW_BLINK))
    } else {
        // 显示输入内容 + 光标
        let display = format!("{}_", app.path_input);
        Span::styled(display, Style::default().fg(Color::Cyan))
    };
    lines.push(Line::from(vec![
        Span::raw("> "),
        input_display,
    ]));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter to load, Esc to cancel",
        Style::default().fg(Color::DarkGray),
    )));

    // 支持的格式
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Supported: flac, wav, aiff, mp3, pcm",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner_area);
}

fn draw_now_playing(f: &mut Frame, app: &App, area: Rect) {
    let outer_block = Block::default().borders(Borders::ALL).title("Now Playing");
    f.render_widget(outer_block, area);

    // 计算内部区域（减去边框）
    let inner_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // 获取统计信息
    let stats = &app.cached_stats;
    let total_secs = app.engine.current_info().map(|i| i.duration_secs.unwrap_or(0.0)).unwrap_or(0.0);
    let progress_ratio = if total_secs > 0.0 {
        (stats.position_secs / total_secs).min(1.0)
    } else {
        0.0
    };

    // 构建显示内容
    let mut lines = Vec::new();

    // 1. 时间显示
    let time_str = if total_secs > 0.0 {
        format!(
            "Time: {:02}:{:02} / {:02}:{:02}",
            (stats.position_secs / 60.0) as u32,
            (stats.position_secs % 60.0) as u32,
            (total_secs / 60.0) as u32,
            (total_secs % 60.0) as u32
        )
    } else {
        format!(
            "Time: {:02}:{:02} / ??:??",
            (stats.position_secs / 60.0) as u32,
            (stats.position_secs % 60.0) as u32
        )
    };
    lines.push(Line::from(time_str));

    // 2. 进度条（文本样式）
    let bar_width = (inner_area.width as usize).saturating_sub(10); // 留空间给百分比
    let filled = (bar_width as f64 * progress_ratio) as usize;
    let empty = bar_width.saturating_sub(filled);
    let progress_bar = format!(
        "[{}{}] {:>3}%",
        "█".repeat(filled),
        "░".repeat(empty),
        (progress_ratio * 100.0) as u32
    );
    lines.push(Line::from(Span::styled(progress_bar, Style::default().fg(Color::Cyan))));

    // 3. 格式信息
    if let Some(info) = app.engine.current_info() {
        let format_str = if info.format == "Unknown" || info.format.is_empty() {
            let path_str = app.current_track_name();
            if let Some(ext_idx) = path_str.rfind('.') {
                path_str[ext_idx + 1..].to_uppercase()
            } else {
                "Unknown".to_string()
            }
        } else {
            info.format.clone()
        };

        let bit_depth_str = info.bit_depth
            .map(|d| format!("{}", d))
            .unwrap_or_else(|| "N/A".to_string());

        let format_line = format!(
            "Format: {} {}kHz/{}bit",
            format_str,
            info.sample_rate / 1000,
            bit_depth_str
        );
        lines.push(Line::from(Span::styled(format_line, Style::default().fg(Color::White))));

        // 4. 输出模式 + Bit-Perfect 状态
        let (hal, exclusive) = app.engine.output_mode().unwrap_or((false, false));
        let bit_perfect = app.engine.is_bit_perfect();
        let output_mode = if hal {
            if exclusive {
                "HAL (Exclusive)"
            } else {
                "HAL"
            }
        } else {
            "System Mixer"
        };
        let output_line = format!("Output: {}", output_mode);
        lines.push(Line::from(Span::styled(output_line, Style::default().fg(Color::White))));
        lines.push(Line::from("")); // 空行

        // Bit-Perfect 状态（使用醒目颜色）
        let (bp_text, bp_color) = if bit_perfect {
            ("BIT-PERFECT", Color::Green)
        } else {
            ("Not Bit-Perfect", Color::Yellow)
        };
        lines.push(Line::from(Span::styled(bp_text, Style::default().fg(bp_color).add_modifier(Modifier::BOLD))));
        lines.push(Line::from("")); // 空行

        // 5. 系统统计
        lines.push(Line::from(Span::styled("System Stats", Style::default().add_modifier(Modifier::BOLD))));

        // Buffer 条形图（动态宽度适应面板）
        let buffer_ratio = stats.buffer_fill_ratio.min(1.0);
        // "Buffer: [" = 9, "] " = 2, "100%" = 4, 共 15 固定字符
        let buffer_bar_width = (inner_area.width as usize).saturating_sub(15).max(5);
        let buffer_filled = (buffer_bar_width as f64 * buffer_ratio) as usize;
        let buffer_empty = buffer_bar_width.saturating_sub(buffer_filled);
        let buffer_line = format!(
            "Buffer: [{}{}] {:>3}%",
            "|".repeat(buffer_filled),
            " ".repeat(buffer_empty),
            (buffer_ratio * 100.0) as u32
        );
        lines.push(Line::from(Span::styled(buffer_line, Style::default().fg(Color::White))));

        // Underruns
        let underrun_color = if stats.underrun_count > 0 {
            Color::Red
        } else {
            Color::Green
        };
        let underrun_line = format!("Underruns: {}", stats.underrun_count);
        lines.push(Line::from(Span::styled(underrun_line, Style::default().fg(underrun_color))));
    } else {
        lines.push(Line::from("No track loaded"));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner_area);
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect) {
    // 只显示最近的一条日志（截图中显示的是单行日志区域）
    let log_text = if app.logs.is_empty() {
        "[LOG] Ready".to_string()
    } else {
        app.logs.last().unwrap_or(&"[LOG] Ready".to_string()).clone()
    };

    let block = Block::default().borders(Borders::ALL);
    let paragraph = Paragraph::new(log_text)
        .block(block)
        .style(Style::default().fg(Color::Gray));
    f.render_widget(paragraph, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let info = if app.input_mode {
        "Enter: Load | Esc: Cancel | q: Quit"
    } else {
        "SPACE: Pause | n/p: Next/Prev | s: Shuffle | r: Repeat | o: Open | q: Quit"
    };
    let block = Block::default().borders(Borders::ALL);
    let paragraph = Paragraph::new(info)
        .block(block)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(paragraph, area);
}
