mod font;
mod pipeline;

use crate::config::FontConfig;
use crate::term::color::Palette;
use crate::term::grid::CellFlags;
use crate::term::Term;
use font::FontAtlas;
use pipeline::{CellPipeline, Instance};
use std::sync::Arc;
use winit::window::Window;

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: CellPipeline,
    atlas: FontAtlas,
    palette: Palette,
}

impl Renderer {
    pub fn new(window: Arc<Window>, font: &FontConfig, palette: Palette) -> Self {
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

        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface is not supported by the adapter");
        surface.configure(&device, &config);

        // Rasterize at physical pixels (point size * scale factor) so text
        // stays crisp on Retina displays instead of being upscaled/blurry.
        let px_size = font.size.max(1.0) * scale_factor as f32;
        let atlas = FontAtlas::new(px_size, font.family.as_deref());
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

        let pipeline = CellPipeline::new(&device, config.format, &atlas_view);
        pipeline.set_screen_size(&queue, config.width as f32, config.height as f32);

        Renderer {
            surface,
            device,
            queue,
            config,
            pipeline,
            atlas,
            palette,
        }
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_width, self.atlas.cell_height)
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
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

    pub fn render(&mut self, term: &Term, scroll_offset: usize) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.config);
                t
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Full-screen apps (vim, less, htop, ...) manage their own scrolling
        // and don't expect the terminal to scroll their alternate screen.
        let effective_offset = if term.using_alt_screen() { 0 } else { scroll_offset };
        let instances = self.build_instances_from_grid(term, effective_offset);
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
                        load: wgpu::LoadOp::Clear(palette_clear_color(&self.palette)),
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
    }

    fn build_instances_from_grid(&self, term: &Term, scroll_offset: usize) -> Vec<Instance> {
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
                let cell_y = row as f32 * ch;

                instances.push(Instance {
                    pos: [cell_x, cell_y],
                    size: [cw, ch],
                    uv_min: self.atlas.white_uv,
                    uv_max: self.atlas.white_uv,
                    color: rgb_to_color(bg),
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
                            });
                        }
                    }
                }
            }
        }

        if term.modes.show_cursor && scroll_offset == 0 {
            let cursor_x = term.cursor.col as f32 * cw;
            let cursor_y = term.cursor.row as f32 * ch;
            instances.push(Instance {
                pos: [cursor_x, cursor_y],
                size: [cw, ch],
                uv_min: self.atlas.white_uv,
                uv_max: self.atlas.white_uv,
                color: [1.0, 1.0, 1.0, 0.45],
            });
        }

        instances
    }
}

fn rgb_to_color((r, g, b): (u8, u8, u8)) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

fn palette_clear_color(palette: &Palette) -> wgpu::Color {
    let (r, g, b) = palette.background;
    wgpu::Color {
        r: r as f64 / 255.0,
        g: g as f64 / 255.0,
        b: b as f64 / 255.0,
        a: 1.0,
    }
}
