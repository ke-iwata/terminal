pub mod chrome;
mod font;
mod pipeline;

use crate::config::FontConfig;
use crate::tab::Tab;
use crate::term::color::Palette;
use crate::term::grid::CellFlags;
use crate::term::Term;
use font::FontAtlas;
use pipeline::{CellPipeline, Instance};
use std::sync::Arc;
use winit::window::Window;

/// What happened when `Renderer::render` was asked to draw a frame.
pub enum RenderOutcome {
    Presented,
    /// The surface wasn't ready yet (most common right after the window is
    /// first created) -- ask for another redraw immediately.
    Retry,
    /// Not currently visible; nothing to do until a real event (resize,
    /// becoming visible again) prompts a redraw on its own.
    Skipped,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: CellPipeline,
    atlas: FontAtlas,
    palette: Palette,
    /// Window background opacity (0..1). Only background fills respect
    /// this -- glyphs and the cursor are always drawn fully opaque.
    opacity: f32,
}

impl Renderer {
    pub fn new(window: Arc<Window>, font: &FontConfig, palette: Palette, opacity: f32) -> Self {
        let size = window.inner_size();
        let scale_factor = window.scale_factor();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let surface = instance
            .create_surface(window)
            .expect("failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .expect("no suitable GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("terminal device"),
            ..Default::default()
        }))
        .expect("failed to request wgpu device");

        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface is not supported by the adapter");
        // `get_default_config` normally picks an opaque compositing mode.
        // Our shader always writes straight (non-premultiplied) color and
        // alpha, so `PostMultiplied` -- where the compositor does the
        // premultiplication -- is the one mode that actually blends
        // correctly against the desktop; opt into it when the adapter
        // offers it. If it doesn't, `opacity` below the max just won't be
        // visible -- a harmless degradation rather than wrong colors.
        if surface.get_capabilities(&adapter).alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            config.alpha_mode = wgpu::CompositeAlphaMode::PostMultiplied;
        }
        surface.configure(&device, &config);

        // Rasterize at physical pixels (point size * scale factor) so text
        // stays crisp on Retina displays instead of being upscaled/blurry.
        let px_size = font.size.max(1.0) * scale_factor as f32;
        let (atlas, pipeline) = build_atlas_and_pipeline(&device, &queue, config.format, px_size, font.family.as_deref());
        pipeline.set_screen_size(&queue, config.width as f32, config.height as f32);

        Renderer {
            surface,
            device,
            queue,
            config,
            pipeline,
            atlas,
            palette,
            opacity,
        }
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
    }

    /// Rebuild the glyph atlas and cell pipeline for a new font (family
    /// and/or size). `scale_factor` is the window's current
    /// `scale_factor()`, needed to keep glyphs crisp on Retina displays.
    /// The caller is responsible for re-deriving cols/rows from the new
    /// `cell_size()` afterward and resizing the pty/Term to match.
    pub fn set_font(&mut self, font: &FontConfig, scale_factor: f64) {
        let px_size = font.size.max(1.0) * scale_factor as f32;
        let (atlas, pipeline) = build_atlas_and_pipeline(
            &self.device,
            &self.queue,
            self.config.format,
            px_size,
            font.family.as_deref(),
        );
        pipeline.set_screen_size(&self.queue, self.config.width as f32, self.config.height as f32);
        self.atlas = atlas;
        self.pipeline = pipeline;
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.config.width = new_size.width;
        self.config.height = new_size.height;
        self.surface.configure(&self.device, &self.config);
        self.pipeline
            .set_screen_size(&self.queue, self.config.width as f32, self.config.height as f32);
    }

    /// Draw the active tab's grid framed by the tab strip (top) and status
    /// bar (bottom). `tabs`/`active` drive the tab strip's labels and
    /// highlight; `status` is pre-resolved shell/cwd/git/tty info --
    /// process/filesystem lookups have no business happening in the
    /// renderer.
    pub fn render(&mut self, tabs: &[Tab], active: usize, scroll_offset: usize, status: &chrome::StatusInfo) -> RenderOutcome {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            // Not visible right now (minimized, fully covered); don't spin
            // retrying since nothing would be seen anyway. The next resize
            // or occlusion-state change requests a redraw on its own.
            wgpu::CurrentSurfaceTexture::Occluded => return RenderOutcome::Skipped,
            // Transient -- most commonly seen on the very first frame,
            // before the native surface is fully ready. With
            // `ControlFlow::Wait` these used to just silently skip the
            // frame and leave the window blank until some unrelated event
            // (a keypress, a resize) happened to trigger another
            // `request_redraw` -- worth an immediate retry instead so the
            // very first frame shows up on its own.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return RenderOutcome::Retry;
            }
            wgpu::CurrentSurfaceTexture::Validation => return RenderOutcome::Skipped,
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let active_term = &tabs[active].term;
        // Full-screen apps (vim, less, htop, ...) manage their own scrolling
        // and don't expect the terminal to scroll their alternate screen.
        let effective_offset = if active_term.using_alt_screen() { 0 } else { scroll_offset };
        let tab_bar_h = chrome::tab_bar_height(self.atlas.cell_height);
        let status_bar_h = chrome::status_bar_height(self.atlas.cell_height);
        let window_width = self.config.width as f32;
        let window_height = self.config.height as f32;

        let mut instances = self.build_instances_from_grid(active_term, effective_offset, tab_bar_h);

        let titles: Vec<String> = tabs.iter().map(|t| t.title.clone()).collect();
        let tab_layout = chrome::tab_bar_layout(&titles, window_width, self.atlas.cell_width);
        instances.extend(chrome::build_tab_bar_instances(&self.atlas, &self.palette, &tab_layout, active, window_width, tab_bar_h, self.opacity));
        instances.extend(chrome::build_status_bar_instances(&self.atlas, &self.palette, status, window_width, window_height, status_bar_h, self.opacity));

        let instance_count = self
            .pipeline
            .upload_instances(&self.device, &self.queue, &instances);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cell pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(palette_clear_color(&self.palette, self.opacity)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            self.pipeline.draw(&mut pass, instance_count);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface_texture);
        RenderOutcome::Presented
    }

    fn build_instances_from_grid(&self, term: &Term, scroll_offset: usize, y_offset: f32) -> Vec<Instance> {
        let grid = term.grid();
        let (cw, ch) = (self.atlas.cell_width, self.atlas.cell_height);
        let mut instances = Vec::with_capacity(grid.cols * grid.rows * 2);

        for row in 0..grid.rows {
            let line = grid.line_at(row, scroll_offset);
            for (col, cell) in line.iter().enumerate() {
                let reverse = cell.flags.contains(CellFlags::REVERSE);
                let (fg_default, bg_default) = if reverse {
                    (self.palette.background, self.palette.foreground)
                } else {
                    (self.palette.foreground, self.palette.background)
                };
                let (fg, bg) = if reverse {
                    (
                        cell.bg.to_rgb(fg_default, &self.palette),
                        cell.fg.to_rgb(bg_default, &self.palette),
                    )
                } else {
                    (
                        cell.fg.to_rgb(fg_default, &self.palette),
                        cell.bg.to_rgb(bg_default, &self.palette),
                    )
                };

                let cell_x = col as f32 * cw;
                let cell_y = row as f32 * ch + y_offset;

                instances.push(Instance {
                    pos: [cell_x, cell_y],
                    size: [cw, ch],
                    uv_min: self.atlas.white_uv,
                    uv_max: self.atlas.white_uv,
                    color: rgba_to_color(bg, self.opacity),
                    top_corner_radius: 0.0,
                });

                if cell.c != ' ' {
                    if let Some(glyph) = self.atlas.glyph(cell.c) {
                        if glyph.width > 0.0 && glyph.height > 0.0 {
                            let gx = cell_x + glyph.xmin;
                            let gy = cell_y + self.atlas.baseline - glyph.ymin - glyph.height;
                            instances.push(Instance {
                                pos: [gx, gy],
                                size: [glyph.width, glyph.height],
                                uv_min: glyph.uv_min,
                                uv_max: glyph.uv_max,
                                color: rgb_to_color(fg),
                                top_corner_radius: 0.0,
                            });
                        }
                    }
                }
            }
        }

        if term.modes.show_cursor && scroll_offset == 0 {
            let cursor_x = term.cursor.col as f32 * cw;
            let cursor_y = term.cursor.row as f32 * ch + y_offset;
            instances.push(Instance {
                pos: [cursor_x, cursor_y],
                size: [cw, ch],
                uv_min: self.atlas.white_uv,
                uv_max: self.atlas.white_uv,
                color: [1.0, 1.0, 1.0, 0.45],
                top_corner_radius: 0.0,
            });
        }

        instances
    }
}

