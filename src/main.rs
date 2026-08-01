mod config;
mod filetree;
mod input;
mod linkify;
mod menu;
mod preview;
mod pty;
mod render;
mod settings_ui;
mod state;
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
    ZoomIn,
    ZoomOut,
    /// Back to the size in the config file on disk.
    ZoomReset,
    ToggleFileTree,
    ToggleHiddenFiles,
    /// Preview the file selected in the tree (Cmd+Y, like Finder's
    /// Quick Look).
    PreviewSelected,
    /// A worker thread finished loading a preview, tagged with the path
    /// it was asked for so a result the user has already moved on from
    /// can be dropped.
    PreviewLoaded(std::path::PathBuf, Result<preview::Loaded, String>),
}

struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// The window's split layout: a tree of tab groups. `Option` only so
    /// group removal can take ownership of the tree to restructure it;
    /// it is never `None` between operations.
    root: Option<tab::GroupNode>,
    /// Which group keyboard input and new tabs go to.
    focused_group: u64,
    /// Tab ids are unique across every group. A shell tab's pane takes
    /// the same id, so a pty reader event -- which carries only the pane
    /// id -- identifies exactly one tab anywhere in the tree.
    next_tab_id: u64,
    next_group_id: u64,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    settings_window: Option<SettingsWindow>,
    proc_info: status::ProcInfo,
    cached_status: chrome::StatusInfo,
    /// The focused pane's cwd as a real path, resolved by the same
    /// (throttled) lookup that feeds the status bar. The file tree reads
    /// it instead of querying `sysinfo` again -- those calls rebuild a
    /// process table, far too expensive to repeat on every redraw.
    cached_cwd: Option<std::path::PathBuf>,
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
    /// A mouse button currently held down inside a pane whose application
    /// asked for mouse reporting: (pane id, xterm button code, SGR
    /// encoding flag, last cell reported) -- the last cell lets motion be
    /// reported only when the pointer actually crosses into a new cell.
    mouse_report_drag: Option<(u64, u8, bool, (u16, u16))>,
    /// The previous left press, for multi-click detection: (when, pane,
    /// cell, running count). A press on the same cell of the same pane
    /// within the double-click window bumps the count (wrapping after a
    /// triple), anything else resets to a single click.
    last_click: Option<(Instant, u64, tab::GridPoint, u8)>,
    /// The window's last known frame, tracked from Moved/Resized events
    /// and written to the state file on exit so the next launch opens
    /// where this one left off.
    window_frame: Option<state::WindowFrame>,
    /// The file-tree sidebar's model. Always present; `file_tree_visible`
    /// decides whether it's drawn and whether it takes window width away
    /// from the panes.
    file_tree: filetree::FileTree,
    file_tree_visible: bool,
    /// When the tree last re-read the filesystem, so a visible sidebar
    /// picks up files created by commands without re-reading directories
    /// on every redraw.
    last_tree_refresh: Option<Instant>,
    /// The sidebar's width in pixels, as dragged by the user. Zero means
    /// "the default width" -- see `chrome::file_tree_width`, which also
    /// clamps this so the sidebar can't swallow the window.
    file_tree_width: f32,
    /// Whether a drag on the sidebar's inner edge is currently resizing
    /// it. Mutually exclusive with the pane-divider drag.
    dragging_sidebar: bool,
    /// Which row the pointer is over in the sidebar, for the hover band.
    file_tree_hover: Option<usize>,
    /// The last-clicked entry, kept highlighted. Stored as a path rather
    /// than a row index so it survives the tree being rebuilt (files
    /// appearing above it would otherwise shift the highlight onto a
    /// different entry).
    file_tree_selected: Option<std::path::PathBuf>,
}

impl App {
    fn new(config: Config, first_tab: Tab, proxy: EventLoopProxy<UserEvent>) -> Self {
        let persisted = state::load();
        App {
            config,
            window: None,
            renderer: None,
            next_tab_id: first_tab.id + 1,
            next_group_id: 1,
            focused_group: 0,
            root: Some(tab::GroupNode::Leaf(Box::new(tab::Group::new(0, first_tab)))),
            proxy,
            modifiers: ModifiersState::empty(),
            settings_window: None,
            proc_info: status::ProcInfo::new(),
            cached_status: chrome::StatusInfo { shell: String::new(), cwd: String::new(), branch: None, tty: String::new() },
            cached_cwd: None,
            last_status_refresh: None,
            cursor_pos: (0.0, 0.0),
            presented_once: false,
            dragging_pane: None,
            dragging_divider: None,
            mouse_report_drag: None,
            last_click: None,
            window_frame: persisted.window,
            file_tree: filetree::FileTree::new(),
            file_tree_visible: persisted.file_tree_visible,
            last_tree_refresh: None,
            file_tree_width: persisted.file_tree_width,
            dragging_sidebar: false,
            file_tree_hover: None,
            file_tree_selected: None,
        }
    }

    fn root(&self) -> &tab::GroupNode {
        self.root.as_ref().expect("the tree always has at least one group")
    }

    fn root_mut(&mut self) -> &mut tab::GroupNode {
        self.root.as_mut().expect("the tree always has at least one group")
    }

    /// The group keyboard input goes to. Falls back to the first group
    /// if `focused_group` ever names one that's gone, so input is never
    /// stranded.
    fn focused_group(&self) -> &tab::Group {
        let focused = self.focused_group;
        self.root()
            .group(focused)
            .unwrap_or_else(|| self.root().groups().into_iter().next().expect("never empty"))
    }

    fn focused_group_mut(&mut self) -> &mut tab::Group {
        let focused = self.focused_group;
        if self.root().group(focused).is_none() {
            self.focused_group = self.root().groups()[0].id;
        }
        let focused = self.focused_group;
        self.root_mut().group_mut(focused).expect("just ensured it exists")
    }

    /// The tab that has keyboard focus: the focused group's active one.
    fn active_tab(&self) -> &Tab {
        self.focused_group().active_tab()
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        self.focused_group_mut().active_tab_mut()
    }

    /// The shell that keystrokes reach, or `None` when the focused tab
    /// is a preview.
    fn focused_pane(&self) -> Option<&tab::Pane> {
        self.active_tab().pane()
    }

    fn focused_pane_mut(&mut self) -> Option<&mut tab::Pane> {
        self.active_tab_mut().pane_mut()
    }

    /// Find a pane anywhere in the tree by id -- pty reader events carry
    /// only the pane id, and the pane may be in a background tab of a
    /// group that isn't focused.
    fn pane_by_id_mut(&mut self, pane_id: u64) -> Option<&mut tab::Pane> {
        self.root_mut()
            .groups_mut()
            .into_iter()
            .flat_map(|g| g.tabs_mut())
            .find_map(|t| t.pane_mut().filter(|p| p.id == pane_id))
    }

