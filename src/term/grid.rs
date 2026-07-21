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
    /// Parallel to `lines`: `wrapped[i]` means row `i`'s content is a
    /// terminal-forced continuation of the same logical line as row
    /// `i + 1` (as opposed to row `i + 1` starting a fresh line because of
    /// an actual `\n`). Used to reflow content across a column-count
    /// resize instead of truncating it. Always kept the same length as
    /// `lines`.
    wrapped: Vec<bool>,
    pub scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    /// How many rows at the back of `scrollback` were hidden by a resize
    /// shrink a moment ago and are still safe for a subsequent grow to
    /// pull back undisturbed. Paired with `scrollback_len_when_pending`,
    /// which pins down the exact `scrollback.len()` those rows were
    /// pushed at: if anything else has touched `scrollback` since (real
    /// output scrolling off via `scroll_up`, or the cap evicting from the
    /// front), the length no longer matches and the pending rows are
    /// treated as gone rather than popped. Without this a grow following
    /// a shrink would otherwise blindly pop whatever is currently at the
    /// back of `scrollback` -- which, if the shell printed anything in
    /// between, is real output that already scrolled off for good, not
    /// this resize's own hidden row -- resurrecting already-displayed
    /// text at the top of the pane.
    rows_pending_restore: usize,
    scrollback_len_when_pending: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        Grid {
            cols,
            rows,
            lines: vec![vec![Cell::default(); cols]; rows],
            wrapped: vec![false; rows],
            scrollback: VecDeque::new(),
            scrollback_limit,
            rows_pending_restore: 0,
            scrollback_len_when_pending: 0,
        }
    }

    fn blank_row(&self) -> Row {
        vec![Cell::default(); self.cols]
    }

    pub fn row_mut(&mut self, idx: usize) -> &mut Row {
        &mut self.lines[idx]
    }

    // Only exercised by tests today (production code reads whole rows via
    // `line_at`), so it's flagged dead code outside `cargo test`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.lines[row][col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        &mut self.lines[row][col]
    }

    /// Mark whether `row`'s content continues onto the next row because a
    /// print forced a line wrap there (as opposed to an actual `\n`).
    /// Called by `Term::print_char` at the moment a wrap actually happens.
    pub fn set_wrapped(&mut self, row: usize, wrapped: bool) {
        if let Some(slot) = self.wrapped.get_mut(row) {
            *slot = wrapped;
        }
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

    /// A viewport row's distance from the bottom of the live screen, in
    /// lines -- 0 is the bottom-most live row, `rows - 1` the top-most
    /// live row, `rows` the most recently scrolled-off line, and so on
    /// back through scrollback. Selection endpoints are stored in this
    /// form (see `crate::tab::Selection`) instead of raw `(view_row,
    /// scroll_offset)` pairs specifically so a selection stays anchored to
    /// the same text while the user scrolls mid-drag: `view_row` alone is
    /// meaningless once `scroll_offset` changes, but a line's distance
    /// from the live bottom does not.
    pub fn distance_from_bottom(&self, view_row: usize, scroll_offset: usize) -> usize {
        scroll_offset + (self.rows - 1 - view_row)
    }

    /// The inverse of `distance_from_bottom`: resolves a stored selection
    /// endpoint back to the row it refers to, or `None` if that line has
    /// since fallen out of scrollback (e.g. the scrollback cap shrank).
    pub fn absolute_line(&self, distance_from_bottom: usize) -> Option<&Row> {
        if distance_from_bottom < self.rows {
            Some(&self.lines[self.rows - 1 - distance_from_bottom])
        } else {
            let k = distance_from_bottom - self.rows;
            (k < self.scrollback.len()).then(|| &self.scrollback[self.scrollback.len() - 1 - k])
        }
    }

    /// Resize to a new column/row count, reflowing content instead of
    /// destroying it: rows linked by `wrapped` are treated as one logical
    /// line, re-wrapped at the new column count, so text that no longer
    /// fits on one row moves to the next instead of being truncated (and
    /// comes back correctly if the terminal widens again). Rows that
    /// overflow the new row count are pushed into scrollback (or pulled
    /// back out of it, if growing) rather than dropped.
    ///
    /// `cursor` is the pre-resize (row, col); returns where it lands
    /// after reflowing.
    pub fn resize_reflow(&mut self, cols: usize, rows: usize, cursor: (usize, usize)) -> (usize, usize) {
        if cols == self.cols && rows == self.rows {
            return cursor;
        }
        let (cursor_row, cursor_col) = if cols != self.cols {
            self.reflow_columns(cols, cursor)
        } else {
            cursor
        };
        let cursor_row = self.adjust_row_count(rows, cursor_row);
        self.cols = cols;
        self.rows = rows;
        (cursor_row.min(rows - 1), cursor_col.min(cols.saturating_sub(1)))
    }

    /// Resize to a new column/row count without reflowing: rows are
    /// truncated or padded with blanks, columns are truncated or padded
    /// per row. Used for the alternate screen, since full-screen apps
    /// (vim, htop, ...) redraw themselves completely on `SIGWINCH` rather
    /// than relying on the terminal to preserve their content.
    pub fn resize_truncate(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        for row in &mut self.lines {
            row.resize(cols, Cell::default());
        }
        self.lines.resize(rows, vec![Cell::default(); cols]);
        self.wrapped.resize(rows, false);
        self.cols = cols;
        self.rows = rows;
    }

    fn reflow_columns(&mut self, new_cols: usize, cursor: (usize, usize)) -> (usize, usize) {
        let old_cols = self.cols;
        let cursor_row = cursor.0.min(self.lines.len().saturating_sub(1));
        let cursor_col = cursor.1.min(old_cols.saturating_sub(1));

        // Group rows into logical lines (runs linked by `wrapped`),
        // flattening each into one cell sequence, and remember where the
        // cursor falls in that flattened representation.
        let mut logical_lines: Vec<Vec<Cell>> = Vec::new();
        let mut cursor_logical = 0usize;
        let mut cursor_offset = 0usize;

        let mut row_idx = 0;
        while row_idx < self.lines.len() {
            let mut cells: Vec<Cell> = Vec::new();
            let is_cursor_line_start = logical_lines.len();
            loop {
                if row_idx == cursor_row {
                    cursor_logical = is_cursor_line_start;
                    cursor_offset = cells.len() + cursor_col;
                }
                cells.extend_from_slice(&self.lines[row_idx]);
                let wraps = self.wrapped.get(row_idx).copied().unwrap_or(false);
                row_idx += 1;
                if !wraps || row_idx >= self.lines.len() {
                    break;
                }
            }
            // Trim trailing blank cells -- only meaningful at the very end
            // of the logical line, since interior rows are always full
            // (that's why they wrapped). A fully blank line trims to
            // empty; the re-wrap loop below turns that back into exactly
            // one blank row.
            while cells.last() == Some(&Cell::default()) {
                cells.pop();
            }
            if logical_lines.len() == cursor_logical {
                cursor_offset = cursor_offset.min(cells.len());
            }
            logical_lines.push(cells);
        }

        // Drop trailing blank logical lines (almost always most of the
        // unused bottom of the screen) rather than re-wrapping each into
        // its own padding row -- that would keep inflating the row count
        // every time a narrower resize adds more wrapped rows, pushing
        // real content into scrollback for no reason. The one holding the
        // cursor is kept even if blank, so it still has somewhere to land.
        while let Some(last) = logical_lines.last() {
            let is_cursor_line = logical_lines.len() - 1 == cursor_logical;
            if last.is_empty() && !is_cursor_line {
                logical_lines.pop();
            } else {
                break;
            }
        }
        if logical_lines.is_empty() {
            logical_lines.push(Vec::new());
            cursor_logical = 0;
            cursor_offset = 0;
        }

        // Re-wrap each logical line at `new_cols`.
        let mut new_lines: Vec<Row> = Vec::new();
        let mut new_wrapped: Vec<bool> = Vec::new();
        let mut new_cursor = (0usize, 0usize);

        for (logical_idx, cells) in logical_lines.into_iter().enumerate() {
            let is_cursor_line = logical_idx == cursor_logical;
            let mut offset = 0usize;
            loop {
                let end = (offset + new_cols).min(cells.len());
                let mut row: Row = cells[offset..end].to_vec();
                row.resize(new_cols, Cell::default());

                if is_cursor_line && cursor_offset >= offset && cursor_offset < offset + new_cols {
                    new_cursor = (new_lines.len(), cursor_offset - offset);
                }

                let more = end < cells.len();
                new_wrapped.push(more);
                new_lines.push(row);

                if !more {
                    if is_cursor_line && cursor_offset >= end {
                        new_cursor = (new_lines.len() - 1, (cursor_offset - offset).min(new_cols.saturating_sub(1)));
                    }
                    break;
                }
                offset += new_cols;
            }
        }

        if new_lines.is_empty() {
            new_lines.push(vec![Cell::default(); new_cols]);
            new_wrapped.push(false);
        }

        self.lines = new_lines;
        self.wrapped = new_wrapped;
        self.cols = new_cols;
        new_cursor
    }

    /// Grow or shrink the row count by pushing/pulling rows through
    /// scrollback (like normal scrolling), instead of truncating. Rows are
    /// only ever added/removed at the top (where scrollback connects), so
    /// takes and returns the cursor's row index, shifted to stay attached
    /// to the same row through any insertions/removals there.
    fn adjust_row_count(&mut self, rows: usize, mut cursor_row: usize) -> usize {
        while self.lines.len() > rows {
            let removed = self.lines.remove(0);
            self.wrapped.remove(0);
            self.scrollback.push_back(removed);
            if self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
            }
            self.rows_pending_restore += 1;
            self.scrollback_len_when_pending = self.scrollback.len();
            cursor_row = cursor_row.saturating_sub(1);
        }
        while self.lines.len() < rows {
            // Scrollback isn't reflowed when the column count changes, so
            // a row stored at an old width can't just be spliced back in
            // here -- that would leave `lines` with rows of mismatched
            // length, corrupting every column-indexed access after it.
            let matches_width = self.scrollback.back().is_some_and(|r| r.len() == self.cols);
            // `scrollback.len()` must still be exactly what it was right
            // after the pending rows were pushed -- if it's higher (real
            // output scrolled off in between) or lower (the cap evicted
            // from the front), something else has touched `scrollback`
            // since, and the tail is no longer guaranteed to be ours.
            let pending_intact = self.rows_pending_restore > 0 && self.scrollback.len() == self.scrollback_len_when_pending;
            if pending_intact && matches_width {
                let row = self.scrollback.pop_back().expect("checked Some above");
                self.lines.insert(0, row);
                self.wrapped.insert(0, false);
                self.rows_pending_restore -= 1;
                self.scrollback_len_when_pending -= 1;
                cursor_row += 1;
            } else {
                self.rows_pending_restore = 0;
                // Padding with a blank row at the bottom doesn't shift any
                // existing row's index.
                self.lines.push(self.blank_row());
                self.wrapped.push(false);
            }
        }
        cursor_row
    }

    /// Update the scrollback cap, trimming immediately if it shrank. Used
    /// when settings are changed live from the settings window.
    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
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
            self.wrapped.remove(top);
            if push_scrollback {
                self.scrollback.push_back(removed);
                if self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
            }
            self.lines.insert(bottom, self.blank_row());
            self.wrapped.insert(bottom, false);
        }
    }

    /// Scroll the inclusive region [top, bottom] down by `n` lines (reverse
    /// index), filling the vacated top lines with blanks.
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize) {
        let region_len = bottom + 1 - top;
        let n = n.min(region_len);
        for _ in 0..n {
            self.lines.remove(bottom);
            self.wrapped.remove(bottom);
            self.lines.insert(top, self.blank_row());
            self.wrapped.insert(top, false);
        }
    }

    pub fn clear_all(&mut self) {
        for row in &mut self.lines {
            row.fill(Cell::default());
        }
        self.wrapped.fill(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_from_bottom_spans_live_rows_at_zero_scroll() {
        let grid = Grid::new(10, 4, 100);
        // Bottom row is 0 lines away from itself; the top row is 3 (rows
        // - 1) lines above it.
        assert_eq!(grid.distance_from_bottom(3, 0), 0);
        assert_eq!(grid.distance_from_bottom(0, 0), 3);
    }

    #[test]
    fn distance_from_bottom_accounts_for_scroll_offset() {
        let grid = Grid::new(10, 4, 100);
        // Scrolled up by 2: the viewport's bottom row is now showing what
        // used to be 2 lines above the true bottom.
        assert_eq!(grid.distance_from_bottom(3, 2), 2);
        assert_eq!(grid.distance_from_bottom(0, 2), 5);
    }

    #[test]
    fn absolute_line_is_the_inverse_of_distance_from_bottom() {
        let mut grid = Grid::new(3, 2, 100);
        grid.row_mut(0)[0].c = 'A'; // top live row
        grid.row_mut(1)[0].c = 'B'; // bottom live row
        assert_eq!(grid.absolute_line(0).unwrap()[0].c, 'B');
        assert_eq!(grid.absolute_line(1).unwrap()[0].c, 'A');
    }

    #[test]
    fn absolute_line_reaches_into_scrollback() {
        let mut grid = Grid::new(3, 2, 100);
        let mut pushed = grid.blank_row();
        pushed[0].c = 'S';
        grid.scrollback.push_back(pushed);
        // Past the two live rows (distance 0, 1), distance 2 is the most
        // recently scrolled-off line.
        assert_eq!(grid.absolute_line(2).unwrap()[0].c, 'S');
    }

    #[test]
    fn absolute_line_none_past_scrollback_end() {
        let grid = Grid::new(3, 2, 100);
        assert!(grid.absolute_line(2).is_none());
    }
}
