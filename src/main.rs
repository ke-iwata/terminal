mod config;
mod input;
mod menu;
mod pty;
mod render;
mod settings_ui;
mod term;

use config::Config;
use settings_ui::{SettingsAction, SettingsWindow};
use term::color::Palette;

use nix::sys::signal::{kill, Signal};

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use nix::unistd::Pid;
use winit::application::ApplicationHandler;
use winit::event::{MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::macos::EventLoopBuilderExtMacOS;
use winit::window::{Window, WindowId};

use render::Renderer;
use term::Term;

enum UserEvent {
    /// Bytes read from the pty, tagged with the generation of the shell
    /// session that produced them (see `App::pty_generation`).
    PtyData(u64, Vec<u8>),
    /// A pty reader thread hit EOF/error, tagged with its generation.
    PtyExited(u64),
    OpenSettings,
    ReloadConfig,
}

struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    term: Option<Term>,
    pty_master: Arc<OwnedFd>,
    pty_child: Pid,
    /// Bumped every time the shell is restarted (config reload with a
    /// changed `shell` section). Lets `user_event` tell current pty
    /// reader-thread events apart from stale ones still in flight from a
    /// shell session that's already been replaced -- see `restart_shell`.
    pty_generation: u64,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    /// How many lines back into scrollback the view is currently scrolled.
    /// 0 means "showing the live bottom of the screen".
    scroll_offset: usize,
    settings_window: Option<SettingsWindow>,
}

impl App {
    fn new(config: Config, pty_master: Arc<OwnedFd>, pty_child: Pid, proxy: EventLoopProxy<UserEvent>) -> Self {
        App {
            config,
            window: None,
            renderer: None,
            term: None,
            pty_master,
            pty_child,
            pty_generation: 0,
            proxy,
            modifiers: ModifiersState::empty(),
            scroll_offset: 0,
            settings_window: None,
        }
    }

    /// Apply a config (just saved from the settings window, or reloaded
    /// from disk via the menu) so every field takes effect right away:
    /// colors and scrollback are cheap in-place updates, a changed font
    /// rebuilds the glyph atlas and re-fits the grid to the window, and a
    /// changed shell restarts the pty session (see `restart_shell` for
    /// what that does to the currently running shell).
    fn apply_config(&mut self, config: Config) {
        let palette = Palette::from(&config.colors);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_palette(palette);
        }
        if let Some(term) = &mut self.term {
            term.set_scrollback_limit(config.scrollback_lines);
        }

        let font_changed = config.font != self.config.font;
        let shell_changed = config.shell != self.config.shell;
        self.config = config;

        if font_changed {
            self.apply_font_change();
        }
        if shell_changed {
            self.restart_shell();
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

    /// Rebuild the glyph atlas for `self.config.font` and re-fit the grid
    /// to the window at the new cell size.
    fn apply_font_change(&mut self) {
        let Some(scale_factor) = self.window.as_ref().map(|w| w.scale_factor()) else {
            return;
        };
        if let Some(renderer) = &mut self.renderer {
            renderer.set_font(&self.config.font, scale_factor);
        }
        self.sync_size_to_window();
    }

    /// End the current shell session and start a fresh one for
    /// `self.config.shell`. This sends SIGHUP to the running shell (the
    /// same signal it would get from a real terminal closing), which also
    /// reaches any foreground job via normal terminal session semantics --
    /// there's no way to swap the shell command without ending whatever
    /// was running under the old one. The screen is cleared for the new
    /// session; scrollback from the old one is discarded.
    fn restart_shell(&mut self) {
        let _ = kill(self.pty_child, Signal::SIGHUP);
        let _ = nix::sys::wait::waitpid(self.pty_child, None);

        let handle = pty::spawn_shell(&self.config.shell);
        self.pty_master = Arc::new(handle.master);
        self.pty_child = handle.child;
        self.pty_generation += 1;
        self.scroll_offset = 0;

        if let Some(term) = &self.term {
            let (cols, rows) = (term.cols(), term.rows());
            self.term = Some(Term::new(cols, rows, self.config.scrollback_lines));
            pty::resize(self.pty_master.as_fd(), cols as u16, rows as u16);
        }

        self.spawn_pty_reader();
    }

    /// Recompute the terminal's cell grid from the current window size and
    /// push the new size to both the pty (so the shell's SIGWINCH-driven
    /// reflow, e.g. `stty size`, matches) and the Term/Grid model.
    fn sync_size_to_window(&mut self) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let cols = ((size.width as f32 / cell_w).floor() as usize).max(1);
        let rows = ((size.height as f32 / cell_h).floor() as usize).max(1);
        pty::resize(self.pty_master.as_fd(), cols as u16, rows as u16);
        if let Some(term) = &mut self.term {
            term.resize(cols, rows);
        }
    }

