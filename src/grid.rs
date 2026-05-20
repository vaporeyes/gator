// ABOUTME: Phase 3 screen model: cache-aligned cells, viewport, and bounded scrollback.
// ABOUTME: Owns all screen state; mutated only through narrow &mut borrows from the parser.

use std::collections::VecDeque;
use vte::Params;

/// Bitmask for text attributes to keep struct size minimal.
pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_ITALIC: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;

/// Optimized to 16 bytes for strict cache alignment.
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
pub struct Cell {
    pub c: char,       // 4 bytes
    pub fg: u32,       // 4 bytes (0x00RRGGBB)
    pub bg: u32,       // 4 bytes (0x00RRGGBB)
    pub flags: u16,    // 2 bytes
    pub _padding: u16, // 2 bytes (alignment)
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: 0xFFFFFF, bg: 0x000000, flags: 0, _padding: 0 }
    }
}

/// Snapshot saved by DECSC (ESC 7 / CSI s) and restored by DECRC (ESC 8 / CSI u).
#[derive(Copy, Clone, Debug)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Cell,
}

/// Cursor position and the active pen carried into newly written cells.
#[derive(Debug)]
pub struct CursorState {
    pub row: usize,
    pub col: usize,
    /// Pen attributes applied to every written cell (set by SGR).
    pub pen: Cell,
    /// DECTCEM (`CSI ?25 h/l`): whether the cursor block is drawn.
    pub visible: bool,
    saved: Option<SavedCursor>,
}

impl CursorState {
    pub fn new() -> Self {
        Self { row: 0, col: 0, pen: Cell::default(), visible: true, saved: None }
    }

    /// DECSC: save position + pen.
    pub fn save(&mut self) {
        self.saved = Some(SavedCursor { row: self.row, col: self.col, pen: self.pen });
    }

    /// DECRC: restore the saved position + pen (no-op if nothing saved).
    pub fn restore(&mut self) {
        if let Some(s) = self.saved {
            self.row = s.row;
            self.col = s.col;
            self.pen = s.pen;
        }
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        }
    }

    /// CSI H / CSI f: parameters are 1-based (row;col); absence means 1.
    pub fn move_to(&mut self, params: &Params) {
        let mut it = params.iter();
        let row = it.next().and_then(|p| p.first().copied()).unwrap_or(1);
        let col = it.next().and_then(|p| p.first().copied()).unwrap_or(1);
        self.row = row.max(1) as usize - 1;
        self.col = col.max(1) as usize - 1;
    }
}

