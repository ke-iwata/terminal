mod config;
mod input;
mod linkify;
mod menu;
mod pty;
mod render;
mod settings_ui;
mod status;
mod tab;
mod term;

use config::Config;
use settings_ui::{SettingsAction, SettingsWindow};
use tab::Tab;
use term::color::Palette;

use nix::sys::signal::{kill, Signal};

use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::macos::EventLoopBuilderExtMacOS;
use winit::window::{Window, WindowId};

use render::chrome;
use render::Renderer;

/// How often the status bar's process/cwd/git lookups are allowed to
/// re-run. Those calls touch `sysinfo` and the filesystem, so redoing them
/// on every keystroke-triggered redraw would be wasteful; this bounds the
/// cost to a few times a second regardless of typing speed.
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_millis(300);

/// Minimum gap between real `TIOCSWINSZ`/`SIGWINCH` deliveries to the same
/// pane while its size is changing continuously (a divider drag). See
/// `App::relayout_all_tabs`.
const PTY_RESIZE_THROTTLE: Duration = Duration::from_millis(50);

enum UserEvent {
    /// Bytes read from a pane's pty, tagged with that pane's id and the
    /// generation of the shell session that produced them (see
    /// `Pane::pty_generation`).
    PtyData(u64, u64, Vec<u8>),
    /// A pty reader thread hit EOF/error, tagged with its pane id and
    /// generation.
    PtyExited(u64, u64),
    OpenSettings,
    ReloadConfig,
    NewTab,
    /// Close the focused pane -- or the whole tab when it's the only pane.
    ClosePane,
    NextTab,
    PrevTab,
    SplitRight,
    SplitDown,
    NextPane,
    PrevPane,
}

struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    /// Pane ids are drawn from their own counter (not the tab counter) and
    /// are unique across every tab -- pty reader events are tagged with
    /// them, and events from a pane closed in one tab must never be
    /// mistaken for a pane in another.
    next_pane_id: u64,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    settings_window: Option<SettingsWindow>,
    proc_info: status::ProcInfo,
    cached_status: chrome::StatusInfo,
    last_status_refresh: Option<Instant>,
    cursor_pos: (f32, f32),
    /// Whether at least one frame has actually reached the screen.
    ///
    /// REGRESSION GUARD -- the "window stays blank until the first
    /// keypress" startup bug. Do not simplify the first-frame machinery
    /// without re-testing cold launches many times; the bug is timing
    /// dependent and only shows up on some launches.
    ///
    /// Why it happens: this app uses `ControlFlow::Wait`, so nothing
    /// draws unless an event asks for it. During the first ~100ms of a
    /// window's life on macOS, both of the triggers we rely on can be
    /// silently dropped:
    ///   1. `request_redraw()` calls made before the window is actually
    ///      visible may never produce a `RedrawRequested` event.
    ///   2. Even a delivered `RedrawRequested` can fail inside
    ///      `Renderer::render` -- the Metal layer may not hand out a
    ///      drawable yet (`Timeout`/`Outdated`/`Lost`).
    ///
    /// If the shell's first prompt output happens to arrive inside
    /// that window (it usually does -- bash starts in tens of ms), its
    /// redraw request is lost with it, and with no further events the
    /// screen stays blank until the user presses a key.
    ///
    /// The fix is layered; all three parts matter:
    ///   - `about_to_wait` keeps re-requesting redraws on a short
    ///     `WaitUntil` timer for as long as this flag is false, so the
    ///     first frame does not depend on any external event arriving.
    ///     Once a frame has been presented, control flow reverts to
    ///     plain `Wait` (zero idle wakeups).
    ///   - `RedrawRequested` retries when `render` reports
    ///     `RenderOutcome::Retry` (transient surface failure).
    ///   - `WindowEvent::Occluded(false)` requests a redraw, since a
    ///     frame skipped while occluded is otherwise never re-drawn.
    presented_once: bool,
    /// The pane a left-button drag is currently selecting text in, from
    /// press to release. Kept explicitly (rather than re-hit-testing on
    /// every cursor move) so a drag that wanders across a divider keeps
    /// extending the selection it started in, clamped to its own pane.
    dragging_pane: Option<u64>,
    /// The divider a left-button drag is currently resizing, from press
    /// to release. Mutually exclusive with `dragging_pane` -- a press
    /// starts one or the other, never both.
    dragging_divider: Option<tab::DividerInfo>,
}

