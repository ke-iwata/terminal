pub mod color;
pub mod grid;
mod perform;

use color::Color;
use grid::{Cell, CellFlags, Grid};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

#[derive(Debug, Clone, Copy)]
pub struct TermModes {
    /// DECCKM (CSI ?1) - arrow keys send SS3 sequences instead of CSI.
    pub app_cursor_keys: bool,
    /// CSI ?25 - whether the cursor should be drawn.
    pub show_cursor: bool,
}

impl Default for TermModes {
    fn default() -> Self {
        TermModes {
            app_cursor_keys: false,
            show_cursor: true,
        }
    }
}

pub struct Term {
    parser: vte::Parser,
    cols: usize,
    rows: usize,
    grid: Grid,
    alt_grid: Grid,
    using_alt_screen: bool,
    pub cursor: Cursor,
    saved_cursor: Cursor,
    alt_saved_cursor: Cursor,
    /// Deferred line-wrap: set when the cursor sits just past the last
    /// column after printing a character that exactly filled the line.
    /// The wrap only actually happens if another character is printed
    /// before any cursor-repositioning control sequence arrives.
    wrap_pending: bool,
    pub modes: TermModes,
    scroll_top: usize,
    scroll_bottom: usize,
    pub title: String,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Term {
            parser: vte::Parser::new(),
            cols,
            rows,
            grid: Grid::new(cols, rows),
            alt_grid: Grid::new(cols, rows),
            using_alt_screen: false,
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            alt_saved_cursor: Cursor::default(),
            wrap_pending: false,
            modes: TermModes::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            title: String::new(),
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn using_alt_screen(&self) -> bool {
        self.using_alt_screen
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.grid.resize(cols, rows);
        self.alt_grid.resize(cols, rows);
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.wrap_pending = false;
        self.clamp_cursor();
    }

    pub fn grid(&self) -> &Grid {
        if self.using_alt_screen {
            &self.alt_grid
        } else {
            &self.grid
        }
    }

    fn active_grid_mut(&mut self) -> &mut Grid {
        if self.using_alt_screen {
            &mut self.alt_grid
        } else {
            &mut self.grid
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
    }

    fn move_cursor_to(&mut self, row: usize, col: usize) {
        self.wrap_pending = false;
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.col = col.min(self.cols - 1);
    }

    fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.cursor.col = 0;
    }

    /// IND: move down one line, scrolling the scroll region if the cursor
    /// is already on its bottom line.
    fn index_down(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row == self.scroll_bottom {
            let (top, bottom) = (self.scroll_top, self.scroll_bottom);
            self.active_grid_mut().scroll_up(top, bottom, 1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// RI: move up one line, scrolling the scroll region if the cursor is
    /// already on its top line.
    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row == self.scroll_top {
            let (top, bottom) = (self.scroll_top, self.scroll_bottom);
            self.active_grid_mut().scroll_down(top, bottom, 1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn line_feed(&mut self) {
        self.index_down();
    }

    fn save_cursor(&mut self) {
        if self.using_alt_screen {
            self.alt_saved_cursor = self.cursor;
        } else {
            self.saved_cursor = self.cursor;
        }
    }

    fn restore_cursor(&mut self) {
        self.cursor = if self.using_alt_screen {
            self.alt_saved_cursor
        } else {
            self.saved_cursor
        };
        self.wrap_pending = false;
        self.clamp_cursor();
    }

    /// Enter or leave the alternate screen buffer (DEC private modes 47 /
    /// 1049). `save_restore_cursor` additionally saves/restores the cursor,
    /// matching real xterm behavior for 1049 vs the older bare 47.
    fn set_alt_screen(&mut self, enable: bool, save_restore_cursor: bool) {
        if enable == self.using_alt_screen {
            return;
        }
        if enable {
            if save_restore_cursor {
                self.save_cursor();
            }
            self.using_alt_screen = true;
            self.alt_grid.clear_all();
            self.cursor = Cursor::default();
        } else {
            self.using_alt_screen = false;
            if save_restore_cursor {
                self.restore_cursor();
            }
        }
        self.wrap_pending = false;
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        self.move_cursor_to(0, 0);
    }

    fn erase_in_display(&mut self, mode: u16) {
        let (cols, rows) = (self.cols, self.rows);
        let (cur_row, cur_col) = (self.cursor.row, self.cursor.col);
        let grid = self.active_grid_mut();
        match mode {
            0 => {
                for col in cur_col..cols {
                    *grid.cell_mut(cur_row, col) = Cell::default();
                }
                for row in (cur_row + 1)..rows {
                    grid.row_mut(row).fill(Cell::default());
                }
            }
            1 => {
                for row in 0..cur_row {
                    grid.row_mut(row).fill(Cell::default());
                }
                for col in 0..=cur_col.min(cols - 1) {
                    *grid.cell_mut(cur_row, col) = Cell::default();
                }
            }
            2 | 3 => {
                grid.clear_all();
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let (cols, cur_row, cur_col) = (self.cols, self.cursor.row, self.cursor.col);
        let grid = self.active_grid_mut();
        match mode {
            0 => {
                for col in cur_col..cols {
                    *grid.cell_mut(cur_row, col) = Cell::default();
                }
            }
            1 => {
                for col in 0..=cur_col.min(cols - 1) {
                    *grid.cell_mut(cur_row, col) = Cell::default();
                }
            }
            2 => {
                grid.row_mut(cur_row).fill(Cell::default());
            }
            _ => {}
        }
    }

    fn print_char(&mut self, c: char) {
        let width = c.width().unwrap_or(1);
        if width == 0 {
            // Combining marks aren't merged into the previous cell in this
            // MVP; dropping them is preferable to corrupting column math.
            return;
        }

        if self.wrap_pending {
            self.wrap_pending = false;
            self.index_down();
            self.cursor.col = 0;
        }

        if self.cursor.col + width > self.cols {
            self.index_down();
            self.cursor.col = 0;
        }

        let (row, col, cols) = (self.cursor.row, self.cursor.col, self.cols);
        let (fg, bg, flags) = (self.cursor.fg, self.cursor.bg, self.cursor.flags);
        let grid = self.active_grid_mut();
        *grid.cell_mut(row, col) = Cell {
            c,
            fg,
            bg,
            flags: if width == 2 { flags | CellFlags::WIDE } else { flags },
        };
        if width == 2 && col + 1 < cols {
            *grid.cell_mut(row, col + 1) = Cell {
                c: ' ',
                fg,
                bg,
                flags: flags | CellFlags::WIDE_SPACER,
            };
        }

        if col + width == cols {
            self.wrap_pending = true;
        } else {
            self.cursor.col += width;
        }
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        self.grid = Grid::new(cols, rows);
        self.alt_grid = Grid::new(cols, rows);
        self.using_alt_screen = false;
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.alt_saved_cursor = Cursor::default();
        self.wrap_pending = false;
        self.modes = TermModes::default();
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.title.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_named_color() {
        let mut term = Term::new(10, 3);
        term.advance(b"\x1b[31mA");
        assert_eq!(term.grid().cell(0, 0).c, 'A');
        assert_eq!(term.grid().cell(0, 0).fg, Color::Indexed(1));
    }

    #[test]
    fn sgr_256_and_truecolor() {
        let mut term = Term::new(10, 3);
        term.advance(b"\x1b[38;5;196mZ");
        assert_eq!(term.grid().cell(0, 0).fg, Color::Indexed(196));

        term.advance(b"\x1b[38;2;10;20;30mY");
        assert_eq!(term.grid().cell(0, 1).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_reset_clears_attributes() {
        let mut term = Term::new(10, 3);
        term.advance(b"\x1b[1;31mA\x1b[0mB");
        assert_eq!(term.grid().cell(0, 0).flags, CellFlags::BOLD);
        assert_eq!(term.grid().cell(0, 0).fg, Color::Indexed(1));
        assert_eq!(term.grid().cell(0, 1).flags, CellFlags::empty());
        assert_eq!(term.grid().cell(0, 1).fg, Color::Default);
    }

    #[test]
    fn cursor_position_csi() {
        let mut term = Term::new(10, 5);
        term.advance(b"\x1b[3;5H");
        term.advance(b"X");
        assert_eq!(term.grid().cell(2, 4).c, 'X');
    }

    #[test]
    fn cursor_relative_movement() {
        let mut term = Term::new(10, 5);
        term.advance(b"\x1b[3;3H"); // row 2, col 2 (0-based)
        term.advance(b"\x1b[1A"); // up 1 -> row 1
        term.advance(b"\x1b[2C"); // right 2 -> col 4
        term.advance(b"X");
        assert_eq!(term.grid().cell(1, 4).c, 'X');
    }

    #[test]
    fn line_wrap_deferred() {
        let mut term = Term::new(5, 3);
        term.advance(b"ABCDE"); // exactly fills row 0
        term.advance(b"F"); // should wrap to row 1, not overwrite row 0
        assert_eq!(term.grid().cell(0, 4).c, 'E');
        assert_eq!(term.grid().cell(1, 0).c, 'F');
        assert_eq!(term.cursor.row, 1);
        assert_eq!(term.cursor.col, 1);
    }

    #[test]
    fn scroll_pushes_to_scrollback() {
        let mut term = Term::new(5, 2);
        term.advance(b"11111\r\n22222\r\n33333");
        assert_eq!(term.grid().cell(0, 0).c, '2');
        assert_eq!(term.grid().cell(1, 0).c, '3');
        assert_eq!(term.grid().scrollback.len(), 1);
        assert_eq!(term.grid().scrollback[0][0].c, '1');
    }

    #[test]
    fn resize_truncates_and_pads() {
        let mut term = Term::new(5, 3);
        term.advance(b"ABCDE\r\nFGHIJ");
        term.resize(3, 2);
        assert_eq!(term.cols(), 3);
        assert_eq!(term.rows(), 2);
        assert_eq!(term.grid().cell(0, 0).c, 'A');
        assert_eq!(term.grid().cell(0, 2).c, 'C');
        assert_eq!(term.grid().cell(1, 0).c, 'F');

        term.resize(6, 4);
        assert_eq!(term.grid().cell(0, 0).c, 'A');
        assert_eq!(term.grid().cell(0, 5).c, ' ');
        assert_eq!(term.grid().cell(3, 0).c, ' ');
    }

    #[test]
    fn scrollback_line_at_walks_history_then_live_grid() {
        let mut term = Term::new(5, 2);
        // Push five lines through a 2-row screen so scrollback accumulates
        // several old lines behind the two still on screen.
        term.advance(b"11111\r\n22222\r\n33333\r\n44444\r\n55555");
        let grid = term.grid();
        assert_eq!(grid.scrollback.len(), 3); // "11111", "22222", "33333" scrolled off

        // scroll_offset 0: live screen ("44444", "55555").
        assert_eq!(grid.line_at(0, 0)[0].c, '4');
        assert_eq!(grid.line_at(1, 0)[0].c, '5');

        // scroll_offset 1: one line back into history.
        assert_eq!(grid.line_at(0, 1)[0].c, '3');
        assert_eq!(grid.line_at(1, 1)[0].c, '4');

        // scroll_offset 3 (fully scrolled back): oldest two lines.
        assert_eq!(grid.line_at(0, 3)[0].c, '1');
        assert_eq!(grid.line_at(1, 3)[0].c, '2');
    }

    #[test]
    fn alt_screen_preserves_primary_and_cursor() {
        let mut term = Term::new(10, 3);
        term.advance(b"hello");
        let (row_before, col_before) = (term.cursor.row, term.cursor.col);

        term.advance(b"\x1b[?1049h");
        assert!(term.using_alt_screen());
        term.advance(b"WORLD");
        assert_eq!(term.grid().cell(0, 0).c, 'W');

        term.advance(b"\x1b[?1049l");
        assert!(!term.using_alt_screen());
        assert_eq!(term.grid().cell(0, 0).c, 'h');
        assert_eq!(term.cursor.row, row_before);
        assert_eq!(term.cursor.col, col_before);
    }

    #[test]
    fn erase_in_display_full_clears_screen() {
        let mut term = Term::new(5, 2);
        term.advance(b"AAAAA\r\nBBBBB");
        term.advance(b"\x1b[2J");
        assert_eq!(term.grid().cell(0, 0).c, ' ');
        assert_eq!(term.grid().cell(1, 4).c, ' ');
    }

    #[test]
    fn osc_sets_title() {
        let mut term = Term::new(10, 3);
        term.advance(b"\x1b]0;my title\x07");
        assert_eq!(term.title, "my title");
    }

    #[test]
    fn real_world_ansi_output_does_not_panic() {
        // Feed genuine `ls --color=always` output (real SGR/erase sequences
        // with whatever quirks macOS's ls actually emits) through the
        // parser as a sanity check beyond the hand-crafted sequences above.
        let output = std::process::Command::new("ls")
            .args(["--color=always", "-la", "/"])
            .output()
            .expect("failed to run ls");
        let mut term = Term::new(80, 24);
        term.advance(&output.stdout);
        assert!(term.cursor.row < term.rows());
        assert!(term.cursor.col <= term.cols());
    }

    #[test]
    fn decckm_mode_toggle() {
        let mut term = Term::new(10, 3);
        assert!(!term.modes.app_cursor_keys);
        term.advance(b"\x1b[?1h");
        assert!(term.modes.app_cursor_keys);
        term.advance(b"\x1b[?1l");
        assert!(!term.modes.app_cursor_keys);
    }
}
