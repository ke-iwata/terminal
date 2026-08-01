use crate::pty::{self, PtyHandle};
use nix::unistd::Pid;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use crate::config::ShellConfig;
use crate::term::grid::{Cell, CellFlags, Grid};
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
    /// The current click-drag text selection, if any. Endpoints are
    /// distance-from-bottom values, so when new output pushes lines into
    /// scrollback the event loop shifts them by the same amount to keep
    /// the selection pinned to its text (see `main.rs`'s `PtyData`
    /// handler); it's dropped when that text falls out of scrollback, or
    /// on any alternate-screen output (full-screen apps redraw
    /// arbitrarily -- nothing stable to stay anchored to).
    pub selection: Option<Selection>,
    /// The scrollback search bar, open (and owning keyboard focus) when
    /// `Some`. Unlike `selection`, left open across new pty output --
    /// `main.rs` re-runs `Search::recompute` when that happens instead of
    /// just clearing it, so a search stays live and useful while output
    /// keeps arriving rather than vanishing the instant something prints.
    pub search: Option<Search>,
    /// The (cols, rows) most recently pushed to the pty via `TIOCSWINSZ`
    /// (which signals the shell with `SIGWINCH`), as opposed to `term`'s
    /// current size. Kept separate so a live divider drag can resize
    /// `term` -- and thus the on-screen rendering -- every frame while
    /// throttling how often the shell itself actually gets told: shells
    /// with a line editor that redraws on `SIGWINCH` (zsh's zle, by
    /// default on macOS) can spam repeated prompt redraws if signaled
    /// faster than they can redisplay, which otherwise reads as garbled,
    /// duplicated-looking output during a fast drag. See
    /// `App::relayout_all_tabs`.
    pub pty_size: (u16, u16),
    pub last_pty_resize_sent: Option<std::time::Instant>,
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
            pty_size: (cols as u16, rows as u16),
            last_pty_resize_sent: None,
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

/// The split tree: leaves are tab groups, interior nodes are ratio
/// splits. A plain binary tree (rather than a flat list of rects) so
/// closing any group always has an unambiguous answer for what reclaims
/// its space -- the sibling subtree it was split from.
///
/// Splitting lives here, above tabs, rather than inside a tab: each half
/// of a split is a full tab strip of its own, so a preview can sit
/// beside a shell (and either side can be switched independently).
pub enum GroupNode {
    /// Boxed so a leaf (a whole `Group`) doesn't inflate every interior
    /// `Split` node to the same size.
    Leaf(Box<Group>),
    Split {
        direction: SplitDirection,
        /// Fraction of the axis given to `first` (0..1). Starts at 0.5;
        /// changed by dragging the divider.
        ratio: f32,
        first: Box<GroupNode>,
        second: Box<GroupNode>,
    },
}

impl GroupNode {
    pub fn group(&self, id: u64) -> Option<&Group> {
        match self {
            GroupNode::Leaf(g) => (g.id == id).then_some(&**g),
            GroupNode::Split { first, second, .. } => first.group(id).or_else(|| second.group(id)),
        }
    }

    pub fn group_mut(&mut self, id: u64) -> Option<&mut Group> {
        match self {
            GroupNode::Leaf(g) => (g.id == id).then_some(&mut **g),
            GroupNode::Split { first, second, .. } => {
                if first.group(id).is_some() {
                    first.group_mut(id)
                } else {
                    second.group_mut(id)
                }
            }
        }
    }

    /// All groups in tree order (which is also visual reading order:
    /// first/top/left before second/bottom/right).
    pub fn groups(&self) -> Vec<&Group> {
        match self {
            GroupNode::Leaf(g) => vec![&**g],
            GroupNode::Split { first, second, .. } => {
                let mut all = first.groups();
                all.extend(second.groups());
                all
            }
        }
    }

    pub fn groups_mut(&mut self) -> Vec<&mut Group> {
        match self {
            GroupNode::Leaf(g) => vec![&mut **g],
            GroupNode::Split { first, second, .. } => {
                let mut all = first.groups_mut();
                all.extend(second.groups_mut());
                all
            }
        }
    }

