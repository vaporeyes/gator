// ABOUTME: History / session storage: raw PTY log, plain-text scrollback log,
// ABOUTME: and cross-session restore of scrollback from the text log.

use crate::config::{expand_path, SessionConfig};
use crate::grid::{Cell, Grid};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

/// Per-session writer handles. Both files are append-mode and best-effort:
/// any failure leaves the corresponding sink as `None` so the rest of the
/// terminal keeps running silently.
pub struct SessionLogger {
    raw: Option<BufWriter<File>>,
    text: Option<BufWriter<File>>,
}

impl SessionLogger {
    pub fn from_config(cfg: &SessionConfig) -> Self {
        Self {
            raw: open_append(&cfg.raw_log),
            text: open_append(&cfg.text_log),
        }
    }

    /// Append a raw PTY chunk byte-for-byte to the raw log (no-op if disabled).
    pub fn log_raw(&mut self, bytes: &[u8]) {
        if let Some(w) = self.raw.as_mut() {
            let _ = w.write_all(bytes);
        }
    }

    /// Flush pending evicted lines from the Grid into the text log.
    /// Returns the number of lines drained (the orchestrator uses this to
    /// keep `view_offset` anchored when the user is reading scrollback).
    pub fn drain_text(&mut self, grid: &mut Grid) -> usize {
        let lines = std::mem::take(&mut grid.pending_log);
        let n = lines.len();
        if let Some(w) = self.text.as_mut() {
            for line in &lines {
                let s: String = line.iter().map(|c| c.c).collect();
                let trimmed = s.trim_end_matches(' ');
                let _ = writeln!(w, "{trimmed}");
            }
            let _ = w.flush();
        }
        n
    }

    pub fn flush(&mut self) {
        if let Some(w) = self.raw.as_mut() {
            let _ = w.flush();
        }
        if let Some(w) = self.text.as_mut() {
            let _ = w.flush();
        }
    }
}

fn open_append(raw_path: &str) -> Option<BufWriter<File>> {
    if raw_path.is_empty() {
        return None;
    }
    let path = expand_path(raw_path);
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
        .map(BufWriter::new)
}

/// On startup, seed the scrollback with the last `max_lines` lines of the
/// configured text log. Each restored line uses default cell attributes;
/// width is clamped to current grid columns.
pub fn restore_into(grid: &mut Grid, text_path: &str, max_lines: usize) {
    if max_lines == 0 || text_path.is_empty() {
        return;
    }
    let path = expand_path(text_path);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut lines: Vec<&str> = contents.lines().collect();
    if lines.len() > max_lines {
        let drop = lines.len() - max_lines;
        lines.drain(0..drop);
    }
    let cols = grid.cols();
    for s in lines {
        let mut cells: Vec<Cell> = s
            .chars()
            .take(cols)
            .map(|c| {
                let mut cell = Cell::default();
                cell.c = c;
                cell
            })
            .collect();
        cells.resize(cols, Cell::default());
        grid.push_history_line(cells);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    #[test]
    fn restore_truncates_to_last_n_lines_and_clamps_width() {
        // Write 5 lines, request the last 2.
        let dir = std::env::temp_dir().join(format!("gaterm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hist.log");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive_too_long_for_4_cols\n").unwrap();

        let mut g = Grid::new(4, 2, 100);
        restore_into(&mut g, path.to_str().unwrap(), 2);
        assert_eq!(g.scrollback_len(), 2);
        // Scroll the view to the top so display_row pulls from scrollback.
        g.scroll_lines(g.scrollback_len() as isize);
        let row0: String = g.display_row(0).iter().map(|c| c.c).collect();
        let row1: String = g.display_row(1).iter().map(|c| c.c).collect();
        assert_eq!(row0.trim_end(), "four");
        // Width clamped to 4: "five_too_long..." becomes "five".
        assert_eq!(row1, "five");
    }

    #[test]
    fn drain_text_consumes_pending_log() {
        let mut g = Grid::new(4, 2, 100);
        g.pending_log.push(vec![Cell::default(); 4]);
        let mut logger = SessionLogger {
            raw: None,
            text: None,
        };
        let n = logger.drain_text(&mut g);
        assert_eq!(n, 1);
        assert!(g.pending_log.is_empty());
    }
}
