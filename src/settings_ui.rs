mod themes;

use crate::config::{ColorConfig, Config, FontConfig, HexColor, ShellConfig};
use egui_wgpu::wgpu;
use std::sync::Arc;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

/// What happened while handling one window event for the settings window.
pub enum SettingsAction {
    None,
    /// The user clicked Save; the config on disk and the returned value are
    /// both up to date. The caller is responsible for applying it live.
    Saved(Config),
    Close,
}

/// An editable, egui-widget-friendly copy of `Config`. Kept separate from
/// `Config` itself so free-typed fields (font family, shell args) don't
/// have to round-trip through `Option`/`Vec` parsing on every keystroke.
struct ConfigDraft {
    font_family: String,
    font_size: f32,
    background: [u8; 3],
    foreground: [u8; 3],
    ansi: [[u8; 3]; 16],
    shell_command: String,
    shell_args: String,
    scrollback_lines: u32,
    opacity: f32,
    status: Option<String>,
}

impl From<&Config> for ConfigDraft {
    fn from(c: &Config) -> Self {
        ConfigDraft {
            font_family: c.font.family.clone().unwrap_or_default(),
            font_size: c.font.size,
            background: hex_to_rgb(c.colors.background),
            foreground: hex_to_rgb(c.colors.foreground),
            ansi: c.colors.ansi.map(hex_to_rgb),
            shell_command: c.shell.command.clone().unwrap_or_default(),
            shell_args: c.shell.args.join(" "),
            scrollback_lines: c.scrollback_lines as u32,
            opacity: c.opacity,
            status: None,
        }
    }
}

impl ConfigDraft {
    fn to_config(&self) -> Config {
        Config {
            font: FontConfig {
                family: non_empty(&self.font_family),
                size: self.font_size.max(1.0),
            },
            colors: ColorConfig {
                background: rgb_to_hex(self.background),
                foreground: rgb_to_hex(self.foreground),
                ansi: self.ansi.map(rgb_to_hex),
            },
            shell: ShellConfig {
                command: non_empty(&self.shell_command),
                args: self.shell_args.split_whitespace().map(String::from).collect(),
            },
            scrollback_lines: self.scrollback_lines as usize,
            opacity: self.opacity.clamp(0.0, 1.0),
        }
    }
}

fn hex_to_rgb(c: HexColor) -> [u8; 3] {
    [c.0, c.1, c.2]
}

fn rgb_to_hex(c: [u8; 3]) -> HexColor {
    HexColor(c[0], c[1], c[2])
}

fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

const ANSI_NAMES: [&str; 16] = [
    "Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "White",
    "Bright Black", "Bright Red", "Bright Green", "Bright Yellow",
    "Bright Blue", "Bright Magenta", "Bright Cyan", "Bright White",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Font,
    Colors,
    Shell,
    Scrollback,
}

impl Category {
    const ALL: [Category; 4] = [Category::Font, Category::Colors, Category::Shell, Category::Scrollback];

    fn title(self) -> &'static str {
        match self {
            Category::Font => "Font",
            Category::Colors => "Colors",
            Category::Shell => "Shell",
            Category::Scrollback => "Scrollback",
        }
    }
}

