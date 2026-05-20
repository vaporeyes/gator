// ABOUTME: Find-in-scrollback: a `FindState` with query/matches/index, plus
// ABOUTME: rebuild and scroll-to-match helpers driven by the orchestrator.

use crate::grid::{AbsCoord, Grid};

/// Inclusive (start, end) coordinates of a single match in absolute rows.
pub type FindMatch = (AbsCoord, AbsCoord);

#[derive(Debug, Default)]
pub struct FindState {
    pub query: String,
    pub matches: Vec<FindMatch>,
    /// Index into `matches` for the currently-highlighted match.
    pub current: usize,
}

impl FindState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recompute matches across scrollback + viewport. Case-sensitive v1.
    pub fn recompute(&mut self, grid: &Grid) {
        self.matches.clear();
        if self.query.is_empty() {
            self.current = 0;
            return;
        }
        let viewport_first = grid.viewport_first_abs_row();
        let total_rows = viewport_first + grid.rows();
        for abs_row in 0..total_rows {
            let text = grid.row_text(abs_row);
            let mut from = 0;
            while let Some(rel) = text[from..].find(&self.query) {
                let start_byte = from + rel;
                let end_byte = start_byte + self.query.len();
                // Convert byte indices to column indices via char count.
                let start_col = text[..start_byte].chars().count();
                let end_col = text[..end_byte].chars().count().saturating_sub(1);
                self.matches.push((
                    AbsCoord { abs_row, col: start_col },
                    AbsCoord { abs_row, col: end_col },
                ));
                // Advance past this match to avoid infinite loop on empty
                // matches and to allow overlapping searches (we don't here).
                from = start_byte + self.query.len().max(1);
                if from > text.len() {
                    break;
                }
            }
        }
        if self.current >= self.matches.len() {
            self.current = 0;
        }
    }

    pub fn push_char(&mut self, c: char, grid: &Grid) {
        self.query.push(c);
        self.recompute(grid);
    }

    pub fn pop_char(&mut self, grid: &Grid) {
        self.query.pop();
        self.recompute(grid);
    }

    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current =
                (self.current + self.matches.len().saturating_sub(1)) % self.matches.len();
        }
    }

    /// Adjust grid view so the current match is visible (centered roughly).
    pub fn scroll_to_current(&self, grid: &mut Grid) {
        let Some(&(start, _)) = self.matches.get(self.current) else {
            return;
        };
        let vp_first = grid.viewport_first_abs_row();
        let rows = grid.rows();
        // Place match near the middle of the view.
        let target_top = start.abs_row.saturating_sub(rows / 2);
        let offset = vp_first.saturating_sub(target_top);
        grid.view_offset = offset.min(grid.scrollback_len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Cell, Grid};

    fn seed_grid_with_rows(rows: &[&str]) -> Grid {
        let cols = rows.iter().map(|s| s.chars().count()).max().unwrap_or(1).max(1);
        let mut g = Grid::new(cols, 2, 100);
        for s in rows {
            let mut line: Vec<Cell> = s
                .chars()
                .map(|c| {
                    let mut cell = Cell::default();
                    cell.c = c;
                    cell
                })
                .collect();
            line.resize(cols, Cell::default());
            g.push_history_line(line);
        }
        g
    }

    #[test]
    fn finds_substring_across_rows() {
        let g = seed_grid_with_rows(&["the quick brown fox", "lazy dog jumps", "the end"]);
        let mut f = FindState::new();
        f.query = "the".into();
        f.recompute(&g);
        assert_eq!(f.matches.len(), 2);
        assert_eq!(f.matches[0].0.col, 0);
        assert_eq!(f.matches[0].0.abs_row, 0);
        assert_eq!(f.matches[1].0.abs_row, 2);
    }

    #[test]
    fn empty_query_clears_matches() {
        let g = seed_grid_with_rows(&["hello"]);
        let mut f = FindState::new();
        f.query = "ll".into();
        f.recompute(&g);
        assert_eq!(f.matches.len(), 1);
        f.query.clear();
        f.recompute(&g);
        assert!(f.matches.is_empty());
    }

    #[test]
    fn next_wraps() {
        let g = seed_grid_with_rows(&["aa", "aa", "aa"]);
        let mut f = FindState::new();
        f.query = "aa".into();
        f.recompute(&g);
        f.next();
        f.next();
        f.next();
        // After wrapping past 3 matches, we're back at index 0.
        assert_eq!(f.current, 0);
    }
}
