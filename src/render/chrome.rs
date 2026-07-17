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
use crate::term::color::Palette;

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

pub fn tab_bar_height(cell_h: f32) -> f32 {
    cell_h * 1.4
}

pub fn status_bar_height(cell_h: f32) -> f32 {
    cell_h * 1.2
}

/// How many terminal rows fit between the two bars at this window height.
pub fn terminal_rows(window_height: f32, cell_h: f32) -> usize {
    let usable = (window_height - tab_bar_height(cell_h) - status_bar_height(cell_h)).max(cell_h);
    ((usable / cell_h).floor() as usize).max(1)
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

    let mut tabs = Vec::with_capacity(titles.len());
    for (i, title) in titles.iter().enumerate() {
        let x0 = (i * tab_cols) as f32 * cell_w;
        let x1 = x0 + tab_cols as f32 * cell_w;
        let close_x0 = x1 - CLOSE_COLS as f32 * cell_w;
        tabs.push(TabRect {
            index: i,
            x0,
            x1,
            close_x0,
            close_x1: x1,
            label: truncate(title, label_cols),
        });
    }

    let new_tab_x0 = (titles.len() * tab_cols) as f32 * cell_w;
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

pub fn build_tab_bar_instances(atlas: &FontAtlas, palette: &Palette, layout: &TabBarLayout, active: usize, bar_width: f32, bar_height: f32, opacity: f32) -> Vec<Instance> {
    let mut instances = Vec::new();

    let backdrop = mix(palette.background, palette.foreground, 0.07);
    let active_bg = palette.background;
    let inactive_bg = mix(palette.background, palette.foreground, 0.14);
    // A bright accent (not just a color/darkness change) is what actually
    // reads as "selected" at a glance -- a background-only difference
    // between tabs is too subtle to register as a real distinction. Kept
    // fully opaque (unlike the backgrounds) so it stays a crisp marker
    // even at low window opacity.
    let accent = mix(palette.ansi[12], palette.background, 0.05);
    let active_fg = palette.foreground;
    let inactive_fg = mix(palette.foreground, palette.background, 0.45);
    let close_fg = mix(palette.foreground, palette.background, 0.6);
    let new_tab_fg = mix(palette.foreground, palette.background, 0.4);

    push_rect(&mut instances, atlas, 0.0, 0.0, bar_width, bar_height, backdrop, opacity, 0.0);

    let text_y = (bar_height - atlas.cell_height) / 2.0;
    let accent_h = (bar_height * 0.08).max(2.0);
    // Rounded-top, flush-bottom shape (Chrome/Arc-style tab): adjacent
    // tabs' rounded shoulders leave a sliver of backdrop showing through
    // between them, which reads as separation on its own -- no divider
    // hairline needed on top of that.
    let tab_radius = (bar_height * 0.35).clamp(4.0, 12.0);

    for tab in &layout.tabs {
        let is_active = tab.index == active;
        let bg = if is_active { active_bg } else { inactive_bg };
        push_rect(&mut instances, atlas, tab.x0, 0.0, tab.x1 - tab.x0, bar_height, bg, opacity, tab_radius);

        let fg = if is_active { active_fg } else { inactive_fg };
        push_text(&mut instances, atlas, &tab.label, tab.x0 + atlas.cell_width * LEFT_PAD_COLS as f32, text_y, fg);
        push_text(&mut instances, atlas, "x", tab.close_x0 + atlas.cell_width * 0.5, text_y, close_fg);

        if is_active {
            push_rect(&mut instances, atlas, tab.x0, bar_height - accent_h, tab.x1 - tab.x0, accent_h, accent, 1.0, 0.0);
        }
    }

    push_text(&mut instances, atlas, "+", layout.new_tab_x0 + atlas.cell_width, text_y, new_tab_fg);

    instances
}

pub fn build_status_bar_instances(atlas: &FontAtlas, palette: &Palette, status: &StatusInfo, window_width: f32, window_height: f32, bar_height: f32, opacity: f32) -> Vec<Instance> {
    let mut instances = Vec::new();
    let y = window_height - bar_height;

    push_rect(&mut instances, atlas, 0.0, y, window_width, bar_height, mix(palette.background, palette.foreground, 0.09), opacity, 0.0);
    // A crisp top edge separates the bar from live terminal content more
    // clearly than a flat background-tint difference alone -- kept fully
    // opaque like the tab's accent underline.
    push_rect(&mut instances, atlas, 0.0, y, window_width, 1.0, mix(palette.background, palette.foreground, 0.26), 1.0, 0.0);

    let sep_color = mix(palette.foreground, palette.background, 0.6);
    let shell_color = mix(palette.foreground, palette.background, 0.35);
    let cwd_color = mix(palette.foreground, palette.background, 0.08);
    // Green reads as "git branch" at a glance in most shell prompts/themes
    // -- reuse that association instead of just dimming the text.
    let branch_color = mix(palette.ansi[10], palette.background, 0.05);
    let tty_color = mix(palette.foreground, palette.background, 0.55);

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

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

fn rgba_to_color((r, g, b): (u8, u8, u8), a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

fn push_rect(instances: &mut Vec<Instance>, atlas: &FontAtlas, x: f32, y: f32, w: f32, h: f32, color: (u8, u8, u8), alpha: f32, top_corner_radius: f32) {
    instances.push(Instance {
        pos: [x, y],
        size: [w, h],
        uv_min: atlas.white_uv,
        uv_max: atlas.white_uv,
        color: rgba_to_color(color, alpha),
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
