// ABOUTME: Phase 3 screen model: cache-aligned cells, viewport, and bounded scrollback.
// ABOUTME: Owns all screen state; mutated only through narrow &mut borrows from the parser.

use std::collections::VecDeque;
use vte::Params;

/// Bitmask for text attributes to keep struct size minimal.
pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_ITALIC: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;
/// Leading half of a double-width cell (CJK, fullwidth forms). The next cell
/// has `ATTR_WIDE_TRAILING` and is occupied by this glyph's right half.
pub const ATTR_WIDE: u16 = 1 << 3;
/// Right half of a double-width cell. Renderers skip drawing its glyph;
/// selection text extraction ignores it.
pub const ATTR_WIDE_TRAILING: u16 = 1 << 4;

/// Optimized to 16 bytes for strict cache alignment.
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
pub struct Cell {
    pub c: char,            // 4 bytes
    pub fg: u32,            // 4 bytes (0x00RRGGBB)
    pub bg: u32,            // 4 bytes (0x00RRGGBB)
    pub flags: u16,         // 2 bytes
    /// Index into `Grid::combining_pool` for zero-width chars attached to
    /// this cell. `0` is the reserved "no extras" sentinel.
    pub combining_id: u16,  // 2 bytes
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: 0xFFFFFF, bg: 0x000000, flags: 0, combining_id: 0 }
    }
}

/// Snapshot saved by DECSC (ESC 7 / CSI s) and restored by DECRC (ESC 8 / CSI u).
#[derive(Copy, Clone, Debug)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Cell,
}

/// DECSCUSR cursor shape (`CSI Ps SP q`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Underline,
    Bar,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self::Block
    }
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
    /// DECSCUSR shape.
    pub style: CursorStyle,
    /// DECSCUSR blink flag. Renderer interprets together with a clock.
    pub blink: bool,
    saved: Option<SavedCursor>,
}

impl CursorState {
    pub fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            pen: Cell::default(),
            visible: true,
            style: CursorStyle::Block,
            blink: false,
            saved: None,
        }
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
    /// DEC mouse-reporting modes (`?1000`/`?1002`/`?1006`).
    pub mouse_mode: MouseMode,
    /// Active text selection in absolute-row coords (anchor + floating head).
    pub selection: Option<Selection>,
    /// Monotonic counter incremented on each BEL (`0x07`). The orchestrator
    /// records the value once per parse pass; an increment triggers the
    /// visual-bell flash (when `[chrome] visual_bell = true`).
    pub bell_seq: u64,
    /// Last `OSC 0`/`OSC 2` window-title string (UTF-8) and a monotonic
    /// counter; the orchestrator pushes the title to the window when seq bumps.
    pub title: String,
    pub title_seq: u64,
    /// Last `OSC 52` clipboard payload waiting to be copied to the OS clipboard.
    /// Drained by the orchestrator each tick.
    pub pending_clipboard: Option<String>,
    /// Active `OSC 8` hyperlink applied to subsequent `print` writes.
    pub active_link: Option<String>,
    /// Sparse map of `(absolute row, col)` -> URL for OSC 8 hyperlinks.
    pub hyperlinks: std::collections::HashMap<(usize, usize), String>,
    /// Interned combining-mark sequences. Index 0 is reserved as the empty
    /// "no extras" sentinel so a fresh `Cell` (combining_id = 0) means
    /// "single codepoint". Cells with combining marks point at later slots.
    combining_pool: Vec<Vec<char>>,
    combining_index: std::collections::HashMap<Vec<char>, u16>,
    /// Lines pushed into scrollback since the orchestrator last drained.
    /// The orchestrator writes these to the plain-text session log (if any)
    /// and uses the count to keep `view_offset` anchored on growth.
    pub pending_log: Vec<Vec<Cell>>,
    /// Rows the live view is scrolled up from the bottom (0 = at bottom).
    /// Always 0 while the alternate screen is active.
    pub view_offset: usize,
}

