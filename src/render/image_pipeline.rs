//! A one-quad textured pipeline for the preview overlay's image.
//!
//! Deliberately separate from `CellPipeline`: that one binds the glyph
//! atlas (a single-channel coverage texture, shared by every quad in the
//! frame) for its whole lifetime, while this binds a full-color texture
//! that is replaced whenever a different file is previewed.

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    rect_pos: [f32; 2],
    rect_size: [f32; 2],
    alpha: f32,
    _pad: f32,
}

pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// One texture per preview tab, keyed by tab id. Keyed rather than
    /// single because a split can put two preview tabs on screen at the
    /// same time, and each needs its own image bound while it draws.
    bind_groups: std::collections::HashMap<u64, wgpu::BindGroup>,
}

impl ImagePipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/image.wgsl").into()),
        });

        // Linear filtering, unlike the atlas's nearest: a preview is
        // almost always scaled to fit, and nearest-neighbour scaling of
        // a photo looks broken rather than crisp.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                // The quad's corners are generated from the vertex index
                // in the shader, so there are no vertex buffers to bind.
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        ImagePipeline {
            pipeline,
            bind_group_layout,
            uniform_buffer,
            sampler,
            bind_groups: std::collections::HashMap::new(),
        }
    }

    /// Upload RGBA8 pixels as the image to draw from now on, replacing
    /// whatever was there. `pixels` must be exactly `width * height * 4`
    /// bytes.
    pub fn set_image(&mut self, tab_id: u64, device: &wgpu::Device, queue: &wgpu::Queue, pixels: &[u8], width: u32, height: u32) {
        if width == 0 || height == 0 || pixels.len() != (width as usize) * (height as usize) * 4 {
            self.bind_groups.remove(&tab_id);
            return;
        }
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Non-sRGB to match the swapchain choice in `Renderer::new`:
            // the shader passes color straight through, so the hardware
            // must not re-encode it on the way in either.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform_buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.bind_groups.insert(tab_id, bind_group);
    }

    /// Drop a tab's texture. Called when a preview tab closes -- one can
    /// be tens of megabytes, and nothing else would ever release it.
    pub fn forget(&mut self, tab_id: u64) {
        self.bind_groups.remove(&tab_id);
    }

    /// Place the image for this frame, in physical pixels.
    pub fn set_rect(&self, queue: &wgpu::Queue, screen: (f32, f32), rect: crate::tab::PaneRect, alpha: f32) {
        let uniforms = Uniforms {
            screen_size: [screen.0, screen.1],
            rect_pos: [rect.x, rect.y],
            rect_size: [rect.w, rect.h],
            alpha,
            _pad: 0.0,
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
    }

    pub fn draw<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>, tab_id: u64) {
        let Some(bind_group) = self.bind_groups.get(&tab_id) else { return };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}
