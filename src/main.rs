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
    PtyData(Vec<u8>),
    PtyExited,
    OpenSettings,
}

struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    term: Option<Term>,
    pty_master: Arc<OwnedFd>,
    pty_child: Pid,
    modifiers: ModifiersState,
    /// How many lines back into scrollback the view is currently scrolled.
    /// 0 means "showing the live bottom of the screen".
    scroll_offset: usize,
    settings_window: Option<SettingsWindow>,
}

impl App {
    fn new(config: Config, pty_master: Arc<OwnedFd>, pty_child: Pid) -> Self {
        App {
            config,
            window: None,
            renderer: None,
            term: None,
            pty_master,
            pty_child,
            modifiers: ModifiersState::empty(),
            scroll_offset: 0,
            settings_window: None,
        }
    }

    /// Apply a saved config live where it's cheap and safe to (colors,
    /// scrollback). Font and shell changes are picked up on next launch --
    /// rebuilding the glyph atlas or restarting the shell mid-session is
    /// out of scope for this iteration.
    fn apply_saved_config(&mut self, config: Config) {
        let palette = Palette::from(&config.colors);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_palette(palette);
        }
        if let Some(term) = &mut self.term {
            term.set_scrollback_limit(config.scrollback_lines);
        }
        self.config = config;
        if let Some(window) = &self.window {
            window.request_redraw();
        }
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
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyData(bytes) => {
                if let Some(term) = &mut self.term {
                    term.advance(&bytes);
                }
                self.scroll_offset = 0;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::PtyExited => {
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
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if let Some(settings) = &mut self.settings_window {
            if window_id == settings.window_id() {
                match settings.on_window_event(&event) {
                    SettingsAction::None => {}
                    SettingsAction::Saved(config) => self.apply_saved_config(config),
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

    menu::install(proxy.clone());

    let reader_master = Arc::clone(&pty_master);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match nix::unistd::read(reader_master.as_fd(), &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if proxy.send_event(UserEvent::PtyData(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = proxy.send_event(UserEvent::PtyExited);
    });

    let mut app = App::new(config, pty_master, pty_child);
    event_loop.run_app(&mut app).expect("event loop error");
}