    /// A tab anywhere in the tree, by id.
    fn tab_mut_by_id(&mut self, tab_id: u64) -> Option<&mut Tab> {
        self.root_mut()
            .groups_mut()
            .into_iter()
            .flat_map(|g| g.tabs_mut())
            .find(|t| t.id == tab_id)
    }

    /// Every live pane, for config changes and shutdown.
    fn all_panes_mut(&mut self) -> Vec<&mut tab::Pane> {
        self.root_mut()
            .groups_mut()
            .into_iter()
            .flat_map(|g| g.tabs_mut())
            .filter_map(tab::Tab::pane_mut)
            .collect()
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
        let scrollback = config.scrollback_lines;
        for pane in self.all_panes_mut() {
            pane.term.set_scrollback_limit(scrollback);
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
        self.relayout(true);
    }

    /// The full grid area's cols/rows for the current window size --
    /// what a tab with a single (unsplit) pane gets. `None` before the
    /// window/renderer exist.
    /// Every group's rectangle at the current window size -- the single
    /// source of truth shared by layout, rendering, and hit-testing.
    fn group_rects(&self) -> Vec<(u64, tab::PaneRect)> {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return Vec::new();
        };
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let sidebar = chrome::file_tree_width(self.file_tree_visible, self.file_tree_width, cell_w, size.width as f32);
        let grid = chrome::grid_rect(size.width as f32, size.height as f32, cell_h, sidebar);
        let mut path = Vec::new();
        let mut groups = Vec::new();
        let mut dividers = Vec::new();
        self.root().layout(grid, chrome::PANE_GAP, &mut path, &mut groups, &mut dividers);
        groups
    }

    /// The cols/rows a tab gets in the focused group, for sizing a shell
    /// at spawn time. `None` before the window/renderer exist.
    fn group_content_size(&self) -> Option<(usize, usize)> {
        let (cell_w, cell_h) = self.renderer.as_ref().map(Renderer::cell_size)?;
        let focused = self.focused_group;
        let (_, rect) = self.group_rects().into_iter().find(|(id, _)| *id == focused)?;
        let content_h = (rect.h - chrome::tab_bar_height(cell_h)).max(cell_h);
        Some((
            ((rect.w / cell_w).floor() as usize).max(1),
            ((content_h / cell_h).floor() as usize).max(1),
        ))
    }

    /// The content rect (below the tab strip) of the group under a
    /// window position, with its id.
    fn group_at(&self, x: f32, y: f32) -> Option<(u64, tab::PaneRect)> {
        let (_, cell_h) = self.renderer.as_ref().map(Renderer::cell_size)?;
        let tab_bar_h = chrome::tab_bar_height(cell_h);
        self.group_rects().into_iter().find(|(_, r)| r.contains(x, y)).map(|(id, r)| {
            (
                id,
                tab::PaneRect { x: r.x, y: r.y + tab_bar_h, w: r.w, h: (r.h - tab_bar_h).max(1.0) },
            )
        })
    }

