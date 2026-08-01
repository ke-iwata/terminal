pub mod chrome;
mod font;
mod image_pipeline;
mod pipeline;

use crate::config::FontConfig;
use crate::linkify;
use crate::tab::{Search, Selection};
use crate::term::color::Palette;
use crate::term::grid::CellFlags;
use crate::term::Term;
use font::FontAtlas;
use pipeline::{CellPipeline, Instance};
use std::sync::Arc;
use winit::window::Window;

/// A cell's resolved foreground and background, after the palette and
/// any reverse-video flag have been applied.
type CellColors = ((u8, u8, u8), (u8, u8, u8));

/// Per-tab overlay state that affects how the grid is drawn, bundled into
/// one struct purely to keep `render`/`build_instances_from_grid`'s
/// argument lists from growing without bound as more of these are added.
struct GridOverlays<'a> {
    selection: Option<&'a Selection>,
    search: Option<&'a Search>,
    /// Whether Cmd is currently held -- URLs only get underlined while
    /// it is, mirroring Terminal.app/iTerm2's "reveal links" gesture.
    cmd_held: bool,
}

/// Everything the sidebar needs to draw itself, borrowed from `App`'s
/// `FileTree` for the duration of one frame. `Some` means visible -- the
/// renderer never decides that on its own.
pub struct FileTreeView<'a> {
    /// The rooted folder's own name, shown uppercase as the section
    /// title the way VS Code labels the open workspace.
    pub title: &'a str,
    pub rows: &'a [crate::filetree::Row],
    pub scroll: usize,
    pub show_hidden: bool,
    /// The user's dragged sidebar width in pixels; zero means the
    /// default. Clamped by `chrome::file_tree_width`.
    pub width: f32,
    /// Index into `rows` of whatever the pointer is over, for the hover
    /// band.
    pub hover: Option<usize>,
    /// Index into `rows` of the last-clicked entry, kept highlighted so
    /// the list shows what you acted on -- VS Code's selected row.
    pub selected: Option<usize>,
}

/// Which body the overlay draws for a preview's current state. Images
/// carry no data here -- their pixels live in a GPU texture keyed by tab
/// id, drawn after all instances.
fn preview_body(preview: &crate::preview::Preview) -> chrome::PreviewBody<'_> {
    match &preview.state {
        crate::preview::State::Loading => chrome::PreviewBody::Loading,
        crate::preview::State::Failed(message) => chrome::PreviewBody::Failed(message),
        crate::preview::State::Ready(crate::preview::Content::Text(lines)) => chrome::PreviewBody::Text { lines, scroll: preview.scroll },
        crate::preview::State::Ready(crate::preview::Content::Image { .. }) => chrome::PreviewBody::Image,
    }
}

