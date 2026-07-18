use crate::pty::{self, PtyHandle};
use nix::unistd::Pid;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use crate::config::ShellConfig;
use crate::term::grid::Grid;
use crate::term::Term;

/// A cell in the grid, in the same `(distance_from_bottom, col)` terms
/// `Grid::distance_from_bottom` uses -- a named struct rather than a bare
/// `(usize, usize)` *specifically* because a bare tuple already caused a
/// real bug once: `App::grid_point_at` built one in `(col, distance)`
/// order while `Selection` read it as `(distance, col)`, and since both
/// fields are `usize` the compiler had no way to flag the mismatch --
/// selections looked plausible but silently landed on the wrong cell.
/// Named fields turn that class of bug into a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub distance: usize,
    pub col: usize,
}

/// A text selection anchored in the grid. `anchor` is where the drag
/// started, `cursor` is its current (or final) end; either can be the
/// later point in reading order, so extracting text always normalizes
/// them first.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: GridPoint,
    pub cursor: GridPoint,
}

impl Selection {
    /// `(start, end)` in reading order (top-to-bottom, left-to-right).
    /// Distance shrinks downward on screen, so the point with the larger
    /// distance (or, on the same line, the smaller column) reads first.
    fn ordered(&self) -> (GridPoint, GridPoint) {
        let (a, b) = (self.anchor, self.cursor);
        let a_reads_first = a.distance != b.distance && a.distance > b.distance || a.distance == b.distance && a.col <= b.col;
        if a_reads_first { (a, b) } else { (b, a) }
    }

    /// If `distance` (see `Grid::distance_from_bottom`) is one of this
    /// selection's lines, the inclusive `(from_col, to_col)` range
    /// highlighted on it -- the renderer's cue for which cells to tint.
    pub fn columns_on_line(&self, distance: usize, cols: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered();
        if distance > start.distance || distance < end.distance {
            return None;
        }
        let from = if distance == start.distance { start.col } else { 0 };
        let to = if distance == end.distance { end.col } else { cols.saturating_sub(1) };
        Some((from, to))
    }
}

/// A pathological query (a single common letter against a full 10,000-line
/// scrollback, say) could otherwise turn every keystroke into scanning and
/// highlighting tens of thousands of hits -- capped well past what anyone
/// would actually page through by hand.
const MAX_SEARCH_MATCHES: usize = 2000;

/// An in-progress scrollback search: the query being typed and the
/// resulting match list. Recomputed from scratch on every query edit (see
/// `recompute`) rather than incrementally, since even a full rescan of a
/// realistic scrollback is well under a millisecond -- not worth the
/// bookkeeping an incremental version would need.
pub struct Search {
    pub query: String,
    /// `(distance_from_bottom, start_col, end_col_inclusive)` -- the same
    /// coordinate system `Selection` uses, and for the same reason (stays
    /// meaningful across scrolling). Sorted in reading order, top to
    /// bottom.
    matches: Vec<(usize, usize, usize)>,
    /// Index into `matches` of the one currently jumped to / drawn with
    /// the brighter highlight. Meaningless (but never out of bounds to
    /// use as an index -- checked against `matches.len()` everywhere)
    /// when `matches` is empty.
    current: usize,
}

impl Search {
    pub fn new() -> Search {
        Search { query: String::new(), matches: Vec::new(), current: 0 }
    }

    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// 1-based position of the current match for display (`"3/12"`), or
    /// `None` when there are no matches to number.
    pub fn current_position(&self) -> Option<usize> {
        (!self.matches.is_empty()).then_some(self.current + 1)
    }