    /// Recompute every group's rectangle from the current window size and
    /// split tree, and push each visible shell's new cols/rows to its
    /// Term/Grid model (so rendering is correct on every call) and, at
    /// most once per `PTY_RESIZE_THROTTLE` unless `force` is set, to its
    /// pty (so the shell's SIGWINCH-driven reflow, e.g. `stty size`,
    /// matches).
    ///
    /// Every tab is sized, not just the visible one: background tabs keep
    /// running and must have already reflowed correctly by the time
    /// they're switched to. They all share their group's content rect,
    /// since that is the size they will have when shown.
    ///
    /// The throttle exists for divider dragging: `update_divider_drag`
    /// calls this on every `CursorMoved`, and a real terminal size change
    /// signals the pty's foreground process with `SIGWINCH` every time.
    /// Shells with a line editor that redraws on `SIGWINCH` (zsh's zle,
    /// the macOS default) redisplay the prompt each time they receive
    /// one -- signaled faster than they can redisplay, that reads as
    /// garbled, duplicated-looking prompt spam during a fast drag. Callers
    /// that aren't a live drag (window resize, font change, tab/group
    /// creation) pass `force: true` so the pty is always in sync
    /// immediately; the drag itself force-flushes once more when it ends
    /// (see the `MouseInput` `Released` handler) so the shell never stays
    /// out of sync with the final size.
    fn relayout(&mut self, force: bool) {
        let Some((cell_w, cell_h)) = self.renderer.as_ref().map(Renderer::cell_size) else {
            return;
        };
        let rects = self.group_rects();
        let tab_bar_h = chrome::tab_bar_height(cell_h);
        let now = Instant::now();
        for (group_id, rect) in rects {
            let content_h = (rect.h - tab_bar_h).max(cell_h);
            let cols = ((rect.w / cell_w).floor() as usize).max(1);
            let rows = ((content_h / cell_h).floor() as usize).max(1);
            let Some(group) = self.root_mut().group_mut(group_id) else { continue };
            for pane in group.tabs_mut().iter_mut().filter_map(tab::Tab::pane_mut) {
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

    /// Build a shell tab, its pty reader already running.
    fn new_shell_tab(&mut self, cols: usize, rows: usize) -> Tab {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        // The pane takes the tab's id, so a pty event -- which carries
        // only a pane id -- names exactly one tab anywhere in the tree.
        let pane = tab::Pane::spawn(id, &self.config.shell, cols, rows, self.config.scrollback_lines);
        self.spawn_pty_reader(&pane);
        Tab::shell(id, pane)
    }

    /// Open a fresh shell as a new tab in the focused group.
    fn open_tab(&mut self) {
        let (cols, rows) = self.group_content_size().unwrap_or((80, 24));
        let tab = self.new_shell_tab(cols, rows);
        self.focused_group_mut().add_tab(tab);
        self.after_layout_change();
    }

    /// Split the focused group in two, putting a fresh shell in the new
    /// half and focusing it. This is what makes "preview on one side,
    /// shell on the other" possible: each half is a full tab strip.
    fn split_focused_group(&mut self, direction: tab::SplitDirection) {
        // Sized by `relayout` below; a placeholder until then.
        let tab = self.new_shell_tab(80, 24);
        let group_id = self.next_group_id;
        self.next_group_id += 1;
        let new_group = tab::Group::new(group_id, tab);

        let target = self.focused_group().id;
        let root = self.root.take().expect("the tree always has at least one group");
        let (root, outcome) = tab::split_group(root, target, direction, new_group);
        self.root = Some(root);
        match outcome {
            Ok(()) => self.focused_group = group_id,
            Err(orphan) => {
                // Nothing owns the shell we just spawned -- end it rather
                // than leaking a process with no group to show it.
                for pane in orphan.drain_tabs().iter().filter_map(tab::Tab::pane) {
                    let _ = kill(pane.pty_child, Signal::SIGHUP);
                    let _ = nix::sys::wait::waitpid(pane.pty_child, None);
                }
            }
        }
        self.after_layout_change();
    }

    /// Close the focused group's active tab. Closing a group's last tab
    /// closes the group, collapsing the split into its sibling; closing
    /// the last group quits.
    fn close_active_tab(&mut self, event_loop: &ActiveEventLoop) {
        let index = self.focused_group().active_index();
        let removed = self.focused_group_mut().close_tab(index);
        if let Some(tab) = removed {
            self.retire_tab(tab);
            self.after_layout_change();
            return;
        }
        // That was the group's only tab, so the group itself goes.
        let group_id = self.focused_group().id;
        self.close_group(group_id, event_loop);
    }

    /// Remove a whole group, ending every shell it held.
    fn close_group(&mut self, group_id: u64, event_loop: &ActiveEventLoop) {
        if self.root().groups().len() <= 1 {
            // The last group closing means the app is done, matching the
            // single-session "shell exits -> app exits" behavior.
            event_loop.exit();
            return;
        }
        let root = self.root.take().expect("the tree always has at least one group");
        let (rest, removed) = tab::remove_group(root, group_id);
        self.root = rest;
        if let Some(group) = removed {
            for tab in group.drain_tabs() {
                self.retire_tab(tab);
            }
        }
        if self.root().group(self.focused_group).is_none() {
            self.focused_group = self.root().groups()[0].id;
        }
        self.after_layout_change();
    }

    /// End a closed tab's shell (SIGHUP, the same signal a real terminal
    /// sends) and release any preview texture it held.
    fn retire_tab(&mut self, tab: Tab) {
        if let Some(pane) = tab.pane() {
            let _ = kill(pane.pty_child, Signal::SIGHUP);
            let _ = nix::sys::wait::waitpid(pane.pty_child, None);
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.forget_preview_image(tab.id);
        }
    }

    /// Everything that has to happen after the tree's shape or the
    /// focused tab changes: re-fit the shells, re-resolve the status bar,
    /// redraw.
    fn after_layout_change(&mut self) {
        self.relayout(true);
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
        let _ = window;
        let tab_bar_h = chrome::tab_bar_height(cell_h);
        // Every group has its own strip, so find the one whose strip the
        // click is in rather than assuming a single bar across the top.
        let (x, y) = self.cursor_pos;
        let Some((group_id, rect)) = self
            .group_rects()
            .into_iter()
            .find(|(_, r)| r.contains(x, y) && y < r.y + tab_bar_h)
        else {
            return;
        };
        let strip = tab::PaneRect { x: rect.x, y: rect.y, w: rect.w, h: tab_bar_h };
        let Some(group) = self.root().group(group_id) else { return };
        let titles: Vec<String> = group.tabs().iter().map(|t| t.title().to_string()).collect();
        let layout = chrome::tab_bar_layout(&titles, strip, cell_w);

        // Clicking any strip focuses that group, so the next keystroke
        // goes where the eye just went.
        self.focused_group = group_id;
        match layout.hit_test(x) {
            Some(chrome::TabBarHit::Switch(index)) => {
                self.focused_group_mut().activate(index);
                self.after_layout_change();
            }
            Some(chrome::TabBarHit::Close(index)) => {
                let removed = self.focused_group_mut().close_tab(index);
                match removed {
                    Some(tab) => {
                        self.retire_tab(tab);
                        self.after_layout_change();
                    }
                    // Its last tab: the group goes with it.
                    None => self.close_group(group_id, event_loop),
                }
            }
            Some(chrome::TabBarHit::NewTab) => self.open_tab(),
            None => self.after_layout_change(),
        }
    }

    /// Every visible shell's content rectangle, keyed by pane id --
    /// computed from the same pure layout the renderer uses, so clicks
    /// always land on what's visually under the cursor. Groups showing a
    /// preview contribute nothing.
    fn pane_rects(&self) -> Vec<(u64, tab::PaneRect)> {
        let Some((_, cell_h)) = self.renderer.as_ref().map(Renderer::cell_size) else {
            return Vec::new();
        };
        let tab_bar_h = chrome::tab_bar_height(cell_h);
        self.group_rects()
            .into_iter()
            .filter_map(|(group_id, rect)| {
                let pane = self.root().group(group_id)?.active_tab().pane()?;
                Some((
                    pane.id,
                    tab::PaneRect { x: rect.x, y: rect.y + tab_bar_h, w: rect.w, h: (rect.h - tab_bar_h).max(1.0) },
                ))
            })
            .collect()
    }

    /// Which visible shell is under the window-pixel position, if any
    /// (tab strips, dividers, and the bars hit nothing).
    fn pane_at(&self, x: f32, y: f32) -> Option<u64> {
        self.pane_rects().into_iter().find(|(_, r)| r.contains(x, y)).map(|(id, _)| id)
    }

    /// The pane behind a pane id, wherever in the tree it lives.
    fn pane_by_id(&self, pane_id: u64) -> Option<&tab::Pane> {
        self.root()
            .groups()
            .into_iter()
            .flat_map(|g| g.tabs())
            .find_map(|t| t.pane().filter(|p| p.id == pane_id))
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
        let (cell_w, cell_h) = renderer.cell_size();
        let sidebar = chrome::file_tree_width(self.file_tree_visible, self.file_tree_width, cell_w, size.width as f32);
        let grid = chrome::grid_rect(size.width as f32, size.height as f32, cell_h, sidebar);
        let mut path = Vec::new();
        let mut groups = Vec::new();
        let mut dividers = Vec::new();
        self.root().layout(grid, chrome::PANE_GAP, &mut path, &mut groups, &mut dividers);
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
        self.root_mut().set_ratio(&divider.path, ratio);
        self.relayout(false);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Show a resize cursor while hovering (or dragging) a divider, and
    /// the plain arrow everywhere else.
    fn update_cursor_icon(&self) {
        use winit::window::CursorIcon;
        let Some(window) = &self.window else { return };
        // The sidebar edge only ever resizes horizontally, so it maps to
        // the same cursor as a vertical pane divider.
        if self.dragging_sidebar || self.on_file_tree_edge(self.cursor_pos.0, self.cursor_pos.1) {
            window.set_cursor(CursorIcon::ColResize);
            return;
        }
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
        let pane = self.pane_by_id(pane_id)?;
        let grid = pane.term.grid();
        let col = (((x - rect.x) / cell_w).floor().max(0.0) as usize).min(grid.cols.saturating_sub(1));
        let view_row = (((y - rect.y) / cell_h).floor().max(0.0) as usize).min(grid.rows.saturating_sub(1));
        let distance = grid.distance_from_bottom(view_row, pane.scroll_offset);
        Some(tab::GridPoint { distance, col })
    }

    /// The 1-based cell coordinates of a window position inside pane
    /// `pane_id`'s viewport, clamped to its bounds.
    fn pane_cell_coords(&self, pane_id: u64, x: f32, y: f32) -> Option<(u16, u16)> {
        let renderer = self.renderer.as_ref()?;
        let (cell_w, cell_h) = renderer.cell_size();
        let (_, rect) = self.pane_rects().into_iter().find(|(id, _)| *id == pane_id)?;
        let pane = self.pane_by_id(pane_id)?;
        let col = (((x - rect.x) / cell_w).floor().max(0.0) as usize).min(pane.term.cols() - 1) as u16 + 1;
        let row = (((y - rect.y) / cell_h).floor().max(0.0) as usize).min(pane.term.rows() - 1) as u16 + 1;
        Some((col, row))
    }

    /// If the pane under the window position has mouse reporting enabled
    /// (and the user isn't holding Option, the "I want a local selection
    /// anyway" bypass -- same convention as iTerm2), returns everything
    /// needed to report an event there: (pane id, col, row, mode, SGR).
    fn mouse_report_target(&self, x: f32, y: f32) -> Option<(u64, u16, u16, term::MouseMode, bool)> {
        if self.modifiers.alt_key() {
            return None;
        }
        let pane_id = self.pane_at(x, y)?;
        let pane = self.pane_by_id(pane_id)?;
        let mode = pane.term.modes.mouse_mode;
        if mode == term::MouseMode::Off {
            return None;
        }
        let (col, row) = self.pane_cell_coords(pane_id, x, y)?;
        Some((pane_id, col, row, mode, pane.term.modes.mouse_sgr))
    }

    /// Write an encoded mouse event (if any) to `pane_id`'s pty.
    fn send_mouse_event(&self, pane_id: u64, bytes: Option<Vec<u8>>) {
        let Some(bytes) = bytes else { return };
        let Some(pane) = self.pane_by_id(pane_id) else { return };
        write_all_to_pty(pane.pty_master.as_fd(), &bytes);
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
        let Some(pane) = self.pane_by_id(pane_id) else {
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
    /// that pane before. A second click on the same cell within the
    /// double-click window selects the word there, a third the whole
    /// line. No-op outside any pane (bars, dividers).
    fn begin_selection(&mut self) {
        const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);
        let Some(pane_id) = self.pane_at(self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        // A click anywhere in a pane focuses it, selection or not --
        // that's the entire mouse story for pane focus.
        // Clicking a shell focuses whichever group is showing it.
        if let Some((group_id, _)) = self.group_at(self.cursor_pos.0, self.cursor_pos.1) {
            if self.focused_group != group_id {
                self.focused_group = group_id;
                self.last_status_refresh = None;
            }
        }
        let Some(point) = self.grid_point_in_pane(pane_id, self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        let now = Instant::now();
        let count = match self.last_click {
            Some((at, pid, p, c)) if pid == pane_id && p == point && now.duration_since(at) < MULTI_CLICK_WINDOW => c % 3 + 1,
            _ => 1,
        };
        self.last_click = Some((now, pane_id, point, count));

        // Multi-click selections are complete as-is: no `dragging_pane`,
        // so a stray pixel of motion before release doesn't collapse the
        // word back to one cell.
        if count == 1 {
            self.dragging_pane = Some(pane_id);
        }
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
            match count {
                2 => {
                    if let Some(selection) = tab::word_selection(pane.term.grid(), point) {
                        pane.selection = Some(selection);
                    }
                }
                3 => pane.selection = tab::line_selection(pane.term.grid(), point),
                _ => pane.selection = Some(tab::Selection { anchor: point, cursor: point }),
            }
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
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
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
        if let Some(pane) = self.pane_by_id_mut(pane_id) {
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
        let Some(pane) = self.focused_pane_mut() else { return };
        if pane.search.is_none() {
            pane.search = Some(tab::Search::new());
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn close_search(&mut self) {
        if let Some(pane) = self.focused_pane_mut() {
            pane.search = None;
        }
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
                if let Some(search) = self.focused_pane_mut().and_then(|p| p.search.as_mut()) {
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
                        if let Some(search) = self.focused_pane_mut().and_then(|p| p.search.as_mut()) {
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
        let Some(pane) = self.focused_pane_mut() else { return };
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
        let Some(pane) = self.focused_pane_mut() else { return };
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
        let Some(pane) = self.focused_pane_mut() else { return };
        let Some(search) = &pane.search else { return };
        let Some((distance, _)) = search.current_target() else { return };
        let rows = pane.term.rows();
        let max_offset = pane.term.grid().scrollback.len();
        pane.scroll_offset = distance.saturating_sub(rows / 2).min(max_offset);
    }

    /// The sidebar's rectangle, or `None` when it's hidden.
    fn file_tree_rect(&self) -> Option<tab::PaneRect> {
        let (window, renderer) = (self.window.as_ref()?, self.renderer.as_ref()?);
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        chrome::file_tree_rect(size.width as f32, size.height as f32, cell_w, cell_h, self.file_tree_visible, self.file_tree_width)
    }

    /// Show or hide the sidebar. Panes give up (or reclaim) the width, so
    /// every pty is resized to match.
    fn toggle_file_tree(&mut self) {
        self.file_tree_visible = !self.file_tree_visible;
        if self.file_tree_visible {
            // Root it at the focused shell's cwd right away rather than
            // waiting for the next throttled status refresh -- opening
            // onto an empty sidebar for a beat reads as broken.
            self.last_status_refresh = None;
            self.refresh_status();
            self.sync_file_tree_root();
            self.file_tree.rebuild();
            self.last_tree_refresh = Some(Instant::now());
        }
        self.relayout(true);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Point the tree at the focused pane's current directory. Reads the
    /// cwd `refresh_status` already resolved rather than querying for it
    /// again, so this is cheap enough to call on every redraw;
    /// `set_root` itself is a no-op when the directory hasn't changed.
    fn sync_file_tree_root(&mut self) {
        if let Some(cwd) = self.cached_cwd.clone() {
            self.file_tree.set_root(&cwd);
        }
    }

    /// Re-read the filesystem for a visible sidebar, at most once a
    /// second -- commands create and delete files constantly, but
    /// `read_dir`-ing every expanded directory on each redraw would be
    /// wasteful during heavy output.
    fn refresh_file_tree(&mut self) {
        const TREE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
        if !self.file_tree_visible {
            return;
        }
        self.sync_file_tree_root();
        if self.last_tree_refresh.is_none_or(|t| t.elapsed() >= TREE_REFRESH_INTERVAL) {
            self.file_tree.rebuild();
            self.last_tree_refresh = Some(Instant::now());
        }
    }

    /// Route a click inside the sidebar. Directories expand and collapse
    /// in place; files open in whatever app the system associates with
    /// them, like double-clicking in Finder. Option+click inserts the
    /// path at the shell prompt instead, for when the point is to type a
    /// command about the file rather than to open it.
    ///
    /// Nothing here moves the shell: the tree browses downward from
    /// wherever the shell already is, and `cd` stays something you type.
    fn handle_file_tree_click(&mut self, x: f32, y: f32) {
        let Some(rect) = self.file_tree_rect() else { return };
        let Some((_, cell_h)) = self.renderer.as_ref().map(Renderer::cell_size) else { return };
        let hit = chrome::file_tree_hit_test(rect, cell_h, self.file_tree.scroll, self.file_tree.rows().len(), x, y);

        if let Some(index) = hit {
            let Some(row) = self.file_tree.rows().get(index) else { return };
            let (path, is_dir) = (row.path.clone(), row.is_dir);
            self.file_tree_selected = Some(path.clone());

            if self.modifiers.alt_key() {
                self.insert_path_at_prompt(&path);
            } else if is_dir {
                self.file_tree.toggle(&path);
            } else if self.modifiers.shift_key() {
                self.open_preview(path);
            } else {
                open_with_default_app(&path);
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Open `path` in a preview tab and start loading it on a worker
    /// thread -- decoding a large image, and especially asking QuickLook
    /// to render a PDF, takes long enough that doing it here would
    /// visibly freeze the window. The tab shows "Loading..." until
    /// `PreviewLoaded` comes back.
    ///
    /// A file already open in a preview tab is switched to rather than
    /// opened twice, so clicking around the tree doesn't pile up
    /// duplicate tabs.
    fn open_preview(&mut self, path: std::path::PathBuf) {
        // Already open somewhere? Switch to it -- in its own group, so
        // clicking around the tree doesn't pile up duplicates or yank
        // the layout around.
        let existing = self
            .root()
            .groups()
            .into_iter()
            .find_map(|g| {
                g.tabs()
                    .iter()
                    .position(|t| t.preview_content().is_some_and(|p| p.path == path))
                    .map(|index| (g.id, index))
            });
        if let Some((group_id, index)) = existing {
            self.focused_group = group_id;
            self.focused_group_mut().activate(index);
            self.after_layout_change();
            return;
        }

        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        self.focused_group_mut().add_tab(Tab::preview(tab_id, preview::Preview::loading(path.clone())));
        self.after_layout_change();

        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = preview::load(&path);
            let _ = proxy.send_event(UserEvent::PreviewLoaded(path, result));
        });
    }

    /// Scroll the active tab's text preview. Returns whether it consumed
    /// the wheel event.
    fn scroll_preview(&mut self, lines: f32) -> bool {
        let n = (lines.abs().ceil() as usize).min(30);
        let Some(preview) = self.active_tab_mut().preview_content_mut() else {
            return false;
        };
        if let preview::State::Ready(preview::Content::Text(text)) = &preview.state {
            let max = text.len().saturating_sub(1);
            preview.scroll = if lines > 0.0 {
                preview.scroll.saturating_sub(n)
            } else {
                (preview.scroll + n).min(max)
            };
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        true
    }

    /// Whether a window position is on the sidebar's draggable inner
    /// edge (a padded strip, since the visible border is a hairline).
    fn on_file_tree_edge(&self, x: f32, y: f32) -> bool {
        let Some(rect) = self.file_tree_rect() else { return false };
        y >= rect.y && y < rect.y + rect.h && (x - rect.x).abs() <= chrome::FILE_TREE_GRAB
    }

    /// Resize the sidebar to follow the cursor mid-drag. Panes are
    /// re-fitted live (with the pty resize throttled, exactly like a
    /// divider drag).
    fn update_sidebar_drag(&mut self) {
        if !self.dragging_sidebar {
            return;
        }
        let Some(window) = &self.window else { return };
        let width = window.inner_size().width as f32 - self.cursor_pos.0;
        // Left unclamped here on purpose: `chrome::file_tree_width` is
        // the single place that decides the limits, so the drag can't
        // disagree with what gets drawn.
        self.file_tree_width = width.max(0.0);
        self.relayout(false);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Insert a path at the shell prompt without executing anything --
    /// relative to the tree's root when possible, since that's what the
    /// user would have typed themselves.
    fn insert_path_at_prompt(&mut self, path: &std::path::Path) {
        let relative = path.strip_prefix(self.file_tree.root()).unwrap_or(path);
        let text = format!("{} ", filetree::shell_quote(&relative.to_string_lossy()));
        let Some(pane) = self.focused_pane() else { return };
        write_all_to_pty(pane.pty_master.as_fd(), text.as_bytes());
        if let Some(pane) = self.focused_pane_mut() {
            pane.scroll_offset = 0;
        }
    }

    /// Recompute what the pointer is over in the sidebar, redrawing only
    /// when it actually moves to a different row -- a hover band that
    /// requested a frame per pixel of mouse travel would redraw the whole
    /// window for nothing.
    fn update_file_tree_hover(&mut self) {
        let hit = self.file_tree_rect().and_then(|rect| {
            let (_, cell_h) = self.renderer.as_ref()?.cell_size();
            chrome::file_tree_hit_test(rect, cell_h, self.file_tree.scroll, self.file_tree.rows().len(), self.cursor_pos.0, self.cursor_pos.1)
        });
        if hit != self.file_tree_hover {
            self.file_tree_hover = hit;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Scroll the sidebar under the mouse. Returns whether it handled the
    /// wheel event (i.e. the pointer was over the sidebar at all).
    fn scroll_file_tree(&mut self, lines: f32) -> bool {
        let Some(rect) = self.file_tree_rect() else { return false };
        if !rect.contains(self.cursor_pos.0, self.cursor_pos.1) {
            return false;
        }
        let Some((_, cell_h)) = self.renderer.as_ref().map(Renderer::cell_size) else { return false };
        let visible = chrome::file_tree_visible_rows(rect, cell_h);
        let max_scroll = self.file_tree.rows().len().saturating_sub(visible);
        let n = (lines.abs().ceil() as usize).min(30);
        self.file_tree.scroll = if lines > 0.0 {
            self.file_tree.scroll.saturating_sub(n)
        } else {
            (self.file_tree.scroll + n).min(max_scroll)
        };
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        true
    }

    /// Record the window's current outer position and inner size, for
    /// the state file written at exit.
    fn track_window_frame(&mut self) {
        let Some(window) = &self.window else { return };
        let Ok(position) = window.outer_position() else { return };
        let size = window.inner_size();
        if size.width > 0 && size.height > 0 {
            self.window_frame = Some(state::WindowFrame {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            });
        }
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

        // A preview tab has no shell to report on, so the bar describes
        // the file instead: where it lives, and its repo if it's in one.
        if let Some(preview) = self.active_tab().preview_content() {
            let dir = preview.path.parent().map(std::path::Path::to_path_buf);
            self.cached_status = chrome::StatusInfo {
                shell: "preview".to_string(),
                cwd: dir.as_deref().map(display_path).unwrap_or_default(),
                branch: dir.as_deref().and_then(status::git_branch),
                tty: String::new(),
            };
            // Deliberately not touching `cached_cwd`: the file tree
            // follows the shell's directory, and switching to a preview
            // tab shouldn't yank the tree somewhere else.
            return;
        }

        let Some((master, pty_child, shell_name, tty_name)) = self
            .focused_pane()
            .map(|p| (Arc::clone(&p.pty_master), p.pty_child, p.shell_name.clone(), p.tty_name.clone()))
        else {
            return;
        };
        let (fg_name, cwd) = match self.proc_info.foreground_process_name(master.as_fd()) {
            // The shell itself sitting at its prompt: use the name we
            // derived from the configured shell path at spawn time rather
            // than whatever sysinfo reports for the pid. Right after a
            // pane opens, that pid can still be the pre-exec fork of this
            // binary (named "terminal"), and losing that race used to
            // mistitle the tab -- the shell's own name is a fact we
            // already know, so never ask the process table for it.
            Some((pid, _)) if pid == pty_child => (shell_name.clone(), self.proc_info.process_cwd(pid)),
            Some((pid, name)) => (name, self.proc_info.process_cwd(pid)),
            None => (shell_name.clone(), self.proc_info.process_cwd(pty_child)),
        };
        if let Some(pane) = self.focused_pane_mut() {
            pane.title = fg_name;
        }

        let cwd_display = cwd.as_deref().map(display_path).unwrap_or_default();
        let branch = cwd.as_deref().and_then(status::git_branch);
        self.cached_cwd = cwd.clone();

        self.cached_status = chrome::StatusInfo {
            shell: shell_name,
            cwd: cwd_display,
            branch,
            tty: tty_name,
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

/// Hand a path to macOS's `open`, which resolves it through the same
/// LaunchServices association Finder uses -- clicking a `.png` in the
/// sidebar opens Preview, a `.rs` opens the editor bound to it. The path
/// is one argv entry, never interpolated into a shell string, so a file
/// named with shell metacharacters can't turn into a command.
fn open_with_default_app(path: &std::path::Path) {
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// Write every byte of `bytes` to the pty, looping over short writes. A
/// single `write(2)` may transfer less than requested (the pty's kernel
/// buffer is only a few KB, and a signal can interrupt mid-write) --
/// noticeable exactly when it matters most, pasting something large.
fn write_all_to_pty(fd: std::os::fd::BorrowedFd, bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        match nix::unistd::write(fd, &bytes[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
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
        let mut attrs = Window::default_attributes().with_title("keterm").with_transparent(true);
        // Reopen where the last run's window was. A frame saved on a
        // display that's since been unplugged may land offscreen; macOS
        // pulls fully-offscreen windows back onto a visible display on
        // its own, so no clamping is attempted here.
        if let Some(frame) = self.window_frame {
            if frame.width > 0 && frame.height > 0 {
                attrs = attrs
                    .with_position(winit::dpi::PhysicalPosition::new(frame.x, frame.y))
                    .with_inner_size(winit::dpi::PhysicalSize::new(frame.width, frame.height));
            }
        }
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
        self.relayout(true);

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
        let first = self.root().groups()[0].active_tab().pane().expect("the first tab is a shell");
        self.spawn_pty_reader(first);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(pane_id, generation, bytes) => {
                let Some(pane) = self.pane_by_id_mut(pane_id) else {
                    return; // pane already closed
                };
                // Ignore output from a shell session that's since been
                // replaced -- its reader thread can still have bytes in
                // flight for a moment after that.
                if generation != pane.pty_generation {
                    return;
                }
                let was_alt = pane.term.using_alt_screen();
                let scrollback_before = pane.term.grid().scrollback.len();
                pane.term.advance(&bytes);
                // Answer any queries the output contained (DSR cursor
                // reports, DA) -- the querying application is blocked
                // waiting on these.
                let responses = pane.term.take_responses();
                if !responses.is_empty() {
                    write_all_to_pty(pane.pty_master.as_fd(), &responses);
                }
                if was_alt || pane.term.using_alt_screen() {
                    // Full-screen apps redraw arbitrarily; there's no
                    // stable content for a scroll position or selection
                    // to stay anchored to.
                    pane.scroll_offset = 0;
                    pane.selection = None;
                } else {
                    // New output pushes lines into scrollback, moving all
                    // existing content further from the live bottom. Both
                    // the scroll position (when scrolled back at all) and
                    // any selection are distance-from-bottom values, so
                    // shifting them by the same amount keeps them pinned
                    // to the text the user was looking at -- reading old
                    // logs during a `tail -f` no longer gets yanked to
                    // the bottom, and a selection survives new output.
                    // At the bottom (offset 0), keep following the tail.
                    let scrollback = pane.term.grid().scrollback.len();
                    let delta = scrollback.saturating_sub(scrollback_before);
                    if pane.scroll_offset > 0 {
                        pane.scroll_offset = (pane.scroll_offset + delta).min(scrollback);
                    }
                    if let Some(selection) = &mut pane.selection {
                        selection.anchor.distance += delta;
                        selection.cursor.distance += delta;
                        // Drop it once the text it covered has fallen out
                        // of scrollback entirely.
                        let reachable = scrollback + pane.term.rows();
                        if selection.anchor.distance >= reachable || selection.cursor.distance >= reachable {
                            pane.selection = None;
                        }
                    }
                }
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
                // A shell exiting closes its tab, exactly like Cmd+W on
                // it -- and its group too, if that was the last tab.
                let Some(pane) = self.pane_by_id(pane_id) else {
                    return; // already closed
                };
                if generation != pane.pty_generation {
                    return;
                }
                let _ = nix::sys::wait::waitpid(pane.pty_child, None);
                let located = self.root().groups().into_iter().find_map(|g| {
                    g.tabs()
                        .iter()
                        .position(|t| t.pane().is_some_and(|p| p.id == pane_id))
                        .map(|index| (g.id, index))
                });
                let Some((group_id, index)) = located else { return };
                let Some(group) = self.root_mut().group_mut(group_id) else { return };
                match group.close_tab(index) {
                    Some(tab) => {
                        self.retire_tab(tab);
                        self.after_layout_change();
                    }
                    None => self.close_group(group_id, event_loop),
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
            UserEvent::ClosePane => self.close_active_tab(event_loop),
            UserEvent::NextTab => {
                self.focused_group_mut().cycle_tab(true);
                self.after_layout_change();
            }
            UserEvent::PrevTab => {
                self.focused_group_mut().cycle_tab(false);
                self.after_layout_change();
            }
            UserEvent::SplitRight => self.split_focused_group(tab::SplitDirection::Vertical),
            UserEvent::SplitDown => self.split_focused_group(tab::SplitDirection::Horizontal),
            UserEvent::PreviewSelected => {
                if let Some(path) = self.file_tree_selected.clone() {
                    self.open_preview(path);
                }
            }
            UserEvent::PreviewLoaded(path, result) => {
                // The tab may have been closed while this was loading.
                let Some(tab_id) = self
                    .root()
                    .groups()
                    .into_iter()
                    .flat_map(|g| g.tabs())
                    .find(|t| t.preview_content().is_some_and(|p| p.path == path))
                    .map(|t| t.id)
                else {
                    return;
                };
                // Image pixels go to a texture keyed by this tab and are
                // dropped here rather than also kept in the tab -- see
                // `preview::Content`. Done before taking the tab borrow,
                // since the upload needs the renderer.
                let state = match result {
                    Ok(preview::Loaded::Text(lines)) => preview::State::Ready(preview::Content::Text(lines)),
                    Ok(preview::Loaded::Image { pixels, width, height }) => {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.set_preview_image(tab_id, &pixels, width, height);
                        }
                        preview::State::Ready(preview::Content::Image { width, height })
                    }
                    Err(message) => preview::State::Failed(message),
                };
                let Some(tab) = self.tab_mut_by_id(tab_id) else { return };
                let Some(preview) = tab.preview_content_mut() else { return };
                preview.scroll = 0;
                preview.state = state;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::ToggleFileTree => self.toggle_file_tree(),
            UserEvent::ToggleHiddenFiles => {
                self.file_tree.toggle_hidden();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::ZoomIn | UserEvent::ZoomOut | UserEvent::ZoomReset => {
                // A session-only override: the config file on disk keeps
                // its size (Cmd+0 re-reads it), so a quick zoom for a
                // presentation doesn't silently rewrite settings.
                let new_size = match event {
                    UserEvent::ZoomIn => self.config.font.size + 1.0,
                    UserEvent::ZoomOut => self.config.font.size - 1.0,
                    _ => Config::load().font.size,
                }
                .clamp(6.0, 72.0);
                if new_size != self.config.font.size {
                    self.config.font.size = new_size;
                    self.apply_font_change();
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            UserEvent::NextPane | UserEvent::PrevPane => {
                // Move focus between split groups, in tree (reading)
                // order.
                let forward = matches!(event, UserEvent::NextPane);
                let ids: Vec<u64> = self.root().groups().iter().map(|g| g.id).collect();
                if let Some(pos) = ids.iter().position(|&id| id == self.focused_group) {
                    let next = if forward { (pos + 1) % ids.len() } else { (pos + ids.len() - 1) % ids.len() };
                    self.focused_group = ids[next];
                }
                self.after_layout_change();
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
                self.relayout(true);
                self.track_window_frame();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Moved(_) => self.track_window_frame(),
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
                // A preview tab has no shell to type into, so its keys
                // are the tab's own: Escape closes it, everything else
                // does nothing rather than vanishing into a pty that
                // isn't there.
                if self.active_tab().preview_content().is_some() {
                    if let winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape) = &event.logical_key {
                        self.close_active_tab(event_loop);
                    }
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
                if self.focused_pane().is_some_and(|p| p.search.is_some()) {
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
                            if let Some(text) = self.focused_pane().and_then(tab::Pane::selected_text) {
                                copy_to_clipboard(&text);
                            }
                            return;
                        }
                        if c.eq_ignore_ascii_case("v") {
                            if let (Some(text), Some(pane)) = (paste_from_clipboard(), self.focused_pane()) {
                                if pane.term.modes.bracketed_paste {
                                    // Strip any end-guard sequence lurking
                                    // inside the pasted text itself so
                                    // adversarial clipboard content can't
                                    // break out of the paste bracket and
                                    // inject keystrokes.
                                    let sanitized = text.replace("\x1b[201~", "");
                                    write_all_to_pty(pane.pty_master.as_fd(), b"\x1b[200~");
                                    write_all_to_pty(pane.pty_master.as_fd(), sanitized.as_bytes());
                                    write_all_to_pty(pane.pty_master.as_fd(), b"\x1b[201~");
                                } else {
                                    write_all_to_pty(pane.pty_master.as_fd(), text.as_bytes());
                                }
                                if let Some(pane) = self.focused_pane_mut() {
                                    pane.scroll_offset = 0;
                                }
                                if let Some(window) = &self.window {
                                    window.request_redraw();
                                }
                            }
                            return;
                        }
                    }
                }
                let Some(pane) = self.focused_pane() else { return };
                let bytes = input::encode_key(
                    &event.logical_key,
                    event.text.as_deref(),
                    event.state.is_pressed(),
                    self.modifiers,
                    &pane.term.modes,
                );
                if let Some(bytes) = bytes {
                    write_all_to_pty(pane.pty_master.as_fd(), &bytes);
                    if let Some(pane) = self.focused_pane_mut() {
                        pane.scroll_offset = 0;
                    }
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
                if lines == 0.0 {
                    return;
                }
                if self.scroll_file_tree(lines) || self.scroll_preview(lines) {
                    return;
                }
                let n = (lines.abs().ceil() as usize).min(30);
                // An app that asked for mouse reporting gets the wheel as
                // button 64/65 events and handles scrolling itself.
                if let Some((pane_id, col, row, _, sgr)) = self.mouse_report_target(self.cursor_pos.0, self.cursor_pos.1) {
                    let button = if lines > 0.0 { 64 } else { 65 };
                    for _ in 0..n {
                        self.send_mouse_event(pane_id, input::encode_mouse(button, input::MouseEventKind::Press, col, row, sgr));
                    }
                    return;
                }
                // Scroll whatever pane is under the mouse (not the focused
                // one) -- matching how iTerm2/macOS scroll views behave.
                let Some(pane_id) = self
                    .pane_at(self.cursor_pos.0, self.cursor_pos.1)
                    .or(self.focused_pane().map(|p| p.id))
                else {
                    return;
                };
                let Some(pane) = self.pane_by_id_mut(pane_id) else {
                    return;
                };
                if pane.term.using_alt_screen() {
                    // Full-screen apps that never asked for the mouse
                    // (plain `less`, `man`) still expect the wheel to do
                    // something -- translate each tick into an arrow key,
                    // the same fallback every other terminal ships.
                    let seq: &[u8] = match (lines > 0.0, pane.term.modes.app_cursor_keys) {
                        (true, false) => b"\x1b[A",
                        (true, true) => b"\x1bOA",
                        (false, false) => b"\x1b[B",
                        (false, true) => b"\x1bOB",
                    };
                    let bytes: Vec<u8> = seq.iter().copied().cycle().take(seq.len() * n).collect();
                    write_all_to_pty(pane.pty_master.as_fd(), &bytes);
                    return;
                }
                let max_offset = pane.term.grid().scrollback.len();
                if lines > 0.0 {
                    pane.scroll_offset = (pane.scroll_offset + n).min(max_offset);
                } else {
                    pane.scroll_offset = pane.scroll_offset.saturating_sub(n);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
                self.update_divider_drag();
                self.update_sidebar_drag();
                self.update_file_tree_hover();
                self.update_selection();
                // Motion with a button held, for apps in drag-reporting
                // mode -- reported once per cell crossed, not per pixel.
                if let Some((pane_id, code, sgr, last_cell)) = self.mouse_report_drag {
                    let wants_motion = self
                        .pane_by_id(pane_id)
                        .is_some_and(|p| p.term.modes.mouse_mode >= term::MouseMode::Drag);
                    if wants_motion {
                        if let Some(cell) = self.pane_cell_coords(pane_id, self.cursor_pos.0, self.cursor_pos.1) {
                            if cell != last_cell {
                                self.mouse_report_drag = Some((pane_id, code, sgr, cell));
                                self.send_mouse_event(pane_id, input::encode_mouse(code, input::MouseEventKind::Drag, cell.0, cell.1, sgr));
                            }
                        }
                    }
                }
                self.update_cursor_icon();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let Some(cell_h) = self.renderer.as_ref().map(|r| r.cell_size().1) else {
                    return;
                };
                if self.cursor_pos.1 < chrome::tab_bar_height(cell_h) {
                    self.handle_tab_bar_click(event_loop);
                } else if self.on_file_tree_edge(self.cursor_pos.0, self.cursor_pos.1) {
                    // Checked before the sidebar body: the grab strip
                    // overlaps its first few pixels.
                    self.dragging_sidebar = true;
                } else if self.file_tree_rect().is_some_and(|r| r.contains(self.cursor_pos.0, self.cursor_pos.1)) {
                    self.handle_file_tree_click(self.cursor_pos.0, self.cursor_pos.1);
                } else if let Some(divider) = self.divider_at(self.cursor_pos.0, self.cursor_pos.1) {
                    self.dragging_divider = Some(divider);
                } else if self.modifiers.super_key() && self.open_url_under_cursor() {
                    // Cmd+click on a link opens it instead of starting a
                    // selection -- Cmd+drag was never a gesture to begin
                    // with, so there's nothing to preserve by falling
                    // through when the click isn't on a link either.
                } else if let Some((pane_id, col, row, _, sgr)) = self.mouse_report_target(self.cursor_pos.0, self.cursor_pos.1) {
                    // The app wants the mouse (vim, htop, ...): forward
                    // the click instead of selecting locally. A click
                    // still focuses the pane -- Option+click bypasses
                    // reporting entirely for a local selection.
                    if let Some((group_id, _)) = self.group_at(self.cursor_pos.0, self.cursor_pos.1) {
                        if self.focused_group != group_id {
                            self.focused_group = group_id;
                            self.last_status_refresh = None;
                        }
                    }
                    self.mouse_report_drag = Some((pane_id, 0, sgr, (col, row)));
                    self.send_mouse_event(pane_id, input::encode_mouse(0, input::MouseEventKind::Press, col, row, sgr));
                } else {
                    self.begin_selection();
                }
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                if self.dragging_divider.take().is_some() || std::mem::take(&mut self.dragging_sidebar) {
                    // Force-flush: either drag may have throttled the pty
                    // out of sync with the final size.
                    self.relayout(true);
                }
                if let Some((pane_id, code, sgr, last_cell)) = self.mouse_report_drag.take() {
                    let (col, row) = self.pane_cell_coords(pane_id, self.cursor_pos.0, self.cursor_pos.1).unwrap_or(last_cell);
                    self.send_mouse_event(pane_id, input::encode_mouse(code, input::MouseEventKind::Release, col, row, sgr));
                }
                self.end_selection();
            }
            WindowEvent::MouseInput { state, button: button @ (MouseButton::Right | MouseButton::Middle), .. } => {
                // Right/middle buttons exist only for mouse-reporting apps;
                // the terminal itself assigns them no local behavior.
                let code = if button == MouseButton::Right { 2 } else { 1 };
                match state {
                    ElementState::Pressed => {
                        if let Some((pane_id, col, row, _, sgr)) = self.mouse_report_target(self.cursor_pos.0, self.cursor_pos.1) {
                            self.mouse_report_drag = Some((pane_id, code, sgr, (col, row)));
                            self.send_mouse_event(pane_id, input::encode_mouse(code, input::MouseEventKind::Press, col, row, sgr));
                        }
                    }
                    ElementState::Released => {
                        if let Some((pane_id, held_code, sgr, last_cell)) = self.mouse_report_drag.take() {
                            if held_code == code {
                                let (col, row) = self.pane_cell_coords(pane_id, self.cursor_pos.0, self.cursor_pos.1).unwrap_or(last_cell);
                                self.send_mouse_event(pane_id, input::encode_mouse(code, input::MouseEventKind::Release, col, row, sgr));
                            } else {
                                self.mouse_report_drag = Some((pane_id, held_code, sgr, last_cell));
                            }
                        }
                    }
                }
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
                self.refresh_file_tree();
                let cmd_held = self.modifiers.super_key();
                // The folder's own name, like VS Code's workspace title;
                // falls back to the abbreviated path at the filesystem
                // root, which has no name of its own.
                let root = self.file_tree.root();
                let title = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| display_path(root));
                let selected = self
                    .file_tree_selected
                    .as_ref()
                    .and_then(|path| self.file_tree.rows().iter().position(|r| &r.path == path));
                let file_tree_view = self.file_tree_visible.then(|| render::FileTreeView {
                    title: &title,
                    rows: self.file_tree.rows(),
                    scroll: self.file_tree.scroll,
                    show_hidden: self.file_tree.show_hidden(),
                    width: self.file_tree_width,
                    hover: self.file_tree_hover,
                    selected,
                });
                // Field access rather than `self.root()`, so the borrow
                // is of `self.root` alone and `self.renderer` stays free
                // to be borrowed mutably alongside it.
                let root = self.root.as_ref().expect("the tree always has at least one group");
                let focused_group = self.focused_group;
                let outcome = self
                    .renderer
                    .as_mut()
                    .map(|renderer| renderer.render(root, focused_group, &self.cached_status, cmd_held, file_tree_view));
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
    /// Runs once when the event loop is about to exit, whichever path
    /// triggered it (window close, last pane's shell exiting, Cmd+Q).
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        state::save(&state::State {
            window: self.window_frame,
            file_tree_visible: self.file_tree_visible,
            file_tree_width: self.file_tree_width,
        });
    }

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
    let first_tab = Tab::shell(0, first_pane);

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
