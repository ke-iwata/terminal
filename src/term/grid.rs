use crate::term::color::Color;
use bitflags::bitflags;
use std::collections::VecDeque;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u8 {
        const BOLD = 1 << 0;
        const ITALIC = 1 << 1;
        const UNDERLINE = 1 << 2;
        const REVERSE = 1 << 3;
        /// First column of a double-width glyph.
        const WIDE = 1 << 4;
        /// Trailing placeholder column following a WIDE cell.
        const WIDE_SPACER = 1 << 5;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: CellFlags::empty(),
        }
    }
}

pub type Row = Vec<Cell>;

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    lines: Vec<Row>,
    pub scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            lines: vec![vec![Cell::default(); cols]; rows],
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
        }
    }

    fn blank_row(&self) -> Row {
        vec![Cell::default(); self.cols]
    }

    pub fn row_mut(&mut self, idx: usize) -> &mut Row {
        &mut self.lines[idx]
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.lines[row][col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.lines[row][col]
    }

    /// Resolves a viewport row to its source line, accounting for how far
    /// back `scroll_offset` has scrolled into scrollback. `view_row` is
    /// clamped to `0..rows` by the caller.
    pub fn line_at(&self, view_row: usize, scroll_offset: usize) -> &Row {
        let scroll_offset = scroll_offset.min(self.scrollback.len());
        if view_row < scroll_offset {
            &self.scrollback[self.scrollback.len() - scroll_offset + view_row]
        } else {
            &self.lines[view_row - scroll_offset]
        }
    }

    /// Resize to a new column/row count. This is a naive MVP resize: rows
    /// are truncated or padded with blanks, columns are truncated or
    /// padded per row. There is no reflow of wrapped lines.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        for row in &mut self.lines {
            row.resize(cols, Cell::default());
        }
        self.lines.resize(rows, vec![Cell::default(); cols]);
        self.cols = cols;
        self.rows = rows;
    }

    /// Scroll the inclusive region [top, bottom] up by `n` lines, filling
    /// the vacated bottom lines with blanks. Lines scrolled off the top are
    /// only kept in scrollback when the region spans the whole screen (a
    /// scroll-region-limited scroll, e.g. inside vim, must not pollute
    /// scrollback with lines that never really left the visible screen).
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize) {
        let region_len = bottom + 1 - top;
        let n = n.min(region_len);
        let push_scrollback = top == 0 && bottom == self.rows - 1;
        for _ in 0..n {
            let removed = self.lines.remove(top);
            if push_scrollback {
                self.scrollback.push_back(removed);
                if self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
            }
            self.lines.insert(bottom, self.blank_row());
        }
    }

    /// Scroll the inclusive region [top, bottom] down by `n` lines (reverse
    /// index), filling the vacated top lines with blanks.
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize) {
        let region_len = bottom + 1 - top;
        let n = n.min(region_len);
        for _ in 0..n {
            self.lines.remove(bottom);
            self.lines.insert(top, self.blank_row());
        }
    }

    pub fn clear_all(&mut self) {
        for row in &mut self.lines {
            row.fill(Cell::default());
        }
    }
}
