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

/// The pixel rectangle between the tab bar and the status bar -- the area
/// panes are laid out in. The single source of truth for that math:
/// rendering, click hit-testing, and pty resizing all start from this.
pub fn grid_rect(window_width: f32, window_height: f32, cell_h: f32) -> PaneRect {
    let top = tab_bar_height(cell_h);
    let bottom = status_bar_height(cell_h);
    PaneRect {
        x: 0.0,
        y: top,
        w: window_width.max(1.0),
        h: (window_height - top - bottom).max(cell_h),
    }
}

/// How many terminal rows fit between the two bars at this window height.
pub fn terminal_rows(window_height: f32, cell_h: f32) -> usize {
    let usable = (window_height - tab_bar_height(cell_h) - status_bar_height(cell_h)).max(cell_h);
    ((usable / cell_h).floor() as usize).max(1)
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

/// Lay tabs out left to right at equal width, computed from how many
/// character columns the window has to spare. Pure/deterministic so it can
/// be called on every click and every redraw without drifting apart.
pub fn tab_bar_layout(titles: &[String], window_width: f32, cell_w: f32) -> TabBarLayout {
    let total_cols = ((window_width / cell_w).floor() as usize).max(1);
    let n = titles.len().max(1);
    let available_for_tabs = total_cols.saturating_sub(NEW_TAB_COLS);
    let tab_cols = (available_for_tabs / n).clamp(MIN_TAB_COLS, MAX_TAB_COLS);
    let label_cols = tab_cols.saturating_sub(LEFT_PAD_COLS + CLOSE_COLS);

    // Breathing room before the first tab, so its rounded corner doesn't
    // sit flush against the window edge. Applied here (not at draw time)
    // so click hit-testing shares the exact same offset.
    let origin = cell_w * 0.5;

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

fn truncate(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return chars.into_iter().take(max_chars).collect();
    }
    let mut s: String = chars.into_iter().take(max_chars - 3).collect();
    s.push_str("...");
    s
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

pub fn build_tab_bar_instances(atlas: &FontAtlas, layout: &TabBarLayout, active: usize, bar_width: f32, bar_height: f32) -> Vec<Instance> {
    let mut instances = Vec::new();

    push_rect(&mut instances, atlas, [0.0, 0.0, bar_width, bar_height], CHROME_BACKDROP, 0.0);

    let text_y = (bar_height - atlas.cell_height) / 2.0;
    let accent_h = (bar_height * 0.08).max(2.0);
    // Rounded-top, flush-bottom shape (Chrome/Arc-style tab): adjacent
    // tabs' rounded shoulders leave a sliver of backdrop showing through
    // between them, which reads as separation on its own -- no divider
    // hairline needed on top of that.
    let tab_radius = (bar_height * 0.35).clamp(4.0, 12.0);

    for tab in &layout.tabs {
        let is_active = tab.index == active;
        let bg = if is_active { CHROME_TAB_ACTIVE } else { CHROME_TAB_INACTIVE };
        push_rect(&mut instances, atlas, [tab.x0, 0.0, tab.x1 - tab.x0, bar_height], bg, tab_radius);

        let fg = if is_active { CHROME_FG_ACTIVE } else { CHROME_FG_INACTIVE };
        push_text(&mut instances, atlas, &tab.label, tab.x0 + atlas.cell_width * LEFT_PAD_COLS as f32, text_y, fg);
        push_text(&mut instances, atlas, "x", tab.close_x0 + atlas.cell_width * 0.5, text_y, CHROME_FG_DIM);

        // A bright accent (not just a background-darkness change) is what
        // actually reads as "selected" at a glance.
        if is_active {
            push_rect(&mut instances, atlas, [tab.x0, bar_height - accent_h, tab.x1 - tab.x0, accent_h], CHROME_ACCENT, 0.0);
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
        let shown_len = shown.chars().count();
        push_text(&mut instances, atlas, &shown, x, text_y, *color);
        x += atlas.cell_width * shown_len as f32;
        used += shown_len;
        if shown_len < text.chars().count() {
            break; // out of room; nothing after this would fit anyway
        }
    }

    instances
}

/// A small floating pill anchored to the top-right of the grid (like a
/// browser's find bar) rather than a full-width bar that would need to
/// resize the grid every time search opens or closes.
pub fn build_search_bar_instances(atlas: &FontAtlas, search: &Search, window_width: f32, tab_bar_bottom: f32) -> Vec<Instance> {
    let mut instances = Vec::new();

    const LABEL: &str = "Find: ";
    let count_text = match search.current_position() {
        Some(pos) => format!("{}/{}", pos, search.match_count()),
        None if search.query.is_empty() => String::new(),
        None => "0/0".to_string(),
    };
    // Reserve room for a reasonably long query so the bar doesn't visibly
    // resize on every keystroke; it still grows past this if needed.
    let query_cols = search.query.chars().count().max(16);
    let content_cols = LABEL.len() + query_cols + 1 + 3 + count_text.chars().count();

    let bar_w = content_cols as f32 * atlas.cell_width;
    let bar_h = atlas.cell_height * 1.6;
    let x0 = (window_width - bar_w - atlas.cell_width).max(0.0);
    let y0 = tab_bar_bottom + atlas.cell_height * 0.3;
    let radius = (bar_h * 0.3).clamp(4.0, 10.0);

    push_rect(&mut instances, atlas, [x0, y0, bar_w, bar_h], CHROME_SEARCH_BG, radius);

    let text_y = y0 + (bar_h - atlas.cell_height) / 2.0;
    let mut x = x0 + atlas.cell_width;
    push_text(&mut instances, atlas, LABEL, x, text_y, CHROME_FG_INACTIVE);
    x += atlas.cell_width * LABEL.len() as f32;

    push_text(&mut instances, atlas, &search.query, x, text_y, CHROME_FG_ACTIVE);
    x += atlas.cell_width * search.query.chars().count() as f32;
    // A plain caret, not a blinking one -- there's no per-frame ticking
    // clock driving redraws (this app only redraws on real events), so an
    // animated blink would freeze mid-phase as often as not.
    push_text(&mut instances, atlas, "|", x, text_y, CHROME_FG_ACTIVE);

    if !count_text.is_empty() {
        let count_x = x0 + bar_w - atlas.cell_width * (count_text.chars().count() + 1) as f32;
        let count_color = if search.match_count() == 0 { CHROME_SEARCH_NO_MATCH } else { CHROME_FG_INACTIVE };
        push_text(&mut instances, atlas, &count_text, count_x, text_y, count_color);
    }

    instances
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
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
        x += atlas.cell_width;
    }
}
