// ABOUTME: winit event loop for the GPU terminal: owns Grid/cursor/parser/PTY
// ABOUTME: and the GpuRenderer; drains the PTY, parses, renders, routes input.

use crate::ansi::StateMutator;
use crate::config::Config;
use crate::effects::EffectsState;
use crate::event::{MouseAction, MouseButton, Mods, ScrollCmd};
use crate::find::FindState;
use crate::gpu::{FindRender, GpuRenderer};
use crate::grid::{AbsCoord, CursorState, Grid, Selection};
use crate::mouse::encode_mouse;
use crate::pty::{PortablePty, TryChunk};
use crate::session::{restore_into, SessionLogger};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::dpi::PhysicalPosition;
use winit::dpi::LogicalSize;
use winit::event::{
    ElementState, Event, KeyEvent, MouseButton as WMouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(not(target_os = "macos"))]
use winit::window::Icon;
use winit::window::WindowBuilder;

/// Idle PTY poll cadence. winit gives us no wakeup on PTY data, so we tick.
const POLL: Duration = Duration::from_millis(5);

pub fn run(shell: String, cfg: Config) -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    let builder = WindowBuilder::new()
        .with_title("gator")
        .with_inner_size(LogicalSize::new(
            cfg.window.width as f64,
            cfg.window.height as f64,
        ));
    let builder = apply_window_icon(builder, &cfg)?;
    let window = Arc::new(builder.build(&event_loop)?);

    let mut renderer = pollster::block_on(GpuRenderer::new(window.clone(), &cfg))?;
    let (mut cols, mut rows) = renderer.grid_dims();

    let mut grid = Grid::new(cols, rows, cfg.scrollback);
    let mut cursor = CursorState::new();
    let mut parser = vte::Parser::new();

    let mut logger = SessionLogger::from_config(&cfg.session);
    restore_into(&mut grid, &cfg.session.text_log, cfg.session.restore_lines);

    let mut effects = EffectsState::new(cfg.effects, cfg.chrome.visual_bell);
    let mut alt_was_active = false;
    let mut last_bell_seq: u64 = 0;
    let mut last_title_seq: u64 = 0;
    let mut find: Option<FindState> = None;
    let app_start = Instant::now();

    let pty = PortablePty::spawn_sync(&shell, rows as u16, cols as u16)?;
    let (mut reader, mut control) = pty.into_split();

    let mut mods = ModifiersState::empty();
    // Mouse state for local selection / passthrough.
    let mut cursor_phys: PhysicalPosition<f64> = PhysicalPosition::new(0.0, 0.0);
    let mut button_down: Option<MouseButton> = None;
    let mut last_cell: Option<(usize, usize)> = None;
    // Clipboard handle; None if the platform refused us one.
    let mut clipboard = arboard::Clipboard::new().ok();

    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));

    event_loop.run(move |event, elwt| match event {
        Event::WindowEvent { event, .. } => match event {
            WindowEvent::CloseRequested => elwt.exit(),

            WindowEvent::ModifiersChanged(m) => mods = m.state(),

            WindowEvent::Resized(size) => {
                effects.record_resize(Instant::now());
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
                // Priority 0: find-mode owns the keyboard.
                if let Some(fs) = find.as_mut() {
                    if handle_find_key(fs, &mut grid, &logical_key, mods) {
                        find = None; // Esc closed it
                    }
                    window.request_redraw();
                    return;
                }
                // Priority 0b: Ctrl+F / Cmd+F opens find.
                if is_find_open_combo(&logical_key, mods) {
                    let mut fs = FindState::new();
                    fs.recompute(&grid);
                    find = Some(fs);
                    window.request_redraw();
                    return;
                }

                // Record the keystroke for the ripple effect at the center
                // of the text cursor cell (where new input visually lands).
                let pad = renderer.padding_phys() as f32;
                let kx = pad + cursor.col as f32 * renderer.cell_w as f32
                    + renderer.cell_w as f32 * 0.5;
                let ky = pad + cursor.row as f32 * renderer.cell_h as f32
                    + renderer.cell_h as f32 * 0.5;
                effects.record_keystroke(Instant::now(), (kx, ky));

                // Priority 1: clipboard copy bind (never reaches PTY).
                if is_copy_combo(&logical_key, mods) {
                    if let Some(sel) = grid.selection {
                        if let Some(cb) = clipboard.as_mut() {
                            let _ = cb.set_text(grid.get_selection_text(sel));
                        }
                    }
                }
                // Priority 1a: clipboard paste (Cmd+V or Ctrl+Shift+V).
                else if is_paste_combo(&logical_key, mods) {
                    if !grid.at_bottom() {
                        grid.scroll_to_bottom();
                    }
                    paste_from_clipboard(&grid, clipboard.as_mut(), &mut control);
                    window.request_redraw();
                }
                // Priority 1b: live font zoom (Cmd/Ctrl =/+/-/0).
                else if let Some(zoom) = zoom_combo(&logical_key, mods) {
                    apply_zoom(
                        &mut renderer,
                        &mut grid,
                        &mut cursor,
                        &mut control,
                        &mut cols,
                        &mut rows,
                        zoom,
                    );
                    window.request_redraw();
                }
                // Priority 2: Shift+navigation = local scrollback view.
                else if mods.shift_key() {
                    if let Some(d) = scroll_for_key(&logical_key, rows) {
                        apply_scroll(&mut grid, d);
                        window.request_redraw();
                    } else if let Some(bytes) = encode_key(&logical_key, mods.control_key()) {
                        snap_and_send(&mut grid, &window, &mut control, &bytes, elwt);
                    }
                }
                // Default: route to PTY; typing snaps the view to the bottom.
                else if let Some(bytes) = encode_key(&logical_key, mods.control_key()) {
                    snap_and_send(&mut grid, &window, &mut control, &bytes, elwt);
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_x, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32) / (renderer.cell_h as f32),
                };
                let m = to_mods(mods);
                if grid.mouse_mode.enabled() && !m.shift {
                    // Passthrough: one synthetic wheel press per line of scroll.
                    let surf = renderer.surface_size();
                    let (mx, my) = effects.inverse_pick(
                        cursor_phys.x,
                        cursor_phys.y,
                        (surf.0 as f64, surf.1 as f64),
                    );
                    let (col, row) = pixel_to_cell(
                        winit::dpi::PhysicalPosition::new(mx, my),
                        &renderer,
                        cols,
                        rows,
                    );
                    let button = if lines > 0.0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    let count = lines.abs().round() as i32;
                    for _ in 0..count.max(1) {
                        if let Some(bytes) =
                            encode_mouse(grid.mouse_mode, MouseAction::Press, button, col, row, m)
                        {
                            let _ = control.write(&bytes);
                        }
                    }
                } else {
                    // Local scroll. 3 lines per wheel "click" matches GTK / GNOME.
                    let delta_lines = (lines * 3.0).round() as isize;
                    if delta_lines != 0 {
                        grid.scroll_lines(delta_lines);
                        window.request_redraw();
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                cursor_phys = position;
                effects.update_cursor(Instant::now(), position.x, position.y);
                if let Some(btn) = button_down {
                    let surf = renderer.surface_size();
                    let (mx, my) = effects.inverse_pick(
                        position.x,
                        position.y,
                        (surf.0 as f64, surf.1 as f64),
                    );
                    let mapped = winit::dpi::PhysicalPosition::new(mx, my);
                    let (col, row) = pixel_to_cell(mapped, &renderer, cols, rows);
                    // Spec 1 edge case: only emit a Drag when the cell changes.
                    if last_cell != Some((col, row)) {
                        last_cell = Some((col, row));
                        dispatch_mouse(
                            MouseAction::Drag,
                            btn,
                            col,
                            row,
                            to_mods(mods),
                            &mut grid,
                            &mut control,
                            &window,
                        );
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let btn = match button {
                    WMouseButton::Left => MouseButton::Left,
                    WMouseButton::Middle => MouseButton::Middle,
                    WMouseButton::Right => MouseButton::Right,
                    _ => return,
                };
                let surf = renderer.surface_size();
                let (mx, my) = effects.inverse_pick(
                    cursor_phys.x,
                    cursor_phys.y,
                    (surf.0 as f64, surf.1 as f64),
                );
                let (col, row) = pixel_to_cell(
                    winit::dpi::PhysicalPosition::new(mx, my),
                    &renderer,
                    cols,
                    rows,
                );
                // Cmd/Ctrl + Left click opens an OSC 8 hyperlink or a
                // detected plain-text URL under the cell. No selection.
                if state == ElementState::Pressed
                    && btn == MouseButton::Left
                    && (mods.super_key() || mods.control_key())
                {
                    let abs_row = grid.displayed_abs_row(row);
                    let link = grid
                        .hyperlinks
                        .get(&(abs_row, col))
                        .cloned()
                        .or_else(|| crate::links::detect_url_at(&grid.row_text(abs_row), col));
                    if let Some(url) = link {
                        crate::links::open(&url);
                        return;
                    }
                }
                let action = match state {
                    ElementState::Pressed => {
                        button_down = Some(btn);
                        last_cell = Some((col, row));
                        MouseAction::Press
                    }
                    ElementState::Released => {
                        button_down = None;
                        MouseAction::Release
                    }
                };
                dispatch_mouse(action, btn, col, row, to_mods(mods), &mut grid, &mut control, &window);
                // On release, copy the finalized selection to clipboard.
                if matches!(action, MouseAction::Release) {
                    if let Some(sel) = grid.selection {
                        if let Some(cb) = clipboard.as_mut() {
                            let text = grid.get_selection_text(sel);
                            if !text.is_empty() {
                                let _ = cb.set_text(text);
                            }
                        }
                    }
                }
            }

            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let (sw, sh) = renderer.surface_size();
                let u = effects.build_uniform(now, (sw as f32, sh as f32));
                let cursor_on_now = cursor_on(&cursor, now, app_start);
                let find_render = find.as_ref().map(|f| FindRender {
                    query: &f.query,
                    ranges: &f.matches,
                    current: f.current,
                });
                renderer.render(
                    &grid,
                    &cursor,
                    cursor_on_now,
                    grid.selection,
                    find_render.as_ref(),
                    &u,
                );
            }

            _ => {}
        },

        // No PTY wakeup from winit, so poll on a short timer.
        Event::AboutToWait => {
            let mut dirty = false;
            let mut closed = false;
            loop {
                match reader.try_recv() {
                    TryChunk::Data(bytes) => {
                        logger.log_raw(&bytes);
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
            // Flush text-log lines and keep the view anchored on growth.
            let pushed = logger.drain_text(&mut grid);
            if pushed > 0 && grid.view_offset > 0 {
                grid.view_offset = (grid.view_offset + pushed).min(grid.scrollback_len());
            }
            // Alt-screen transitions trigger the cube/page-turn effect.
            if grid.is_alt() != alt_was_active {
                effects.record_alt_toggle(Instant::now(), grid.is_alt());
                alt_was_active = grid.is_alt();
            }
            // BEL rang at least once during this parse pass?
            if grid.bell_seq != last_bell_seq {
                effects.record_bell(Instant::now());
                last_bell_seq = grid.bell_seq;
            }
            // OSC 0/2 -> window title.
            if grid.title_seq != last_title_seq {
                window.set_title(&grid.title);
                last_title_seq = grid.title_seq;
            }
            // OSC 52 -> system clipboard.
            if let Some(text) = grid.pending_clipboard.take() {
                if let Some(cb) = clipboard.as_mut() {
                    let _ = cb.set_text(text);
                }
            }
            if closed {
                logger.flush();
                elwt.exit();
                return;
            }
            // Animation tick: drive 60Hz redraws while any effect is alive,
            // 4Hz for the blink-only case, otherwise drop back to passive PTY
            // polling for battery.
            let now = Instant::now();
            let animating_fast = effects.animation_active(now);
            let animating_blink =
                cursor.blink && cursor.visible && grid.at_bottom() && !grid.is_alt();
            if dirty || animating_fast || animating_blink {
                window.request_redraw();
            }
            let next = if animating_fast {
                now + Duration::from_millis(16)
            } else if animating_blink {
                now + Duration::from_millis(250)
            } else {
                now + POLL
            };
            elwt.set_control_flow(ControlFlow::WaitUntil(next));
        }

        _ => {}
    })?;

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply_window_icon(mut builder: WindowBuilder, cfg: &Config) -> anyhow::Result<WindowBuilder> {
    if cfg.chrome.titlebar_icon {
        builder = builder.with_window_icon(Some(gator_window_icon()?));
    }
    Ok(builder)
}

#[cfg(target_os = "macos")]
fn apply_window_icon(builder: WindowBuilder, _cfg: &Config) -> anyhow::Result<WindowBuilder> {
    Ok(builder)
}

#[cfg(not(target_os = "macos"))]
fn gator_window_icon() -> anyhow::Result<Icon> {
    const SIZE: u32 = 64;
    let mut rgba = vec![0; (SIZE * SIZE * 4) as usize];

    fill_rounded_rect(&mut rgba, SIZE, 6, 6, 52, 52, 10, [8, 17, 11, 255]);
    fill_rounded_rect(&mut rgba, SIZE, 12, 18, 40, 34, 6, [12, 26, 16, 255]);
    fill_rect(&mut rgba, SIZE, 12, 18, 40, 6, [20, 40, 26, 255]);
    fill_rect(&mut rgba, SIZE, 16, 35, 30, 4, [221, 230, 184, 255]);
    fill_rect(&mut rgba, SIZE, 16, 35, 12, 4, [143, 191, 90, 255]);
    draw_prompt(&mut rgba, SIZE);
    draw_gator_ridge(&mut rgba, SIZE);
    fill_rect(&mut rgba, SIZE, 28, 45, 16, 4, [211, 180, 92, 255]);

    Icon::from_rgba(rgba, SIZE, SIZE).map_err(Into::into)
}

#[cfg(not(target_os = "macos"))]
fn draw_prompt(rgba: &mut [u8], size: u32) {
    for i in 0..12 {
        fill_rect(rgba, size, 18 + i, 28 + i / 2, 2, 3, [221, 230, 184, 255]);
        fill_rect(rgba, size, 29 - i, 34 + i / 2, 2, 3, [221, 230, 184, 255]);
    }
}

#[cfg(not(target_os = "macos"))]
fn draw_gator_ridge(rgba: &mut [u8], size: u32) {
    let points = [(18, 28), (24, 24), (31, 29), (38, 24), (46, 29)];
    for pair in points.windows(2) {
        draw_line(rgba, size, pair[0], pair[1], [143, 191, 90, 255]);
    }
    fill_rect(rgba, size, 40, 27, 2, 2, [8, 17, 11, 255]);
}

#[cfg(not(target_os = "macos"))]
fn draw_line(rgba: &mut [u8], size: u32, from: (u32, u32), to: (u32, u32), color: [u8; 4]) {
    let (mut x0, mut y0) = (from.0 as i32, from.1 as i32);
    let (x1, y1) = (to.0 as i32, to.1 as i32);
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        fill_rect(rgba, size, x0 as u32, y0 as u32, 3, 3, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn fill_rounded_rect(
    rgba: &mut [u8],
    size: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: [u8; 4],
) {
    let right = x + w - 1;
    let bottom = y + h - 1;
    for py in y..=bottom {
        for px in x..=right {
            let cx = if px < x + radius {
                x + radius
            } else if px > right - radius {
                right - radius
            } else {
                px
            };
            let cy = if py < y + radius {
                y + radius
            } else if py > bottom - radius {
                bottom - radius
            } else {
                py
            };
            let dx = px as i32 - cx as i32;
            let dy = py as i32 - cy as i32;
            if dx * dx + dy * dy <= (radius as i32) * (radius as i32) {
                set_pixel(rgba, size, px, py, color);
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn fill_rect(rgba: &mut [u8], size: u32, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    for py in y..y + h {
        for px in x..x + w {
            set_pixel(rgba, size, px, py, color);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn set_pixel(rgba: &mut [u8], size: u32, x: u32, y: u32, color: [u8; 4]) {
    if x >= size || y >= size {
        return;
    }
    let idx = ((y * size + x) * 4) as usize;
    rgba[idx..idx + 4].copy_from_slice(&color);
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

fn to_mods(m: ModifiersState) -> Mods {
    Mods {
        ctrl: m.control_key(),
        shift: m.shift_key(),
        alt: m.alt_key(),
        super_: m.super_key(),
    }
}

/// Map a physical pixel position to a clamped (col, row) cell coordinate.
fn pixel_to_cell(
    p: PhysicalPosition<f64>,
    renderer: &GpuRenderer,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let pad = renderer.padding_phys() as f64;
    let cw = renderer.cell_w.max(1) as f64;
    let ch = renderer.cell_h.max(1) as f64;
    let col = ((p.x - pad).max(0.0) / cw).floor() as usize;
    let row = ((p.y - pad).max(0.0) / ch).floor() as usize;
    (col.min(cols.saturating_sub(1)), row.min(rows.saturating_sub(1)))
}

/// Either encode the event for the child PTY (mouse-mode + no Shift override)
/// or drive the local selection model.
fn dispatch_mouse(
    action: MouseAction,
    button: MouseButton,
    col: usize,
    row: usize,
    mods: Mods,
    grid: &mut Grid,
    control: &mut crate::pty::PtyControl,
    window: &winit::window::Window,
) {
    let passthrough = grid.mouse_mode.enabled() && !mods.shift;
    if passthrough {
        if let Some(bytes) = encode_mouse(grid.mouse_mode, action, button, col, row, mods) {
            let _ = control.write(&bytes);
        }
        return;
    }
    if button != MouseButton::Left {
        return;
    }
    let abs = AbsCoord { abs_row: grid.displayed_abs_row(row), col };
    match action {
        MouseAction::Press => grid.selection = Some(Selection::new(abs)),
        MouseAction::Drag => {
            if let Some(sel) = grid.selection.as_mut() {
                sel.head = abs;
            }
        }
        MouseAction::Release => {
            if let Some(sel) = grid.selection.as_mut() {
                sel.head = abs;
            }
        }
    }
    window.request_redraw();
}

fn scroll_for_key(key: &Key, rows: usize) -> Option<ScrollCmd> {
    let page = rows.saturating_sub(1) as isize;
    match key {
        Key::Named(NamedKey::PageUp) => Some(ScrollCmd::Lines(page)),
        Key::Named(NamedKey::PageDown) => Some(ScrollCmd::Lines(-page)),
        Key::Named(NamedKey::ArrowUp) => Some(ScrollCmd::Lines(1)),
        Key::Named(NamedKey::ArrowDown) => Some(ScrollCmd::Lines(-1)),
        Key::Named(NamedKey::Home) => Some(ScrollCmd::ToTop),
        Key::Named(NamedKey::End) => Some(ScrollCmd::ToBottom),
        _ => None,
    }
}

fn apply_scroll(grid: &mut Grid, d: ScrollCmd) {
    match d {
        ScrollCmd::Lines(n) => grid.scroll_lines(n),
        ScrollCmd::ToTop => grid.scroll_to_top(),
        ScrollCmd::ToBottom => grid.scroll_to_bottom(),
    }
}

/// Typing while scrolled snaps back to the bottom so the user sees their input.
fn snap_and_send(
    grid: &mut Grid,
    window: &winit::window::Window,
    control: &mut crate::pty::PtyControl,
    bytes: &[u8],
    elwt: &winit::event_loop::EventLoopWindowTarget<()>,
) {
    if !grid.at_bottom() {
        grid.scroll_to_bottom();
        window.request_redraw();
    }
    if control.write(bytes).is_err() {
        elwt.exit();
    }
}

fn is_find_open_combo(key: &Key, mods: ModifiersState) -> bool {
    if !(mods.control_key() || mods.super_key()) {
        return false;
    }
    match key {
        Key::Character(s) => s.eq_ignore_ascii_case("f"),
        _ => false,
    }
}

/// Returns `true` to close find (Esc); `false` keeps it open.
fn handle_find_key(
    fs: &mut FindState,
    grid: &mut Grid,
    key: &Key,
    mods: ModifiersState,
) -> bool {
    match key {
        Key::Named(NamedKey::Escape) => return true,
        Key::Named(NamedKey::Enter) => {
            if mods.shift_key() {
                fs.prev();
            } else {
                fs.next();
            }
            fs.scroll_to_current(grid);
        }
        Key::Named(NamedKey::Backspace) => {
            fs.pop_char(grid);
            fs.scroll_to_current(grid);
        }
        Key::Character(s) => {
            // Modifier-prefixed combos (Ctrl/Cmd + letter) are ignored: they
            // are app-level shortcuts, not query characters.
            if mods.control_key() || mods.super_key() {
                return false;
            }
            for c in s.chars() {
                if !c.is_control() {
                    fs.push_char(c, grid);
                }
            }
            fs.scroll_to_current(grid);
        }
        _ => {}
    }
    false
}

/// Half-second blink phase: cursor draws when (elapsed / 500ms) is even.
fn cursor_on(cursor: &CursorState, now: Instant, start: Instant) -> bool {
    if !cursor.blink {
        return true;
    }
    let ms = now.saturating_duration_since(start).as_millis();
    (ms / 500) % 2 == 0
}

/// Live font zoom binding result; +1 / -1 / 0 (reset).
enum Zoom {
    In,
    Out,
    Reset,
}

fn zoom_combo(key: &Key, mods: ModifiersState) -> Option<Zoom> {
    if !(mods.control_key() || mods.super_key()) {
        return None;
    }
    let s = match key {
        Key::Character(s) => s.as_str(),
        _ => return None,
    };
    match s {
        "=" | "+" => Some(Zoom::In),
        "-" | "_" => Some(Zoom::Out),
        "0" => Some(Zoom::Reset),
        _ => None,
    }
}

fn apply_zoom(
    renderer: &mut GpuRenderer,
    grid: &mut Grid,
    cursor: &mut CursorState,
    control: &mut crate::pty::PtyControl,
    cols: &mut usize,
    rows: &mut usize,
    z: Zoom,
) {
    let pt = match z {
        Zoom::In => renderer.font_pt() + 1.0,
        Zoom::Out => renderer.font_pt() - 1.0,
        Zoom::Reset => renderer.font_pt_default(),
    };
    renderer.set_font_pt(pt);
    let (nc, nr) = renderer.grid_dims();
    if (nc, nr) != (*cols, *rows) {
        *cols = nc;
        *rows = nr;
        grid.resize(*cols, *rows, cursor);
        let _ = control.resize(*rows as u16, *cols as u16);
    }
}

/// Ctrl+Shift+V (Linux/Windows convention) or Cmd+V (macOS native).
fn is_paste_combo(key: &Key, mods: ModifiersState) -> bool {
    let is_v = match key {
        Key::Character(s) => s.eq_ignore_ascii_case("v"),
        _ => false,
    };
    if !is_v {
        return false;
    }
    (mods.control_key() && mods.shift_key()) || mods.super_key()
}

/// Read the OS clipboard and write the result to the PTY. When the child app
/// has enabled DEC bracketed paste (`?2004h`), the payload is wrapped so the
/// shell can distinguish pasted input from typed input.
fn paste_from_clipboard(
    grid: &Grid,
    clipboard: Option<&mut arboard::Clipboard>,
    control: &mut crate::pty::PtyControl,
) {
    let Some(cb) = clipboard else { return };
    let Ok(text) = cb.get_text() else { return };
    if text.is_empty() {
        return;
    }
    let mut out = Vec::new();
    if grid.bracketed_paste {
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
    } else {
        out.extend_from_slice(text.as_bytes());
    }
    let _ = control.write(&out);
}

/// Ctrl+Shift+C (Linux/Windows convention) or Cmd+C (macOS native).
fn is_copy_combo(key: &Key, mods: ModifiersState) -> bool {
    let is_c = match key {
        Key::Character(s) => s.eq_ignore_ascii_case("c"),
        _ => false,
    };
    if !is_c {
        return false;
    }
    (mods.control_key() && mods.shift_key()) || mods.super_key()
}