/// Monospace font family names available on the system, for the Font
/// page's picker. Queried once per settings-window open (CoreText lookups
/// are fast, but there's no need to repeat this every frame).
fn list_monospace_fonts() -> Vec<String> {
    use font_kit::source::SystemSource;

    let source = SystemSource::new();
    let Ok(families) = source.all_families() else {
        return Vec::new();
    };

    let mut names: Vec<String> = families
        .into_iter()
        .filter(|name| {
            source
                .select_family_by_name(name)
                .ok()
                .and_then(|family| family.fonts().first().cloned())
                .and_then(|handle| handle.load().ok())
                .is_some_and(|font| font.is_monospace())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Valid login shells from `/etc/shells`, for the Shell page's picker.
fn list_shells() -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string("/etc/shells") else {
        return Vec::new();
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect()
}

pub struct SettingsWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    draft: ConfigDraft,
    category: Category,
    available_fonts: Vec<String>,
    /// Set while `list_monospace_fonts` is still running on a background
    /// thread. Walking every installed font family through CoreText to
    /// check `is_monospace()` is slow enough (hundreds of families on a
    /// typical Mac) that doing it synchronously on the main thread made
    /// opening Preferences visibly freeze the whole app, not just this
    /// window. Polled once per frame in `redraw`.
    fonts_loading: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    available_shells: Vec<String>,
}

impl SettingsWindow {
    /// Builds an independent wgpu instance/device/surface for this window.
    ///
    /// This is deliberately NOT shared with the main terminal `Renderer`:
    /// `egui-wgpu` 0.35 depends on `wgpu` 29.x while the terminal renderer
    /// uses `wgpu` 30.x. Those are two distinct, incompatible copies of the
    /// crate as far as the type system is concerned, so a `wgpu::Device`
    /// from one cannot be handed to the other. A second small GPU device
    /// for an occasionally-opened settings window is a fine trade for not
    /// having to hold the whole terminal renderer back on an older wgpu.
    pub fn new(event_loop: &ActiveEventLoop, config: &Config) -> Self {
        let attrs = Window::default_attributes()
            .with_title("Preferences")
            .with_inner_size(winit::dpi::LogicalSize::new(680.0, 520.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(520.0, 400.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create settings window"),
        );

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create settings window surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("no suitable GPU adapter for settings window");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("settings device"),
            ..Default::default()
        }))
        .expect("failed to request settings device");

        let size = window.inner_size();
        let mut surface_config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("settings surface is not supported by the adapter");
        // egui-wgpu wants a linear (non-sRGB) format; it does its own gamma
        // handling and will otherwise double-apply the sRGB curve.
        let capabilities = surface.get_capabilities(&adapter);
        if let Some(linear_format) = capabilities.formats.iter().find(|f| !f.is_srgb()) {
            surface_config.format = *linear_format;
        }
        surface.configure(&device, &surface_config);

        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_config.format,
            egui_wgpu::RendererOptions::default(),
        );

        let (fonts_tx, fonts_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = fonts_tx.send(list_monospace_fonts());
        });

        SettingsWindow {
            window,
            surface,
            device,
            queue,
            surface_config,
            egui_ctx,
            egui_state,
            egui_renderer,
            draft: ConfigDraft::from(config),
            category: Category::Font,
            available_fonts: Vec::new(),
            fonts_loading: Some(fonts_rx),
            available_shells: list_shells(),
        }
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Discard unsaved edits and repopulate the form from `config`. Used
    /// when the config is reloaded from disk (menu bar > Reload Config)
    /// while this window happens to be open, so it doesn't keep showing
    /// stale values.
    pub fn reset_draft(&mut self, config: &Config) {
        self.draft = ConfigDraft::from(config);
        self.request_redraw();
    }

    pub fn on_window_event(&mut self, event: &WindowEvent) -> SettingsAction {
        let response = self.egui_state.on_window_event(&self.window, event);
        if response.repaint {
            self.window.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => SettingsAction::Close,
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    self.surface_config.width = size.width;
                    self.surface_config.height = size.height;
                    self.surface.configure(&self.device, &self.surface_config);
                }
                SettingsAction::None
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => SettingsAction::None,
        }
    }

    fn redraw(&mut self) -> SettingsAction {
        if let Some(rx) = &self.fonts_loading {
            match rx.try_recv() {
                Ok(fonts) => {
                    self.available_fonts = fonts;
                    self.fonts_loading = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.fonts_loading = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Still loading; keep polling on the next frame.
                    self.window.request_redraw();
                }
            }
        }

        let raw_input = self.egui_state.take_egui_input(&self.window);

        let category = &mut self.category;
        let draft = &mut self.draft;
        let available_fonts = &self.available_fonts;
        let fonts_loading = self.fonts_loading.is_some();
        let available_shells = &self.available_shells;
        let mut saved_config = None;
        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            draw_sidebar(ui, category);
            draw_footer(ui, draft, &mut saved_config);
            draw_category_page(ui, *category, draft, available_fonts, fonts_loading, available_shells);
        });

        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        for (id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, image_delta);
        }

        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.surface_config);
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return SettingsAction::None;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return SettingsAction::None;
            }
            wgpu::CurrentSurfaceTexture::Validation => return SettingsAction::None,
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("settings encoder"),
            });

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: self.egui_ctx.pixels_per_point(),
        };
        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("settings pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.106, g: 0.106, b: 0.106, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            let mut render_pass = render_pass.forget_lifetime();
            self.egui_renderer.render(&mut render_pass, &paint_jobs, &screen_descriptor);
        }

        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();

        match saved_config {
            Some(config) => SettingsAction::Saved(config),
            None => SettingsAction::None,
        }
    }
}

