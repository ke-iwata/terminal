//! Layout and instance-building for the two bars framing the terminal
//! grid: the tab strip (top) and the status bar (bottom). Both are drawn
//! with the same colored-rect + glyph-rect `Instance` primitives the grid
//! itself uses -- no separate UI toolkit involved.
//!
//! `tab_bar_layout` is the single source of truth for where each tab (and
//! its close button, and the trailing "+" button) sits on screen: both
//! `build_tab_bar_instances` (drawing) and `App`'s click handler (hit
//! testing) call it, so the two can never disagree about where things are.

use super::font::FontAtlas;
use super::pipeline::Instance;
use crate::tab::{PaneRect, Search};
use unicode_width::UnicodeWidthChar;

/// How many monospace columns `text` occupies. Not its character count:
/// CJK and other East Asian characters are drawn two columns wide, and
/// treating them as one makes every width calculation here -- pen
/// advance, truncation, centering -- lay text on top of itself.
fn text_cols(text: &str) -> usize {
    text.chars().map(char_cols).sum()
}

fn char_cols(c: char) -> usize {
    c.width().unwrap_or(0)
}

// Fixed chrome colors, deliberately NOT derived from the terminal palette:
// deriving them from the configured background made the active tab blend
// into the grid below it (both were the palette background), and made the
// whole strip shift with every theme change. A neutral dark chrome -- the
// same choice browsers make -- stays readable over any terminal theme.
// These are also always drawn fully opaque: the window-opacity setting
// only lets the desktop show through the terminal *content*, never
// through the frame around it.
const CHROME_BACKDROP: (u8, u8, u8) = (0x17, 0x18, 0x1c);
const CHROME_TAB_ACTIVE: (u8, u8, u8) = (0x3c, 0x3f, 0x46);
const CHROME_TAB_INACTIVE: (u8, u8, u8) = (0x24, 0x26, 0x2b);
const CHROME_FG_ACTIVE: (u8, u8, u8) = (0xe8, 0xea, 0xed);
const CHROME_FG_INACTIVE: (u8, u8, u8) = (0x8b, 0x8f, 0x97);
const CHROME_FG_DIM: (u8, u8, u8) = (0x6a, 0x6e, 0x76);
const CHROME_ACCENT: (u8, u8, u8) = (0x4d, 0x9f, 0xff);
const CHROME_STATUS_BG: (u8, u8, u8) = (0x1d, 0x1f, 0x24);
const CHROME_STATUS_EDGE: (u8, u8, u8) = (0x3a, 0x3d, 0x44);
const CHROME_STATUS_BRANCH: (u8, u8, u8) = (0x7e, 0xc9, 0x7a);
const CHROME_SEARCH_BG: (u8, u8, u8) = (0x2a, 0x2c, 0x33);
const CHROME_SEARCH_NO_MATCH: (u8, u8, u8) = (0xf3, 0x8b, 0xa8);

/// Tabs shrink toward this floor as more are opened; below it they stop
/// shrinking and the strip simply overflows the window (no scrolling in
/// v1 -- seeing badly truncated titles is a lesser evil than adding a
/// whole scroll interaction for a rare case).
const MIN_TAB_COLS: usize = 8;
const MAX_TAB_COLS: usize = 22;
const NEW_TAB_COLS: usize = 3;
const LEFT_PAD_COLS: usize = 1;
/// Width reserved at a tab's right edge for its close button (" x").
const CLOSE_COLS: usize = 2;

/// Width of the gap between split panes, in physical pixels.
pub const PANE_GAP: f32 = 2.0;

pub fn tab_bar_height(cell_h: f32) -> f32 {
    cell_h * 1.4
}

pub fn status_bar_height(cell_h: f32) -> f32 {
    cell_h * 1.2
}

/// The pixel rectangle above the status bar, minus the file-tree
/// sidebar when it's open -- the area groups are laid out in. The single
/// source of truth for that math: rendering, click hit-testing, and pty
/// resizing all start from this.
///
/// Nothing is reserved at the top: tab strips belong to the groups now,
/// each drawn inside its own rect, so a window-wide band up there would
/// just be an empty gap.
pub fn grid_rect(window_width: f32, window_height: f32, cell_h: f32, sidebar_width: f32) -> PaneRect {
    let bottom = status_bar_height(cell_h);
    PaneRect {
        x: 0.0,
        y: 0.0,
        w: (window_width - sidebar_width).max(1.0),
        h: (window_height - bottom).max(cell_h),
    }
}

/// Default sidebar width in character columns -- wide enough for a
/// couple of indent levels plus a typical file name. Only the starting
/// point: the user drags the edge from there.
const FILE_TREE_DEFAULT_COLS: f32 = 26.0;
/// Narrowest the sidebar can be dragged before names stop being
/// readable at all.
const FILE_TREE_MIN_COLS: f32 = 10.0;
/// Width of the draggable strip along the sidebar's inner edge. The
/// visible border is 1px -- far too thin to grab -- so the hit zone is
/// padded either side, the same trick the pane dividers use.
pub const FILE_TREE_GRAB: f32 = 4.0;

/// How wide the sidebar is right now -- zero when hidden, which makes it
/// safe to feed straight into `grid_rect` unconditionally. `requested`
/// is the user's dragged width in pixels; zero or less means "use the
/// default". The result is always clamped so the sidebar can neither
/// collapse to nothing nor crowd out the terminal.
pub fn file_tree_width(visible: bool, requested: f32, cell_w: f32, window_width: f32) -> f32 {
    if !visible {
        return 0.0;
    }
    let target = if requested > 0.0 { requested } else { FILE_TREE_DEFAULT_COLS * cell_w };
    let min = FILE_TREE_MIN_COLS * cell_w;
    let max = (window_width * 0.6).max(min);
    target.clamp(min, max)
}