pub struct Grid {
    cols: usize,
    rows: usize,
    /// 1D array representing the active viewport.
    viewport: Vec<Cell>,
    /// Ring buffer enforcing a strict memory ceiling for historical lines.
    scrollback: VecDeque<Vec<Cell>>,
    max_scrollback: usize,
    /// DECSTBM scroll region, inclusive row bounds (default full screen).
    scroll_top: usize,
    scroll_bottom: usize,
    /// DECAWM (`CSI ?7 h/l`): wrap to next line at the right edge.
    autowrap: bool,
    /// Alternate screen active (vim/less/htop); suppresses scrollback.
    alt_active: bool,
    /// Primary viewport stashed while the alternate screen is active.
    saved_primary: Option<Vec<Cell>>,
    /// `CSI ?2004 h/l`: orchestrator wraps pasted text in ESC[200~/201~.
    pub bracketed_paste: bool,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        Self {
            cols,
            rows,
            viewport: vec![Cell::default(); cols * rows],
            scrollback: VecDeque::with_capacity(max_scrollback),
            max_scrollback,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            autowrap: true,
            alt_active: false,
            saved_primary: None,
            bracketed_paste: false,
        }
    }

    /// Translates 2D coordinate to 1D index.
    #[inline(always)]
    pub fn index(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    // Accessors: viewport/cols are private, so the renderer's FrameDiff
    // (a separate type) reaches the grid through these.
    #[inline(always)]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[inline(always)]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[inline(always)]
    pub fn viewport(&self) -> &[Cell] {
        &self.viewport
    }

    /// Standard printable character insertion with right-edge wrap.
    pub fn write_char(&mut self, cursor: &mut CursorState, c: char) {
        if cursor.col >= self.cols {
            if self.autowrap {
                cursor.col = 0;
                self.line_feed(cursor);
            } else {
                // DECAWM off: stay on the last column, overwriting it.
                cursor.col = self.cols - 1;
            }
        }
        let idx = self.index(cursor.col, cursor.row);
        let mut cell = cursor.pen;
        cell.c = c;
        self.viewport[idx] = cell;
        cursor.col += 1;
    }

    /// Line feed: advance a row, scrolling the region when at its bottom margin.
    pub fn line_feed(&mut self, cursor: &mut CursorState) {
        if cursor.row == self.scroll_bottom {
            self.scroll_region_up();
        } else if cursor.row + 1 < self.rows {
            cursor.row += 1;
        }
    }

    /// RI (ESC M): move up one row, scrolling the region down at the top margin.
    pub fn reverse_index(&mut self, cursor: &mut CursorState) {
        if cursor.row == self.scroll_top {
            self.scroll_region_down();
        } else if cursor.row > 0 {
            cursor.row -= 1;
        }
    }

    /// Scroll the [top, bottom] region up by one line. Only a full-screen
    /// primary scroll feeds scrollback (alt screen / regions never do).
    fn scroll_region_up(&mut self) {
        let (top, bot) = (self.scroll_top, self.scroll_bottom);
        if bot < top {
            return;
        }
        if !self.alt_active && top == 0 && bot == self.rows - 1 && self.max_scrollback > 0 {
            let line0: Vec<Cell> = self.viewport[0..self.cols].to_vec();
            if self.scrollback.len() == self.max_scrollback {
                self.scrollback.pop_front();
            }
            self.scrollback.push_back(line0);
        }
        let start = top * self.cols;
        let end = (bot + 1) * self.cols;
        self.viewport.copy_within(start + self.cols..end, start);
        let b = bot * self.cols;
        for cell in &mut self.viewport[b..b + self.cols] {
            *cell = Cell::default();
        }
    }

    /// Scroll the [top, bottom] region down by one line, clearing the top row.
    fn scroll_region_down(&mut self) {
        let (top, bot) = (self.scroll_top, self.scroll_bottom);
        if bot < top {
            return;
        }
        let start = top * self.cols;
        let end = (bot + 1) * self.cols;
        self.viewport.copy_within(start..end - self.cols, start + self.cols);
        let t = top * self.cols;
        for cell in &mut self.viewport[t..t + self.cols] {
            *cell = Cell::default();
        }
    }

    /// CSI L (IL): insert n blank lines at the cursor, within the region.
    pub fn insert_lines(&mut self, cursor: &CursorState, n: usize) {
        let (row, bot) = (cursor.row, self.scroll_bottom);
        if row < self.scroll_top || row > bot {
            return;
        }
        let n = n.min(bot - row + 1);
        let start = row * self.cols;
        let end = (bot + 1) * self.cols;
        self.viewport.copy_within(start..end - n * self.cols, start + n * self.cols);
        for cell in &mut self.viewport[start..start + n * self.cols] {
            *cell = Cell::default();
        }
    }

    /// CSI M (DL): delete n lines at the cursor, within the region.
    pub fn delete_lines(&mut self, cursor: &CursorState, n: usize) {
        let (row, bot) = (cursor.row, self.scroll_bottom);
        if row < self.scroll_top || row > bot {
            return;
        }
        let n = n.min(bot - row + 1);
        let start = row * self.cols;
        let end = (bot + 1) * self.cols;
        self.viewport.copy_within(start + n * self.cols..end, start);
        for cell in &mut self.viewport[end - n * self.cols..end] {
            *cell = Cell::default();
        }
    }

    /// DECSTBM (CSI t;b r): set scroll region (1-based) and home the cursor.
    pub fn set_scroll_region(&mut self, top_1based: usize, bottom_1based: usize, cursor: &mut CursorState) {
        let top = top_1based.max(1) - 1;
        let bottom = (bottom_1based.max(1) - 1).min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        cursor.row = 0;
        cursor.col = 0;
    }

    pub fn set_autowrap(&mut self, on: bool) {
        self.autowrap = on;
    }

    /// Enter the alternate screen: stash primary, present a cleared buffer.
    pub fn enter_alt_screen(&mut self) {
        if self.alt_active {
            return;
        }
        let cleared = vec![Cell::default(); self.cols * self.rows];
        self.saved_primary = Some(std::mem::replace(&mut self.viewport, cleared));
        self.alt_active = true;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    /// Leave the alternate screen, restoring the primary buffer.
    pub fn exit_alt_screen(&mut self) {
        if !self.alt_active {
            return;
        }
        let primary = self.saved_primary.take();
        self.viewport = match primary {
            Some(p) if p.len() == self.cols * self.rows => p,
            // Size changed while in alt screen: fall back to a clear primary.
            _ => vec![Cell::default(); self.cols * self.rows],
        };
        self.alt_active = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    /// SGR (CSI m): mutate the active pen on the cursor.
    pub fn update_attributes(&mut self, cursor: &mut CursorState, params: &Params) {
        let mut it = params.iter();
        while let Some(p) = it.next() {
            match p.first().copied().unwrap_or(0) {
                0 => cursor.pen = Cell::default(),
                1 => cursor.pen.flags |= ATTR_BOLD,
                3 => cursor.pen.flags |= ATTR_ITALIC,
                4 => cursor.pen.flags |= ATTR_UNDERLINE,
                22 => cursor.pen.flags &= !ATTR_BOLD,
                23 => cursor.pen.flags &= !ATTR_ITALIC,
                24 => cursor.pen.flags &= !ATTR_UNDERLINE,
                n @ 30..=37 => cursor.pen.fg = ansi_16(n - 30),
                39 => cursor.pen.fg = Cell::default().fg,
                n @ 40..=47 => cursor.pen.bg = ansi_16(n - 40),
                49 => cursor.pen.bg = Cell::default().bg,
                n @ 90..=97 => cursor.pen.fg = ansi_16(n - 90 + 8),
                n @ 100..=107 => cursor.pen.bg = ansi_16(n - 100 + 8),
                38 => {
                    if let Some(rgb) = read_extended_color(&mut it) {
                        cursor.pen.fg = rgb;
                    }
                }
                48 => {
                    if let Some(rgb) = read_extended_color(&mut it) {
                        cursor.pen.bg = rgb;
                    }
                }
                _ => {}
            }
        }
    }

    /// CSI J: clear screen variants (0 = cursor->end, 1 = start->cursor, 2 = all).
    pub fn erase_in_display(&mut self, cursor: &CursorState, params: &Params) {
        let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
        let cursor_idx = self.index(cursor.col.min(self.cols - 1), cursor.row.min(self.rows - 1));
        let len = self.viewport.len();
        let (start, end) = match mode {
            0 => (cursor_idx, len),
            1 => (0, (cursor_idx + 1).min(len)),
            2 | 3 => (0, len),
            _ => return,
        };
        for cell in &mut self.viewport[start..end] {
            *cell = Cell::default();
        }
    }

    /// CSI G / HPA: set column, 1-based, clamped to the row.
    pub fn set_col(&self, cursor: &mut CursorState, col_1based: usize) {
        cursor.col = col_1based.max(1) - 1;
        cursor.col = cursor.col.min(self.cols.saturating_sub(1));
    }

    /// CSI d / VPA: set row, 1-based, clamped to the screen.
    pub fn set_row(&self, cursor: &mut CursorState, row_1based: usize) {
        cursor.row = (row_1based.max(1) - 1).min(self.rows.saturating_sub(1));
    }

    /// CSI A/B/C/D: relative cursor move, clamped (no scroll, no wrap).
    pub fn move_rel(&self, cursor: &mut CursorState, dcol: isize, drow: isize) {
        let col = cursor.col as isize + dcol;
        let row = cursor.row as isize + drow;
        cursor.col = col.clamp(0, self.cols as isize - 1) as usize;
        cursor.row = row.clamp(0, self.rows as isize - 1) as usize;
    }

    /// CSI K: erase in line (0 = cursor->eol, 1 = bol->cursor, 2 = whole row).
    pub fn erase_in_line(&mut self, cursor: &CursorState, mode: u16) {
        let row = cursor.row.min(self.rows - 1);
        let base = row * self.cols;
        let col = cursor.col.min(self.cols);
        let (s, e) = match mode {
            0 => (col, self.cols),
            1 => (0, (col + 1).min(self.cols)),
            2 => (0, self.cols),
            _ => return,
        };
        for cell in &mut self.viewport[base + s..base + e] {
            *cell = Cell::default();
        }
    }

    /// CSI P (DCH): delete n chars at the cursor, shifting the rest of the row left.
    pub fn delete_chars(&mut self, cursor: &CursorState, n: usize) {
        let row = cursor.row.min(self.rows - 1);
        let base = row * self.cols;
        let col = cursor.col.min(self.cols.saturating_sub(1));
        let n = n.min(self.cols - col);
        let line = &mut self.viewport[base..base + self.cols];
        line.copy_within(col + n..self.cols, col);
        for cell in &mut line[self.cols - n..] {
            *cell = Cell::default();
        }
    }

    /// CSI @ (ICH): insert n blanks at the cursor, shifting the rest of the row right.
    pub fn insert_chars(&mut self, cursor: &CursorState, n: usize) {
        let row = cursor.row.min(self.rows - 1);
        let base = row * self.cols;
        let col = cursor.col.min(self.cols.saturating_sub(1));
        let n = n.min(self.cols - col);
        let line = &mut self.viewport[base..base + self.cols];
        line.copy_within(col..self.cols - n, col + n);
        for cell in &mut line[col..col + n] {
            *cell = Cell::default();
        }
    }

    /// CSI X (ECH): erase n chars at the cursor in place (no shift).
    pub fn erase_chars(&mut self, cursor: &CursorState, n: usize) {
        let row = cursor.row.min(self.rows - 1);
        let base = row * self.cols;
        let col = cursor.col.min(self.cols);
        let end = (col + n).min(self.cols);
        for cell in &mut self.viewport[base + col..base + end] {
            *cell = Cell::default();
        }
    }

    /// Simple resize: reallocate and clamp. True reflow/rewrap is deferred (see spec
    /// Phase 3 directive); rewrapping historical lines is a separate work item.
    pub fn resize(&mut self, cols: usize, rows: usize, cursor: &mut CursorState) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut next = vec![Cell::default(); cols * rows];
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for y in 0..copy_rows {
            for x in 0..copy_cols {
                next[y * cols + x] = self.viewport[y * self.cols + x];
            }
        }
        self.viewport = next;
        self.cols = cols;
        self.rows = rows;
        // Scroll region and the stashed alt-primary are sized to the old
        // geometry; reset them rather than index out of bounds.
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.saved_primary = None;
        cursor.row = cursor.row.min(rows.saturating_sub(1));
        cursor.col = cursor.col.min(cols.saturating_sub(1));
    }
}

/// Map the 16 ANSI palette indices to 0x00RRGGBB.
fn ansi_16(i: u16) -> u32 {
    const PALETTE: [u32; 16] = [
        0x000000, 0xCD0000, 0x00CD00, 0xCDCD00, 0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5,
        0x7F7F7F, 0xFF0000, 0x00FF00, 0xFFFF00, 0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
    ];
    PALETTE[(i as usize) & 0xF]
}

/// Consume the tail of an SGR 38/48 sequence: `;2;r;g;b` truecolor or `;5;n` 256-color.
fn read_extended_color(it: &mut vte::ParamsIter) -> Option<u32> {
    match it.next().and_then(|p| p.first().copied())? {
        2 => {
            let r = it.next().and_then(|p| p.first().copied())? as u32;
            let g = it.next().and_then(|p| p.first().copied())? as u32;
            let b = it.next().and_then(|p| p.first().copied())? as u32;
            Some((r << 16) | (g << 8) | b)
        }
        5 => {
            let n = it.next().and_then(|p| p.first().copied())? as u32;
            Some(xterm_256(n))
        }
        _ => None,
    }
}

/// Resolve an xterm 256-color index to RGB.
fn xterm_256(n: u32) -> u32 {
    match n {
        0..=15 => ansi_16(n as u16),
        16..=231 => {
            let n = n - 16;
            let levels = [0u32, 95, 135, 175, 215, 255];
            let r = levels[(n / 36) as usize];
            let g = levels[((n / 6) % 6) as usize];
            let b = levels[(n % 6) as usize];
            (r << 16) | (g << 8) | b
        }
        _ => {
            let v = 8 + (n - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}