/// What happened when `Renderer::render` was asked to draw a frame.
///
/// This exists because silently skipping a failed frame caused the
/// "blank window until first keypress" startup bug -- with
/// `ControlFlow::Wait`, a dropped first frame is never retried unless
/// the caller is told about it. See `App::presented_once` in `main.rs`
/// for the full failure story and the other layers of that fix; keep
/// the two in sync when changing anything here.
pub enum RenderOutcome {
    Presented,
    /// The surface wasn't ready yet (most common right after the window is
    /// first created) -- the caller should request another redraw
    /// immediately.
    Retry,
    /// Not currently visible (occluded/minimized); retrying now would
    /// draw to nothing. The caller must redraw when visibility returns
    /// (`WindowEvent::Occluded(false)`) instead.
    Skipped,
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: CellPipeline,
    image_pipeline: image_pipeline::ImagePipeline,
    atlas: FontAtlas,
    atlas_texture: wgpu::Texture,
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
        // Prefer a non-sRGB swapchain format: the shader passes color
        // values through untouched, and ours are already sRGB-encoded
        // (straight from `#rrggbb` config values). On an `*Srgb` format
        // the hardware would re-encode them on write -- treating them as
        // linear -- which visibly washes out every mid-tone (e.g. a
        // #17181c chrome rendered as ~#545454 gray).
        let caps = surface.get_capabilities(&adapter);
        if let Some(format) = caps.formats.iter().copied().find(|f| !f.is_srgb()) {
            config.format = format;
        }
        // `get_default_config` normally picks an opaque compositing mode.
        // Our shader always writes straight (non-premultiplied) color and
        // alpha, so `PostMultiplied` -- where the compositor does the
        // premultiplication -- is the one mode that actually blends
        // correctly against the desktop; opt into it when the adapter
        // offers it. If it doesn't, `opacity` below the max just won't be
        // visible -- a harmless degradation rather than wrong colors.
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            config.alpha_mode = wgpu::CompositeAlphaMode::PostMultiplied;
        }
        surface.configure(&device, &config);

        // Rasterize at physical pixels (point size * scale factor) so text
        // stays crisp on Retina displays instead of being upscaled/blurry.
        let px_size = font.size.max(1.0) * scale_factor as f32;
        let (atlas, atlas_texture, pipeline) = build_atlas_and_pipeline(&device, config.format, px_size, font.family.as_deref());
        pipeline.set_screen_size(&queue, config.width as f32, config.height as f32);

        let image_pipeline = image_pipeline::ImagePipeline::new(&device, config.format);

        Renderer {
            surface,
            device,
            queue,
            config,
            pipeline,
            image_pipeline,
            atlas,
            atlas_texture,
            palette,
            opacity,
        }
    }

    /// Copy every glyph rasterized while building this frame's instances
    /// into the atlas texture. Must run after the instances are built
    /// (that's what discovers which characters are needed) and before the
    /// draw, or a character seen for the first time renders blank for a
    /// frame.
    fn flush_atlas_uploads(&self) {
        for upload in self.atlas.take_pending_uploads() {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: upload.x, y: upload.y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &upload.coverage,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(upload.width),
                    rows_per_image: Some(upload.height),
                },
                wgpu::Extent3d { width: upload.width, height: upload.height, depth_or_array_layers: 1 },
            );
        }
    }

    /// Give a preview tab its decoded image. Kept per tab id, since a
    /// split can show two previews at once.
    pub fn set_preview_image(&mut self, tab_id: u64, pixels: &[u8], width: u32, height: u32) {
        self.image_pipeline.set_image(tab_id, &self.device, &self.queue, pixels, width, height);
    }

    /// Release a closed preview tab's texture -- it can be tens of
    /// megabytes, and nothing else would free it.
    pub fn forget_preview_image(&mut self, tab_id: u64) {
        self.image_pipeline.forget(tab_id);
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
        let (atlas, atlas_texture, pipeline) = build_atlas_and_pipeline(
            &self.device,
            self.config.format,
            px_size,
            font.family.as_deref(),
        );
        pipeline.set_screen_size(&self.queue, self.config.width as f32, self.config.height as f32);
        self.atlas = atlas;
        self.atlas_texture = atlas_texture;
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

    /// Draw the active tab's panes framed by the tab strip (top) and
    /// status bar (bottom). `tabs`/`active` drive the tab strip's labels
    /// and highlight; `status` is pre-resolved shell/cwd/git/tty info --
    /// process/filesystem lookups have no business happening in the
    /// renderer.
    pub fn render(
        &mut self,
        root: &crate::tab::GroupNode,
        focused_group: u64,
        status: &chrome::StatusInfo,
        cmd_held: bool,
        file_tree: Option<FileTreeView>,
        preedit: &str,
    ) -> RenderOutcome {
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

        let tab_bar_h = chrome::tab_bar_height(self.atlas.cell_height);
        let status_bar_h = chrome::status_bar_height(self.atlas.cell_height);
        let window_width = self.config.width as f32;
        let window_height = self.config.height as f32;

        let sidebar_width = file_tree
            .as_ref()
            .map_or(0.0, |v| chrome::file_tree_width(true, v.width, self.atlas.cell_width, window_width));
        let grid_rect = chrome::grid_rect(window_width, window_height, self.atlas.cell_height, sidebar_width);
        let mut path = Vec::new();
        let mut group_rects = Vec::new();
        let mut dividers = Vec::new();
        root.layout(grid_rect, chrome::PANE_GAP, &mut path, &mut group_rects, &mut dividers);
        let divider_rects: Vec<crate::tab::PaneRect> = dividers.iter().map(|d| d.rect).collect();

        let mut instances = chrome::build_divider_instances(&self.atlas, &divider_rects);
        // Each group draws its own tab strip and, below it, whichever of
        // its tabs is active. Preview images are collected rather than
        // drawn here: they belong to a different pipeline, which runs
        // after all of these instances.
        let mut preview_draws = Vec::new();
        for (group_id, rect) in &group_rects {
            let Some(group) = root.group(*group_id) else { continue };
            let strip = crate::tab::PaneRect { x: rect.x, y: rect.y, w: rect.w, h: tab_bar_h };
            let content = crate::tab::PaneRect {
                x: rect.x,
                y: rect.y + tab_bar_h,
                w: rect.w,
                h: (rect.h - tab_bar_h).max(1.0),
            };
            let group_focused = *group_id == focused_group;

            let titles: Vec<String> = group.tabs().iter().map(|t| t.title().to_string()).collect();
            let tab_layout = chrome::tab_bar_layout(&titles, strip, self.atlas.cell_width);
            instances.extend(chrome::build_tab_bar_instances(&self.atlas, &tab_layout, group.active_index(), strip, group_focused));

            let tab = group.active_tab();
            match &tab.kind {
                crate::tab::TabKind::Shell(pane) => {
                    // Full-screen apps (vim, less, htop, ...) manage
                    // their own scrolling and don't expect the terminal
                    // to scroll their alternate screen.
                    let effective_offset = if pane.term.using_alt_screen() { 0 } else { pane.scroll_offset };
                    let overlays = GridOverlays {
                        selection: pane.selection.as_ref(),
                        search: pane.search.as_ref(),
                        cmd_held,
                    };
                    instances.extend(self.build_instances_from_pane(&pane.term, effective_offset, content, &overlays, group_focused));
                    if group_focused {
                        if let Some(search) = pane.search.as_ref() {
                            instances.extend(chrome::build_search_bar_instances(&self.atlas, search, content, self.atlas.cell_height));
                        }
                        // Only the focused group can be composing.
                        let cursor = (
                            content.x + pane.term.cursor.col as f32 * self.atlas.cell_width,
                            content.y + pane.term.cursor.row as f32 * self.atlas.cell_height,
                        );
                        instances.extend(chrome::build_preedit_instances(&self.atlas, preedit, cursor, content));
                    }
                }
                crate::tab::TabKind::Preview(preview) => {
                    let layout = chrome::preview_layout(content, self.atlas.cell_height);
                    let body = preview_body(preview);
                    instances.extend(chrome::build_preview_instances(&self.atlas, &layout, &preview.subtitle(), &body));
                    if let crate::preview::State::Ready(crate::preview::Content::Image { width, height }) = &preview.state {
                        preview_draws.push((tab.id, chrome::preview_image_rect(layout.content, *width, *height)));
                    }
                }
            }

            // Only worth pointing out which group is focused when there
            // is more than one to choose between.
            if group_rects.len() > 1 && group_focused {
                instances.extend(chrome::build_focus_border_instances(&self.atlas, content));
            }
        }

        if let Some(view) = &file_tree {
            if let Some(rect) = chrome::file_tree_rect(window_width, window_height, self.atlas.cell_width, self.atlas.cell_height, true, view.width) {
                instances.extend(chrome::build_file_tree_instances(&self.atlas, rect, view));
            }
        }

        instances.extend(chrome::build_status_bar_instances(&self.atlas, status, window_width, window_height, status_bar_h));

        // Building the instances above is what asked the atlas for every
        // character on screen; anything new has to reach the texture
        // before the draw below reads from it.
        self.flush_atlas_uploads();

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
            // Last, so each decoded image lands on top of its own tab's
            // backdrop. Every rect is inside a group's content area, so
            // none can reach the bars drawn above.
            for (tab_id, rect) in &preview_draws {
                self.image_pipeline.set_rect(&self.queue, (window_width, window_height), *rect, 1.0);
                self.image_pipeline.draw(&mut pass, *tab_id);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(surface_texture);
        RenderOutcome::Presented
    }

    fn build_instances_from_pane(&self, term: &Term, scroll_offset: usize, rect: crate::tab::PaneRect, overlays: &GridOverlays, focused: bool) -> Vec<Instance> {
        let grid = term.grid();
        let (cw, ch) = (self.atlas.cell_width, self.atlas.cell_height);
        let mut instances = Vec::with_capacity(grid.cols * grid.rows * 2);

        for row in 0..grid.rows {
            let line = grid.line_at(row, scroll_offset);
            let distance = grid.distance_from_bottom(row, scroll_offset);
            let selected_cols = overlays.selection.and_then(|s| s.columns_on_line(distance, grid.cols));
            let search_ranges = overlays.search.map(|s| s.ranges_on_line(distance)).unwrap_or_default();
            // Recomputing per visible row (not once for the whole
            // scrollback) so this costs nothing unless Cmd is actually
            // held right now.
            let url_ranges = if overlays.cmd_held {
                let text: String = line.iter().map(|c| c.c).collect();
                linkify::find_urls(&text)
            } else {
                Vec::new()
            };
            // Three passes over the row rather than one, because a
            // wide (CJK) glyph spills into the cell after it: drawing
            // background-then-glyph per cell meant the *next* cell's
            // background painted over the right half of the glyph before
            // it. Backgrounds first, then glyphs on top of all of them,
            // then the translucent tints on top of everything.
            let colors: Vec<CellColors> = line
                .iter()
                .map(|cell| {
                    let reverse = cell.flags.contains(CellFlags::REVERSE);
                    let (fg_default, bg_default) = if reverse {
                        (self.palette.background, self.palette.foreground)
                    } else {
                        (self.palette.foreground, self.palette.background)
                    };
                    if reverse {
                        (
                            cell.bg.to_rgb(fg_default, &self.palette),
                            cell.fg.to_rgb(bg_default, &self.palette),
                        )
                    } else {
                        (
                            cell.fg.to_rgb(fg_default, &self.palette),
                            cell.bg.to_rgb(bg_default, &self.palette),
                        )
                    }
                })
                .collect();

            let cell_y = rect.y + row as f32 * ch;
            for (col, (_, bg)) in colors.iter().enumerate() {
                instances.push(Instance {
                    pos: [rect.x + col as f32 * cw, cell_y],
                    size: [cw, ch],
                    uv_min: self.atlas.white_uv,
                    uv_max: self.atlas.white_uv,
                    color: rgba_to_color(*bg, self.opacity),
                    top_corner_radius: 0.0,
                });
            }

            for (col, cell) in line.iter().enumerate() {
                if cell.c == ' ' {
                    continue;
                }
                // The trailing half of a wide character carries a
                // placeholder space, and drawing it would just be a
                // second blank quad over the glyph beside it.
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                let Some(glyph) = self.atlas.glyph(cell.c) else { continue };
                if glyph.width <= 0.0 || glyph.height <= 0.0 {
                    continue;
                }
                let gx = rect.x + col as f32 * cw + glyph.xmin;
                let gy = cell_y + self.atlas.baseline - glyph.ymin - glyph.height;
                instances.push(Instance {
                    pos: [gx, gy],
                    size: [glyph.width, glyph.height],
                    uv_min: glyph.uv_min,
                    uv_max: glyph.uv_max,
                    color: rgb_to_color(colors[col].0),
                    top_corner_radius: 0.0,
                });
            }

            for col in 0..line.len() {
                let cell_x = rect.x + col as f32 * cw;
                // Tinted on top of the cell's own background/glyph (not
                // baked into `bg` above) so it reads the same regardless
                // of the cell's own colors, and at a fixed alpha
                // independent of `self.opacity` -- a selection you can't
                // see against a transparent window isn't useful.
                if selected_cols.is_some_and(|(from, to)| col >= from && col <= to) {
                    instances.push(Instance {
                        pos: [cell_x, cell_y],
                        size: [cw, ch],
                        uv_min: self.atlas.white_uv,
                        uv_max: self.atlas.white_uv,
                        color: [0.35, 0.55, 0.9, 0.4],
                        top_corner_radius: 0.0,
                    });
                }

                // The current match gets a brighter tint than the rest so
                // "where am I" is obvious at a glance while stepping
                // through results.
                if let Some(&(_, _, is_current)) = search_ranges.iter().find(|(from, to, _)| col >= *from && col <= *to) {
                    let color = if is_current { [1.0, 0.65, 0.0, 0.55] } else { [0.85, 0.7, 0.15, 0.35] };
                    instances.push(Instance {
                        pos: [cell_x, cell_y],
                        size: [cw, ch],
                        uv_min: self.atlas.white_uv,
                        uv_max: self.atlas.white_uv,
                        color,
                        top_corner_radius: 0.0,
                    });
                }

                if url_ranges.iter().any(|(from, to)| col >= *from && col <= *to) {
                    instances.push(Instance {
                        pos: [cell_x, cell_y + ch - 2.0],
                        size: [cw, 1.5],
                        uv_min: self.atlas.white_uv,
                        uv_max: self.atlas.white_uv,
                        color: [0.45, 0.7, 1.0, 0.9],
                        top_corner_radius: 0.0,
                    });
                }
            }
        }

        if term.modes.show_cursor && scroll_offset == 0 {
            let cursor_x = rect.x + term.cursor.col as f32 * cw;
            let cursor_y = rect.y + term.cursor.row as f32 * ch;
            // Unfocused panes keep a faint cursor: enough to see where
            // each shell is sitting, unmistakably different from the pane
            // that will actually receive the next keystroke. Thin shapes
            // (bar/underline) get a stronger alpha -- at block's 0.45
            // a 2px sliver would be nearly invisible.
            let (pos, size, alpha) = match term.cursor_shape {
                crate::term::CursorShape::Block => ([cursor_x, cursor_y], [cw, ch], if focused { 0.45 } else { 0.18 }),
                crate::term::CursorShape::Underline => ([cursor_x, cursor_y + ch - 2.0], [cw, 2.0], if focused { 0.9 } else { 0.35 }),
                crate::term::CursorShape::Bar => ([cursor_x, cursor_y], [2.0, ch], if focused { 0.9 } else { 0.35 }),
            };
            instances.push(Instance {
                pos,
                size,
                uv_min: self.atlas.white_uv,
                uv_max: self.atlas.white_uv,
                color: [1.0, 1.0, 1.0, alpha],
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
    surface_format: wgpu::TextureFormat,
    px_size: f32,
    family: Option<&str>,
) -> (FontAtlas, wgpu::Texture, CellPipeline) {
    let atlas = FontAtlas::new(px_size, family);
    // Allocated empty and filled in as glyphs are rasterized (see
    // `flush_atlas_uploads`). The texture is bound to the pipeline for
    // its whole life, so its size can't change -- which is why the atlas
    // picks a fixed one big enough to never need to.
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
    let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let pipeline = CellPipeline::new(device, surface_format, &atlas_view);
    (atlas, atlas_texture, pipeline)
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