/// Where the sidebar sits: the full height between the two bars, flush
/// against the window's right edge.
pub fn file_tree_rect(window_width: f32, window_height: f32, cell_w: f32, cell_h: f32, visible: bool, requested_width: f32) -> Option<PaneRect> {
    let w = file_tree_width(visible, requested_width, cell_w, window_width);
    if w <= 0.0 {
        return None;
    }
    let bottom = status_bar_height(cell_h);
    Some(PaneRect {
        x: window_width - w,
        y: 0.0,
        w,
        h: (window_height - bottom).max(cell_h),
    })
}

// Sidebar palette, lifted from VS Code's Dark+ theme so the tree reads as
// the file explorer it's modeled on: one flat panel color, one neutral
// text color for every entry (VS Code tints icons, never names), and
// hover/selection that differ from the panel only by a few percent of
// lightness.
const TREE_BG: (u8, u8, u8) = (0x25, 0x25, 0x26);
const TREE_FG: (u8, u8, u8) = (0xcc, 0xcc, 0xcc);
const TREE_FG_DIM: (u8, u8, u8) = (0x8c, 0x8c, 0x8c);
/// Files sit a shade below folders in the hierarchy, and their names
/// read a shade dimmer to match -- paired with the icon shape, that's
/// two independent cues for which is which.
const TREE_FG_FILE: (u8, u8, u8) = (0xa8, 0xa8, 0xa8);
/// Folder icons in the blue every desktop uses for them; file icons in
/// plain grey so folders are what the eye lands on first.
const TREE_ICON_DIR: (u8, u8, u8) = (0x7a, 0xa6, 0xd8);
const TREE_ICON_FILE: (u8, u8, u8) = (0x86, 0x8a, 0x90);
const TREE_HOVER: (u8, u8, u8) = (0x2a, 0x2d, 0x2e);
const TREE_SELECTED: (u8, u8, u8) = (0x37, 0x37, 0x3d);
const TREE_INDENT_GUIDE: (u8, u8, u8) = (0x58, 0x58, 0x58);
const TREE_SCROLL_THUMB: (u8, u8, u8) = (0x4f, 0x4f, 0x4f);

/// One indent level, in character columns. VS Code indents 8px per level
/// at its default 13px font; one column is within a pixel of that at the
/// sizes a terminal font runs at.
const TREE_INDENT_COLS: f32 = 1.0;

/// Draw a filled triangle -- the twisty next to a directory -- out of
/// stacked one-pixel bars. Built from quads rather than a glyph because
/// the monospace fonts a terminal picks from don't reliably carry the
/// arrow characters (SF Mono and Menlo both render U+25B8 as tofu), and
/// a font-independent shape is worth more here than reusing the text
/// path.
fn push_twisty(instances: &mut Vec<Instance>, atlas: &FontAtlas, x: f32, y: f32, size: f32, expanded: bool, color: (u8, u8, u8)) {
    let steps = (size.round() as usize).max(3);
    for i in 0..steps {
        let t = i as f32 / (steps - 1) as f32;
        let (bar_x, bar_w) = if expanded {
            // Pointing down: full width at the top, tapering to a point.
            let half = (size / 2.0) * (1.0 - t);
            (x + size / 2.0 - half, half * 2.0)
        } else {
            // Pointing right: widest at the vertical midpoint.
            let extent = 1.0 - (t - 0.5).abs() * 2.0;
            (x, size * extent)
        };
        if bar_w >= 1.0 {
            push_rect(instances, atlas, [bar_x.round(), (y + i as f32).round(), bar_w.round().max(1.0), 1.0], color, 0.0);
        }
    }
}

/// A folder glyph: a filled body with a tab along its top-left, the
/// universal shorthand. Like the twisty, built from quads so it doesn't
/// depend on the terminal font carrying a symbol for it.
fn push_folder_icon(instances: &mut Vec<Instance>, atlas: &FontAtlas, x: f32, y: f32, size: f32, color: (u8, u8, u8)) {
    let tab_h = (size * 0.2).max(1.0);
    push_rect(instances, atlas, [x.round(), y.round(), (size * 0.45).round(), tab_h.round()], color, 0.0);
    push_rect(instances, atlas, [x.round(), (y + tab_h).round(), size.round(), (size * 0.68).round()], color, 0.0);
}

/// A document glyph: a portrait rectangle with its top-right corner
/// notched out, drawn by painting the panel color back over it.
fn push_file_icon(instances: &mut Vec<Instance>, atlas: &FontAtlas, x: f32, y: f32, size: f32, color: (u8, u8, u8)) {
    let w = (size * 0.72).max(2.0);
    let x0 = (x + (size - w) / 2.0).round();
    push_rect(instances, atlas, [x0, y.round(), w.round(), size.round()], color, 0.0);
    let notch = (w * 0.42).max(1.0);
    push_rect(instances, atlas, [(x0 + w - notch).round(), y.round(), notch.round(), notch.round()], TREE_BG, 0.0);
}

/// Rows are slightly taller than a terminal line -- VS Code's list rows
/// have noticeably more leading than its editor lines, and that airiness
/// is a lot of what makes the explorer feel like a list rather than
/// dumped text.
fn file_tree_row_height(cell_h: f32) -> f32 {
    (cell_h * 1.2).round()
}

/// The section-title band above the tree (the uppercase folder name),
/// not counting the `..` row directly below it.
fn file_tree_title_height(cell_h: f32) -> f32 {
    (cell_h * 1.8).round()
}

/// Everything above the scrolling list. Just the section title: the tree
/// only ever browses downward from the shell's directory, so there's no
/// `..` row to leave room for.
fn file_tree_header_height(cell_h: f32) -> f32 {
    file_tree_title_height(cell_h)
}