    // Splitting is implemented on owned nodes (see `split_owned`) rather
    // than `&mut self`: replacing a leaf with a split that *contains* that
    // leaf can't be expressed through a mutable borrow without a
    // placeholder node, and `Group` has no cheap placeholder to offer.

    /// Compute every group's pixel rectangle (and each divider's) for
    /// this subtree laid out inside `rect`. Pure function of the tree, so
    /// rendering and click hit-testing can both call it and always agree.
    /// `path` is the running tree address of this node (see
    /// `DividerInfo::path`); callers start with an empty one.
    pub fn layout(&self, rect: PaneRect, gap: f32, path: &mut Vec<bool>, groups: &mut Vec<(u64, PaneRect)>, dividers: &mut Vec<DividerInfo>) {
        match self {
            GroupNode::Leaf(g) => groups.push((g.id, rect)),
            GroupNode::Split { direction, ratio, first, second } => {
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
                first.layout(first_rect, gap, path, groups, dividers);
                path.pop();
                path.push(true);
                second.layout(second_rect, gap, path, groups, dividers);
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
            GroupNode::Leaf(_) => {}
            GroupNode::Split { ratio, first, second, .. } => match path.split_first() {
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

/// Replaces the leaf holding `target` with a split of it and `new_group`
/// (the existing group keeps the first/top/left slot). Returns the new
/// group back unchanged if `target` isn't in this subtree.
pub fn split_group(node: GroupNode, target: u64, direction: SplitDirection, new_group: Group) -> (GroupNode, Result<(), Group>) {
    match node {
        GroupNode::Leaf(g) if g.id == target => (
            GroupNode::Split {
                direction,
                ratio: 0.5,
                first: Box::new(GroupNode::Leaf(g)),
                second: Box::new(GroupNode::Leaf(Box::new(new_group))),
            },
            Ok(()),
        ),
        GroupNode::Leaf(g) => (GroupNode::Leaf(g), Err(new_group)),
        GroupNode::Split { direction: d, ratio, first, second } => {
            let (first, outcome) = split_group(*first, target, direction, new_group);
            match outcome {
                Ok(()) => (
                    GroupNode::Split { direction: d, ratio, first: Box::new(first), second },
                    Ok(()),
                ),
                Err(new_group) => {
                    let (second, outcome) = split_group(*second, target, direction, new_group);
                    (
                        GroupNode::Split { direction: d, ratio, first: Box::new(first), second: Box::new(second) },
                        outcome,
                    )
                }
            }
        }
    }
}

/// Removes the leaf holding `id`, collapsing its split into the sibling.
/// Returns the rebuilt subtree (`None` when the removed leaf *was* the
/// whole subtree) and the group taken out.
pub fn remove_group(node: GroupNode, id: u64) -> (Option<GroupNode>, Option<Group>) {
    match node {
        GroupNode::Leaf(g) if g.id == id => (None, Some(*g)),
        GroupNode::Leaf(g) => (Some(GroupNode::Leaf(g)), None),
        GroupNode::Split { direction, ratio, first, second } => {
            // A branch only collapses when the removal emptied it
            // *entirely*. Collapsing whenever the removal happened
            // anywhere in the branch would throw away the sibling
            // subtree along with it -- which, for a group holding live
            // shells, means panes vanishing and their processes leaking.
            let (rebuilt_first, removed) = remove_group(*first, id);
            if let Some(removed) = removed {
                return match rebuilt_first {
                    Some(rebuilt) => (
                        Some(GroupNode::Split { direction, ratio, first: Box::new(rebuilt), second }),
                        Some(removed),
                    ),
                    // The whole first branch is gone: the sibling takes
                    // over the rect the split had.
                    None => (Some(*second), Some(removed)),
                };
            }
            let first = rebuilt_first.expect("nothing was removed from the first branch");
            let (rebuilt_second, removed) = remove_group(*second, id);
            if let Some(removed) = removed {
                return match rebuilt_second {
                    Some(rebuilt) => (
                        Some(GroupNode::Split { direction, ratio, first: Box::new(first), second: Box::new(rebuilt) }),
                        Some(removed),
                    ),
                    None => (Some(first), Some(removed)),
                };
            }
            (
                Some(GroupNode::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(rebuilt_second.expect("nothing was removed, so nothing collapsed")),
                }),
                None,
            )
        }
    }
}

/// One region of the split layout: its own strip of tabs, drawn with its
/// own tab bar. The unit that gets split, and the unit keyboard focus
/// lands on.
pub struct Group {
    /// Stable identity, independent of position in the tree. Never
    /// reused within one run.
    pub id: u64,
    /// Never empty: a group with no tabs left is removed from the tree
    /// by its owner rather than being kept around blank.
    tabs: Vec<Tab>,
    /// Index into `tabs`. Always in range.
    active: usize,
}

impl Group {
    pub fn new(id: u64, tab: Tab) -> Group {
        Group { id, tabs: vec![tab], active: 0 }
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active.min(self.tabs.len() - 1)]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        let index = self.active.min(self.tabs.len() - 1);
        &mut self.tabs[index]
    }

    /// Add a tab and make it active -- opening one is always in order to
    /// use it.
    pub fn add_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    pub fn activate(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    /// Remove the tab at `index` and hand it back. Returns `None` when
    /// it was the last one -- emptying a group means removing the group,
    /// which is its owner's decision to make.
    pub fn close_tab(&mut self, index: usize) -> Option<Tab> {
        if self.tabs.len() <= 1 || index >= self.tabs.len() {
            return None;
        }
        let removed = self.tabs.remove(index);
        // Stay on the tab that slid into this slot, or the new last one.
        self.active = self.active.min(self.tabs.len() - 1);
        Some(removed)
    }

    /// Take every tab out, for when the whole group is being removed.
    pub fn drain_tabs(self) -> Vec<Tab> {
        self.tabs
    }

    pub fn cycle_tab(&mut self, forward: bool) {
        if self.tabs.len() < 2 {
            return;
        }
        self.active = if forward {
            (self.active + 1) % self.tabs.len()
        } else {
            (self.active + self.tabs.len() - 1) % self.tabs.len()
        };
    }
}

/// What a tab holds: one shell session, or one previewed file.
///
/// A shell tab holds exactly one pane -- splitting is a property of the
/// layout above tabs now (see `GroupNode`), not something a tab does
/// internally.
pub enum TabKind {
    Shell(Box<Pane>),
    Preview(Box<crate::preview::Preview>),
}

pub struct Tab {
    /// Stable identity, unique across every group. Never reused within
    /// one run -- the image pipeline keys preview textures by it.
    pub id: u64,
    pub kind: TabKind,
}

impl Tab {
    pub fn shell(id: u64, pane: Pane) -> Tab {
        Tab { id, kind: TabKind::Shell(Box::new(pane)) }
    }

    pub fn preview(id: u64, preview: crate::preview::Preview) -> Tab {
        Tab { id, kind: TabKind::Preview(Box::new(preview)) }
    }

    /// This tab's shell, or `None` on a preview tab. Callers written for
    /// shells fall out here rather than needing a kind check first.
    pub fn pane(&self) -> Option<&Pane> {
        match &self.kind {
            TabKind::Shell(pane) => Some(pane),
            TabKind::Preview(_) => None,
        }
    }

    pub fn pane_mut(&mut self) -> Option<&mut Pane> {
        match &mut self.kind {
            TabKind::Shell(pane) => Some(pane),
            TabKind::Preview(_) => None,
        }
    }

    pub fn preview_content(&self) -> Option<&crate::preview::Preview> {
        match &self.kind {
            TabKind::Preview(preview) => Some(preview),
            TabKind::Shell(_) => None,
        }
    }

    pub fn preview_content_mut(&mut self) -> Option<&mut crate::preview::Preview> {
        match &mut self.kind {
            TabKind::Preview(preview) => Some(preview),
            TabKind::Shell(_) => None,
        }
    }

    /// What the tab strip shows: the shell's running command, or the
    /// previewed file's name.
    pub fn title(&self) -> &str {
        match &self.kind {
            TabKind::Shell(pane) => &pane.title,
            TabKind::Preview(preview) => &preview.title,
        }
    }
}

/// Whether a cell belongs to a "word" for double-click selection.
/// Alphanumerics plus the path-ish punctuation iTerm2 defaults to, so a
/// double-click grabs a whole filename, URL path segment, or flag. A
/// wide character's trailing spacer cell counts as part of its word --
/// otherwise every CJK character would be its own one-cell "word".
fn is_word_cell(cell: &Cell) -> bool {
    cell.flags.contains(CellFlags::WIDE_SPACER) || cell.c.is_alphanumeric() || "_./-~+@".contains(cell.c)
}

/// The selection covering the word around `point` (double-click), or
/// `None` when the clicked cell isn't part of a word.
pub fn word_selection(grid: &Grid, point: GridPoint) -> Option<Selection> {
    let row = grid.absolute_line(point.distance)?;
    let col = point.col.min(row.len().saturating_sub(1));
    if !is_word_cell(&row[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word_cell(&row[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < row.len() && is_word_cell(&row[end + 1]) {
        end += 1;
    }
    Some(Selection {
        anchor: GridPoint { distance: point.distance, col: start },
        cursor: GridPoint { distance: point.distance, col: end },
    })
}

/// The selection covering the whole row under `point` (triple-click).
pub fn line_selection(grid: &Grid, point: GridPoint) -> Option<Selection> {
    let row = grid.absolute_line(point.distance)?;
    Some(Selection {
        anchor: GridPoint { distance: point.distance, col: 0 },
        cursor: GridPoint { distance: point.distance, col: row.len().saturating_sub(1) },
    })
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
        let text: String = row
            .get(from..to.min(row.len()))
            .unwrap_or(&[])
            .iter()
            // A double-width character occupies two cells; the trailing
            // spacer cell holds a placeholder ' ' that isn't real text --
            // copying "日本語" must not come out as "日 本 語 ".
            .filter(|cell| !cell.flags.contains(CellFlags::WIDE_SPACER))
            .map(|cell| cell.c)
            .collect();
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
    fn wide_characters_copy_without_spacer_padding() {
        let mut term = Term::new(20, 5, 100);
        term.advance("日本語".as_bytes()); // 3 wide chars = 6 cells
        let text = extract_selected_text(term.grid(), selection((4, 0), (4, 5)));
        assert_eq!(text.as_deref(), Some("日本語"));
    }

    #[test]
    fn word_selection_grabs_the_word_under_the_point() {
        let mut term = Term::new(30, 5, 100);
        term.advance(b"run ./scripts/build.sh now");
        // Click on the 'b' of "build" (col 10). Path chars join the word.
        let sel = word_selection(term.grid(), GridPoint { distance: 4, col: 10 }).unwrap();
        assert_eq!((sel.anchor.col, sel.cursor.col), (4, 21));
        let text = extract_selected_text(term.grid(), sel);
        assert_eq!(text.as_deref(), Some("./scripts/build.sh"));
        // Clicking the space between words selects nothing.
        assert!(word_selection(term.grid(), GridPoint { distance: 4, col: 3 }).is_none());
    }

    #[test]
    fn word_selection_spans_wide_characters() {
        let mut term = Term::new(20, 5, 100);
        term.advance("ab 日本語 cd".as_bytes());
        let sel = word_selection(term.grid(), GridPoint { distance: 4, col: 5 }).unwrap();
        let text = extract_selected_text(term.grid(), sel);
        assert_eq!(text.as_deref(), Some("日本語"));
    }

    #[test]
    fn line_selection_covers_the_whole_row() {
        let mut term = Term::new(10, 3, 100);
        term.advance(b"hello");
        let sel = line_selection(term.grid(), GridPoint { distance: 2, col: 3 }).unwrap();
        assert_eq!((sel.anchor.col, sel.cursor.col), (0, 9));
        let text = extract_selected_text(term.grid(), sel);
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
            pty_size: (10, 4),
            last_pty_resize_sent: None,
        }
    }

    fn rect(w: f32, h: f32) -> PaneRect {
        PaneRect { x: 0.0, y: 0.0, w, h }
    }

    /// A group holding one shell tab, for exercising tree operations
    /// without forking shells.
    fn dummy_group(id: u64) -> Group {
        Group::new(id, Tab::shell(id, dummy_pane(id)))
    }

    fn tree(id: u64) -> GroupNode {
        GroupNode::Leaf(Box::new(dummy_group(id)))
    }

    /// Split `root` at `target`, panicking if the tree refused -- the
    /// shape of `split_group`'s owned-value API makes this noisy inline.
    fn split(root: GroupNode, target: u64, direction: SplitDirection, new_id: u64) -> GroupNode {
        let (root, outcome) = split_group(root, target, direction, dummy_group(new_id));
        assert!(outcome.is_ok(), "expected {target} to be in the tree");
        root
    }

    fn layout_of(root: &GroupNode, w: f32, h: f32) -> (Vec<(u64, PaneRect)>, Vec<DividerInfo>) {
        let (mut groups, mut dividers, mut path) = (Vec::new(), Vec::new(), Vec::new());
        root.layout(rect(w, h), 2.0, &mut path, &mut groups, &mut dividers);
        (groups, dividers)
    }

    #[test]
    fn splitting_puts_the_new_group_in_the_second_slot() {
        let root = split(tree(1), 1, SplitDirection::Vertical, 2);
        let ids: Vec<u64> = root.groups().iter().map(|g| g.id).collect();
        assert_eq!(ids, vec![1, 2], "tree order is reading order: left before right");
    }

    #[test]
    fn splitting_an_absent_target_hands_the_group_back() {
        // The caller has already spawned a shell by then, so it must come
        // back rather than being dropped with its process still running.
        let (_, outcome) = split_group(tree(1), 99, SplitDirection::Vertical, dummy_group(2));
        assert_eq!(outcome.err().map(|g| g.id), Some(2));
    }

    #[test]
    fn layout_splits_the_rect_between_groups() {
        let root = split(tree(1), 1, SplitDirection::Vertical, 2);
        let (groups, dividers) = layout_of(&root, 102.0, 50.0);
        assert_eq!(groups[0].1, PaneRect { x: 0.0, y: 0.0, w: 50.0, h: 50.0 });
        assert_eq!(groups[1].1, PaneRect { x: 52.0, y: 0.0, w: 50.0, h: 50.0 });
        assert_eq!(dividers.len(), 1);
        assert_eq!(dividers[0].rect, PaneRect { x: 50.0, y: 0.0, w: 2.0, h: 50.0 });
    }

    #[test]
    fn a_horizontal_split_stacks_groups() {
        let root = split(tree(1), 1, SplitDirection::Horizontal, 2);
        let (groups, _) = layout_of(&root, 50.0, 102.0);
        assert_eq!(groups[0].1, PaneRect { x: 0.0, y: 0.0, w: 50.0, h: 50.0 });
        assert_eq!(groups[1].1, PaneRect { x: 0.0, y: 52.0, w: 50.0, h: 50.0 });
    }

    #[test]
    fn removing_a_group_collapses_the_split_into_its_sibling() {
        let root = split(tree(1), 1, SplitDirection::Vertical, 2);
        let (rest, removed) = remove_group(root, 1);
        assert_eq!(removed.map(|g| g.id), Some(1));
        let rest = rest.expect("the sibling survives");
        assert_eq!(rest.groups().len(), 1);
        // The survivor takes the whole rect the split occupied.
        let (groups, dividers) = layout_of(&rest, 102.0, 50.0);
        assert_eq!(groups[0].1, rect(102.0, 50.0));
        assert!(dividers.is_empty());
    }

    #[test]
    fn removing_the_only_group_leaves_nothing() {
        let (rest, removed) = remove_group(tree(1), 1);
        assert!(rest.is_none());
        assert_eq!(removed.map(|g| g.id), Some(1));
    }

    #[test]
    fn nested_splits_lay_out_and_collapse() {
        // 1 | (2 / 3)
        let root = split(tree(1), 1, SplitDirection::Vertical, 2);
        let root = split(root, 2, SplitDirection::Horizontal, 3);
        let (groups, dividers) = layout_of(&root, 102.0, 102.0);
        assert_eq!(groups.iter().map(|g| g.0).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(groups[0].1, PaneRect { x: 0.0, y: 0.0, w: 50.0, h: 102.0 });
        assert_eq!(groups[1].1, PaneRect { x: 52.0, y: 0.0, w: 50.0, h: 50.0 });
        assert_eq!(groups[2].1, PaneRect { x: 52.0, y: 52.0, w: 50.0, h: 50.0 });
        assert_eq!(dividers.len(), 2);

        let (rest, _) = remove_group(root, 3);
        let rest = rest.expect("still two groups");
        let (groups, _) = layout_of(&rest, 102.0, 102.0);
        assert_eq!(groups[1].1, PaneRect { x: 52.0, y: 0.0, w: 50.0, h: 102.0 }, "2 reclaims 3's half");
    }

    #[test]
    fn set_ratio_moves_the_divider() {
        let mut root = split(tree(1), 1, SplitDirection::Vertical, 2);
        root.set_ratio(&[], 0.25);
        let (groups, dividers) = layout_of(&root, 202.0, 100.0);
        // (202 - 2) * 0.25 = 50
        assert_eq!(groups[0].1.w, 50.0);
        assert_eq!(dividers[0].rect.x, 50.0);
    }

    #[test]
    fn set_ratio_reaches_nested_splits_by_path() {
        let mut root = split(tree(1), 1, SplitDirection::Vertical, 2);
        root = split(root, 2, SplitDirection::Horizontal, 3);
        let (_, dividers) = layout_of(&root, 202.0, 102.0);
        assert_eq!(dividers[0].path, Vec::<bool>::new(), "root split");
        assert_eq!(dividers[1].path, vec![true], "the nested split is in the root's second branch");

        root.set_ratio(&[true], 0.25);
        let (groups, _) = layout_of(&root, 202.0, 102.0);
        // (102 - 2) * 0.25 = 25
        assert_eq!(groups[1].1.h, 25.0);
    }

    #[test]
    fn set_ratio_clamps_and_survives_stale_paths() {
        let mut root = split(tree(1), 1, SplitDirection::Vertical, 2);
        root.set_ratio(&[], 0.0);
        let (groups, _) = layout_of(&root, 202.0, 100.0);
        assert!(groups[0].1.w > 0.0, "ratio clamps above zero so a group can't vanish");
        // A path into a branch that is a leaf, not a split: a no-op.
        root.set_ratio(&[false, true], 0.9);
    }

    #[test]
    fn a_group_cycles_and_closes_its_tabs() {
        let mut group = dummy_group(1);
        group.add_tab(Tab::shell(20, dummy_pane(20)));
        group.add_tab(Tab::shell(30, dummy_pane(30)));
        assert_eq!(group.active_tab().id, 30, "a new tab is the one you wanted to use");

        group.cycle_tab(true);
        assert_eq!(group.active_tab().id, 1, "forward from the last wraps to the first");
        group.cycle_tab(false);
        assert_eq!(group.active_tab().id, 30);

        let closed = group.close_tab(2).expect("not the last tab");
        assert_eq!(closed.id, 30);
        assert_eq!(group.tabs().len(), 2);
        assert!(group.active_index() < group.tabs().len(), "active stays in range");
    }

    #[test]
    fn a_group_refuses_to_close_its_last_tab() {
        // Emptying a group means removing the group, which is its
        // owner's call -- the group itself never ends up blank.
        let mut group = dummy_group(1);
        assert!(group.close_tab(0).is_none());
        assert_eq!(group.tabs().len(), 1);
    }

    #[test]
    fn a_preview_tab_has_no_shell_and_is_titled_by_its_file() {
        let tab = Tab::preview(7, crate::preview::Preview::loading("/tmp/report.pdf".into()));
        assert!(tab.pane().is_none());
        assert!(tab.preview_content().is_some());
        assert_eq!(tab.title(), "report.pdf");
    }
}
