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

/// One shell session: its own pty, shell process, and screen/scrollback
/// state, plus the per-session interaction state (selection, search,
/// scroll position). A `Tab` holds one or more of these arranged in a
/// split tree; unfocused panes keep running and buffering output exactly
/// like background tabs do.
pub struct Pane {
    /// Stable identity, unique across the whole app (not just one tab) --
    /// pty reader events are tagged with it, and a closed pane's id must
    /// never be confused with a live one's. Never reused within one run.
    pub id: u64,
    pub term: Term,
    pub pty_master: Arc<OwnedFd>,
    pub pty_child: Pid,
    /// Bumped when this pane's shell is restarted, so stale reader-thread
    /// events from a just-replaced shell session are told apart from the
    /// current one.
    pub pty_generation: u64,
    pub scroll_offset: usize,
    pub shell_name: String,
    pub tty_name: String,
    /// What the tab strip shows while this pane is focused: the
    /// foreground process's name while one is running (e.g. "vim"), the
    /// shell's own name otherwise. Refreshed opportunistically on redraw.
    pub title: String,
    /// The current click-drag text selection, if any. Cleared whenever
    /// this pane's content changes underneath it (new pty output) since
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

impl Pane {
    /// Spawn a fresh shell and wrap it in a new pane. Does *not* start the
    /// pty reader thread -- the caller does that once it can route the
    /// resulting bytes (see `App::spawn_pty_reader`).
    pub fn spawn(id: u64, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Pane {
        let handle = pty::spawn_shell(shell);
        Pane::from_handle(id, handle, shell, cols, rows, scrollback_lines)
    }

    /// Wrap an already-spawned `PtyHandle` in a new pane, without forking a
    /// shell itself. Needed for the very first pane: `pty::spawn_shell`
    /// must run before the winit event loop exists (see its doc comment),
    /// so `main()` calls it directly and hands the result here.
    pub fn from_handle(id: u64, handle: PtyHandle, shell: &ShellConfig, cols: usize, rows: usize, scrollback_lines: usize) -> Pane {
        let PtyHandle { master, child } = handle;
        let master = Arc::new(master);
        pty::resize(std::os::fd::AsFd::as_fd(&*master), cols as u16, rows as u16);

        let shell_path = shell.command.clone().or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/zsh".to_string());
        let shell_name = shell_path.rsplit('/').next().unwrap_or(&shell_path).to_string();
        let tty_name = pty::tty_name(std::os::fd::AsFd::as_fd(&*master)).unwrap_or_default();

        Pane {
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

/// Which way a split's divider runs. Named after the divider (matching
/// iTerm2's menu wording), not the stacking direction -- "vertical" means
/// a vertical divider, i.e. panes side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Vertical divider: panes sit side by side (Cmd+D).
    Vertical,
    /// Horizontal divider: panes stack top and bottom (Cmd+Shift+D).
    Horizontal,
}

/// A pixel-space rectangle inside the window's grid area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PaneRect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// One divider produced by `PaneNode::layout`: the gap strip itself plus
/// everything a divider drag needs to know -- which split node it belongs
/// to (`path`) and the full region that split divides, so a new ratio can
/// be computed directly from a cursor position inside that region.
#[derive(Debug, Clone)]
pub struct DividerInfo {
    /// The split node's address in the tree, as first/second branch steps
    /// from the root (`false` = first). Only valid until the tree next
    /// changes shape, which is fine: layout is recomputed (and paths
    /// refreshed) on every frame and every hit test.
    pub path: Vec<bool>,
    pub direction: SplitDirection,
    /// The rect the split divides: both children plus the gap.
    pub region: PaneRect,
    /// The visible gap strip.
    pub rect: PaneRect,
}

/// The split tree: leaves are live panes, interior nodes are ratio
/// splits. A plain binary tree (rather than a flat list of rects) so
/// closing any pane always has an unambiguous answer for what reclaims
/// its space -- the sibling subtree it was split from.
pub enum PaneNode {
    /// Boxed so a leaf (a full `Pane`, ~1KB) doesn't inflate every
    /// interior `Split` node to the same size.
    Leaf(Box<Pane>),
    Split {
        direction: SplitDirection,
        /// Fraction of the axis given to `first` (0..1). Starts at 0.5;
        /// changed by dragging the divider.
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    pub fn pane(&self, id: u64) -> Option<&Pane> {
        match self {
            PaneNode::Leaf(p) => (p.id == id).then_some(&**p),
            PaneNode::Split { first, second, .. } => first.pane(id).or_else(|| second.pane(id)),
        }
    }

    pub fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        match self {
            PaneNode::Leaf(p) => (p.id == id).then_some(&mut **p),
            PaneNode::Split { first, second, .. } => {
                if first.pane(id).is_some() {
                    first.pane_mut(id)
                } else {
                    second.pane_mut(id)
                }
            }
        }
    }

    /// All panes in tree order (which is also visual reading order:
    /// first/top/left before second/bottom/right).
    pub fn panes(&self) -> Vec<&Pane> {
        match self {
            PaneNode::Leaf(p) => vec![&**p],
            PaneNode::Split { first, second, .. } => {
                let mut all = first.panes();
                all.extend(second.panes());
                all
            }
        }
    }

    pub fn panes_mut(&mut self) -> Vec<&mut Pane> {
        match self {
            PaneNode::Leaf(p) => vec![&mut **p],
            PaneNode::Split { first, second, .. } => {
                let mut all = first.panes_mut();
                all.extend(second.panes_mut());
                all
            }
        }
    }

    // Splitting is implemented on owned nodes (see `split_owned`) rather
    // than `&mut self`: replacing a leaf with a split that *contains* that
    // leaf can't be expressed through a mutable borrow without a
    // placeholder node, and `Pane` has no cheap placeholder to offer.

    /// Compute every pane's pixel rectangle (and each divider's) for
    /// this subtree laid out inside `rect`. Pure function of the tree, so
    /// rendering and click hit-testing can both call it and always agree.
    /// `path` is the running tree address of this node (see
    /// `DividerInfo::path`); callers start with an empty one.
    pub fn layout(&self, rect: PaneRect, gap: f32, path: &mut Vec<bool>, panes: &mut Vec<(u64, PaneRect)>, dividers: &mut Vec<DividerInfo>) {
        match self {
            PaneNode::Leaf(p) => panes.push((p.id, rect)),
            PaneNode::Split { direction, ratio, first, second } => {
                let (first_rect, divider_rect, second_rect) = match direction {
                    SplitDirection::Vertical => {
                        let w1 = ((rect.w - gap) * ratio).floor();
                        (
                            PaneRect { x: rect.x, y: rect.y, w: w1, h: rect.h },
                            PaneRect { x: rect.x + w1, y: rect.y, w: gap, h: rect.h },
                            PaneRect { x: rect.x + w1 + gap, y: rect.y, w: rect.w - w1 - gap, h: rect.h },
                        )
                    }
                    SplitDirection::Horizontal => {
                        let h1 = ((rect.h - gap) * ratio).floor();
                        (
                            PaneRect { x: rect.x, y: rect.y, w: rect.w, h: h1 },
                            PaneRect { x: rect.x, y: rect.y + h1, w: rect.w, h: gap },
                            PaneRect { x: rect.x, y: rect.y + h1 + gap, w: rect.w, h: rect.h - h1 - gap },
                        )
                    }
                };
                dividers.push(DividerInfo {
                    path: path.clone(),
                    direction: *direction,
                    region: rect,
                    rect: divider_rect,
                });
                path.push(false);
                first.layout(first_rect, gap, path, panes, dividers);
                path.pop();
                path.push(true);
                second.layout(second_rect, gap, path, panes, dividers);
                path.pop();
            }
        }
    }

    /// Set the ratio of the split node addressed by `path` (see
    /// `DividerInfo::path`). Silently does nothing if the path no longer
    /// leads to a split -- the tree may have changed shape since the path
    /// was computed, and a stale drag should drop dead rather than
    /// resize some unrelated node.
    pub fn set_ratio(&mut self, path: &[bool], new_ratio: f32) {
        match self {
            PaneNode::Leaf(_) => {}
            PaneNode::Split { ratio, first, second, .. } => match path.split_first() {
                None => *ratio = new_ratio.clamp(0.05, 0.95),
                Some((&step, rest)) => {
                    if step {
                        second.set_ratio(rest, new_ratio);
                    } else {
                        first.set_ratio(rest, new_ratio);
                    }
                }
            },
        }
    }
}

/// Replaces the leaf holding `target` with a split of it and `new_pane`
/// (the existing pane keeps the first/top/left slot). Returns the new
/// pane back unchanged if `target` isn't in this subtree.
fn split_owned(node: PaneNode, target: u64, direction: SplitDirection, new_pane: Pane) -> (PaneNode, Result<(), Pane>) {
    match node {
        PaneNode::Leaf(p) if p.id == target => (
            PaneNode::Split {
                direction,
                ratio: 0.5,
                first: Box::new(PaneNode::Leaf(p)),
                second: Box::new(PaneNode::Leaf(Box::new(new_pane))),
            },
            Ok(()),
        ),
        leaf @ PaneNode::Leaf(_) => (leaf, Err(new_pane)),
        PaneNode::Split { direction: dir, ratio, first, second } => {
            let (first, outcome) = split_owned(*first, target, direction, new_pane);
            let first = Box::new(first);
            match outcome {
                Ok(()) => (PaneNode::Split { direction: dir, ratio, first, second }, Ok(())),
                Err(pane) => {
                    let (second, outcome) = split_owned(*second, target, direction, pane);
                    (PaneNode::Split { direction: dir, ratio, first, second: Box::new(second) }, outcome)
                }
            }
        }
    }
}

/// Removes the pane `id` from an owned subtree, collapsing its parent
/// split into the sibling. Returns the remaining tree (`None` if the
/// removed pane WAS the whole tree) and the removed pane.
fn remove_owned(node: PaneNode, id: u64) -> (Option<PaneNode>, Option<Pane>) {
    match node {
        PaneNode::Leaf(p) if p.id == id => (None, Some(*p)),
        leaf @ PaneNode::Leaf(_) => (Some(leaf), None),
        PaneNode::Split { direction, ratio, first, second } => {
            let (first_rest, removed) = remove_owned(*first, id);
            if removed.is_some() {
                return match first_rest {
                    None => (Some(*second), removed),
                    Some(f) => (Some(PaneNode::Split { direction, ratio, first: Box::new(f), second }), removed),
                };
            }
            let first = Box::new(first_rest.expect("nothing was removed from `first`, so it survives intact"));
            let (second_rest, removed) = remove_owned(*second, id);
            match second_rest {
                None => (Some(*first), removed),
                Some(s) => (Some(PaneNode::Split { direction, ratio, first, second: Box::new(s) }), removed),
            }
        }
    }
}

/// One tab in the tab strip: a tree of one or more panes plus which of
/// them owns keyboard focus.
pub struct Tab {
    /// Stable identity, independent of position in the tab strip. Never
    /// reused within one run of the app.
    pub id: u64,
    /// Always `Some` from the outside; `Option` only so `remove_pane` can
    /// temporarily take ownership of the tree to restructure it.
    root: Option<PaneNode>,
    /// Which pane keyboard input goes to. Always a live pane's id.
    pub focused: u64,
}

impl Tab {
    pub fn new(id: u64, pane: Pane) -> Tab {
        let focused = pane.id;
        Tab { id, root: Some(PaneNode::Leaf(Box::new(pane))), focused }
    }

    pub fn root(&self) -> &PaneNode {
        self.root.as_ref().expect("a tab always has a root pane")
    }

    pub fn root_mut(&mut self) -> &mut PaneNode {
        self.root.as_mut().expect("a tab always has a root pane")
    }

    pub fn pane_count(&self) -> usize {
        self.root().panes().len()
    }

    pub fn focused_pane(&self) -> &Pane {
        self.root().pane(self.focused).expect("focused always points at a live pane")
    }

    pub fn focused_pane_mut(&mut self) -> &mut Pane {
        let focused = self.focused;
        self.root_mut().pane_mut(focused).expect("focused always points at a live pane")
    }

    /// Split the focused pane, giving the new pane the second (right or
    /// bottom) half, and focus it -- matching what iTerm2/tmux do, since
    /// the reason you split is almost always to use the new shell now.
    pub fn split_focused(&mut self, direction: SplitDirection, new_pane: Pane) {
        let new_id = new_pane.id;
        let root = self.root.take().expect("a tab always has a root pane");
        let (root, outcome) = split_owned(root, self.focused, direction, new_pane);
        self.root = Some(root);
        if outcome.is_ok() {
            self.focused = new_id;
        }
    }

    /// Remove pane `id`, collapsing its split into the sibling. Refuses
    /// (returns `None`) when it's the tab's only pane -- closing the last
    /// pane means closing the tab, which is the caller's decision to make.
    pub fn remove_pane(&mut self, id: u64) -> Option<Pane> {
        if self.pane_count() <= 1 {
            return None;
        }
        let root = self.root.take().expect("a tab always has a root pane");
        let (rest, removed) = remove_owned(root, id);
        self.root = Some(rest.expect("pane_count > 1 means a sibling survives the removal"));
        if removed.is_some() && self.focused == id {
            self.focused = self.root().panes()[0].id;
        }
        removed
    }

    /// Move focus to the next/previous pane in tree (reading) order.
    pub fn cycle_focus(&mut self, forward: bool) {
        let ids: Vec<u64> = self.root().panes().iter().map(|p| p.id).collect();
        let Some(pos) = ids.iter().position(|&id| id == self.focused) else {
            return;
        };
        let next = if forward {
            (pos + 1) % ids.len()
        } else {
            (pos + ids.len() - 1) % ids.len()
        };
        self.focused = ids[next];
    }

    /// Every pane's rect (and each divider's) laid out inside `rect`.
    pub fn layout(&self, rect: PaneRect, gap: f32) -> (Vec<(u64, PaneRect)>, Vec<DividerInfo>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        let mut path = Vec::new();
        self.root().layout(rect, gap, &mut path, &mut panes, &mut dividers);
        (panes, dividers)
    }

    /// Set the ratio of the split addressed by `path` -- see
    /// `PaneNode::set_ratio`.
    pub fn set_split_ratio(&mut self, path: &[bool], ratio: f32) {
        self.root_mut().set_ratio(path, ratio);
    }

    /// What the tab strip shows for this tab: the focused pane's title.
    pub fn title(&self) -> &str {
        &self.focused_pane().title
    }
}

/// Reads `selection`'s text out of `grid`, joined with `\n` between lines
/// and with each line's trailing padding blanks trimmed. Doesn't attempt
/// to know whether a line was a hard newline or just a terminal-forced
/// wrap (that information isn't tracked for scrollback rows), so a
/// selection spanning a wrapped line copies out with an extra newline it
/// didn't originally have -- a reasonable simplification given how rarely
/// a copy both spans a wrap point and cares about it. A free function
/// (rather than a `Pane` method) so it's testable against a bare `Term`
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

    // ---- pane-tree tests -------------------------------------------------

    /// A pane with a real (harmless) fd and a fake pid, for exercising
    /// tree operations without forking shells.
    fn dummy_pane(id: u64) -> Pane {
        let file = std::fs::File::open("/dev/null").expect("/dev/null always opens");
        Pane {
            id,
            term: Term::new(10, 4, 100),
            pty_master: Arc::new(OwnedFd::from(file)),
            pty_child: Pid::from_raw(0),
            pty_generation: 0,
            scroll_offset: 0,
            shell_name: "test".into(),
            tty_name: String::new(),
            title: "test".into(),
            selection: None,
            search: None,
        }
    }

    fn rect(w: f32, h: f32) -> PaneRect {
        PaneRect { x: 0.0, y: 0.0, w, h }
    }

    #[test]
    fn split_focused_focuses_the_new_pane() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        assert_eq!(tab.pane_count(), 2);
        assert_eq!(tab.focused, 11);
    }

    #[test]
    fn layout_splits_the_rect_between_panes() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        let (panes, dividers) = tab.layout(rect(100.0, 50.0), 2.0);
        assert_eq!(panes.len(), 2);
        assert_eq!(dividers.len(), 1);
        let (first_id, first) = panes[0];
        let (second_id, second) = panes[1];
        assert_eq!(first_id, 10, "the original pane keeps the left slot");
        assert_eq!(second_id, 11);
        assert_eq!(first.x, 0.0);
        assert!(second.x > first.x + first.w, "second pane starts past the divider");
        assert!((first.w + second.w + 2.0 - 100.0).abs() < 1.0, "panes + gap fill the rect");
        assert_eq!(first.h, 50.0);
        assert_eq!(second.h, 50.0);
    }