/// Hit-test a window-pixel position against the sidebar's list, giving
/// the row index under it. Shares its geometry with
/// `build_file_tree_instances` so the two can't disagree.
pub fn file_tree_hit_test(rect: PaneRect, cell_h: f32, scroll: usize, row_count: usize, x: f32, y: f32) -> Option<usize> {
    if !rect.contains(x, y) {
        return None;
    }
    let header_h = file_tree_header_height(cell_h);
    if y < rect.y + header_h {
        return None; // the uppercase title is a label, not a button
    }
    let index = ((y - rect.y - header_h) / file_tree_row_height(cell_h)).floor() as usize + scroll;
    (index < row_count).then_some(index)
}

/// How many tree rows fit below the header.
pub fn file_tree_visible_rows(rect: PaneRect, cell_h: f32) -> usize {
    (((rect.h - file_tree_header_height(cell_h)) / file_tree_row_height(cell_h)).floor() as usize).max(1)
}

/// Draw the sidebar in the shape of VS Code's explorer: an uppercase
/// section title, then one row per visible entry -- twisty, indent
/// guides, and a full-width hover/selection band.
pub fn build_file_tree_instances(atlas: &FontAtlas, rect: PaneRect, view: &super::FileTreeView) -> Vec<Instance> {
    let mut instances = Vec::new();
    let (cw, ch) = (atlas.cell_width, atlas.cell_height);
    let row_h = file_tree_row_height(ch);
    let title_h = file_tree_title_height(ch);
    let header_h = file_tree_header_height(ch);
    // Vertical offset that centers a line of text inside a taller row.
    let text_dy = ((row_h - ch) / 2.0).max(0.0);

    push_rect(&mut instances, atlas, [rect.x, rect.y, rect.w, rect.h], TREE_BG, 0.0);
    push_rect(&mut instances, atlas, [rect.x, rect.y, 1.0, rect.h], CHROME_STATUS_EDGE, 0.0);

    let avail_cols = ((rect.w / cw).floor() as usize).saturating_sub(2);
    let pad_x = rect.x + cw * 0.75;

    // Section title: the rooted folder's own name, uppercased, the way
    // VS Code labels the open workspace. The full path lives in the
    // status bar already, so this doesn't repeat it.
    let title_y = rect.y + (title_h - ch) / 2.0;
    push_twisty(&mut instances, atlas, pad_x, title_y + ch * 0.32, ch * 0.4, true, TREE_FG_DIM);
    push_text(
        &mut instances,
        atlas,
        &truncate(&view.title.to_uppercase(), avail_cols.saturating_sub(2)),
        pad_x + cw * 1.5,
        title_y,
        TREE_FG,
    );

    if view.show_hidden {
        // Only worth saying when it's on -- hidden entries showing with
        // no explanation is the confusing state, not the default.
        let note = " (hidden shown)";
        let note_x = rect.x + rect.w - cw * (text_cols(note) as f32 + 0.5);
        push_text(&mut instances, atlas, note, note_x.max(pad_x), title_y, TREE_FG_DIM);
    }

    let visible = file_tree_visible_rows(rect, ch);
    for (i, row) in view.rows.iter().skip(view.scroll).take(visible).enumerate() {
        let y = rect.y + header_h + i as f32 * row_h;
        let index = view.scroll + i;

        // Selection wins over hover, like every list widget.
        let band = if view.selected == Some(index) {
            Some(TREE_SELECTED)
        } else if view.hover == Some(index) {
            Some(TREE_HOVER)
        } else {
            None
        };
        if let Some(color) = band {
            push_rect(&mut instances, atlas, [rect.x + 1.0, y, rect.w - 1.0, row_h], color, 0.0);
        }

        // One hairline per ancestor level, running the full row height so
        // consecutive rows join into a continuous guide.
        for level in 0..row.depth {
            let gx = (pad_x + level as f32 * TREE_INDENT_COLS * cw + cw * 0.5).round();
            push_rect(&mut instances, atlas, [gx, y, 1.0, row_h], TREE_INDENT_GUIDE, 0.0);
        }

        let indent = row.depth as f32 * TREE_INDENT_COLS * cw;
        let twisty_x = pad_x + indent;
        let icon_size = (ch * 0.5).round().max(4.0);
        if row.is_dir {
            let twisty_size = ch * 0.4;
            push_twisty(&mut instances, atlas, twisty_x + cw * 0.1, y + (row_h - twisty_size) / 2.0, twisty_size, row.expanded, TREE_FG_DIM);
        }
        // Files leave the twisty column empty so their icons and names
        // line up with the folders' in the same directory.
        let icon_x = twisty_x + cw * 1.3;
        let icon_y = y + (row_h - icon_size) / 2.0;
        if row.is_dir {
            push_folder_icon(&mut instances, atlas, icon_x, icon_y, icon_size, TREE_ICON_DIR);
        } else {
            push_file_icon(&mut instances, atlas, icon_x, icon_y, icon_size, TREE_ICON_FILE);
        }

        let name_x = icon_x + icon_size + cw * 0.4;
        let used_cols = ((name_x - rect.x) / cw).ceil() as usize;
        let name_cols = avail_cols.saturating_sub(used_cols);
        let color = if row.is_dir { TREE_FG } else { TREE_FG_FILE };
        push_text(&mut instances, atlas, &truncate(&row.name, name_cols), name_x, y + text_dy, color);
    }

    // A minimal scroll thumb, drawn only when there's more than fits --
    // otherwise there's no scroll position worth communicating.
    if view.rows.len() > visible {
        let track_y = rect.y + header_h;
        let track_h = rect.h - header_h;
        let thumb_h = (track_h * visible as f32 / view.rows.len() as f32).max(12.0);
        let max_scroll = (view.rows.len() - visible) as f32;
        let progress = if max_scroll > 0.0 { view.scroll as f32 / max_scroll } else { 0.0 };
        let thumb_y = track_y + progress * (track_h - thumb_h);
        push_rect(&mut instances, atlas, [rect.x + rect.w - 4.0, thumb_y, 4.0, thumb_h], TREE_SCROLL_THUMB, 0.0);
    }

    instances
}