    /// Re-scans `grid` for the current query, replacing the match list and
    /// resetting to the first result. ASCII-only case-folding: cell
    /// content is exactly one character per column, and a full
    /// Unicode-aware lowercasing can *expand* a character (e.g. German
    /// `ẞ` -> `ss`), which would desync every column index this whole
    /// feature depends on. Matching non-ASCII letters case-sensitively
    /// only is an acceptable narrowing for a terminal's own scrollback.
    pub fn recompute(&mut self, grid: &Grid) {
        self.matches.clear();
        self.current = 0;
        let needle: Vec<char> = self.query.chars().map(|c| c.to_ascii_lowercase()).collect();
        if needle.is_empty() {
            return;
        }
        let total_lines = grid.rows + grid.scrollback.len();
        'lines: for distance in (0..total_lines).rev() {
            let Some(row) = grid.absolute_line(distance) else { continue };
            if needle.len() > row.len() {
                continue;
            }
            let haystack: Vec<char> = row.iter().map(|c| c.c.to_ascii_lowercase()).collect();
            for start in 0..=(haystack.len() - needle.len()) {
                if haystack[start..start + needle.len()] == needle[..] {
                    self.matches.push((distance, start, start + needle.len() - 1));
                    if self.matches.len() >= MAX_SEARCH_MATCHES {
                        break 'lines;
                    }
                }
            }
        }
    }

    pub fn go_next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    pub fn go_prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// `(distance_from_bottom, start_col)` of the current match, for the
    /// caller to scroll into view -- `None` when there's nothing to jump
    /// to.
    pub fn current_target(&self) -> Option<(usize, usize)> {
        self.matches.get(self.current).map(|&(d, c, _)| (d, c))
    }

    /// Every match on `distance`'s line, as `(from_col, to_col_inclusive,
    /// is_current)` -- the renderer's cue for which cells to tint, and
    /// with which of the two highlight strengths.
    pub fn ranges_on_line(&self, distance: usize) -> Vec<(usize, usize, bool)> {
        self.matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.0 == distance)
            .map(|(i, &(_, from, to))| (from, to, i == self.current))
            .collect()
    }
}

/// One terminal session: its own pty, shell process, and screen/scrollback
/// state. The window holds a `Vec<Tab>` and renders/feeds input to whichever
/// one is active; the others keep running (and buffering output into their
/// own `Term`) in the background exactly like a real terminal's tabs do.
pub struct Tab {
    /// Stable identity, independent of position in the tab strip -- closing
    /// an earlier tab must not confuse in-flight pty reader events tagged
    /// with a later tab's id. Never reused within one run of the app.
    pub id: u64,
    pub term: Term,
    pub pty_master: Arc<OwnedFd>,
    pub pty_child: Pid,
    /// Bumped when this tab's shell is restarted, so stale reader-thread
    /// events from a just-replaced shell session are told apart from the
    /// current one.
    pub pty_generation: u64,
    pub scroll_offset: usize,
    pub shell_name: String,
    pub tty_name: String,
    /// What the tab strip shows for this tab: the foreground process's name
    /// while one is running (e.g. "vim"), the shell's own name otherwise.
    /// Refreshed opportunistically on redraw, not on every keystroke.
    pub title: String,
    /// The current click-drag text selection, if any. Cleared whenever
    /// this tab's content changes underneath it (new pty output) since
    /// there's no cheap way to know whether the selected text moved,
    /// shrank, or still exists at all.
    pub selection: Option<Selection>,
    /// The scrollback search bar, open (and owning keyboard focus) when
    /// `Some`. Unlike `selection`, left open across new pty output --
    /// `main.rs` re-runs `Search::recompute` when that happens instead of
    /// just clearing it, so a search stays live and useful while output
    /// keeps arriving rather than vanishing the instant something prints.
    pub search: Option<Search>,
}