impl App {
    fn new(config: Config, first_tab: Tab, proxy: EventLoopProxy<UserEvent>) -> Self {
        App {
            config,
            window: None,
            renderer: None,
            next_tab_id: first_tab.id + 1,
            next_pane_id: first_tab.focused + 1,
            tabs: vec![first_tab],
            active: 0,
            proxy,
            modifiers: ModifiersState::empty(),
            settings_window: None,
            proc_info: status::ProcInfo::new(),
            cached_status: chrome::StatusInfo { shell: String::new(), cwd: String::new(), branch: None, tty: String::new() },
            last_status_refresh: None,
            cursor_pos: (0.0, 0.0),
            presented_once: false,
            dragging_pane: None,
            dragging_divider: None,
        }
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Apply a config (just saved from the settings window, or reloaded
    /// from disk via the menu) so every field takes effect right away:
    /// colors and scrollback are cheap in-place updates on every open tab,
    /// and a changed font rebuilds the glyph atlas and re-fits every tab's
    /// grid to the window. A changed shell is deliberately *not* applied
    /// to tabs that are already running -- only tabs opened from now on
    /// pick up the new `shell` (see `open_tab`), since restarting every
    /// open session out from under the user on a config save would be far
    /// more destructive than useful.
    fn apply_config(&mut self, config: Config) {
        let palette = Palette::from(&config.colors);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_palette(palette);
            renderer.set_opacity(config.opacity);
        }
        for tab in &mut self.tabs {
            for pane in tab.root_mut().panes_mut() {
                pane.term.set_scrollback_limit(config.scrollback_lines);
            }
        }

        let font_changed = config.font != self.config.font;
        self.config = config;

        if font_changed {
            self.apply_font_change();
        }

        // Keep an open settings window's form in sync, so it doesn't show
        // stale values after a reload.
        if let Some(settings) = &mut self.settings_window {
            settings.reset_draft(&self.config);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Rebuild the glyph atlas for `self.config.font` and re-fit every
    /// tab's panes to the window at the new cell size.
    fn apply_font_change(&mut self) {
        let Some(scale_factor) = self.window.as_ref().map(|w| w.scale_factor()) else {
            return;
        };
        if let Some(renderer) = &mut self.renderer {
            renderer.set_font(&self.config.font, scale_factor);
        }
        self.relayout_all_tabs(true);
    }

    /// The full grid area's cols/rows for the current window size --
    /// what a tab with a single (unsplit) pane gets. `None` before the
    /// window/renderer exist.
    fn grid_size(&self) -> Option<(usize, usize)> {
        let window = self.window.as_ref()?;
        let renderer = self.renderer.as_ref()?;
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let cols = ((size.width as f32 / cell_w).floor() as usize).max(1);
        let rows = chrome::terminal_rows(size.height as f32, cell_h);
        Some((cols, rows))
    }

    /// Recompute every pane's rectangle from the current window size and
    /// split tree, and push each pane's new cols/rows to its Term/Grid
    /// model (so rendering is correct on every call) and, at most once
    /// per `PTY_RESIZE_THROTTLE` unless `force` is set, to its pty (so the
    /// shell's SIGWINCH-driven reflow, e.g. `stty size`, matches). Runs
    /// over every tab, not just the active one -- background tabs keep
    /// running and must have already reflowed correctly by the time
    /// they're switched to.
    ///
    /// The throttle exists for divider dragging: `update_divider_drag`
    /// calls this on every `CursorMoved`, and a real terminal size change
    /// signals the pty's foreground process with `SIGWINCH` every time.
    /// Shells with a line editor that redraws on `SIGWINCH` (zsh's zle,
    /// the macOS default) redisplay the prompt each time they receive
    /// one -- signaled faster than they can redisplay, that reads as
    /// garbled, duplicated-looking prompt spam during a fast drag. Callers
    /// that aren't a live drag (window resize, font change, tab/pane
    /// creation) pass `force: true` so the pty is always in sync
    /// immediately; the drag itself force-flushes once more when it ends
    /// (see the `MouseInput` `Released` handler) so the shell never stays
    /// out of sync with the final size.
    fn relayout_all_tabs(&mut self, force: bool) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let grid = chrome::grid_rect(size.width as f32, size.height as f32, cell_h);
        let now = Instant::now();
        for tab in &mut self.tabs {
            let (rects, _) = tab.layout(grid, chrome::PANE_GAP);
            for (pane_id, rect) in rects {
                let pane = tab.root_mut().pane_mut(pane_id).expect("layout only yields live panes");
                let cols = ((rect.w / cell_w).floor() as usize).max(1);
                let rows = ((rect.h / cell_h).floor() as usize).max(1);
                if cols != pane.term.cols() || rows != pane.term.rows() {
                    pane.term.resize(cols, rows);
                }
                let target = (cols as u16, rows as u16);
                if target != pane.pty_size {
                    let due = force || pane.last_pty_resize_sent.is_none_or(|t| now.duration_since(t) >= PTY_RESIZE_THROTTLE);
                    if due {
                        pty::resize(pane.pty_master.as_fd(), target.0, target.1);
                        pane.pty_size = target;
                        pane.last_pty_resize_sent = Some(now);
                    }
                }
            }
        }
    }

    /// Spawn a fresh tab running `self.config.shell`, make it active, and
    /// start reading its pty.
    fn open_tab(&mut self) {
        let (cols, rows) = self.grid_size().unwrap_or((80, 24));
        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let pane = tab::Pane::spawn(pane_id, &self.config.shell, cols, rows, self.config.scrollback_lines);
        self.spawn_pty_reader(&pane);
        self.tabs.push(Tab::new(tab_id, pane));
        self.active = self.tabs.len() - 1;
        self.last_status_refresh = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Split the active tab's focused pane, putting a fresh shell in the
    /// new half and focusing it.
    fn split_focused_pane(&mut self, direction: tab::SplitDirection) {
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        // Spawned at a placeholder size; `relayout_all_tabs` below sizes
        // every pane (this one included) to its real rectangle.
        let pane = tab::Pane::spawn(pane_id, &self.config.shell, 80, 24, self.config.scrollback_lines);
        self.spawn_pty_reader(&pane);
        self.active_tab_mut().split_focused(direction, pane);
        self.relayout_all_tabs(true);
        self.last_status_refresh = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Close the focused pane: its split collapses into the sibling. When
    /// it's the tab's only pane, this is just closing the tab.
    fn close_focused_pane(&mut self, event_loop: &ActiveEventLoop) {
        let tab = self.active_tab_mut();
        if tab.pane_count() <= 1 {
            let id = tab.id;
            self.close_tab(id, event_loop);
            return;
        }
        let focused = tab.focused;
        if let Some(pane) = tab.remove_pane(focused) {
            let _ = kill(pane.pty_child, Signal::SIGHUP);
            let _ = nix::sys::wait::waitpid(pane.pty_child, None);
        }
        self.relayout_all_tabs(true);
        self.last_status_refresh = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// End every shell in the tab (SIGHUP, same signal a real terminal
    /// sends its shell on close) and remove it.
    fn close_tab(&mut self, id: u64, event_loop: &ActiveEventLoop) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        for pane in self.tabs[index].root().panes() {
            let _ = kill(pane.pty_child, Signal::SIGHUP);
            let _ = nix::sys::wait::waitpid(pane.pty_child, None);
        }
        self.remove_tab(index, event_loop);
    }

    /// Drop tab `index` from `self.tabs` and reassign `self.active` so it
    /// keeps pointing at a sensible neighbor, or quit the app if that was
    /// the last tab -- matching today's single-session "shell exits ->
    /// app exits" behavior. Assumes the tab's shell process has already
    /// been signaled/reaped by the caller (`close_tab`, or `PtyExited` for
    /// a shell that exited on its own).
    fn remove_tab(&mut self, index: usize, event_loop: &ActiveEventLoop) {
        if self.tabs.len() == 1 {
            event_loop.exit();
            return;
        }
        self.tabs.remove(index);
        let new_len = self.tabs.len();
        self.active = if self.active > index { self.active - 1 } else { self.active.min(new_len - 1) };
        self.last_status_refresh = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Hit-test a left click against the tab strip using the exact same
    /// `chrome::tab_bar_layout` the renderer draws it with, so a click
    /// always lands on whatever's visually under the cursor.
    fn handle_tab_bar_click(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_size();
        if self.cursor_pos.1 >= chrome::tab_bar_height(cell_h) {
            return;
        }
        let window_width = window.inner_size().width as f32;
        let titles: Vec<String> = self.tabs.iter().map(|t| t.title().to_string()).collect();
        let layout = chrome::tab_bar_layout(&titles, window_width, cell_w);

        match layout.hit_test(self.cursor_pos.0) {
            Some(chrome::TabBarHit::Switch(index)) => {
                self.active = index;
                self.last_status_refresh = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Some(chrome::TabBarHit::Close(index)) => {
                let id = self.tabs[index].id;
                self.close_tab(id, event_loop);
            }
            Some(chrome::TabBarHit::NewTab) => self.open_tab(),
            None => {}
        }
    }

    /// The active tab's pane rectangles at the current window size --
    /// computed on demand from the same pure layout the renderer uses, so
    /// clicks always land on what's visually under the cursor.
    fn pane_rects(&self) -> Vec<(u64, tab::PaneRect)> {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return Vec::new();
        };
        let size = window.inner_size();
        let grid = chrome::grid_rect(size.width as f32, size.height as f32, renderer.cell_size().1);
        self.active_tab().layout(grid, chrome::PANE_GAP).0
    }

    /// Which of the active tab's panes is under the window-pixel position,
    /// if any (dividers and the bars hit nothing).
    fn pane_at(&self, x: f32, y: f32) -> Option<u64> {
        self.pane_rects().into_iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| id)
    }

    /// The divider under the window-pixel position, if any. The visible
    /// gap is only `PANE_GAP` (2px) wide -- too thin a target to actually
    /// grab -- so the hit zone is padded a few pixels to either side,
    /// like every app with draggable splitters does. Pane content still
    /// wins clicks beyond the padded zone.
    fn divider_at(&self, x: f32, y: f32) -> Option<tab::DividerInfo> {
        const GRAB: f32 = 3.0;
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return None;
        };
        let size = window.inner_size();
        let grid = chrome::grid_rect(size.width as f32, size.height as f32, renderer.cell_size().1);
        let (_, dividers) = self.active_tab().layout(grid, chrome::PANE_GAP);
        dividers.into_iter().find(|d| {
            let r = d.rect;
            let padded = tab::PaneRect { x: r.x - GRAB, y: r.y - GRAB, w: r.w + GRAB * 2.0, h: r.h + GRAB * 2.0 };
            padded.contains(x, y)
        })
    }

    /// Move the divider currently being dragged to the cursor position:
    /// recompute its split's ratio from where the cursor sits inside the
    /// split's region, clamped so neither side collapses below about two
    /// cells, and re-fit every affected pane immediately (live resize).
    fn update_divider_drag(&mut self) {
        let Some(divider) = self.dragging_divider.clone() else {
            return;
        };
        let Some((cell_w, cell_h)) = self.renderer.as_ref().map(Renderer::cell_size) else {
            return;
        };
        let (pos, start, extent, min_px) = match divider.direction {
            tab::SplitDirection::Vertical => (self.cursor_pos.0, divider.region.x, divider.region.w - chrome::PANE_GAP, cell_w * 2.0),
            tab::SplitDirection::Horizontal => (self.cursor_pos.1, divider.region.y, divider.region.h - chrome::PANE_GAP, cell_h * 2.0),
        };
        if extent <= min_px * 2.0 {
            return; // region too small to meaningfully resize
        }
        let ratio = ((pos - start) / extent).clamp(min_px / extent, 1.0 - min_px / extent);
        self.active_tab_mut().set_split_ratio(&divider.path, ratio);
        self.relayout_all_tabs(false);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Show a resize cursor while hovering (or dragging) a divider, and
    /// the plain arrow everywhere else.
    fn update_cursor_icon(&self) {
        use winit::window::CursorIcon;
        let Some(window) = &self.window else { return };
        let direction = self
            .dragging_divider
            .as_ref()
            .map(|d| d.direction)
            .or_else(|| self.divider_at(self.cursor_pos.0, self.cursor_pos.1).map(|d| d.direction));
        let icon = match direction {
            Some(tab::SplitDirection::Vertical) => CursorIcon::ColResize,
            Some(tab::SplitDirection::Horizontal) => CursorIcon::RowResize,
            None => CursorIcon::Default,
        };
        window.set_cursor(icon);
    }

    /// Maps a window-pixel position to a grid cell of pane `pane_id`,
    /// clamped into that pane's bounds -- a drag that wanders outside the
    /// pane still selects to its nearest real cell instead of stopping
    /// dead or jumping to a neighboring pane.
    fn grid_point_in_pane(&self, pane_id: u64, x: f32, y: f32) -> Option<tab::GridPoint> {
        let renderer = self.renderer.as_ref()?;
        let (cell_w, cell_h) = renderer.cell_size();
        let (_, rect) = self.pane_rects().into_iter().find(|(id, _)| *id == pane_id)?;
        let pane = self.active_tab().root().pane(pane_id)?;
        let grid = pane.term.grid();
        let col = (((x - rect.x) / cell_w).floor().max(0.0) as usize).min(grid.cols.saturating_sub(1));
        let view_row = (((y - rect.y) / cell_h).floor().max(0.0) as usize).min(grid.rows.saturating_sub(1));
        let distance = grid.distance_from_bottom(view_row, pane.scroll_offset);
        Some(tab::GridPoint { distance, col })
    }

    /// If the current cursor position lands on a URL (see
    /// `linkify::find_urls`), opens it with the system's default handler
    /// and returns `true`. Only ever called with Cmd held -- `false`
    /// means the caller should fall back to its normal click handling.
    fn open_url_under_cursor(&mut self) -> bool {
        let Some(pane_id) = self.pane_at(self.cursor_pos.0, self.cursor_pos.1) else {
            return false;
        };
        let Some(tab::GridPoint { distance, col }) = self.grid_point_in_pane(pane_id, self.cursor_pos.0, self.cursor_pos.1) else {
            return false;
        };
        let Some(pane) = self.active_tab().root().pane(pane_id) else {
            return false;
        };
        let Some(row) = pane.term.grid().absolute_line(distance) else {
            return false;
        };
        let text: String = row.iter().map(|c| c.c).collect();
        let Some((start, end)) = linkify::find_urls(&text).into_iter().find(|(s, e)| col >= *s && col <= *e) else {
            return false;
        };
        let url: String = text.chars().skip(start).take(end - start + 1).collect();
        // `open` resolves the same way as double-clicking the link in
        // Finder would -- default browser for http(s), no shell involved
        // (the URL is one argv entry, not interpolated into a command
        // string), so there's no injection risk from clicking on
        // adversarial terminal output.
        let _ = std::process::Command::new("open").arg(&url).spawn();
        true
    }

    /// Start a new text selection at the current cursor position (also
    /// focusing the pane under it), replacing whatever was selected in
    /// that pane before. No-op outside any pane (bars, dividers).
    fn begin_selection(&mut self) {
        let Some(pane_id) = self.pane_at(self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        // A click anywhere in a pane focuses it, selection or not --
        // that's the entire mouse story for pane focus.
        if self.active_tab().focused != pane_id {
            self.active_tab_mut().focused = pane_id;
            self.last_status_refresh = None;
        }
        let Some(point) = self.grid_point_in_pane(pane_id, self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        self.dragging_pane = Some(pane_id);
        if let Some(pane) = self.active_tab_mut().root_mut().pane_mut(pane_id) {
            pane.selection = Some(tab::Selection { anchor: point, cursor: point });
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Extend the in-progress selection to the current cursor position.
    fn update_selection(&mut self) {
        let Some(pane_id) = self.dragging_pane else {
            return;
        };
        let Some(point) = self.grid_point_in_pane(pane_id, self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        if let Some(pane) = self.active_tab_mut().root_mut().pane_mut(pane_id) {
            if let Some(selection) = &mut pane.selection {
                selection.cursor = point;
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Finish a drag begun by `begin_selection`. A press-and-release with
    /// no movement in between is a plain click, not a selection -- clear
    /// it rather than leaving a zero-width one that would otherwise just
    /// sit there uncopiable and unclearable by any other click.
    fn end_selection(&mut self) {
        let Some(pane_id) = self.dragging_pane.take() else {
            return;
        };
        if let Some(pane) = self.active_tab_mut().root_mut().pane_mut(pane_id) {
            if pane.selection.is_some_and(|s| s.anchor == s.cursor) {
                pane.selection = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    /// Opens the focused pane's search bar if it isn't already open. A
    /// second Cmd+F while one's already open is a no-op -- keeps whatever
    /// query was typed rather than clearing it, since there's no reason a
    /// repeated Cmd+F should throw away progress.
    fn open_search(&mut self) {
        let pane = self.active_tab_mut().focused_pane_mut();
        if pane.search.is_none() {
            pane.search = Some(tab::Search::new());
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn close_search(&mut self) {
        self.active_tab_mut().focused_pane_mut().search = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Routes one key event to the open search bar: text edits the query
    /// (re-running the search after every change), Enter/Shift+Enter step
    /// through results, Escape closes it. Anything else is swallowed --
    /// while search is open nothing should reach the pty.
    fn handle_search_key(&mut self, event: &winit::event::KeyEvent) {
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.close_search(),
            Key::Named(NamedKey::Enter) => {
                if self.modifiers.shift_key() {
                    self.step_search(false);
                } else {
                    self.step_search(true);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(search) = &mut self.active_tab_mut().focused_pane_mut().search {
                    search.query.pop();
                }
                self.recompute_search();
            }
            _ => {
                if let Some(text) = event.text.as_deref() {
                    // Filters out the control characters winit still
                    // reports `text` for for some named keys (e.g. Tab)
                    // -- only append genuinely printable input.
                    if !text.is_empty() && text.chars().all(|c| !c.is_control()) {
                        if let Some(search) = &mut self.active_tab_mut().focused_pane_mut().search {
                            search.query.push_str(text);
                        }
                        self.recompute_search();
                    }
                }
            }
        }
    }

    /// Re-runs the focused pane's search after its query changed and jumps
    /// the view to the (new) first match.
    fn recompute_search(&mut self) {
        let pane = self.active_tab_mut().focused_pane_mut();
        let grid = pane.term.grid();
        if let Some(search) = &mut pane.search {
            search.recompute(grid);
        }
        self.jump_to_search_match();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn step_search(&mut self, forward: bool) {
        let pane = self.active_tab_mut().focused_pane_mut();
        let Some(search) = &mut pane.search else { return };
        if forward {
            search.go_next();
        } else {
            search.go_prev();
        }
        self.jump_to_search_match();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Scrolls the focused pane so its search's current match is roughly
    /// centered in the viewport. No-op if there's no open search or no
    /// current match (an empty query, or one with no hits).
    fn jump_to_search_match(&mut self) {
        let pane = self.active_tab_mut().focused_pane_mut();
        let Some(search) = &pane.search else { return };
        let Some((distance, _)) = search.current_target() else { return };
        let rows = pane.term.rows();
        let max_offset = pane.term.grid().scrollback.len();
        pane.scroll_offset = distance.saturating_sub(rows / 2).min(max_offset);
    }

    /// Spawn the thread that blocking-reads `pane`'s pty and forwards
    /// bytes to the event loop, tagged with `pane`'s id and generation so
    /// `user_event` can route them (and can tell a since-closed pane's
    /// trailing events apart from a live one's).
    fn spawn_pty_reader(&self, pane: &tab::Pane) {
        let reader_master = Arc::clone(&pane.pty_master);
        let proxy = self.proxy.clone();
        let pane_id = pane.id;
        let generation = pane.pty_generation;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match nix::unistd::read(reader_master.as_fd(), &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if proxy
                            .send_event(UserEvent::PtyData(pane_id, generation, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = proxy.send_event(UserEvent::PtyExited(pane_id, generation));
        });
    }

    /// Recompute the status bar text and the focused pane's title from the
    /// live shell state (foreground process, cwd, git branch), but no more
    /// often than `STATUS_REFRESH_INTERVAL` -- these calls touch `sysinfo`
    /// and the filesystem, so redoing them on every keystroke-triggered
    /// redraw would be wasteful. Background tabs/panes keep whatever title
    /// they last had until they're focused again.
    fn refresh_status(&mut self) {
        let due = self.last_status_refresh.is_none_or(|t| t.elapsed() >= STATUS_REFRESH_INTERVAL);
        if !due {
            return;
        }
        self.last_status_refresh = Some(Instant::now());

        let pane = self.tabs[self.active].focused_pane_mut();
        let master = pane.pty_master.as_fd();
        let (fg_name, cwd) = match self.proc_info.foreground_process_name(master) {
            // The shell itself sitting at its prompt: use the name we
            // derived from the configured shell path at spawn time rather
            // than whatever sysinfo reports for the pid. Right after a
            // pane opens, that pid can still be the pre-exec fork of this
            // binary (named "terminal"), and losing that race used to
            // mistitle the tab -- the shell's own name is a fact we
            // already know, so never ask the process table for it.
            Some((pid, _)) if pid == pane.pty_child => (pane.shell_name.clone(), self.proc_info.process_cwd(pid)),
            Some((pid, name)) => (name, self.proc_info.process_cwd(pid)),
            None => (pane.shell_name.clone(), self.proc_info.process_cwd(pane.pty_child)),
        };
        pane.title = fg_name;

        let cwd_display = cwd.as_deref().map(display_path).unwrap_or_default();
        let branch = cwd.as_deref().and_then(status::git_branch);

        self.cached_status = chrome::StatusInfo {
            shell: pane.shell_name.clone(),
            cwd: cwd_display,
            branch,
            tty: pane.tty_name.clone(),
        };
    }
}

/// Abbreviate `path` with `~` for display in the status bar, if it's under
/// the user's home directory.
fn display_path(path: &std::path::Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

/// Write `text` to the system clipboard via `pbcopy`, macOS's own clipboard
/// CLI -- simplest possible route to `NSPasteboard` without adding a
/// clipboard crate as a dependency for what's otherwise a one-line job.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return;
    };
    // `.take()` so the `ChildStdin` (and the pipe's write end with it) is
    // dropped once we're done writing -- `wait()` would otherwise block
    // forever, since `pbcopy` doesn't see EOF until that happens.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

/// The system clipboard's text contents via `pbpaste`, the read-side
/// counterpart to `copy_to_clipboard`.
fn paste_from_clipboard() -> Option<String> {
    let output = std::process::Command::new("pbpaste").output().ok()?;
    String::from_utf8(output.stdout).ok()
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Always created transparent, regardless of the configured
        // opacity: an opaque window can't become see-through later
        // without recreating it, while a "transparent" window whose
        // pixels all happen to have alpha=1 (opacity's default) looks
        // identical to a normal opaque one. This lets opacity change live
        // from Preferences instead of requiring a restart.
        let attrs = Window::default_attributes().with_title("keterm").with_transparent(true);
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        let palette = Palette::from(&self.config.colors);
        let renderer = Renderer::new(window.clone(), &self.config.font, palette, self.config.opacity);

        self.window = Some(window);
        self.renderer = Some(renderer);

        // The first pane was constructed in `main()` at a placeholder
        // size (before the window/renderer existed to know the real one)
        // -- fit it to the actual window now.
        self.relayout_all_tabs(true);

        self.window.as_ref().unwrap().request_redraw();

        // Only start reading the pty now that the pane's Term is correctly
        // sized: the shell starts producing output the moment it's forked
        // (in `main`, before the event loop even runs), and any bytes read
        // before this point would be silently dropped by `user_event`'s
        // `PtyData` handler -- which used to lose the shell's very first
        // prompt if it arrived before this point, showing nothing until
        // the next keypress produced fresh output. The pty's kernel-side
        // buffer holds onto that early output until we're ready to read
        // it, so nothing is lost by waiting.
        self.spawn_pty_reader(self.tabs[0].root().panes()[0]);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(pane_id, generation, bytes) => {
                let Some(pane) = self.tabs.iter_mut().find_map(|t| t.root_mut().pane_mut(pane_id)) else {
                    return; // pane already closed
                };
                // Ignore output from a shell session that's since been
                // replaced -- its reader thread can still have bytes in
                // flight for a moment after that.
                if generation != pane.pty_generation {
                    return;
                }
                pane.term.advance(&bytes);
                pane.scroll_offset = 0;
                // The content a selection pointed at may have just
                // scrolled, changed, or stopped existing -- see the
                // field doc on `Pane::selection`.
                pane.selection = None;
                // Unlike selection, a search stays open across new
                // output -- just refreshed against it (see the field doc
                // on `Pane::search`) rather than cleared. Doesn't jump the
                // view to the current match here: new output already
                // snaps the view to the live bottom via `scroll_offset =
                // 0` above, and fighting that would be more surprising
                // than just leaving the match list/count up to date.
                if let Some(search) = &mut pane.search {
                    search.recompute(pane.term.grid());
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::PtyExited(pane_id, generation) => {
                let Some(tab_index) = self.tabs.iter().position(|t| t.root().pane(pane_id).is_some()) else {
                    return; // pane already closed
                };
                let tab = &mut self.tabs[tab_index];
                let pane = tab.root().pane(pane_id).expect("position() just found it");
                if generation != pane.pty_generation {
                    return;
                }
                let _ = nix::sys::wait::waitpid(pane.pty_child, None);
                if tab.pane_count() > 1 {
                    // A shell in one pane of a split exited: collapse just
                    // that pane, exactly like Cmd+W on it.
                    let _ = tab.remove_pane(pane_id);
                    self.relayout_all_tabs(true);
                    self.last_status_refresh = None;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                } else {
                    self.remove_tab(tab_index, event_loop);
                }
            }
            UserEvent::OpenSettings => {
                if let Some(settings) = &self.settings_window {
                    settings.request_redraw();
                } else {
                    self.settings_window = Some(SettingsWindow::new(event_loop, &self.config));
                }
            }
            UserEvent::ReloadConfig => {
                self.apply_config(Config::load());
            }
            UserEvent::NewTab => self.open_tab(),
            UserEvent::ClosePane => self.close_focused_pane(event_loop),
            UserEvent::NextTab => {
                self.active = (self.active + 1) % self.tabs.len();
                self.last_status_refresh = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::PrevTab => {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
                self.last_status_refresh = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::SplitRight => self.split_focused_pane(tab::SplitDirection::Vertical),
            UserEvent::SplitDown => self.split_focused_pane(tab::SplitDirection::Horizontal),
            UserEvent::NextPane | UserEvent::PrevPane => {
                let forward = matches!(event, UserEvent::NextPane);
                self.active_tab_mut().cycle_focus(forward);
                self.last_status_refresh = None;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if let Some(settings) = &mut self.settings_window {
            if window_id == settings.window_id() {
                match settings.on_window_event(&event) {
                    SettingsAction::None => {}
                    SettingsAction::Saved(config) => self.apply_config(config),
                    SettingsAction::Close => self.settings_window = None,
                }
                return;
            }
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(new_size);
                }
                self.relayout_all_tabs(true);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                // Cmd held/released toggles URL underlines in the grid --
                // redraw right away instead of waiting for an unrelated
                // event to happen to show/hide them.
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic || !event.state.is_pressed() {
                    return;
                }
                // Cmd+F always opens/keeps-open the search bar, checked
                // before anything else so it works the same whether or
                // not a search is already in progress.
                if self.modifiers.super_key() {
                    if let winit::keyboard::Key::Character(c) = &event.logical_key {
                        if c.eq_ignore_ascii_case("f") {
                            self.open_search();
                            return;
                        }
                    }
                }
                // While the search bar is open it owns the keyboard --
                // every key edits or navigates the query instead of
                // reaching the pty, and none of it falls through past
                // this block.
                if self.active_tab().focused_pane().search.is_some() {
                    self.handle_search_key(&event);
                    return;
                }
                // Cmd+C/Cmd+V: copy/paste rather than passing the
                // keystroke through. Ctrl+C (SIGINT) is a separate combo
                // on macOS and isn't affected. On a plain click (no
                // selection), Cmd+C intentionally does nothing rather
                // than falling through to the pty -- winit doesn't
                // report `text` for Cmd-held key events on macOS anyway,
                // so this matches what already silently happened before
                // selection existed.
                if self.modifiers.super_key() {
                    if let winit::keyboard::Key::Character(c) = &event.logical_key {
                        if c.eq_ignore_ascii_case("c") {
                            if let Some(text) = self.active_tab().focused_pane().selected_text() {
                                copy_to_clipboard(&text);
                            }
                            return;
                        }
                        if c.eq_ignore_ascii_case("v") {
                            if let Some(text) = paste_from_clipboard() {
                                let pane = self.active_tab().focused_pane();
                                let _ = nix::unistd::write(pane.pty_master.as_fd(), text.as_bytes());
                                self.active_tab_mut().focused_pane_mut().scroll_offset = 0;
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            return;
                        }
                    }
                }
                let pane = self.active_tab().focused_pane();
                let bytes = input::encode_key(
                    &event.logical_key,
                    event.text.as_deref(),
                    event.state.is_pressed(),
                    self.modifiers,
                    &pane.term.modes,
                );
                if let Some(bytes) = bytes {
                    let _ = nix::unistd::write(pane.pty_master.as_fd(), &bytes);
                    self.active_tab_mut().focused_pane_mut().scroll_offset = 0;
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (_, cell_h) = self
                    .renderer
                    .as_ref()
                    .map(Renderer::cell_size)
                    .unwrap_or((1.0, 1.0));
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y as f32) / cell_h,
                };
                // Scroll whatever pane is under the mouse (not the focused
                // one) -- matching how iTerm2/macOS scroll views behave.
                let pane_id = self
                    .pane_at(self.cursor_pos.0, self.cursor_pos.1)
                    .unwrap_or(self.active_tab().focused);
                let Some(pane) = self.active_tab_mut().root_mut().pane_mut(pane_id) else {
                    return;
                };
                if pane.term.using_alt_screen() {
                    return;
                }
                let max_offset = pane.term.grid().scrollback.len();
                if lines > 0.0 {
                    pane.scroll_offset = (pane.scroll_offset + lines.ceil() as usize).min(max_offset);
                } else if lines < 0.0 {
                    pane.scroll_offset = pane.scroll_offset.saturating_sub((-lines).ceil() as usize);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
                self.update_divider_drag();
                self.update_selection();
                self.update_cursor_icon();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let Some(cell_h) = self.renderer.as_ref().map(|r| r.cell_size().1) else {
                    return;
                };
                if self.cursor_pos.1 < chrome::tab_bar_height(cell_h) {
                    self.handle_tab_bar_click(event_loop);
                } else if let Some(divider) = self.divider_at(self.cursor_pos.0, self.cursor_pos.1) {
                    self.dragging_divider = Some(divider);
                } else if self.modifiers.super_key() && self.open_url_under_cursor() {
                    // Cmd+click on a link opens it instead of starting a
                    // selection -- Cmd+drag was never a gesture to begin
                    // with, so there's nothing to preserve by falling
                    // through when the click isn't on a link either.
                } else {
                    self.begin_selection();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                if self.dragging_divider.take().is_some() {
                    // Force-flush: the drag may have throttled the pty out
                    // of sync with the final size.
                    self.relayout_all_tabs(true);
                }
                self.end_selection();
            }
            // A frame skipped while occluded (see `RenderOutcome::Skipped`)
            // is never retried on its own -- redraw as soon as the window
            // becomes visible again. Part of the first-frame regression
            // guard documented on `App::presented_once`.
            WindowEvent::Occluded(false) => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.refresh_status();
                let cmd_held = self.modifiers.super_key();
                let outcome = self
                    .renderer
                    .as_mut()
                    .map(|renderer| renderer.render(&self.tabs, self.active, &self.cached_status, cmd_held));
                match outcome {
                    Some(render::RenderOutcome::Presented) => self.presented_once = true,
                    Some(render::RenderOutcome::Retry) => {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                    Some(render::RenderOutcome::Skipped) | None => {}
                }
            }
            _ => {}
        }
    }

    /// Runs after every batch of events, just before the loop sleeps.
    ///
    /// Until the first frame has actually been presented, keep the loop
    /// awake on a short timer and re-request a redraw on every pass --
    /// this is the layer of the first-frame fix that does NOT depend on
    /// any event being delivered (see `App::presented_once` for the full
    /// story; `request_redraw` calls and pty output can both be dropped
    /// or mistimed during the window's first moments). Once something is
    /// on screen, revert to pure `Wait` so an idle terminal costs zero
    /// wakeups.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.presented_once {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }
}

/// `spawn_shell` must run before the winit event loop is created (see its
/// doc comment for why), so the first tab is built here rather than in
/// `App::open_tab`. It's given a placeholder 80x24 size -- `resumed()`
/// fits it to the real window once one exists.
fn main() {
    env_logger::init();

    let config = Config::load();

    let pty_handle = pty::spawn_shell(&config.shell);
    let first_pane = tab::Pane::from_handle(0, pty_handle, &config.shell, 80, 24, config.scrollback_lines);
    let first_tab = Tab::new(0, first_pane);

    // winit would otherwise install its own placeholder macOS menu bar,
    // which would fight the one built in `menu::install`.
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event()
        .with_default_menu(false)
        .build()
        .expect("failed to create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();

    // Must stay alive for the whole run: the native menu bar holds raw
    // pointers back into this value (see `menu::install`'s doc comment).
    let _menu = menu::install(proxy.clone());

    // The pty reader thread is started in `resumed()` instead of here, once
    // the tab's Term is correctly sized -- see the comment there.
    let mut app = App::new(config, first_tab, proxy);
    event_loop.run_app(&mut app).expect("event loop error");
}