/// Thin separator strips between split panes. Opaque like the rest of the
/// chrome -- see the note on the chrome color constants.
pub fn build_divider_instances(atlas: &FontAtlas, dividers: &[PaneRect]) -> Vec<Instance> {
    let mut instances = Vec::with_capacity(dividers.len());
    for d in dividers {
        push_rect(&mut instances, atlas, [d.x, d.y, d.w, d.h], CHROME_STATUS_EDGE, 0.0);
    }
    instances
}

/// A thin accent outline around the focused pane, drawn only when a tab
/// actually has multiple panes -- with a single pane there's nothing to
/// disambiguate and the outline would just be noise.
pub fn build_focus_border_instances(atlas: &FontAtlas, rect: PaneRect) -> Vec<Instance> {
    const T: f32 = 2.0;
    let mut instances = Vec::with_capacity(4);
    push_rect(&mut instances, atlas, [rect.x, rect.y, rect.w, T], CHROME_ACCENT, 0.0);
    push_rect(&mut instances, atlas, [rect.x, rect.y + rect.h - T, rect.w, T], CHROME_ACCENT, 0.0);
    push_rect(&mut instances, atlas, [rect.x, rect.y, T, rect.h], CHROME_ACCENT, 0.0);
    push_rect(&mut instances, atlas, [rect.x + rect.w - T, rect.y, T, rect.h], CHROME_ACCENT, 0.0);
    instances
}

pub struct TabRect {
    pub index: usize,
    pub x0: f32,
    pub x1: f32,
    pub close_x0: f32,
    pub close_x1: f32,
    /// Truncated/ellipsized display label -- already fitted to the rect.
    pub label: String,
}

pub struct TabBarLayout {
    pub tabs: Vec<TabRect>,
    pub new_tab_x0: f32,
    pub new_tab_x1: f32,
}

pub enum TabBarHit {
    Switch(usize),
    Close(usize),
    NewTab,
}

impl TabBarLayout {
    pub fn hit_test(&self, x: f32) -> Option<TabBarHit> {
        for tab in &self.tabs {
            if x >= tab.close_x0 && x < tab.close_x1 {
                return Some(TabBarHit::Close(tab.index));
            }
            if x >= tab.x0 && x < tab.x1 {
                return Some(TabBarHit::Switch(tab.index));
            }
        }
        if x >= self.new_tab_x0 && x < self.new_tab_x1 {
            return Some(TabBarHit::NewTab);
        }
        None
    }
}

/// Lay tabs out left to right at equal width inside `strip` -- one
/// group's slice of the window, not necessarily the whole width.
/// Pure/deterministic so it can be called on every click and every
/// redraw without drifting apart.
pub fn tab_bar_layout(titles: &[String], strip: PaneRect, cell_w: f32) -> TabBarLayout {
    let total_cols = ((strip.w / cell_w).floor() as usize).max(1);
    let n = titles.len().max(1);
    let available_for_tabs = total_cols.saturating_sub(NEW_TAB_COLS);
    let tab_cols = (available_for_tabs / n).clamp(MIN_TAB_COLS, MAX_TAB_COLS);
    let label_cols = tab_cols.saturating_sub(LEFT_PAD_COLS + CLOSE_COLS);

    // Breathing room before the first tab, so its rounded corner doesn't
    // sit flush against the group's edge. Applied here (not at draw
    // time) so click hit-testing shares the exact same offset.
    let origin = strip.x + cell_w * 0.5;

    let mut tabs = Vec::with_capacity(titles.len());
    for (i, title) in titles.iter().enumerate() {
        let x0 = origin + (i * tab_cols) as f32 * cell_w;
        let x1 = x0 + tab_cols as f32 * cell_w;
        let close_x0 = x1 - CLOSE_COLS as f32 * cell_w;
        // "1: bash" -- the number is the tab's current position in the
        // strip (not a stable id), matching how every browser/terminal
        // numbers its Cmd+N tab shortcuts.
        let label = format!("{}: {}", i + 1, title);
        tabs.push(TabRect {
            index: i,
            x0,
            x1,
            close_x0,
            close_x1: x1,
            label: truncate(&label, label_cols),
        });
    }

    let new_tab_x0 = origin + (titles.len() * tab_cols) as f32 * cell_w;
    TabBarLayout {
        tabs,
        new_tab_x0,
        new_tab_x1: new_tab_x0 + NEW_TAB_COLS as f32 * cell_w,
    }
}