/// Left-hand category list, VS Code settings style.
fn draw_sidebar(ui: &mut egui::Ui, category: &mut Category) {
    let frame = egui::Frame::side_top_panel(ui.style()).fill(egui::Color32::from_gray(24));
    egui::Panel::left("categories").resizable(false).exact_size(160.0).frame(frame).show(ui, |ui| {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.heading("Preferences");
        });
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        for c in Category::ALL {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                let selected = *category == c;
                if ui
                    .add_sized([136.0, 28.0], egui::Button::selectable(selected, c.title()))
                    .clicked()
                {
                    *category = c;
                }
            });
        }
    });
}

/// Bottom bar: Save button and status text, always visible regardless of
/// which category page is showing or how far it's scrolled.
fn draw_footer(ui: &mut egui::Ui, draft: &mut ConfigDraft, saved: &mut Option<Config>) {
    let frame =
        egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(16, 10));
    egui::Panel::bottom("footer").frame(frame).show(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new("Save").min_size(egui::vec2(80.0, 26.0))).clicked() {
                let config = draft.to_config();
                match config.save() {
                    Ok(()) => {
                        draft.status = Some(
                            "Saved and applied immediately. A changed shell only affects tabs \
                             opened from now on -- already-open tabs keep running as they were."
                                .to_string(),
                        )
                    }
                    Err(e) => draft.status = Some(format!("Failed to save: {e}")),
                }
                *saved = Some(config);
            }
            if let Some(status) = &draft.status {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(status).weak().small());
            }
        });
    });
}

fn draw_category_page(
    ui: &mut egui::Ui,
    category: Category,
    draft: &mut ConfigDraft,
    available_fonts: &[String],
    fonts_loading: bool,
    available_shells: &[String],
) {
    let frame =
        egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(24, 20));
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| match category {
            Category::Font => draw_font_page(ui, draft, available_fonts, fonts_loading),
            Category::Colors => draw_colors_page(ui, draft),
            Category::Shell => draw_shell_page(ui, draft, available_shells),
            Category::Scrollback => draw_scrollback_page(ui, draft),
        });
    });
}

fn page_title(ui: &mut egui::Ui, title: &str) {
    ui.heading(title);
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(12.0);
}

fn field_description(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).weak().small());
    ui.add_space(4.0);
}

fn draw_font_page(ui: &mut egui::Ui, draft: &mut ConfigDraft, available_fonts: &[String], fonts_loading: bool) {
    page_title(ui, "Font");

    ui.label("Family");
    field_description(ui, "Leave blank to use SF Mono, falling back to Menlo.");

    let selected_text = if draft.font_family.is_empty() {
        "Auto".to_string()
    } else {
        draft.font_family.clone()
    };
    ui.add_enabled_ui(!fonts_loading, |ui| {
        egui::ComboBox::from_id_salt("font_family_combo")
            .width(260.0)
            .selected_text(if fonts_loading { "Loading fonts..." } else { &selected_text })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.font_family, String::new(), "Auto");
                for name in available_fonts {
                    ui.selectable_value(&mut draft.font_family, name.clone(), name.as_str());
                }
            });
    });
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut draft.font_family)
            .hint_text("...or type a custom family name")
            .desired_width(260.0),
    );

    ui.add_space(16.0);
    ui.label("Size");
    field_description(ui, "Point size before Retina scaling.");
    ui.add(egui::DragValue::new(&mut draft.font_size).range(6.0..=72.0).speed(0.5).suffix(" pt"));
}