impl Tab {
    /// Spawn a fresh shell and wrap it in a new tab. Does *not* start the
    /// pty reader thread -- the caller does that once it can route the
    /// resulting bytes (see `App::spawn_pty_reader`).
    pub fn spawn(id: u64, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Tab {
        let handle = pty::spawn_shell(shell);
        Tab::from_handle(id, handle, shell, cols, rows, scrollback_lines)
    }

    /// Wrap an already-spawned `PtyHandle` in a new tab, without forking a
    /// shell itself. Needed for the very first tab: `pty::spawn_shell` must
    /// run before the winit event loop exists (see its doc comment), so
    /// `main()` calls it directly and hands the result here rather than
    /// going through `Tab::spawn`.
    pub fn from_handle(id: u64, handle: PtyHandle, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Tab {
        let PtyHandle { master, child } = handle;
        let master = Arc::new(master);
        pty::resize(std::os::fd::AsFd::as_fd(&*master), cols as u16, rows as u16);

        let shell_path = shell.command.clone().or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/zsh".to_string());
        let shell_name = shell_path.rsplit('/').next().unwrap_or(&shell_path).to_string();
        let tty_name = pty::tty_name(std::os::fd::AsFd::as_fd(&*master)).unwrap_or_default();

        Tab {
            id,
            term: Term::new(cols, rows, scrollback_lines),
            pty_master: master,
            pty_child: child,
            pty_generation: 0,
            scroll_offset: 0,
            shell_name: shell_name.clone(),
            tty_name,
            title: shell_name,
            selection: None,
            search: None,
        }
    }

    /// The selected text, if any -- see `extract_selected_text`.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        extract_selected_text(self.term.grid(), selection)
    }
}

/// Reads `selection`'s text out of `grid`, joined with `\n` between lines
/// and with each line's trailing padding blanks trimmed. Doesn't attempt
/// to know whether a line was a hard newline or just a terminal-forced
/// wrap (that information isn't tracked for scrollback rows), so a
/// selection spanning a wrapped line copies out with an extra newline it
/// didn't originally have -- a reasonable simplification given how rarely
/// a copy both spans a wrap point and cares about it. A free function
/// (rather than a `Tab` method) so it's testable against a bare `Term`
/// without spawning a real shell.
fn extract_selected_text(grid: &Grid, selection: Selection) -> Option<String> {
    let (start, end) = selection.ordered();
    if start == end {
        return None;
    }
    let mut lines = Vec::new();
    let mut distance = start.distance;
    loop {
        let row = grid.absolute_line(distance)?;
        let from = if distance == start.distance { start.col } else { 0 };
        // `end.col` is the last *included* column, so the slice's
        // exclusive upper bound is one past it.
        let to = if distance == end.distance { end.col + 1 } else { row.len() };
        let text: String = row.get(from..to.min(row.len())).unwrap_or(&[]).iter().map(|cell| cell.c).collect();
        lines.push(text.trim_end().to_string());
        if distance == end.distance {
            break;
        }
        distance -= 1;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Term;

    fn point((distance, col): (usize, usize)) -> GridPoint {
        GridPoint { distance, col }
    }

    fn selection(anchor: (usize, usize), cursor: (usize, usize)) -> Selection {
        Selection { anchor: point(anchor), cursor: point(cursor) }
    }

    #[test]
    fn single_line_selection() {
        let mut term = Term::new(20, 5, 100);
        term.advance(b"hello world");
        // A freshly created Term's cursor starts at row 0 (the top of a
        // 5-row grid), which is 4 lines above the true bottom -> distance 4.
        let text = extract_selected_text(term.grid(), selection((4, 0), (4, 4)));
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[test]
    fn selection_order_does_not_matter() {
        let mut term = Term::new(20, 5, 100);
        term.advance(b"hello world");
        let forward = extract_selected_text(term.grid(), selection((4, 0), (4, 4)));
        let backward = extract_selected_text(term.grid(), selection((4, 4), (4, 0)));
        assert_eq!(forward, backward);
    }

    #[test]
    fn multi_line_selection_joins_with_newline() {
        let mut term = Term::new(20, 5, 100);
        term.advance(b"AAAAA\r\nBBBBB\r\nCCCCC");
        // A 5-row grid with 3 lines printed leaves 2 blank rows below
        // them, so counting up from the true bottom: distance 0 and 1 are
        // blank, "CCCCC" is 2, "BBBBB" is 3, "AAAAA" is 4.
        let text = extract_selected_text(term.grid(), selection((4, 0), (2, 19)));
        assert_eq!(text.as_deref(), Some("AAAAA\nBBBBB\nCCCCC"));
    }

    #[test]
    fn trailing_blanks_are_trimmed() {
        let mut term = Term::new(20, 5, 100);
        term.advance(b"hi");
        let text = extract_selected_text(term.grid(), selection((4, 0), (4, 19)));
        assert_eq!(text.as_deref(), Some("hi"));
    }

    #[test]
    fn empty_selection_is_none() {
        let mut term = Term::new(20, 5, 100);
        term.advance(b"hello");
        assert_eq!(extract_selected_text(term.grid(), selection((0, 3), (0, 3))), None);
    }

    #[test]
    fn columns_on_line_only_matches_selected_lines() {
        let sel = selection((2, 5), (0, 3));
        assert_eq!(sel.columns_on_line(3, 20), None);
        assert_eq!(sel.columns_on_line(2, 20), Some((5, 19)));
        assert_eq!(sel.columns_on_line(1, 20), Some((0, 19)));
        assert_eq!(sel.columns_on_line(0, 20), Some((0, 3)));
    }

    #[test]
    fn columns_on_line_single_row_clips_to_both_ends() {
        let sel = selection((0, 2), (0, 8));
        assert_eq!(sel.columns_on_line(0, 20), Some((2, 8)));
    }

    fn search_for(term: &Term, query: &str) -> Search {
        let mut search = Search::new();
        search.query = query.to_string();
        search.recompute(term.grid());
        search
    }

    #[test]
    fn recompute_is_case_insensitive_and_reading_order() {
        let mut term = Term::new(20, 3, 100);
        term.advance(b"FOO bar\r\nbar foo\r\nfoo");
        let search = search_for(&term, "foo");
        assert_eq!(search.match_count(), 3);
        // Row 0 ("FOO bar", the top of a 3-row grid) is distance 2, row 1
        // is distance 1, row 2 (bottom) is distance 0. `matches[0]`
        // (-> the initial current match) should be the topmost hit.
        assert_eq!(search.ranges_on_line(2), vec![(0, 2, true)]);
        assert_eq!(search.ranges_on_line(1), vec![(4, 6, false)]);
        assert_eq!(search.ranges_on_line(0), vec![(0, 2, false)]);
    }

    #[test]
    fn current_target_is_the_first_match_by_default() {
        let mut term = Term::new(20, 3, 100);
        term.advance(b"xxx needle xxx");
        let search = search_for(&term, "needle");
        assert_eq!(search.current_target(), Some((2, 4)));
    }

    #[test]
    fn go_next_and_go_prev_wrap_around() {
        let mut term = Term::new(20, 3, 100);
        term.advance(b"a\r\na\r\na");
        let mut search = search_for(&term, "a");
        assert_eq!(search.match_count(), 3);
        assert_eq!(search.current_position(), Some(1));
        search.go_next();
        assert_eq!(search.current_position(), Some(2));
        search.go_next();
        assert_eq!(search.current_position(), Some(3));
        search.go_next();
        assert_eq!(search.current_position(), Some(1), "next from the last match should wrap to the first");
        search.go_prev();
        assert_eq!(search.current_position(), Some(3), "prev from the first match should wrap to the last");
    }

    #[test]
    fn empty_query_has_no_matches() {
        let mut term = Term::new(20, 3, 100);
        term.advance(b"hello");
        let search = search_for(&term, "");
        assert_eq!(search.match_count(), 0);
        assert_eq!(search.current_target(), None);
        assert_eq!(search.current_position(), None);
    }

    #[test]
    fn recompute_replaces_stale_matches_from_a_previous_query() {
        let mut term = Term::new(20, 3, 100);
        term.advance(b"apples and oranges");
        let mut search = search_for(&term, "apples");
        assert_eq!(search.match_count(), 1);
        search.query = "oranges".to_string();
        search.recompute(term.grid());
        assert_eq!(search.match_count(), 1);
        assert_eq!(search.current_target(), Some((2, 11)));
    }
}
