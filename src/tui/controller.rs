use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::{model::App, view};

/// TUI 运行入口
pub fn run(mut app: App) -> io::Result<()> {
    // 1. Setup Terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // 2. Run Loop
    let tick_rate = Duration::from_millis(50); // 20 FPS，足够流畅且不占用太多 CPU
    let mut last_tick = Instant::now();

    // 自动播放第一首
    if !app.playlist.is_empty() {
        app.play_current();
    } else {
        app.log("No audio files found in directory.".to_string());
    }

    loop {
        // Draw
        terminal.draw(|f| view::draw(f, &mut app))?;

        // Handle Input
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
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
                        KeyCode::Down | KeyCode::Char('j') => {
                            if !app.playlist.is_empty() {
                                let i = match app.playlist_state.selected() {
                                    Some(i) => {
                                        if i >= app.playlist.len() - 1 {
                                            0
                                        } else {
                                            i + 1
                                        }
                                    }
                                    None => 0,
                                };
                                app.playlist_state.select(Some(i));
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if !app.playlist.is_empty() {
                                let i = match app.playlist_state.selected() {
                                    Some(i) => {
                                        if i == 0 {
                                            app.playlist.len() - 1
                                        } else {
                                            i - 1
                                        }
                                    }
                                    None => 0,
                                };
                                app.playlist_state.select(Some(i));
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(i) = app.playlist_state.selected() {
                                app.current_index = i;
                                app.play_current();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Handle Tick
        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
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