/// DEC mouse-reporting modes the child app has enabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct MouseMode {
    /// `?1000`: report press and release.
    pub button: bool,
    /// `?1002`: also report drag while a button is held.
    pub drag: bool,
    /// `?1006`: SGR-encoded reports (`\x1b[<b;c;rM`/`m`), else X10 fallback.
    pub sgr_encoded: bool,
}

impl MouseMode {
    pub fn enabled(&self) -> bool {
        self.button || self.drag
    }
}

/// A row in either scrollback (0..scrollback.len()) or the viewport above.
/// Absolute indexing keeps selections stable as new lines scroll in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsCoord {
    pub abs_row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: AbsCoord,
    pub head: AbsCoord,
}

impl Selection {
    pub fn new(at: AbsCoord) -> Self {
        Self { anchor: at, head: at }
    }

    /// Reading-order (top-left, bottom-right) range. Both ends inclusive.
    pub fn normalized(&self) -> (AbsCoord, AbsCoord) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn contains(&self, p: AbsCoord) -> bool {
        let (a, b) = self.normalized();
        p >= a && p <= b
    }
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
            mouse_mode: MouseMode::default(),
            selection: None,
            bell_seq: 0,
            title: String::new(),
            title_seq: 0,
            pending_clipboard: None,
            active_link: None,
            hyperlinks: std::collections::HashMap::new(),
            combining_pool: vec![Vec::new()], // index 0 = empty sentinel
            combining_index: std::collections::HashMap::new(),
            pending_log: Vec::new(),
            view_offset: 0,
        }
    }

    /// Intern a combining sequence and return its `combining_id`. Empty
    /// sequences map to 0. The pool grows monotonically; pool entries become
    /// unreferenced when their cells are overwritten (bounded leak).
    fn intern_combining(&mut self, marks: Vec<char>) -> u16 {
        if marks.is_empty() {
            return 0;
        }
        if let Some(id) = self.combining_index.get(&marks) {
            return *id;
        }
        let id = self.combining_pool.len().min(u16::MAX as usize) as u16;
        if id == u16::MAX {
            // Pool exhausted; degrade gracefully (drop the combining mark).
            return 0;
        }
        self.combining_pool.push(marks.clone());
        self.combining_index.insert(marks, id);
        id
    }

    /// Concatenate a cell's base codepoint with its combining marks (if any).
    pub fn cluster_string(&self, cell: &Cell) -> String {
        if cell.combining_id == 0 {
            return cell.c.to_string();
        }
        let mut s = String::with_capacity(4);
        s.push(cell.c);
        if let Some(extras) = self.combining_pool.get(cell.combining_id as usize) {
            for &c in extras {
                s.push(c);
            }
        }
        s
    }

    /// Attach a zero-width codepoint to the cell at the previous cursor
    /// position. No-op if the cursor is at the start of a row.
    pub fn attach_combining(&mut self, cursor: &CursorState, c: char) {
        if cursor.col == 0 {
            return;
        }
        // Find the visible (non wide-trailing) cell to attach to.
        let mut col = cursor.col - 1;
        let row = cursor.row;
        loop {
            let idx = self.index(col, row);
            let cur_id = self.viewport[idx].combining_id;
            if self.viewport[idx].flags & ATTR_WIDE_TRAILING != 0 && col > 0 {
                col -= 1;
                continue;
            }
            let mut extras: Vec<char> = self
                .combining_pool
                .get(cur_id as usize)
                .cloned()
                .unwrap_or_default();
            extras.push(c);
            let new_id = self.intern_combining(extras);
            self.viewport[idx].combining_id = new_id;
            return;
        }
    }

    /// Place a width-2 character spanning two adjacent cells. Wraps at the
    /// right edge if autowrap is on; clamps and overwrites otherwise.
    pub fn write_wide(&mut self, cursor: &mut CursorState, c: char) {
        if cursor.col + 2 > self.cols {
            if self.autowrap {
                cursor.col = 0;
                self.line_feed(cursor);
                if cursor.col + 2 > self.cols {
                    return; // viewport too narrow for any wide char
                }
            } else {
                cursor.col = self.cols.saturating_sub(2);
            }
        }
        let row = cursor.row;
        let lead_idx = self.index(cursor.col, row);
        let trail_idx = self.index(cursor.col + 1, row);
        let mut lead = cursor.pen;
        lead.c = c;
        lead.flags |= ATTR_WIDE;
        let mut trail = cursor.pen;
        trail.c = ' ';
        trail.flags |= ATTR_WIDE_TRAILING;
        self.viewport[lead_idx] = lead;
        self.viewport[trail_idx] = trail;
        if !self.alt_active {
            if let Some(url) = &self.active_link {
                let abs_row = self.scrollback.len() + row;
                self.hyperlinks.insert((abs_row, cursor.col), url.clone());
            }
        }
        cursor.col += 2;
    }

    /// Called from the parser on BEL (`0x07`); increments the counter the
    /// orchestrator watches to fire the visual bell.
    pub fn ring_bell(&mut self) {
        self.bell_seq = self.bell_seq.wrapping_add(1);
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

    /// Raw viewport slice. Renderers now use `display_row` (scrollback-aware);
    /// kept public for the spec-defined Grid surface and grid-internal tests.
    #[allow(dead_code)]
    #[inline(always)]
    pub fn viewport(&self) -> &[Cell] {
        &self.viewport
    }

    /// Absolute row index of viewport row 0 (== `scrollback.len()`).
    #[inline(always)]
    pub fn viewport_first_abs_row(&self) -> usize {
        self.scrollback.len()
    }

    #[inline(always)]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// `true` while a child app is on the alternate screen (vim, htop, ...).
    #[inline(always)]
    pub fn is_alt(&self) -> bool {
        self.alt_active
    }

    /// Scroll the live view up (positive delta) or down (negative). No-op when
    /// the alternate screen is active (vim/htop own scrolling there).
    pub fn scroll_lines(&mut self, delta: isize) {
        if self.alt_active {
            return;
        }
        let cur = self.view_offset as isize + delta;
        let max = self.scrollback.len() as isize;
        self.view_offset = cur.clamp(0, max) as usize;
    }

    pub fn scroll_to_top(&mut self) {
        if !self.alt_active {
            self.view_offset = self.scrollback.len();
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.view_offset = 0;
    }

    /// Are we showing the live viewport (the bottom) right now?
    #[inline(always)]
    pub fn at_bottom(&self) -> bool {
        self.view_offset == 0
    }

    /// Return cells for displayed row `y` (0 = top of view). When
    /// `view_offset > 0`, the topmost rows come from scrollback.
    pub fn display_row(&self, y: usize) -> &[Cell] {
        let cols = self.cols;
        if y >= self.rows {
            return &self.viewport[0..0];
        }
        // Absolute row currently shown at viewport row 0.
        let top_abs = self.viewport_first_abs_row().saturating_sub(self.view_offset);
        let abs_row = top_abs + y;
        let vp_top = self.viewport_first_abs_row();
        if abs_row < vp_top {
            // Scrollback row.
            self.scrollback
                .get(abs_row)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        } else {
            let vy = abs_row - vp_top;
            if vy < self.rows {
                let base = vy * cols;
                &self.viewport[base..base + cols]
            } else {
                &[]
            }
        }
    }

    /// Append a line to scrollback without recording it in `pending_log`
    /// (used by cross-session restore so we don't re-log restored history).
    pub fn push_history_line(&mut self, mut line: Vec<Cell>) {
        if self.max_scrollback == 0 {
            return;
        }
        // Pad/truncate to current grid width.
        line.resize(self.cols, Cell::default());
        if self.scrollback.len() == self.max_scrollback {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(line);
    }

    /// Return the text of an absolute row as a String (cells joined, trailing
    /// spaces preserved). Used for URL detection at click sites.
    pub fn row_text(&self, abs_row: usize) -> String {
        let cols = self.cols;
        let top = self.viewport_first_abs_row();
        if abs_row < top {
            match self.scrollback.get(abs_row) {
                Some(cells) => cells.iter().map(|c| c.c).collect(),
                None => String::new(),
            }
        } else {
            let vy = abs_row - top;
            if vy < self.rows {
                let base = vy * cols;
                self.viewport[base..base + cols].iter().map(|c| c.c).collect()
            } else {
                String::new()
            }
        }
    }

    /// Extract selection text in reading order. Each row is trimmed of trailing
    /// spaces (DECAWM wrap-tracking is a deferred refinement). Rows are joined
    /// with '\n'. Out-of-bounds rows yield empty contributions.
    pub fn get_selection_text(&self, sel: Selection) -> String {
        let (start, end) = sel.normalized();
        let top = self.viewport_first_abs_row();
        let cols = self.cols;
        let mut out = String::new();
        for abs_row in start.abs_row..=end.abs_row {
            let row_cells: Option<&[Cell]> = if abs_row < top {
                self.scrollback.get(abs_row).map(|v| v.as_slice())
            } else {
                let vy = abs_row - top;
                if vy < self.rows {
                    let base = vy * cols;
                    Some(&self.viewport[base..base + cols])
                } else {
                    None
                }
            };

            let col_start = if abs_row == start.abs_row { start.col } else { 0 };
            // Inclusive end column, clamped.
            let col_end_excl = if abs_row == end.abs_row {
                (end.col + 1).min(cols)
            } else {
                cols
            };

            let mut line = String::new();
            if let Some(cells) = row_cells {
                if col_start < col_end_excl {
                    for cell in &cells[col_start..col_end_excl] {
                        if cell.flags & ATTR_WIDE_TRAILING != 0 {
                            continue; // glyph belongs to the leading cell
                        }
                        line.push_str(&self.cluster_string(cell));
                    }
                }
            }
            // Trim trailing spaces from this row only.
            let trimmed_len = line.trim_end_matches(' ').len();
            line.truncate(trimmed_len);
            out.push_str(&line);
            if abs_row != end.abs_row {
                out.push('\n');
            }
        }
        out
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
        // Record OSC 8 hyperlink for this cell (primary screen only).
        if !self.alt_active {
            if let Some(url) = &self.active_link {
                let abs_row = self.scrollback.len() + cursor.row;
                self.hyperlinks.insert((abs_row, cursor.col), url.clone());
            }
        }
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
            // Tee the about-to-scroll line to the orchestrator for plain-text
            // session logging. The clone is bounded by max_scrollback growth.
            self.pending_log.push(line0.clone());
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
        // Alt screen has no scrollback semantics; snap the view to the bottom.
        self.view_offset = 0;
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
        // View offset can't outlive scrollback after a resize-driven trim.
        self.view_offset = self.view_offset.min(self.scrollback.len());
        cursor.row = cursor.row.min(rows.saturating_sub(1));
        cursor.col = cursor.col.min(cols.saturating_sub(1));
    }
}

/// The 16-color ANSI palette (`0x00RRGGBB`). Exposed so renderers can
/// implement "bold-as-bright" (palette[i] -> palette[i+8] for i in 0..=7).
pub const ANSI_PALETTE: [u32; 16] = [
    0x000000, 0xCD0000, 0x00CD00, 0xCDCD00, 0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5,
    0x7F7F7F, 0xFF0000, 0x00FF00, 0xFFFF00, 0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
];

/// If `fg` is one of the first 8 palette colors, return its bright counterpart.
/// Else return `fg` unchanged. Used at render time when `ATTR_BOLD` is set
/// (this is the conventional "bold = bright" terminal behavior).
pub fn brighten_palette_color(fg: u32) -> u32 {
    for i in 0..8 {
        if ANSI_PALETTE[i] == fg {
            return ANSI_PALETTE[i + 8];
        }
    }
    fg
}

fn ansi_16(i: u16) -> u32 {
    ANSI_PALETTE[(i as usize) & 0xF]
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
