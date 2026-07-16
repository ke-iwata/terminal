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
            .with_title("Terminal Preferences")
            .with_inner_size(winit::dpi::LogicalSize::new(480.0, 620.0));
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
            power_preference: wgpu::PowerPreference::LowPower,
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
        }
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
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
        let raw_input = self.egui_state.take_egui_input(&self.window);

        let draft = &mut self.draft;
        let mut saved_config = None;
        let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            saved_config = build_form(ui, draft);
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
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.12, g: 0.12, b: 0.13, a: 1.0 }),
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

/// Draws the settings form. Returns `Some(config)` the frame the Save
/// button is clicked.
fn build_form(ui: &mut egui::Ui, draft: &mut ConfigDraft) -> Option<Config> {
    let mut saved = None;

    ui.heading("Font");
    egui::Grid::new("font_grid").num_columns(2).show(ui, |ui| {
        ui.label("Family (blank = auto)");
        ui.text_edit_singleline(&mut draft.font_family);
        ui.end_row();

        ui.label("Size");
        ui.add(egui::DragValue::new(&mut draft.font_size).range(6.0..=72.0).speed(0.5));
        ui.end_row();
    });

    ui.add_space(8.0);
    ui.separator();
    ui.heading("Colors");
    egui::Grid::new("base_color_grid").num_columns(2).show(ui, |ui| {
        ui.label("Background");
        ui.color_edit_button_srgb(&mut draft.background);
        ui.end_row();

        ui.label("Foreground");
        ui.color_edit_button_srgb(&mut draft.foreground);
        ui.end_row();
    });

    ui.add_space(4.0);
    ui.label("ANSI palette (0-15)");
    egui::Grid::new("ansi_color_grid").num_columns(8).show(ui, |ui| {
        for (i, slot) in draft.ansi.iter_mut().enumerate() {
            ui.color_edit_button_srgb(slot);
            if i % 8 == 7 {
                ui.end_row();
            }
        }
    });

    ui.add_space(8.0);
    ui.separator();
    ui.heading("Shell");
    egui::Grid::new("shell_grid").num_columns(2).show(ui, |ui| {
        ui.label("Command (blank = $SHELL)");
        ui.text_edit_singleline(&mut draft.shell_command);
        ui.end_row();

        ui.label("Extra args");
        ui.text_edit_singleline(&mut draft.shell_args);
        ui.end_row();
    });

    ui.add_space(8.0);
    ui.separator();
    ui.heading("Scrollback");
    ui.add(
        egui::DragValue::new(&mut draft.scrollback_lines)
            .range(0..=1_000_000)
            .speed(50.0),
    );

    ui.add_space(16.0);
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Save").clicked() {
            let config = draft.to_config();
            match config.save() {
                Ok(()) => draft.status = Some("Saved. Colors and scrollback apply immediately; \
                    font and shell changes take effect on next launch.".to_string()),
                Err(e) => draft.status = Some(format!("Failed to save: {e}")),
            }
            saved = Some(config);
        }
    });
    if let Some(status) = &draft.status {
        ui.label(status);
    }

    saved
}