fn rgba_to_color((r, g, b): (u8, u8, u8), a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Rasterize `family` (or the auto-fallback chain) at `px_size` into a
/// fresh glyph atlas texture and the cell pipeline bound to it. Shared by
/// initial construction and by `Renderer::set_font`'s live font swap.
fn build_atlas_and_pipeline(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    surface_format: wgpu::TextureFormat,
    px_size: f32,
    family: Option<&str>,
) -> (FontAtlas, CellPipeline) {
    let atlas = FontAtlas::new(px_size, family);
    let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph atlas"),
        size: wgpu::Extent3d {
            width: atlas.width,
            height: atlas.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &atlas_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &atlas.pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(atlas.width),
            rows_per_image: Some(atlas.height),
        },
        wgpu::Extent3d {
            width: atlas.width,
            height: atlas.height,
            depth_or_array_layers: 1,
        },
    );
    let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let pipeline = CellPipeline::new(device, surface_format, &atlas_view);
    (atlas, pipeline)
}

fn palette_clear_color(palette: &Palette, opacity: f32) -> wgpu::Color {
    let (r, g, b) = palette.background;
    wgpu::Color {
        r: r as f64 / 255.0,
        g: g as f64 / 255.0,
        b: b as f64 / 255.0,
        a: opacity as f64,
    }
}
