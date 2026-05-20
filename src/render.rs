// ABOUTME: Phase 4 rendering: Renderer trait, dirty-cell FrameDiff, crossterm backend.
// ABOUTME: Draw calls are batched via crossterm's queue and flushed once per frame.

use crate::grid::{Cell, CursorState, Grid, ATTR_BOLD, ATTR_ITALIC, ATTR_UNDERLINE};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::io::{Stdout, Write};

/// Abstract rendering target.
pub trait Renderer {
    type Error: std::error::Error;

    /// Allocate necessary buffers or initialize target context.
    fn init(&mut self, cols: u16, rows: u16) -> Result<(), Self::Error>;

    /// Compares current grid to previous frame and draws diffs.
    fn render_frame(&mut self, grid: &Grid, cursor: &CursorState) -> Result<(), Self::Error>;

    /// Teardown, flush buffers, and restore target state.
    fn shutdown(&mut self) -> Result<(), Self::Error>;
}

/// Utility for tracking rendering deltas.
pub struct FrameDiff {
    pub previous_viewport: Vec<Cell>,
}

impl FrameDiff {
    pub fn new(cols: usize, rows: usize) -> Self {
        // Sentinel that differs from Cell::default() so the first frame is
        // fully drawn (every cell counts as dirty).
        let mut sentinel = Cell::default();
        sentinel.c = '\0';
        Self { previous_viewport: vec![sentinel; cols * rows] }
    }

    /// Resize the diff buffer; forces a full repaint next frame.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let mut sentinel = Cell::default();
        sentinel.c = '\0';
        self.previous_viewport = vec![sentinel; cols * rows];
    }

    /// Yields only the cells that have mutated since the last render pass.
    pub fn calculate_diff<'a>(
        &'a mut self,
        current_grid: &'a Grid,
    ) -> impl Iterator<Item = (usize, usize, &'a Cell)> {
        let cols = current_grid.cols();

        current_grid
            .viewport()
            .iter()
            .zip(self.previous_viewport.iter_mut())
            .enumerate()
            .filter_map(move |(idx, (curr, prev))| {
                if curr != prev {
                    *prev = *curr; // Update diff buffer inline
                    let x = idx % cols;
                    let y = idx / cols;
                    Some((x, y, curr))
                } else {
                    None
                }
            })
    }

    /// Force a single cell to repaint next frame (used for cursor invalidation).
    fn invalidate(&mut self, idx: usize) {
        if let Some(prev) = self.previous_viewport.get_mut(idx) {
            prev.c = '\0';
            prev.fg = 0xDEAD;
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("render io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct CrosstermRenderer {
    out: Stdout,
    diff: FrameDiff,
    cols: usize,
    rows: usize,
    last_cursor: Option<(usize, usize)>,
}

impl CrosstermRenderer {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            out: std::io::stdout(),
            diff: FrameDiff::new(cols, rows),
            cols,
            rows,
            last_cursor: None,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.diff.resize(cols, rows);
        self.last_cursor = None;
    }
}

fn to_color(rgb: u32) -> Color {
    Color::Rgb {
        r: ((rgb >> 16) & 0xFF) as u8,
        g: ((rgb >> 8) & 0xFF) as u8,
        b: (rgb & 0xFF) as u8,
    }
}

/// Queue the draw for one cell at (x, y). `inverse` swaps fg/bg for the cursor.
fn draw_cell(out: &mut Stdout, x: usize, y: usize, cell: &Cell, inverse: bool) -> std::io::Result<()> {
    let (fg, bg) = if inverse { (cell.bg, cell.fg) } else { (cell.fg, cell.bg) };
    queue!(
        out,
        cursor::MoveTo(x as u16, y as u16),
        SetForegroundColor(to_color(fg)),
        SetBackgroundColor(to_color(bg)),
        SetAttribute(if cell.flags & ATTR_BOLD != 0 { Attribute::Bold } else { Attribute::NormalIntensity }),
        SetAttribute(if cell.flags & ATTR_ITALIC != 0 { Attribute::Italic } else { Attribute::NoItalic }),
        SetAttribute(if cell.flags & ATTR_UNDERLINE != 0 { Attribute::Underlined } else { Attribute::NoUnderline }),
        Print(cell.c),
    )
}

impl Renderer for CrosstermRenderer {
    type Error = RenderError;

    fn init(&mut self, cols: u16, rows: u16) -> Result<(), Self::Error> {
        terminal::enable_raw_mode()?;
        queue!(
            self.out,
            terminal::EnterAlternateScreen,
            terminal::Clear(terminal::ClearType::All),
            EnableBracketedPaste,
            cursor::Hide
        )?;
        self.out.flush()?;
        self.cols = cols as usize;
        self.rows = rows as usize;
        Ok(())
    }

    fn render_frame(&mut self, grid: &Grid, cursor: &CursorState) -> Result<(), Self::Error> {
        // Cursor invalidation: the old and new cursor cells must repaint even
        // if their Cell data is unchanged, so the block masks/unmasks text.
        // When DECTCEM hides the cursor, only the old cell repaints (no block).
        if let Some((px, py)) = self.last_cursor {
            self.diff.invalidate(py * self.cols + px);
        }
        let new_cursor = if cursor.visible {
            let nc = (
                cursor.col.min(self.cols.saturating_sub(1)),
                cursor.row.min(self.rows.saturating_sub(1)),
            );
            self.diff.invalidate(nc.1 * self.cols + nc.0);
            Some(nc)
        } else {
            None
        };

        // Snapshot dirty cells first so the immutable grid borrow ends before
        // we take the mutable stdout borrow for drawing.
        let dirty: Vec<(usize, usize, Cell)> = self
            .diff
            .calculate_diff(grid)
            .map(|(x, y, c)| (x, y, *c))
            .collect();

        for (x, y, cell) in &dirty {
            let is_cursor = new_cursor == Some((*x, *y));
            draw_cell(&mut self.out, *x, *y, cell, is_cursor)?;
        }

        self.out.flush()?;
        self.last_cursor = new_cursor;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), Self::Error> {
        queue!(
            self.out,
            DisableBracketedPaste,
            cursor::Show,
            terminal::LeaveAlternateScreen
        )?;
        self.out.flush()?;
        terminal::disable_raw_mode()?;
        Ok(())
    }
}
