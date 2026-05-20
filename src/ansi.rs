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
    /// Standard printable character insertion. Dispatches by Unicode display
    /// width so wide codepoints occupy two cells and combining marks attach
    /// to the previous cell instead of advancing the cursor.
    fn print(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        match UnicodeWidthChar::width(c) {
            Some(0) => self.grid.attach_combining(self.cursor, c),
            Some(2) => self.grid.write_wide(self.cursor, c),
            // Width 1 (most printable), or None (control); print as a single cell.
            _ => self.grid.write_char(self.cursor, c),
        }
    }

    /// C0 control characters (e.g., \n, \r, \b).
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.grid.line_feed(self.cursor),
            b'\r' => self.cursor.col = 0,
            0x07 => self.grid.ring_bell(),
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
            'H' | 'f' => self.grid.move_to(self.cursor, params),      // Cursor Position
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
                        1000 => self.grid.mouse_mode.button = set,    // basic mouse
                        1002 => self.grid.mouse_mode.drag = set,      // + drag tracking
                        1006 => self.grid.mouse_mode.sgr_encoded = set, // SGR encoding
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
            'q' if intermediates.first() == Some(&b' ') => {
                // DECSCUSR: CSI Ps SP q. Ps = 0..6 set shape + blink.
                use crate::grid::CursorStyle;
                let ps = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                let (style, blink) = match ps {
                    0 | 1 => (CursorStyle::Block, true),
                    2 => (CursorStyle::Block, false),
                    3 => (CursorStyle::Underline, true),
                    4 => (CursorStyle::Underline, false),
                    5 => (CursorStyle::Bar, true),
                    6 => (CursorStyle::Bar, false),
                    _ => return,
                };
                self.cursor.style = style;
                self.cursor.blink = blink;
            }
            _ => {} // Unimplemented CSI
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() {
            return;
        }
        // OSC code is the first parameter, as decimal ASCII.
        let code = std::str::from_utf8(params[0])
            .ok()
            .and_then(|s| s.parse::<u32>().ok());
        match code {
            // Window/icon title: OSC 0;..., OSC 1;..., OSC 2;...
            Some(0) | Some(1) | Some(2) => {
                if let Some(bytes) = params.get(1) {
                    self.grid.title = String::from_utf8_lossy(bytes).into_owned();
                    self.grid.title_seq = self.grid.title_seq.wrapping_add(1);
                }
            }
            // OSC 8 ; params ; URL ST  -> set/clear active hyperlink.
            Some(8) => {
                let url = params.get(2).map(|b| String::from_utf8_lossy(b).into_owned());
                self.grid.active_link = match url {
                    Some(u) if !u.is_empty() => Some(u),
                    _ => None,
                };
            }
            // OSC 52 ; <selection> ; <base64> -> system clipboard.
            Some(52) => {
                if let Some(payload) = params.get(2) {
                    use base64::Engine;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            self.grid.pending_clipboard = Some(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

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
    fn cup_clamps_to_screen_bounds() {
        let (g, c) = drive(4, 2, b"\x1b[999;999HX");
        assert_eq!((c.row, c.col), (1, 4));
        assert_eq!(screen(&g)[1], "   X");
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
    fn alt_screen_resize_preserves_primary() {
        let (mut g, mut c) = drive(4, 2, b"hi\x1b[?1049hALT");
        g.resize(6, 3, &mut c);
        g.exit_alt_screen();
        assert_eq!(screen(&g)[0], "hi");
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

    #[test]
    fn mouse_modes_track_dec_set_reset() {
        let (g, _) = drive(10, 2, b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        assert!(g.mouse_mode.button);
        assert!(g.mouse_mode.drag);
        assert!(g.mouse_mode.sgr_encoded);
        assert!(g.mouse_mode.enabled());
        let (g, _) = drive(10, 2, b"\x1b[?1000h\x1b[?1000l");
        assert!(!g.mouse_mode.button);
        assert!(!g.mouse_mode.enabled());
    }

    #[test]
    fn brighten_palette_color_maps_basic_to_bright() {
        use crate::grid::{brighten_palette_color, ANSI_PALETTE};
        // The eight basic colors map to indices i + 8.
        for i in 0..8 {
            assert_eq!(brighten_palette_color(ANSI_PALETTE[i]), ANSI_PALETTE[i + 8]);
        }
        // A non-palette color (e.g., truecolor) is unchanged.
        assert_eq!(brighten_palette_color(0xABCDEF), 0xABCDEF);
        // Bright palette colors are also non-basic (no double-brighten).
        assert_eq!(brighten_palette_color(ANSI_PALETTE[12]), ANSI_PALETTE[12]);
    }

    #[test]
    fn wide_char_occupies_two_cells_with_flags() {
        use crate::grid::{ATTR_WIDE, ATTR_WIDE_TRAILING};
        // "中" (U+4E2D) is width 2.
        let (g, c) = drive(4, 2, b"\xe4\xb8\xad");
        let v = g.viewport();
        assert_ne!(v[0].flags & ATTR_WIDE, 0);
        assert_ne!(v[1].flags & ATTR_WIDE_TRAILING, 0);
        assert_eq!(v[0].c, '\u{4E2D}');
        // Cursor advanced by 2 columns.
        assert_eq!(c.col, 2);
    }

    #[test]
    fn combining_attaches_to_previous_cell() {
        // 'e' (width 1) then U+0301 combining acute (width 0).
        // UTF-8: 'e' = 0x65 ; U+0301 = 0xCC 0x81
        let (g, c) = drive(4, 2, b"e\xcc\x81");
        let v = g.viewport();
        assert_eq!(v[0].c, 'e');
        assert_ne!(v[0].combining_id, 0);
        assert_eq!(g.cluster_string(&v[0]), "e\u{0301}");
        assert_eq!(c.col, 1);
    }

    #[test]
    fn selection_skips_wide_trailing_and_keeps_clusters() {
        use crate::grid::{AbsCoord, Selection};
        // "中e" + combining acute. Width sum: 2 + 1 + 0 = 3 cells (0,1,2);
        // cell 0 is wide leading, cell 1 wide trailing, cell 2 is 'e' with
        // combining. Selecting cols 0..=2 should yield "中é" (3 chars).
        let (g, _) = drive(6, 2, b"\xe4\xb8\xade\xcc\x81");
        let sel = Selection {
            anchor: AbsCoord { abs_row: 0, col: 0 },
            head: AbsCoord { abs_row: 0, col: 2 },
        };
        assert_eq!(g.get_selection_text(sel), "\u{4E2D}e\u{0301}");
    }

    #[test]
    fn osc_0_sets_window_title() {
        let (g, _) = drive(20, 2, b"\x1b]0;hello world\x07");
        assert_eq!(g.title, "hello world");
        assert_eq!(g.title_seq, 1);
    }

    #[test]
    fn osc_52_decodes_clipboard() {
        // "hello" -> base64 "aGVsbG8="
        let (g, _) = drive(10, 2, b"\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(g.pending_clipboard.as_deref(), Some("hello"));
    }

    #[test]
    fn osc_8_link_attaches_to_cells() {
        let (g, _) = drive(20, 2, b"\x1b]8;;https://a.example\x07X\x1b]8;;\x07Y");
        // 'X' is written under the active link at (abs_row 0, col 0);
        // 'Y' is written after the link was closed and should have none.
        assert_eq!(
            g.hyperlinks.get(&(0, 0)).map(String::as_str),
            Some("https://a.example")
        );
        assert!(g.hyperlinks.get(&(0, 1)).is_none());
    }

    #[test]
    fn bel_increments_bell_seq() {
        let (g, _) = drive(4, 2, b"\x07hi\x07");
        assert_eq!(g.bell_seq, 2);
    }

    #[test]
    fn decscusr_sets_style_and_blink() {
        use crate::grid::CursorStyle;
        let (_, c) = drive(4, 2, b"\x1b[3 q");
        assert_eq!(c.style, CursorStyle::Underline);
        assert!(c.blink);
        let (_, c) = drive(4, 2, b"\x1b[2 q");
        assert_eq!(c.style, CursorStyle::Block);
        assert!(!c.blink);
        let (_, c) = drive(4, 2, b"\x1b[6 q");
        assert_eq!(c.style, CursorStyle::Bar);
        assert!(!c.blink);
    }

    #[test]
    fn selection_within_one_row_extracts_substring() {
        use crate::grid::{AbsCoord, Selection};
        let (g, _) = drive(20, 2, b"hello world");
        // Empty scrollback -> viewport row 0 has absolute row 0.
        let sel = Selection {
            anchor: AbsCoord { abs_row: 0, col: 6 },
            head: AbsCoord { abs_row: 0, col: 10 },
        };
        assert_eq!(g.get_selection_text(sel), "world");
    }

    #[test]
    fn pending_log_records_evicted_lines_only_on_primary_full_screen() {
        // 4 cols, 2 rows; print enough to scroll once.
        let (mut g, _) = drive(4, 2, b"aaa\r\nbbb\r\nccc");
        // Two newlines past row 0 cause one scroll => one pending_log entry.
        assert_eq!(g.pending_log.len(), 1);
        assert_eq!(
            g.pending_log[0].iter().map(|c| c.c).collect::<String>().trim_end(),
            "aaa"
        );
        // Alt screen scrolls do not feed pending_log.
        g.pending_log.clear();
        let (g2, _) = drive(4, 2, b"\x1b[?1049hX\r\nY\r\nZ");
        assert!(g2.pending_log.is_empty());
    }

    #[test]
    fn scroll_lines_clamps_and_at_bottom_resets() {
        use crate::grid::Grid;
        let mut g = Grid::new(4, 2, 100);
        g.push_history_line(vec![crate::grid::Cell::default(); 4]);
        g.push_history_line(vec![crate::grid::Cell::default(); 4]);
        assert!(g.at_bottom());
        g.scroll_lines(5);
        assert_eq!(g.view_offset, 2); // clamped to scrollback.len()
        assert!(!g.at_bottom());
        g.scroll_lines(-10);
        assert!(g.at_bottom());
    }

    #[test]
    fn scrolling_disabled_on_alt_screen() {
        let (mut g, _) = drive(4, 2, b"\x1b[?1049h");
        g.scroll_lines(10);
        assert!(g.at_bottom());
    }

    #[test]
    fn display_row_pulls_from_scrollback_when_scrolled() {
        // Push three logical lines through scrolling, then scroll view up by 2.
        let (mut g, _) = drive(4, 2, b"AAA\r\nBBB\r\nCCC\r\nDDD");
        g.scroll_lines(2);
        // Top-of-view row 0 should be scrollback's oldest (AAA).
        let top: String = g.display_row(0).iter().map(|c| c.c).collect();
        assert_eq!(top.trim_end(), "AAA");
    }

    #[test]
    fn displayed_abs_row_accounts_for_scrollback_offset() {
        let (mut g, _) = drive(4, 2, b"AAA\r\nBBB\r\nCCC\r\nDDD");
        g.scroll_lines(2);
        assert_eq!(g.displayed_abs_row(0), 0);
        assert_eq!(g.displayed_abs_row(1), 1);
        g.scroll_to_bottom();
        assert_eq!(g.displayed_abs_row(0), g.viewport_first_abs_row());
    }

    #[test]
    fn selection_spans_two_rows_strips_trailing_spaces() {
        use crate::grid::{AbsCoord, Selection};
        let (g, _) = drive(10, 3, b"abc\r\nXYZ");
        // anchor row1 col1 -> head row0 col1 (reversed). Normalized is row0..row1.
        let sel = Selection {
            anchor: AbsCoord { abs_row: 1, col: 1 },
            head: AbsCoord { abs_row: 0, col: 1 },
        };
        assert_eq!(g.get_selection_text(sel), "bc\nXY");
    }
}