    #[test]
    fn horizontal_split_stacks_panes() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Horizontal, dummy_pane(11));
        let (panes, _) = tab.layout(rect(100.0, 50.0), 2.0);
        let (_, first) = panes[0];
        let (_, second) = panes[1];
        assert_eq!(first.y, 0.0);
        assert!(second.y > first.y + first.h);
        assert_eq!(first.w, 100.0);
    }

    #[test]
    fn remove_pane_collapses_the_split_into_the_sibling() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        let removed = tab.remove_pane(11).expect("pane 11 exists and is not the last");
        assert_eq!(removed.id, 11);
        assert_eq!(tab.pane_count(), 1);
        // Focus was on the removed pane; it must land on a live one.
        assert_eq!(tab.focused, 10);
        // The sibling reclaims the whole rect.
        let (panes, dividers) = tab.layout(rect(100.0, 50.0), 2.0);
        assert_eq!(panes.len(), 1);
        assert!(dividers.is_empty());
        assert_eq!(panes[0].1, rect(100.0, 50.0));
    }

    #[test]
    fn remove_refuses_the_last_pane() {
        let mut tab = Tab::new(1, dummy_pane(10));
        assert!(tab.remove_pane(10).is_none());
        assert_eq!(tab.pane_count(), 1);
    }

    #[test]
    fn nested_splits_layout_and_remove() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11)); // 10 | 11
        tab.split_focused(SplitDirection::Horizontal, dummy_pane(12)); // 10 | (11 / 12)
        assert_eq!(tab.pane_count(), 3);
        let (panes, dividers) = tab.layout(rect(200.0, 100.0), 2.0);
        assert_eq!(panes.len(), 3);
        assert_eq!(dividers.len(), 2);

        // Removing the middle pane leaves 10 | 12.
        let removed = tab.remove_pane(11).expect("not the last pane");
        assert_eq!(removed.id, 11);
        let ids: Vec<u64> = tab.root().panes().iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![10, 12]);
    }

    #[test]
    fn set_split_ratio_moves_the_divider() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        tab.set_split_ratio(&[], 0.25);
        let (panes, dividers) = tab.layout(rect(202.0, 100.0), 2.0);
        let (_, first) = panes[0];
        // (202 - 2) * 0.25 = 50
        assert_eq!(first.w, 50.0);
        assert_eq!(dividers[0].rect.x, 50.0);
        assert_eq!(dividers[0].region, rect(202.0, 100.0));
    }

    #[test]
    fn set_split_ratio_reaches_nested_splits_by_path() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11)); // 10 | 11
        tab.split_focused(SplitDirection::Horizontal, dummy_pane(12)); // 10 | (11 / 12)
        let (_, dividers) = tab.layout(rect(202.0, 102.0), 2.0);
        assert_eq!(dividers[0].path, Vec::<bool>::new(), "root split");
        assert_eq!(dividers[1].path, vec![true], "nested split lives in the root's second branch");

        tab.set_split_ratio(&[true], 0.25);
        let (panes, _) = tab.layout(rect(202.0, 102.0), 2.0);
        let (id, top_right) = panes[1];
        assert_eq!(id, 11);
        // (102 - 2) * 0.25 = 25
        assert_eq!(top_right.h, 25.0);
    }

    #[test]
    fn set_split_ratio_clamps_and_survives_stale_paths() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        tab.set_split_ratio(&[], 0.0);
        let (panes, _) = tab.layout(rect(202.0, 100.0), 2.0);
        assert!(panes[0].1.w > 0.0, "ratio clamps above zero so a pane can't vanish");
        // A path into a branch that is a leaf, not a split: must be a no-op.
        tab.set_split_ratio(&[false, true], 0.9);
    }

    #[test]
    fn cycle_focus_walks_panes_in_order_and_wraps() {
        let mut tab = Tab::new(1, dummy_pane(10));
        tab.split_focused(SplitDirection::Vertical, dummy_pane(11));
        tab.split_focused(SplitDirection::Horizontal, dummy_pane(12));
        assert_eq!(tab.focused, 12);
        tab.cycle_focus(true);
        assert_eq!(tab.focused, 10, "forward from the last pane wraps to the first");
        tab.cycle_focus(true);
        assert_eq!(tab.focused, 11);
        tab.cycle_focus(false);
        assert_eq!(tab.focused, 10);
        tab.cycle_focus(false);
        assert_eq!(tab.focused, 12, "backward from the first pane wraps to the last");
    }
}