fn draw_colors_page(ui: &mut egui::Ui, draft: &mut ConfigDraft) {
    page_title(ui, "Colors");

    ui.label("Theme");
    field_description(ui, "Applies all colors below at once; tweak any swatch afterward.");
    egui::ComboBox::from_id_salt("theme_combo").width(260.0).selected_text("Choose a preset...").show_ui(
        ui,
        |ui| {
            for (name, palette) in themes::THEMES {
                if ui.selectable_label(false, *name).clicked() {
                    draft.background = [palette.background.0, palette.background.1, palette.background.2];
                    draft.foreground = [palette.foreground.0, palette.foreground.1, palette.foreground.2];
                    draft.ansi = palette.ansi.map(|(r, g, b)| [r, g, b]);
                }
            }
        },
    );
    ui.add_space(16.0);

    egui::Grid::new("base_color_grid").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
        ui.label("Background");
        ui.color_edit_button_srgb(&mut draft.background);
        ui.end_row();

        ui.label("Foreground");
        ui.color_edit_button_srgb(&mut draft.foreground);
        ui.end_row();
    });

    ui.add_space(20.0);
    ui.label("Window Opacity");
    field_description(ui, "How much of the desktop behind the window shows through. Text and the cursor always stay fully opaque.");
    ui.add(egui::Slider::new(&mut draft.opacity, 0.3..=1.0).show_value(true));

    ui.add_space(20.0);
    ui.label("ANSI palette");
    field_description(ui, "The 16 standard/bright colors escape sequences select from.");
    ui.add_space(6.0);

    egui::Grid::new("ansi_color_grid").num_columns(4).spacing([20.0, 12.0]).show(ui, |ui| {
        for (i, (slot, name)) in draft.ansi.iter_mut().zip(ANSI_NAMES).enumerate() {
            ui.horizontal(|ui| {
                ui.color_edit_button_srgb(slot);
                ui.label(name);
            });
            if i % 4 == 3 {
                ui.end_row();
            }
        }
    });
}

fn draw_shell_page(ui: &mut egui::Ui, draft: &mut ConfigDraft, available_shells: &[String]) {
    page_title(ui, "Shell");

    ui.label("Command");
    field_description(ui, "Leave blank to use $SHELL.");

    let selected_text = if draft.shell_command.is_empty() {
        "Auto ($SHELL)".to_string()
    } else {
        draft.shell_command.clone()
    };
    egui::ComboBox::from_id_salt("shell_command_combo")
        .width(260.0)
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut draft.shell_command, String::new(), "Auto ($SHELL)");
            for shell in available_shells {
                ui.selectable_value(&mut draft.shell_command, shell.clone(), shell.as_str());
            }
        });
    ui.add_space(4.0);
    ui.add(
        egui::TextEdit::singleline(&mut draft.shell_command)
            .hint_text("...or type a custom path")
            .desired_width(260.0),
    );

    ui.add_space(16.0);
    ui.label("Extra arguments");
    field_description(ui, "Space-separated, appended after the login-shell argv0.");
    ui.add(
        egui::TextEdit::singleline(&mut draft.shell_args)
            .hint_text("-l")
            .desired_width(260.0),
    );
}

fn draw_scrollback_page(ui: &mut egui::Ui, draft: &mut ConfigDraft) {
    page_title(ui, "Scrollback");

    ui.label("Lines");
    field_description(ui, "How many lines of history to keep above the visible screen.");
    ui.add(
        egui::DragValue::new(&mut draft.scrollback_lines)
            .range(0..=1_000_000)
            .speed(50.0),
    );
}
