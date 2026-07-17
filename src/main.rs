mod config;
mod input;
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

enum UserEvent {
    /// Bytes read from a tab's pty, tagged with that tab's id and the
    /// generation of the shell session that produced them (see
    /// `Tab::pty_generation`).
    PtyData(u64, u64, Vec<u8>),
    /// A pty reader thread hit EOF/error, tagged with its tab id and
    /// generation.
    PtyExited(u64, u64),
    OpenSettings,
    ReloadConfig,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
}

struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    settings_window: Option<SettingsWindow>,
    proc_info: status::ProcInfo,
    cached_status: chrome::StatusInfo,
    last_status_refresh: Option<Instant>,
    cursor_pos: (f32, f32),
}

impl App {
    fn new(config: Config, first_tab: Tab, proxy: EventLoopProxy<UserEvent>) -> Self {
        App {
            config,
            window: None,
            renderer: None,
            next_tab_id: first_tab.id + 1,
            tabs: vec![first_tab],
            active: 0,
            proxy,
            modifiers: ModifiersState::empty(),
            settings_window: None,
            proc_info: status::ProcInfo::new(),
            cached_status: chrome::StatusInfo { shell: String::new(), cwd: String::new(), branch: None, tty: String::new() },
            last_status_refresh: None,
            cursor_pos: (0.0, 0.0),
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
            tab.term.set_scrollback_limit(config.scrollback_lines);
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
    /// tab's grid to the window at the new cell size.
    fn apply_font_change(&mut self) {
        let Some(scale_factor) = self.window.as_ref().map(|w| w.scale_factor()) else {
            return;
        };
        if let Some(renderer) = &mut self.renderer {
            renderer.set_font(&self.config.font, scale_factor);
        }
        self.sync_size_to_window();
    }

    /// The terminal grid's cols/rows for the current window size, with the
    /// tab bar's and status bar's pixel heights already carved out of the
    /// row count. `None` before the window/renderer exist.
    fn grid_size(&self) -> Option<(usize, usize)> {
        let window = self.window.as_ref()?;
        let renderer = self.renderer.as_ref()?;
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let cols = ((size.width as f32 / cell_w).floor() as usize).max(1);
        let rows = chrome::terminal_rows(size.height as f32, cell_h);
        Some((cols, rows))
    }

    /// Recompute the terminal grid from the current window size and push
    /// the new size to every open tab's pty (so the shell's SIGWINCH-driven
    /// reflow, e.g. `stty size`, matches) and Term/Grid model -- not just
    /// the active one, since background tabs keep running and must have
    /// already reflowed correctly by the time they're switched to.
    fn sync_size_to_window(&mut self) {
        let Some((cols, rows)) = self.grid_size() else {
            return;
        };
        for tab in &mut self.tabs {
            pty::resize(tab.pty_master.as_fd(), cols as u16, rows as u16);
            tab.term.resize(cols, rows);
        }
    }

    /// Spawn a fresh tab running `self.config.shell`, make it active, and
    /// start reading its pty.
    fn open_tab(&mut self) {
        let (cols, rows) = self.grid_size().unwrap_or((80, 24));
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = Tab::spawn(id, &self.config.shell, cols, rows, self.config.scrollback_lines);
        self.spawn_pty_reader(&tab);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.last_status_refresh = None;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// End the tab's shell (SIGHUP, same signal a real terminal sends its
    /// shell on close) and remove it.
    fn close_tab(&mut self, id: u64, event_loop: &ActiveEventLoop) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let child = self.tabs[index].pty_child;
        let _ = kill(child, Signal::SIGHUP);
        let _ = nix::sys::wait::waitpid(child, None);
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
        let titles: Vec<String> = self.tabs.iter().map(|t| t.title.clone()).collect();
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

    /// Spawn the thread that blocking-reads `tab`'s pty and forwards bytes
    /// to the event loop, tagged with `tab`'s id and generation so
    /// `user_event` can route them (and can tell a since-closed tab's
    /// trailing events apart from a live one's).
    fn spawn_pty_reader(&self, tab: &Tab) {
        let reader_master = Arc::clone(&tab.pty_master);
        let proxy = self.proxy.clone();
        let tab_id = tab.id;
        let generation = tab.pty_generation;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match nix::unistd::read(reader_master.as_fd(), &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if proxy
                            .send_event(UserEvent::PtyData(tab_id, generation, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = proxy.send_event(UserEvent::PtyExited(tab_id, generation));
        });
    }

    /// Recompute the status bar text and the active tab's title from the
    /// live shell state (foreground process, cwd, git branch), but no more
    /// often than `STATUS_REFRESH_INTERVAL` -- these calls touch `sysinfo`
    /// and the filesystem, so redoing them on every keystroke-triggered
    /// redraw would be wasteful. Background tabs keep whatever title they
    /// last had until they're switched to.
    fn refresh_status(&mut self) {
        let due = self.last_status_refresh.is_none_or(|t| t.elapsed() >= STATUS_REFRESH_INTERVAL);
        if !due {
            return;
        }
        self.last_status_refresh = Some(Instant::now());

        let tab = &mut self.tabs[self.active];
        let master = tab.pty_master.as_fd();
        let (fg_name, cwd) = match self.proc_info.foreground_process_name(master) {
            Some((pid, name)) => (name, self.proc_info.process_cwd(pid)),
            None => (tab.shell_name.clone(), self.proc_info.process_cwd(tab.pty_child)),
        };
        tab.title = fg_name;

        let cwd_display = cwd.as_deref().map(display_path).unwrap_or_default();
        let branch = cwd.as_deref().and_then(status::git_branch);

        self.cached_status = chrome::StatusInfo {
            shell: tab.shell_name.clone(),
            cwd: cwd_display,
            branch,
            tty: tab.tty_name.clone(),
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
        let attrs = Window::default_attributes().with_title("terminal").with_transparent(true);
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        let palette = Palette::from(&self.config.colors);
        let renderer = Renderer::new(window.clone(), &self.config.font, palette, self.config.opacity);
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let cols = ((size.width as f32 / cell_w).floor() as usize).max(1);
        let rows = chrome::terminal_rows(size.height as f32, cell_h);

        self.window = Some(window);
        self.renderer = Some(renderer);

        // The first tab was constructed in `main()` at a placeholder size
        // (before the window/renderer existed to know the real one) --
        // fit it to the actual window now.
        let first_tab = &mut self.tabs[0];
        first_tab.term.resize(cols, rows);
        pty::resize(first_tab.pty_master.as_fd(), cols as u16, rows as u16);

        self.window.as_ref().unwrap().request_redraw();

        // Only start reading the pty now that the tab's Term is correctly
        // sized: the shell starts producing output the moment it's forked
        // (in `main`, before the event loop even runs), and any bytes read
        // before this point would be silently dropped by `user_event`'s
        // `PtyData` handler -- which used to lose the shell's very first
        // prompt if it arrived before this point, showing nothing until
        // the next keypress produced fresh output. The pty's kernel-side
        // buffer holds onto that early output until we're ready to read
        // it, so nothing is lost by waiting.
        self.spawn_pty_reader(&self.tabs[0]);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(tab_id, generation, bytes) => {
                let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
                    return; // tab already closed
                };
                // Ignore output from a shell session that's since been
                // replaced -- its reader thread can still have bytes in
                // flight for a moment after that.
                if generation != tab.pty_generation {
                    return;
                }
                tab.term.advance(&bytes);
                tab.scroll_offset = 0;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::PtyExited(tab_id, generation) => {
                let Some(index) = self.tabs.iter().position(|t| t.id == tab_id) else {
                    return; // tab already closed
                };
                if generation != self.tabs[index].pty_generation {
                    return;
                }
                let _ = nix::sys::wait::waitpid(self.tabs[index].pty_child, None);
                self.remove_tab(index, event_loop);
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
            UserEvent::CloseTab => {
                let id = self.tabs[self.active].id;
                self.close_tab(id, event_loop);
            }
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
                self.sync_size_to_window();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, is_synthetic, .. } => {
                if is_synthetic {
                    return;
                }
                let tab = self.active_tab();
                let bytes = input::encode_key(
                    &event.logical_key,
                    event.text.as_deref(),
                    event.state.is_pressed(),
                    self.modifiers,
                    &tab.term.modes,
                );
                if let Some(bytes) = bytes {
                    let _ = nix::unistd::write(tab.pty_master.as_fd(), &bytes);
                    self.active_tab_mut().scroll_offset = 0;
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
                let tab = self.active_tab_mut();
                if tab.term.using_alt_screen() {
                    return;
                }
                let max_offset = tab.term.grid().scrollback.len();
                if lines > 0.0 {
                    tab.scroll_offset = (tab.scroll_offset + lines.ceil() as usize).min(max_offset);
                } else if lines < 0.0 {
                    tab.scroll_offset = tab.scroll_offset.saturating_sub((-lines).ceil() as usize);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                self.handle_tab_bar_click(event_loop);
            }
            WindowEvent::RedrawRequested => {
                self.refresh_status();
                let scroll_offset = self.tabs[self.active].scroll_offset;
                let outcome = self
                    .renderer
                    .as_mut()
                    .map(|renderer| renderer.render(&self.tabs, self.active, scroll_offset, &self.cached_status));
                if let Some(render::RenderOutcome::Retry) = outcome {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            _ => {}
        }
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
    let first_tab = Tab::from_handle(0, pty_handle, &config.shell, 80, 24, config.scrollback_lines);

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