/// Shorten `text` to fit `max_cols` display columns, with an ellipsis
/// when something was cut. Budgets in columns rather than characters so
/// a line of Japanese doesn't overflow twice its allowance.
fn truncate(text: &str, max_cols: usize) -> String {
    if text_cols(text) <= max_cols {
        return text.to_string();
    }
    // Reserve room for the "..." unless there isn't even that much.
    let budget = if max_cols > 3 { max_cols - 3 } else { max_cols };
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_cols(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    if max_cols > 3 {
        out.push_str("...");
    }
    out
}

/// What the status bar shows, pre-resolved by `App::refresh_status` --
/// rendering just lays these out and colors them, no process/filesystem
/// lookups here.
pub struct StatusInfo {
    pub shell: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub tty: String,
}

/// Draw one group's tab strip inside `strip`. `group_focused` dims the
/// whole strip when this isn't the group keyboard input goes to, so
/// several strips on screen at once still read as one being current.
pub fn build_tab_bar_instances(atlas: &FontAtlas, layout: &TabBarLayout, active: usize, strip: PaneRect, group_focused: bool) -> Vec<Instance> {
    let mut instances = Vec::new();
    let bar_height = strip.h;

    push_rect(&mut instances, atlas, [strip.x, strip.y, strip.w, bar_height], CHROME_BACKDROP, 0.0);

    let text_y = strip.y + (bar_height - atlas.cell_height) / 2.0;
    let accent_h = (bar_height * 0.08).max(2.0);
    // Rounded-top, flush-bottom shape (Chrome/Arc-style tab): adjacent
    // tabs' rounded shoulders leave a sliver of backdrop showing through
    // between them, which reads as separation on its own -- no divider
    // hairline needed on top of that.
    let tab_radius = (bar_height * 0.35).clamp(4.0, 12.0);

    for tab in &layout.tabs {
        let is_active = tab.index == active;
        let bg = if is_active { CHROME_TAB_ACTIVE } else { CHROME_TAB_INACTIVE };
        push_rect(&mut instances, atlas, [tab.x0, strip.y, tab.x1 - tab.x0, bar_height], bg, tab_radius);

        let fg = match (is_active, group_focused) {
            (true, true) => CHROME_FG_ACTIVE,
            (true, false) => CHROME_FG_INACTIVE,
            (false, _) => CHROME_FG_DIM,
        };
        push_text(&mut instances, atlas, &tab.label, tab.x0 + atlas.cell_width * LEFT_PAD_COLS as f32, text_y, fg);
        push_text(&mut instances, atlas, "x", tab.close_x0 + atlas.cell_width * 0.5, text_y, CHROME_FG_DIM);

        // A bright accent (not just a background-darkness change) is what
        // actually reads as "selected" at a glance -- muted in a group
        // that isn't focused so only one strip claims to be current.
        if is_active {
            let accent = if group_focused { CHROME_ACCENT } else { CHROME_STATUS_EDGE };
            push_rect(&mut instances, atlas, [tab.x0, strip.y + bar_height - accent_h, tab.x1 - tab.x0, accent_h], accent, 0.0);
        }
    }

    push_text(&mut instances, atlas, "+", layout.new_tab_x0 + atlas.cell_width, text_y, CHROME_FG_INACTIVE);

    instances
}

pub fn build_status_bar_instances(atlas: &FontAtlas, status: &StatusInfo, window_width: f32, window_height: f32, bar_height: f32) -> Vec<Instance> {
    let mut instances = Vec::new();
    let y = window_height - bar_height;

    push_rect(&mut instances, atlas, [0.0, y, window_width, bar_height], CHROME_STATUS_BG, 0.0);
    // A crisp top edge separates the bar from live terminal content more
    // clearly than a flat background-tint difference alone.
    push_rect(&mut instances, atlas, [0.0, y, window_width, 1.0], CHROME_STATUS_EDGE, 0.0);

    let sep_color = CHROME_FG_DIM;
    let shell_color = CHROME_FG_INACTIVE;
    let cwd_color = CHROME_FG_ACTIVE;
    // Green reads as "git branch" at a glance in most shell prompts/themes
    // -- reuse that association instead of just dimming the text.
    let branch_color = CHROME_STATUS_BRANCH;
    let tty_color = CHROME_FG_DIM;

    let mut parts: Vec<(&str, (u8, u8, u8))> = vec![(status.shell.as_str(), shell_color), (status.cwd.as_str(), cwd_color)];
    if let Some(branch) = &status.branch {
        parts.push((branch.as_str(), branch_color));
    }
    parts.push((status.tty.as_str(), tty_color));

    let max_chars = ((window_width / atlas.cell_width) as usize).saturating_sub(2);
    let text_y = y + 1.0 + (bar_height - 1.0 - atlas.cell_height) / 2.0;
    let mut x = atlas.cell_width;
    let mut used = 0usize;
    for (i, (text, color)) in parts.iter().enumerate() {
        if i > 0 {
            if max_chars.saturating_sub(used) < 3 {
                break;
            }
            push_text(&mut instances, atlas, " | ", x, text_y, sep_color);
            x += atlas.cell_width * 3.0;
            used += 3;
        }
        let remaining = max_chars.saturating_sub(used);
        let shown = truncate(text, remaining);
        let shown_len = text_cols(&shown);
        push_text(&mut instances, atlas, &shown, x, text_y, *color);
        x += atlas.cell_width * shown_len as f32;
        used += shown_len;
        if shown_len < text_cols(text) {
            break; // out of room; nothing after this would fit anyway
        }
    }

    instances
}

/// A small floating pill anchored to the top-right of the grid (like a
/// browser's find bar) rather than a full-width bar that would need to
/// resize the grid every time search opens or closes.
pub fn build_search_bar_instances(atlas: &FontAtlas, search: &Search, area: PaneRect, _cell_h: f32) -> Vec<Instance> {
    let mut instances = Vec::new();

    const LABEL: &str = "Find: ";
    let count_text = match search.current_position() {
        Some(pos) => format!("{}/{}", pos, search.match_count()),
        None if search.query.is_empty() => String::new(),
        None => "0/0".to_string(),
    };
    // Reserve room for a reasonably long query so the bar doesn't visibly
    // resize on every keystroke; it still grows past this if needed.
    let query_cols = text_cols(&search.query).max(16);
    let content_cols = LABEL.len() + query_cols + 1 + 3 + text_cols(&count_text);

    let bar_w = content_cols as f32 * atlas.cell_width;
    let bar_h = atlas.cell_height * 1.6;
    let x0 = (area.x + area.w - bar_w - atlas.cell_width).max(area.x);
    let y0 = area.y + atlas.cell_height * 0.3;
    let radius = (bar_h * 0.3).clamp(4.0, 10.0);

    push_rect(&mut instances, atlas, [x0, y0, bar_w, bar_h], CHROME_SEARCH_BG, radius);

    let text_y = y0 + (bar_h - atlas.cell_height) / 2.0;
    let mut x = x0 + atlas.cell_width;
    push_text(&mut instances, atlas, LABEL, x, text_y, CHROME_FG_INACTIVE);
    x += atlas.cell_width * LABEL.len() as f32;

    push_text(&mut instances, atlas, &search.query, x, text_y, CHROME_FG_ACTIVE);
    x += atlas.cell_width * text_cols(&search.query) as f32;
    // A plain caret, not a blinking one -- there's no per-frame ticking
    // clock driving redraws (this app only redraws on real events), so an
    // animated blink would freeze mid-phase as often as not.
    push_text(&mut instances, atlas, "|", x, text_y, CHROME_FG_ACTIVE);

    if !count_text.is_empty() {
        let count_x = x0 + bar_w - atlas.cell_width * (text_cols(&count_text) + 1) as f32;
        let count_color = if search.match_count() == 0 { CHROME_SEARCH_NO_MATCH } else { CHROME_FG_INACTIVE };
        push_text(&mut instances, atlas, &count_text, count_x, text_y, count_color);
    }

    instances
}

// ---- preview overlay ---------------------------------------------------

const PREVIEW_BACKDROP: (u8, u8, u8) = (0x10, 0x11, 0x14);
/// Not fully opaque: enough of the terminal shows through to keep the
/// preview feeling like a layer over the session rather than a screen
/// that replaced it.
const PREVIEW_BACKDROP_ALPHA: f32 = 0.96;
const PREVIEW_TITLE_BG: (u8, u8, u8) = (0x1d, 0x1f, 0x24);
const PREVIEW_ERROR_FG: (u8, u8, u8) = (0xf3, 0x8b, 0xa8);

/// What the overlay is showing right now. The image case carries no
/// data: its pixels live in a GPU texture that `ImagePipeline` draws,
/// not in the instance stream.
pub enum PreviewBody<'a> {
    Loading,
    Failed(&'a str),
    Text { lines: &'a [String], scroll: usize },
    Image,
}

pub struct PreviewLayout {
    /// The whole overlay, including its title strip.
    pub area: PaneRect,
    /// Where the image or text goes, below the title strip.
    pub content: PaneRect,
}

/// Lay the overlay over the pane area (never the sidebar -- the tree
/// stays clickable so you can step through files without closing the
/// preview each time).
pub fn preview_layout(grid: PaneRect, cell_h: f32) -> PreviewLayout {
    let title_h = (cell_h * 2.0).round();
    let pad = (cell_h * 0.6).round();
    PreviewLayout {
        area: grid,
        content: PaneRect {
            x: grid.x + pad,
            y: grid.y + title_h + pad,
            w: (grid.w - pad * 2.0).max(1.0),
            h: (grid.h - title_h - pad * 2.0).max(1.0),
        },
    }
}

/// Fit `(image_w, image_h)` inside `content`, preserving aspect ratio
/// and centering it. Scales up as well as down, like every image viewer:
/// a 32x32 icon shown at 32x32 in the middle of a large window would
/// read as a bug rather than as fidelity.
pub fn preview_image_rect(content: PaneRect, image_w: u32, image_h: u32) -> PaneRect {
    if image_w == 0 || image_h == 0 {
        return content;
    }
    let scale = (content.w / image_w as f32).min(content.h / image_h as f32);
    let w = (image_w as f32 * scale).max(1.0);
    let h = (image_h as f32 * scale).max(1.0);
    PaneRect {
        x: content.x + (content.w - w) / 2.0,
        y: content.y + (content.h - h) / 2.0,
        w,
        h,
    }
}

/// How many text lines fit in the content area.
pub fn preview_visible_lines(layout: &PreviewLayout, cell_h: f32) -> usize {
    ((layout.content.h / cell_h).floor() as usize).max(1)
}

/// Draw the overlay's backdrop, title strip, and -- for text and status
/// states -- its body. An image body is drawn separately by
/// `ImagePipeline`, on top of these instances.
pub fn build_preview_instances(atlas: &FontAtlas, layout: &PreviewLayout, subtitle: &str, body: &PreviewBody) -> Vec<Instance> {
    let mut instances = Vec::new();
    let (cw, ch) = (atlas.cell_width, atlas.cell_height);
    let area = layout.area;
    let title_h = (ch * 2.0).round();

    push_rect_alpha(&mut instances, atlas, [area.x, area.y, area.w, area.h], PREVIEW_BACKDROP, PREVIEW_BACKDROP_ALPHA);
    push_rect(&mut instances, atlas, [area.x, area.y, area.w, title_h], PREVIEW_TITLE_BG, 0.0);
    push_rect(&mut instances, atlas, [area.x, area.y + title_h - 1.0, area.w, 1.0], CHROME_STATUS_EDGE, 0.0);

    let title_y = area.y + (title_h - ch) / 2.0;
    let total_cols = ((area.w / cw).floor() as usize).saturating_sub(2);
    // The dismissal hint is pinned right; the note gets whatever's left.
    const HINT: &str = "esc to close";
    let hint_cols = HINT.len() + 2;
    push_text(&mut instances, atlas, &truncate(subtitle, total_cols.saturating_sub(hint_cols)), area.x + cw, title_y, CHROME_FG_INACTIVE);
    let hint_x = area.x + area.w - cw * (HINT.len() as f32 + 1.0);
    push_text(&mut instances, atlas, HINT, hint_x, title_y, CHROME_FG_DIM);

    match body {
        // The image itself is a texture, drawn by the image pipeline
        // after these instances so it lands on top of the backdrop.
        PreviewBody::Image => {}
        PreviewBody::Loading => push_centered(&mut instances, atlas, &layout.content, "Loading...", CHROME_FG_INACTIVE),
        PreviewBody::Failed(message) => push_centered(&mut instances, atlas, &layout.content, message, PREVIEW_ERROR_FG),
        PreviewBody::Text { lines, scroll } => {
            let visible = preview_visible_lines(layout, ch);
            let cols = ((layout.content.w / cw).floor() as usize).max(1);
            for (i, line) in lines.iter().skip(*scroll).take(visible).enumerate() {
                let y = layout.content.y + i as f32 * ch;
                push_text(&mut instances, atlas, &truncate(line, cols), layout.content.x, y, CHROME_FG_ACTIVE);
            }
            if lines.len() > visible {
                let track_h = layout.content.h;
                let thumb_h = (track_h * visible as f32 / lines.len() as f32).max(12.0);
                let max_scroll = (lines.len() - visible) as f32;
                let progress = if max_scroll > 0.0 { *scroll as f32 / max_scroll } else { 0.0 };
                let thumb_y = layout.content.y + progress * (track_h - thumb_h);
                push_rect(&mut instances, atlas, [area.x + area.w - 5.0, thumb_y, 4.0, thumb_h], TREE_SCROLL_THUMB, 0.0);
            }
        }
    }

    instances
}

fn push_centered(instances: &mut Vec<Instance>, atlas: &FontAtlas, area: &PaneRect, text: &str, color: (u8, u8, u8)) {
    let width = atlas.cell_width * text_cols(text) as f32;
    let x = area.x + (area.w - width) / 2.0;
    let y = area.y + (area.h - atlas.cell_height) / 2.0;
    push_text(instances, atlas, text, x.max(area.x), y, color);
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// `push_rect` for the one place that needs a translucent fill.
fn push_rect_alpha(instances: &mut Vec<Instance>, atlas: &FontAtlas, rect: [f32; 4], color: (u8, u8, u8), alpha: f32) {
    let [x, y, w, h] = rect;
    let [r, g, b, _] = rgb_to_color(color);
    instances.push(Instance {
        pos: [x, y],
        size: [w, h],
        uv_min: atlas.white_uv,
        uv_max: atlas.white_uv,
        color: [r, g, b, alpha],
        top_corner_radius: 0.0,
    });
}

/// `rect` is `[x, y, w, h]` in window pixels.
fn push_rect(instances: &mut Vec<Instance>, atlas: &FontAtlas, rect: [f32; 4], color: (u8, u8, u8), top_corner_radius: f32) {
    let [x, y, w, h] = rect;
    instances.push(Instance {
        pos: [x, y],
        size: [w, h],
        uv_min: atlas.white_uv,
        uv_max: atlas.white_uv,
        color: rgb_to_color(color),
        top_corner_radius,
    });
}

fn push_text(instances: &mut Vec<Instance>, atlas: &FontAtlas, text: &str, start_x: f32, y: f32, color: (u8, u8, u8)) {
    let color = rgb_to_color(color);
    let mut x = start_x;
    for ch in text.chars() {
        if ch != ' ' {
            if let Some(glyph) = atlas.glyph(ch) {
                if glyph.width > 0.0 && glyph.height > 0.0 {
                    let gx = x + glyph.xmin;
                    let gy = y + atlas.baseline - glyph.ymin - glyph.height;
                    instances.push(Instance {
                        pos: [gx, gy],
                        size: [glyph.width, glyph.height],
                        uv_min: glyph.uv_min,
                        uv_max: glyph.uv_max,
                        color,
                        top_corner_radius: 0.0,
                    });
                }
            }
        }
        // Wide characters occupy two cells; advancing by one would draw
        // the next glyph on top of this one.
        x += atlas.cell_width * char_cols(ch).max(1) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_W: f32 = 8.0;
    const CELL_H: f32 = 17.0;
    const WINDOW_W: f32 = 1200.0;

    #[test]
    fn hidden_sidebar_takes_no_width() {
        assert_eq!(file_tree_width(false, 0.0, CELL_W, WINDOW_W), 0.0);
        // Even a width the user dragged earlier stays out of the layout
        // while the sidebar is closed.
        assert_eq!(file_tree_width(false, 400.0, CELL_W, WINDOW_W), 0.0);
    }

    #[test]
    fn zero_requested_width_means_the_default() {
        assert_eq!(file_tree_width(true, 0.0, CELL_W, WINDOW_W), FILE_TREE_DEFAULT_COLS * CELL_W);
    }

    #[test]
    fn dragged_width_is_clamped_at_both_ends() {
        // Dragged past the right edge: floored so names stay readable.
        assert_eq!(file_tree_width(true, 5.0, CELL_W, WINDOW_W), FILE_TREE_MIN_COLS * CELL_W);
        // Dragged across the whole window: capped so the terminal keeps
        // most of it.
        assert_eq!(file_tree_width(true, 5000.0, CELL_W, WINDOW_W), WINDOW_W * 0.6);
        // In between, honored as-is.
        assert_eq!(file_tree_width(true, 300.0, CELL_W, WINDOW_W), 300.0);
    }

    #[test]
    fn the_minimum_wins_on_a_window_too_narrow_to_honor_the_cap() {
        // A 60%-of-window cap below the readable minimum would otherwise
        // invert the clamp range and panic.
        let narrow = 100.0;
        assert_eq!(file_tree_width(true, 400.0, CELL_W, narrow), FILE_TREE_MIN_COLS * CELL_W);
    }

    #[test]
    fn hit_test_maps_rows_and_ignores_the_title() {
        let rect = file_tree_rect(WINDOW_W, 600.0, CELL_W, CELL_H, true, 0.0).unwrap();
        let row_h = file_tree_row_height(CELL_H);
        let first_row_y = rect.y + file_tree_header_height(CELL_H);

        // The uppercase title band is a label, not a row.
        assert_eq!(file_tree_hit_test(rect, CELL_H, 0, 10, rect.x + 20.0, rect.y + 2.0), None);
        assert_eq!(file_tree_hit_test(rect, CELL_H, 0, 10, rect.x + 20.0, first_row_y + 1.0), Some(0));
        assert_eq!(file_tree_hit_test(rect, CELL_H, 0, 10, rect.x + 20.0, first_row_y + row_h * 2.5), Some(2));
        // Scrolling shifts which row a given pixel belongs to.
        assert_eq!(file_tree_hit_test(rect, CELL_H, 5, 10, rect.x + 20.0, first_row_y + 1.0), Some(5));
        // Past the last row, and outside the sidebar entirely.
        assert_eq!(file_tree_hit_test(rect, CELL_H, 0, 3, rect.x + 20.0, first_row_y + row_h * 8.0), None);
        assert_eq!(file_tree_hit_test(rect, CELL_H, 0, 10, rect.x - 5.0, first_row_y + 1.0), None);
    }

    #[test]
    fn the_grid_gives_up_exactly_the_sidebar_width() {
        let sidebar = file_tree_width(true, 300.0, CELL_W, WINDOW_W);
        let grid = grid_rect(WINDOW_W, 600.0, CELL_H, sidebar);
        let rect = file_tree_rect(WINDOW_W, 600.0, CELL_W, CELL_H, true, 300.0).unwrap();
        assert_eq!(grid.w + rect.w, WINDOW_W, "no overlap and no gap between them");
        assert_eq!(grid.x + grid.w, rect.x);
    }

    #[test]
    fn nothing_is_reserved_above_the_groups() {
        // Regression: the window-wide tab bar moved into the groups, but
        // the layout kept reserving its height at the top, leaving an
        // empty band there -- and pushing every strip below the band the
        // click handler was testing against.
        let grid = grid_rect(WINDOW_W, 600.0, CELL_H, 0.0);
        assert_eq!(grid.y, 0.0);
        assert_eq!(grid.h, 600.0 - status_bar_height(CELL_H));
        let sidebar = file_tree_rect(WINDOW_W, 600.0, CELL_W, CELL_H, true, 0.0).unwrap();
        assert_eq!(sidebar.y, 0.0, "the sidebar starts at the top too");
    }

    #[test]
    fn a_tab_strip_lays_out_and_hit_tests_where_its_group_is() {
        // A group below a horizontal split has its strip partway down the
        // window and offset from x=0; the layout has to follow it there,
        // or clicks land on the wrong tab (or on nothing).
        let titles = vec!["one".to_string(), "two".to_string()];
        let strip = PaneRect { x: 500.0, y: 583.0, w: 400.0, h: tab_bar_height(CELL_H) };
        let layout = tab_bar_layout(&titles, strip, CELL_W);

        assert!(layout.tabs[0].x0 >= strip.x, "the first tab starts inside the strip");
        assert!(layout.new_tab_x1 <= strip.x + strip.w, "the + button stays inside it");

        let first = &layout.tabs[0];
        let second = &layout.tabs[1];
        assert!(matches!(layout.hit_test((first.x0 + first.close_x0) / 2.0), Some(TabBarHit::Switch(0))));
        assert!(matches!(layout.hit_test((second.x0 + second.close_x0) / 2.0), Some(TabBarHit::Switch(1))));
        assert!(matches!(layout.hit_test(second.close_x0 + 1.0), Some(TabBarHit::Close(1))));
        assert!(matches!(layout.hit_test(layout.new_tab_x0 + 1.0), Some(TabBarHit::NewTab)));
        // Left of the strip entirely: not this group's business.
        assert!(layout.hit_test(strip.x - 50.0).is_none());
    }

    #[test]
    fn width_is_measured_in_columns_not_characters() {
        // The bug this guards: wide characters were advanced and budgeted
        // as one column each, so Japanese text drew on top of itself and
        // overflowed whatever it was supposed to fit inside.
        assert_eq!(text_cols("abc"), 3);
        assert_eq!(text_cols("日本語"), 6, "each is two columns wide");
        assert_eq!(text_cols("mixed 混在"), 10);
    }

    #[test]
    fn truncation_budgets_columns_and_marks_what_it_cut() {
        assert_eq!(truncate("short", 10), "short");
        // Six columns of text in a six-column budget: untouched.
        assert_eq!(truncate("日本語", 6), "日本語");
        // One column short, so it has to cut -- and the "..." has to fit
        // in the budget too.
        let cut = truncate("日本語テスト", 8);
        assert!(text_cols(&cut) <= 8, "{cut:?} overflows its budget");
        assert!(cut.ends_with("..."));
        // A budget too small even for the ellipsis still never overflows.
        assert!(text_cols(&truncate("日本語", 3)) <= 3);
        assert!(text_cols(&truncate("日本語", 1)) <= 1);
    }

    #[test]
    fn a_wide_tab_title_stays_inside_its_tab() {
        let titles = vec!["読み書き.txt".to_string(), "bash".to_string()];
        let strip = PaneRect { x: 0.0, y: 0.0, w: 600.0, h: tab_bar_height(CELL_H) };
        let layout = tab_bar_layout(&titles, strip, CELL_W);
        for tab in &layout.tabs {
            let label_width = text_cols(&tab.label) as f32 * CELL_W;
            assert!(
                label_width <= tab.close_x0 - tab.x0,
                "{:?} ({label_width}px) spills past its close button",
                tab.label
            );
        }
    }
}
