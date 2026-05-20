// ABOUTME: Phase 2 ANSI/VT parser bridge implementing vte::Perform over the Grid.
// ABOUTME: Stateless mutator: it only forwards parsed actions into borrowed state.

use crate::grid::{CursorState, Grid};
use vte::{Params, Perform};

/// First CSI parameter, or `default` when absent/zero-as-default applies.
fn param1(params: &Params, default: usize) -> usize {
    match params.iter().next().and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v as usize,
    }
}

/// The Mutator bridges the stateless VTE parser and the stateful Grid.
///
/// `responses` is the downward-flowing reply sink: queries like DA1/DSR push
/// their answer bytes here, and the orchestrator writes them back to the PTY
/// after the parse pass. This keeps the parser free of any PTY back-reference.
pub struct StateMutator<'a> {
    pub grid: &'a mut Grid,
    pub cursor: &'a mut CursorState,
    pub responses: &'a mut Vec<u8>,
}

impl<'a> Perform for StateMutator<'a> {
    /// Standard printable character insertion.
    fn print(&mut self, c: char) {
        self.grid.write_char(self.cursor, c);
    }

    /// C0 control characters (e.g., \n, \r, \b).
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.grid.line_feed(self.cursor),
            b'\r' => self.cursor.col = 0,
            0x08 => self.cursor.backspace(),
            b'\t' => {
                // Advance to the next 8-column tab stop.
                let next = (self.cursor.col / 8 + 1) * 8;
                self.cursor.col = next.min(self.grid.cols().saturating_sub(1));
            }
            _ => {} // Unhandled C0
        }
    }

    /// ANSI escape sequences (e.g., color changes, cursor movement).
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        match action {
            'm' => self.grid.update_attributes(self.cursor, params), // SGR
            'H' | 'f' => self.cursor.move_to(params),                // Cursor Position
            'A' => self.grid.move_rel(self.cursor, 0, -(param1(params, 1) as isize)),
            'B' => self.grid.move_rel(self.cursor, 0, param1(params, 1) as isize),
            'C' => self.grid.move_rel(self.cursor, param1(params, 1) as isize, 0),
            'D' => self.grid.move_rel(self.cursor, -(param1(params, 1) as isize), 0),
            'G' | '`' => self.grid.set_col(self.cursor, param1(params, 1)), // CHA / HPA
            'd' => self.grid.set_row(self.cursor, param1(params, 1)),       // VPA
            'J' => self.grid.erase_in_display(self.cursor, params),  // Clear screen
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.grid.erase_in_line(self.cursor, mode); // Erase in Line
            }
            'P' => self.grid.delete_chars(self.cursor, param1(params, 1)), // DCH
            '@' => self.grid.insert_chars(self.cursor, param1(params, 1)), // ICH
            'X' => self.grid.erase_chars(self.cursor, param1(params, 1)),  // ECH
            'c' if intermediates.is_empty() => {
                // Primary Device Attributes (DA1): reply as a VT102. fish only
                // needs a valid response; without it, it warns and waits 2s.
                self.responses.extend_from_slice(b"\x1b[?6c");
            }
            'n' => {
                // Device Status Report.
                match params.iter().next().and_then(|p| p.first().copied()) {
                    Some(5) => self.responses.extend_from_slice(b"\x1b[0n"), // OK
                    Some(6) => {
                        // Cursor Position Report: 1-based row;col.
                        let report = format!(
                            "\x1b[{};{}R",
                            self.cursor.row + 1,
                            self.cursor.col + 1
                        );
                        self.responses.extend_from_slice(report.as_bytes());
                    }
                    _ => {}
                }
            }
            'h' | 'l' if intermediates.first() == Some(&b'?') => {
                // DEC private mode set (h) / reset (l).
                let set = action == 'h';
                for p in params.iter() {
                    match p.first().copied().unwrap_or(0) {
                        7 => self.grid.set_autowrap(set),               // DECAWM
                        25 => self.cursor.visible = set,                // DECTCEM
                        47 | 1047 => {
                            if set {
                                self.grid.enter_alt_screen();
                            } else {
                                self.grid.exit_alt_screen();
                            }
                        }
                        1049 => {
                            // Save cursor + alt screen as one unit.
                            if set {
                                self.cursor.save();
                                self.grid.enter_alt_screen();
                            } else {
                                self.grid.exit_alt_screen();
                                self.cursor.restore();
                            }
                        }
                        2004 => self.grid.bracketed_paste = set,
                        _ => {}
                    }
                }
            }
            'r' if intermediates.is_empty() => {
                // DECSTBM: top;bottom scroll margins (defaults to full screen).
                let mut it = params.iter();
                let top = it.next().and_then(|p| p.first().copied()).unwrap_or(1) as usize;
                let bottom = it
                    .next()
                    .and_then(|p| p.first().copied())
                    .map(|v| v as usize)
                    .filter(|&v| v != 0)
                    .unwrap_or(self.grid.rows());
                self.grid.set_scroll_region(top.max(1), bottom, self.cursor);
            }
            's' if intermediates.is_empty() => self.cursor.save(), // DECSC
            'u' if intermediates.is_empty() => self.cursor.restore(), // DECRC
            'L' => self.grid.insert_lines(self.cursor, param1(params, 1)), // IL
            'M' => self.grid.delete_lines(self.cursor, param1(params, 1)), // DL
            _ => {} // Unimplemented CSI
        }
    }

    // Stubbed required trait methods: osc_dispatch, hook, put, unhook.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    /// ESC-sequence dispatch: cursor save/restore and the index operations.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.cursor.save(),                        // DECSC
            b'8' => self.cursor.restore(),                     // DECRC
            b'M' => self.grid.reverse_index(self.cursor),      // RI
            b'D' => self.grid.line_feed(self.cursor),          // IND
            b'E' => {
                self.cursor.col = 0;
                self.grid.line_feed(self.cursor); // NEL
            }
            _ => {}
        }
    }
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    /// Feed bytes through the real vte parser and capture query replies.
    fn run(input: &[u8]) -> (Vec<u8>, CursorState) {
        let mut grid = Grid::new(80, 24, 100);
        let mut cursor = CursorState::new();
        let mut responses = Vec::new();
        {
            let mut m = StateMutator {
                grid: &mut grid,
                cursor: &mut cursor,
                responses: &mut responses,
            };
            let mut parser = vte::Parser::new();
            for &b in input {
                parser.advance(&mut m, b);
            }
        }
        (responses, cursor)
    }

    #[test]
    fn da1_query_gets_vt102_reply() {
        // Reproduces the fish "Primary Device Attribute query" timeout.
        let (resp, _) = run(b"\x1b[c");
        assert_eq!(resp, b"\x1b[?6c");
    }

    #[test]
    fn dsr_cursor_position_reports_1_based() {
        let (resp, _) = run(b"\x1b[3;5H\x1b[6n");
        assert_eq!(resp, b"\x1b[3;5R");
    }

    #[test]
    fn dsr_device_status_ok() {
        let (resp, _) = run(b"\x1b[5n");
        assert_eq!(resp, b"\x1b[0n");
    }

    #[test]
    fn plain_text_produces_no_replies() {
        let (resp, cursor) = run(b"hello");
        assert!(resp.is_empty());
        assert_eq!(cursor.col, 5);
    }

    /// Feed bytes and return row 0 as a trimmed string.
    fn row0(input: &[u8]) -> String {
        let mut grid = Grid::new(20, 4, 100);
        let mut cursor = CursorState::new();
        let mut responses = Vec::new();
        {
            let mut m = StateMutator {
                grid: &mut grid,
                cursor: &mut cursor,
                responses: &mut responses,
            };
            let mut parser = vte::Parser::new();
            for &b in input {
                parser.advance(&mut m, b);
            }
        }
        let cols = grid.cols();
        grid.viewport()[..cols]
            .iter()
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn cha_then_erase_line_repaint() {
        // Reproduces fish's per-keystroke repaint: print, jump to col 1,
        // erase line, reprint. Before the fix this garbled to "lsls -l".
        assert_eq!(row0(b"ls\x1b[1G\x1b[Kls -l"), "ls -l");
    }

    #[test]
    fn erase_in_line_to_eol() {
        assert_eq!(row0(b"hello\r\x1b[Khi"), "hi");
    }

    #[test]
    fn delete_chars_shifts_left() {
        assert_eq!(row0(b"abcde\x1b[1G\x1b[2P"), "cde");
    }

    #[test]
    fn insert_chars_shifts_right() {
        assert_eq!(row0(b"abc\x1b[1G\x1b[2@"), "  abc");
    }

    #[test]
    fn cursor_back_then_overwrite() {
        assert_eq!(row0(b"abc\x1b[2DX"), "aXc");
    }

    /// Drive a sized grid and return it plus the cursor for inspection.
    fn drive(cols: usize, rows: usize, input: &[u8]) -> (Grid, CursorState) {
        let mut grid = Grid::new(cols, rows, 100);
        let mut cursor = CursorState::new();
        let mut responses = Vec::new();
        {
            let mut m = StateMutator {
                grid: &mut grid,
                cursor: &mut cursor,
                responses: &mut responses,
            };
            let mut parser = vte::Parser::new();
            for &b in input {
                parser.advance(&mut m, b);
            }
        }
        (grid, cursor)
    }

    fn screen(grid: &Grid) -> Vec<String> {
        let cols = grid.cols();
        grid.viewport()
            .chunks(cols)
            .map(|row| row.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect()
    }

    #[test]
    fn autowrap_off_clamps_last_column() {
        // DECAWM off: past the right edge overwrites the last column.
        let (g, _) = drive(5, 2, b"\x1b[?7labcdefgh");
        assert_eq!(screen(&g)[0], "abcdh");
    }

    #[test]
    fn alt_screen_round_trip_restores_primary() {
        let (g, c) = drive(20, 2, b"hi\x1b[?1049hGARBAGE\x1b[?1049l");
        assert_eq!(screen(&g)[0], "hi");
        // ?1049 restores the saved cursor too.
        assert_eq!((c.row, c.col), (0, 2));
    }

    #[test]
    fn alt_screen_clears_on_enter() {
        let (g, _) = drive(20, 2, b"shell\x1b[?1049h");
        assert_eq!(screen(&g)[0], "");
    }

    #[test]
    fn dectcem_toggles_cursor_visibility() {
        assert!(!drive(10, 2, b"\x1b[?25l").1.visible);
        assert!(drive(10, 2, b"\x1b[?25l\x1b[?25h").1.visible);
    }

    #[test]
    fn decsc_decrc_via_esc_save_restore() {
        // Write at (0,0..3), ESC7 save, move + write, ESC8 restore.
        let (_, c) = drive(20, 4, b"abc\x1b7\x1b[3;10HX\x1b8");
        assert_eq!((c.row, c.col), (0, 3));
    }

    #[test]
    fn decstbm_scrolls_only_within_region() {
        // Region = rows 2..3 (1-based). Park cursor at the region bottom
        // (row 3) and line-feed: only rows 2-3 rotate, row 1 is untouched.
        let (g, _) = drive(
            4,
            4,
            b"top\x1b[2;3r\x1b[3;1Hone\rN\x1b[3;1H\ntwo",
        );
        let s = screen(&g);
        assert_eq!(s[0], "top"); // outside region: preserved
        assert_eq!(s[2], "two"); // region bottom after scroll
    }

    #[test]
    fn insert_and_delete_line_within_region() {
        // Three rows of text, delete the first line: lines shift up.
        let (g, _) = drive(4, 3, b"aaa\r\nbbb\r\nccc\x1b[1;1H\x1b[M");
        let s = screen(&g);
        assert_eq!(s[0], "bbb");
        assert_eq!(s[1], "ccc");
        assert_eq!(s[2], "");
    }

    #[test]
    fn bracketed_paste_mode_tracked() {
        assert!(drive(10, 2, b"\x1b[?2004h").0.bracketed_paste);
        assert!(!drive(10, 2, b"\x1b[?2004h\x1b[?2004l").0.bracketed_paste);
    }
}