    /// Spawn the thread that blocking-reads `self.pty_master` and forwards
    /// bytes to the event loop, tagged with the current `pty_generation`.
    fn spawn_pty_reader(&self) {
        let reader_master = Arc::clone(&self.pty_master);
        let proxy = self.proxy.clone();
        let generation = self.pty_generation;
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match nix::unistd::read(reader_master.as_fd(), &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if proxy
                            .send_event(UserEvent::PtyData(generation, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = proxy.send_event(UserEvent::PtyExited(generation));
        });
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("terminal");
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        let palette = Palette::from(&self.config.colors);
        let renderer = Renderer::new(window.clone(), &self.config.font, palette);
        let (cell_w, cell_h) = renderer.cell_size();
        let size = window.inner_size();
        let cols = ((size.width as f32 / cell_w).floor() as usize).max(1);
        let rows = ((size.height as f32 / cell_h).floor() as usize).max(1);

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.term = Some(Term::new(cols, rows, self.config.scrollback_lines));
        pty::resize(self.pty_master.as_fd(), cols as u16, rows as u16);

        self.window.as_ref().unwrap().request_redraw();

        // Only start reading the pty now that `self.term` exists: the
        // shell starts producing output the moment it's forked (in
        // `main`, before the event loop even runs), and any bytes read
        // before `self.term` is `Some` would be silently dropped by
        // `user_event`'s `PtyData` handler -- which used to lose the
        // shell's very first prompt if it arrived before this point,
        // showing nothing until the next keypress produced fresh output.
        // The pty's kernel-side buffer holds onto that early output
        // until we're ready to read it, so nothing is lost by waiting.
        self.spawn_pty_reader();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(generation, bytes) => {
                // Ignore output from a shell session that's since been
                // replaced by `restart_shell` -- its reader thread can
                // still have bytes in flight for a moment after that.
                if generation != self.pty_generation {
                    return;
                }
                if let Some(term) = &mut self.term {
                    term.advance(&bytes);
                }
                self.scroll_offset = 0;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::PtyExited(generation) => {
                if generation != self.pty_generation {
                    // The old shell from before a restart; `restart_shell`
                    // already reaped it directly, and the app shouldn't
                    // quit just because that old session ended.
                    return;
                }
                let _ = nix::sys::wait::waitpid(self.pty_child, None);
                event_loop.exit();
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
                if let Some(term) = &self.term {
                    let bytes = input::encode_key(
                        &event.logical_key,
                        event.text.as_deref(),
                        event.state.is_pressed(),
                        self.modifiers,
                        &term.modes,
                    );
                    if let Some(bytes) = bytes {
                        let _ = nix::unistd::write(self.pty_master.as_fd(), &bytes);
                        self.scroll_offset = 0;
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(term) = &self.term else { return };
                if term.using_alt_screen() {
                    return;
                }
                let (_, cell_h) = self
                    .renderer
                    .as_ref()
                    .map(Renderer::cell_size)
                    .unwrap_or((1.0, 1.0));
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y as f32) / cell_h,
                };
                let max_offset = term.grid().scrollback.len();
                if lines > 0.0 {
                    self.scroll_offset = (self.scroll_offset + lines.ceil() as usize).min(max_offset);
                } else if lines < 0.0 {
                    self.scroll_offset = self.scroll_offset.saturating_sub((-lines).ceil() as usize);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(renderer), Some(term)) = (&mut self.renderer, &self.term) {
                    renderer.render(term, self.scroll_offset);
                }
            }
            _ => {}
        }
    }
}

/// Phase 5: real PTY <-> window wiring. `spawn_shell` must run before the
/// winit event loop is created (see its doc comment for why).
fn main() {
    env_logger::init();

    let config = Config::load();

    let pty_handle = pty::spawn_shell(&config.shell);
    let pty_master = Arc::new(pty_handle.master);
    let pty_child = pty_handle.child;

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
    // `self.term` exists to receive its output -- see the comment there.
    let mut app = App::new(config, pty_master, pty_child, proxy);
    event_loop.run_app(&mut app).expect("event loop error");
}
