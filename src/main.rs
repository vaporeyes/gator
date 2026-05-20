// ABOUTME: Orchestrator wiring Phases 1-4: PTY <-> parser <-> grid <-> renderer.
// ABOUTME: Single owner of grid/cursor/parser; tasks feed it via a bounded channel.

mod ansi;
mod app;
mod config;
mod event;
mod gpu;
mod grid;
mod pty;
mod render;

use ansi::StateMutator;
use config::Config;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use event::TerminalEvent;
use grid::{CursorState, Grid};
use pty::{PortablePty, PtyBackend};
use render::{CrosstermRenderer, Renderer};
use tokio::sync::mpsc;

const EVENT_CHANNEL_CAP: usize = 512;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from);
    let cfg = Config::load(config_path)?;

    let shell = cfg
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".to_string());

    // Default path is the GPU windowed compositor. `--tui` keeps the
    // crossterm renderer (terminal-in-a-terminal) as a fallback.
    if !args.iter().any(|a| a == "--tui") {
        return app::run(shell, cfg);
    }

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    let mut grid = Grid::new(cols as usize, rows as usize, cfg.scrollback);
    let mut cursor = CursorState::new();
    let mut parser = vte::Parser::new();

    let mut renderer = CrosstermRenderer::new(cols as usize, rows as usize);
    renderer.init(cols, rows)?;

    let pty = PortablePty::spawn(&shell, rows, cols).await?;
    let (mut reader, mut control) = pty.into_split();

    let (tx, mut rx) = mpsc::channel::<TerminalEvent>(EVENT_CHANNEL_CAP);

    // PTY reader pump: blocking reads already happen on a dedicated thread
    // inside spawn(); this just forwards chunks onto the event bus.
    let pty_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(chunk) = reader.recv().await {
            if pty_tx.send(TerminalEvent::PtyOutput(chunk)).await.is_err() {
                return;
            }
        }
        let _ = pty_tx.send(TerminalEvent::Shutdown).await;
    });

    // User input pump: crossterm reads are blocking, so they live on a
    // blocking thread that translates events into PTY bytes / resize.
    let input_tx = tx.clone();
    tokio::task::spawn_blocking(move || loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) => {
                if let Some(bytes) = encode_key(&key) {
                    if input_tx.blocking_send(TerminalEvent::UserInput(bytes)).is_err() {
                        return;
                    }
                }
            }
            Ok(Event::Resize(c, r)) => {
                // Coalesce: a stale resize is worthless, so drop it if the
                // channel is full rather than blocking the input thread.
                let _ = input_tx.try_send(TerminalEvent::Resize { rows: r, cols: c });
            }
            Ok(Event::Paste(s)) => {
                if input_tx.blocking_send(TerminalEvent::Paste(s)).is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });

    // Central orchestrator: the sole owner of grid/cursor/parser.
    while let Some(ev) = rx.recv().await {
        match ev {
            TerminalEvent::PtyOutput(bytes) => {
                let mut responses = Vec::new();
                let mut mutator = StateMutator {
                    grid: &mut grid,
                    cursor: &mut cursor,
                    responses: &mut responses,
                };
                // vte 0.13's Parser::advance is byte-at-a-time; this is what
                // makes fragmented multi-byte UTF-8 across reads safe.
                for &b in &bytes {
                    parser.advance(&mut mutator, b);
                }
                // Query replies (DA1/DSR) flow back to the shell's stdin.
                if !responses.is_empty() && control.write(&responses).is_err() {
                    break;
                }
                renderer.render_frame(&grid, &cursor)?;
            }
            TerminalEvent::UserInput(bytes) => {
                if control.write(&bytes).is_err() {
                    break;
                }
            }
            TerminalEvent::Paste(text) => {
                // Wrap in DEC bracketed-paste markers only if the app enabled
                // ?2004; otherwise deliver the raw bytes.
                let mut out = Vec::new();
                if grid.bracketed_paste {
                    out.extend_from_slice(b"\x1b[200~");
                    out.extend_from_slice(text.as_bytes());
                    out.extend_from_slice(b"\x1b[201~");
                } else {
                    out.extend_from_slice(text.as_bytes());
                }
                if control.write(&out).is_err() {
                    break;
                }
            }
            TerminalEvent::Resize { rows, cols } => {
                grid.resize(cols as usize, rows as usize, &mut cursor);
                renderer.resize(cols as usize, rows as usize);
                let _ = control.resize(rows, cols);
                renderer.render_frame(&grid, &cursor)?;
            }
            TerminalEvent::Shutdown => break,
        }
    }

    renderer.shutdown()?;
    Ok(())
}

/// Translate a crossterm key event into the bytes a PTY expects.
fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii() {
                    return Some(vec![(upper as u8) & 0x1f]);
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}
