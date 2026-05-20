// ABOUTME: winit event loop for the GPU terminal: owns Grid/cursor/parser/PTY
// ABOUTME: and the GpuRenderer; drains the PTY, parses, renders, routes input.

use crate::ansi::StateMutator;
use crate::config::Config;
use crate::gpu::GpuRenderer;
use crate::grid::{CursorState, Grid};
use crate::pty::{PortablePty, TryChunk};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Event, KeyEvent, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::WindowBuilder;

/// Idle PTY poll cadence. winit gives us no wakeup on PTY data, so we tick.
const POLL: Duration = Duration::from_millis(5);

pub fn run(shell: String, cfg: Config) -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("gaterminal")
            .with_inner_size(LogicalSize::new(
                cfg.window.width as f64,
                cfg.window.height as f64,
            ))
            .build(&event_loop)?,
    );

    let mut renderer = pollster::block_on(GpuRenderer::new(window.clone(), &cfg))?;
    let (mut cols, mut rows) = renderer.grid_dims();

    let mut grid = Grid::new(cols, rows, cfg.scrollback);
    let mut cursor = CursorState::new();
    let mut parser = vte::Parser::new();

    let pty = PortablePty::spawn_sync(&shell, rows as u16, cols as u16)?;
    let (mut reader, mut control) = pty.into_split();

    let mut mods = ModifiersState::empty();

    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));

    event_loop.run(move |event, elwt| match event {
        Event::WindowEvent { event, .. } => match event {
            WindowEvent::CloseRequested => elwt.exit(),

            WindowEvent::ModifiersChanged(m) => mods = m.state(),

            WindowEvent::Resized(size) => {
                renderer.resize(size.width, size.height);
                let (nc, nr) = renderer.grid_dims();
                if (nc, nr) != (cols, rows) {
                    cols = nc;
                    rows = nr;
                    grid.resize(cols, rows, &mut cursor);
                    let _ = control.resize(rows as u16, cols as u16);
                }
                window.request_redraw();
            }

            // HiDPI: re-rasterize glyphs at the new backing scale, then
            // recompute the cell grid for the (physical-pixel) surface.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                renderer.rescale(scale_factor as f32);
                let size = window.inner_size();
                renderer.resize(size.width, size.height);
                let (nc, nr) = renderer.grid_dims();
                cols = nc;
                rows = nr;
                grid.resize(cols, rows, &mut cursor);
                let _ = control.resize(rows as u16, cols as u16);
                window.request_redraw();
            }

            WindowEvent::KeyboardInput {
                event: KeyEvent { state: ElementState::Pressed, logical_key, .. },
                ..
            } => {
                if let Some(bytes) = encode_key(&logical_key, mods.control_key()) {
                    if control.write(&bytes).is_err() {
                        elwt.exit();
                    }
                }
            }

            WindowEvent::RedrawRequested => renderer.render(&grid, &cursor),

            _ => {}
        },

        // No PTY wakeup from winit, so poll on a short timer.
        Event::AboutToWait => {
            let mut dirty = false;
            let mut closed = false;
            loop {
                match reader.try_recv() {
                    TryChunk::Data(bytes) => {
                        let mut responses = Vec::new();
                        let mut mutator = StateMutator {
                            grid: &mut grid,
                            cursor: &mut cursor,
                            responses: &mut responses,
                        };
                        for &b in &bytes {
                            parser.advance(&mut mutator, b);
                        }
                        if !responses.is_empty() && control.write(&responses).is_err() {
                            closed = true;
                        }
                        dirty = true;
                    }
                    TryChunk::Empty => break,
                    TryChunk::Closed => {
                        closed = true;
                        break;
                    }
                }
            }
            if closed {
                elwt.exit();
            } else if dirty {
                window.request_redraw();
            }
            elwt.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
        }

        _ => {}
    })?;

    Ok(())
}

/// Translate a winit logical key into the bytes a PTY expects.
fn encode_key(key: &Key, ctrl: bool) -> Option<Vec<u8>> {
    match key {
        Key::Character(s) => {
            if ctrl {
                if let Some(c) = s.chars().next() {
                    let u = c.to_ascii_uppercase();
                    if u.is_ascii() {
                        return Some(vec![(u as u8) & 0x1f]);
                    }
                }
            }
            Some(s.as_bytes().to_vec())
        }
        Key::Named(n) => match n {
            NamedKey::Enter => Some(vec![b'\r']),
            NamedKey::Backspace => Some(vec![0x7f]),
            NamedKey::Tab => Some(vec![b'\t']),
            NamedKey::Escape => Some(vec![0x1b]),
            NamedKey::Space => Some(vec![b' ']),
            NamedKey::ArrowUp => Some(b"\x1b[A".to_vec()),
            NamedKey::ArrowDown => Some(b"\x1b[B".to_vec()),
            NamedKey::ArrowRight => Some(b"\x1b[C".to_vec()),
            NamedKey::ArrowLeft => Some(b"\x1b[D".to_vec()),
            NamedKey::Home => Some(b"\x1b[H".to_vec()),
            NamedKey::End => Some(b"\x1b[F".to_vec()),
            NamedKey::PageUp => Some(b"\x1b[5~".to_vec()),
            NamedKey::PageDown => Some(b"\x1b[6~".to_vec()),
            NamedKey::Delete => Some(b"\x1b[3~".to_vec()),
            _ => None,
        },
        _ => None,
    }
}
