use std::sync::Arc;

use winit::window::Window;

use crate::{AtlasCfg, Frame, Viewport};

/// The hires texture carries a small mip chain so the selected tile can
/// show the quickview-resolution stream downscaled without shimmering.
const HIRES_MIP_LEVELS: u32 = 5;

/// Fullscreen-triangle blit used to fill each hires mip from the previous.
const BLIT_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let xy = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.pos = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    border: [f32; 4],
    radius: f32,
    border_width: f32,
    tex_mix: f32,
    frame_fade: f32,
    uv: [f32; 4],
    uv2: [f32; 4],
    tex_source: f32,
    pie: f32,
    _pad: [f32; 2],
}

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    atlas: wgpu::Texture,
    hires: wgpu::Texture,
    blit_pipeline: wgpu::RenderPipeline,
    hires_mip_views: Vec<wgpu::TextureView>,
    hires_mip_bgs: Vec<wgpu::BindGroup>,
    atlas_cfg: AtlasCfg,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    sampler: wgpu::Sampler,
    blit_bgl: wgpu::BindGroupLayout,
    /// Downsample blit targeting the surface format (backdrop mips).
    backdrop_pipeline: wgpu::RenderPipeline,
    /// Fullscreen draw of a backdrop mip, faded in via the blend constant.
    present_pipeline: wgpu::RenderPipeline,
    backdrop: Backdrop,
}

/// Offscreen copy of the grid layer plus its downsample chain — mip N is
/// the frosted image the quickview backdrop samples. Window-sized;
/// rebuilt on resize.
struct Backdrop {
    views: Vec<wgpu::TextureView>,
    /// Sample mip i-1 while rendering mip i.
    down_bgs: Vec<wgpu::BindGroup>,
    /// Sample mip i for the fullscreen blurred layer.
    present_bgs: Vec<wgpu::BindGroup>,
}

/// Deepest backdrop downsample (level 4 = 1/16 resolution).
const BACKDROP_MIP_LEVELS: u32 = 5;

impl Backdrop {
    fn new(
        device: &wgpu::Device,
        blit_bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let (w, h) = (width.max(2), height.max(2));
        let mips = (32 - w.max(h).leading_zeros()).min(BACKDROP_MIP_LEVELS);
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("quickview backdrop"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let views: Vec<wgpu::TextureView> = (0..mips)
            .map(|i| {
                tex.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: i,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let bg = |view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("backdrop mip"),
                layout: blit_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        };
        let down_bgs = (1..mips as usize).map(|i| bg(&views[i - 1])).collect();
        let present_bgs = (0..mips as usize).map(|i| bg(&views[i])).collect();
        Self {
            views,
            down_bgs,
            present_bgs,
        }
    }
}

impl Gpu {
    pub async fn new(window: Arc<Window>, atlas_cfg: AtlasCfg) -> anyhow::Result<Self> {
        let size = window.inner_size();
        // No display handle: only needed for GLES/Wayland presentation,
        // and it lives on the event loop, not the window.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        // wgpu's default limits cap textures at 8192² — half the slot
        // count the atlas can use on modern GPUs (Apple silicon and
        // desktop GPUs all do 16384²). Ask for what the adapter has.
        let mut limits = wgpu::Limits::default();
        limits.max_texture_dimension_2d =
            adapter.limits().max_texture_dimension_2d.clamp(8192, 16384);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_limits: limits,
                ..Default::default()
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            // Auto = wgpu's historical color-space choice (sRGB here).
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tiles"),
            source: wgpu::ShaderSource::Wgsl(include_str!("tiles.wgsl").into()),
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumb atlas"),
            size: wgpu::Extent3d {
                width: atlas_cfg.tex_w(),
                height: atlas_cfg.tex_h(),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());

        let hires_mips = (32
            - atlas_cfg
                .hires_w
                .max(atlas_cfg.hires_h)
                .max(2)
                .leading_zeros())
        .min(HIRES_MIP_LEVELS);
        let hires = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("hires (selected stream / quickview)"),
            size: wgpu::Extent3d {
                width: atlas_cfg.hires_w.max(2),
                height: atlas_cfg.hires_h.max(2),
                depth_or_array_layers: 1,
            },
            mip_level_count: hires_mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let hires_view = hires.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("thumb sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tiles"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tiles"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&hires_view),
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tiles"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2,
                1 => Float32x2,
                2 => Float32x4,
                3 => Float32x4,
                4 => Float32,
                5 => Float32,
                6 => Float32,
                7 => Float32,
                8 => Float32x4,
                9 => Float32x4,
                10 => Float32,
                11 => Float32,
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tiles"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(instance_layout)],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Mip-downsample pipeline for the hires texture (one blit per level
        // after each video frame upload — trivial GPU cost).
        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hires mip blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit"),
            bind_group_layouts: &[Some(&blit_bgl)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("hires mip blit"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let hires_mip_views: Vec<wgpu::TextureView> = (0..hires_mips)
            .map(|i| {
                hires.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: i,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let hires_mip_bgs: Vec<wgpu::BindGroup> = (1..hires_mips as usize)
            .map(|i| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("hires mip"),
                    layout: &blit_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&hires_mip_views[i - 1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                    ],
                })
            })
            .collect();

        // Backdrop blur: the same blit shader as the hires mips, but
        // targeting the surface format — one pipeline to fill the
        // downsample chain, one (blend-constant faded) to draw the blurred
        // layer back fullscreen.
        let backdrop_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop mip blit"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let fade_blend = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Constant,
            dst_factor: wgpu::BlendFactor::OneMinusConstant,
            operation: wgpu::BlendOperation::Add,
        };
        let present_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("backdrop present"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState {
                        color: fade_blend,
                        alpha: fade_blend,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let backdrop = Backdrop::new(
            &device,
            &blit_bgl,
            &sampler,
            config.format,
            config.width,
            config.height,
        );

        // Residency telemetry (PERFORMANCE-TASKS.md P0.1): report what
        // this startup reserved on the GPU so atlas sizing (P0.5) is
        // measured, not guessed. All three are RGBA/BGRA (4 B/px).
        let mip_bytes = |w: u32, h: u32, mips: u32| -> u64 {
            (0..mips)
                .map(|l| u64::from((w >> l).max(1)) * u64::from((h >> l).max(1)) * 4)
                .sum()
        };
        const MIB: u64 = 1024 * 1024;
        let atlas_bytes = u64::from(atlas_cfg.tex_w()) * u64::from(atlas_cfg.tex_h()) * 4;
        let hires_bytes = mip_bytes(
            atlas_cfg.hires_w.max(2),
            atlas_cfg.hires_h.max(2),
            hires_mips,
        );
        let (bw, bh) = (config.width.max(2), config.height.max(2));
        let backdrop_bytes = mip_bytes(
            bw,
            bh,
            (32 - bw.max(bh).leading_zeros()).min(BACKDROP_MIP_LEVELS),
        );
        log::info!(
            "gpu residency: atlas {} MiB ({} slots of {}x{}) + hires {} MiB + backdrop {} MiB = {} MiB",
            atlas_bytes / MIB,
            atlas_cfg.slots(),
            atlas_cfg.slot_w,
            atlas_cfg.slot_h,
            hires_bytes / MIB,
            backdrop_bytes / MIB,
            (atlas_bytes + hires_bytes + backdrop_bytes) / MIB,
        );
        if atlas_bytes > 512 * MIB {
            log::warn!(
                "atlas reserves {} MiB for {} slots — verify visible+prefetch demand actually \
                 needs this (smaller atlas_width/height in switchblade.toml frees the rest; \
                 PERFORMANCE-TASKS.md P0.5)",
                atlas_bytes / MIB,
                atlas_cfg.slots(),
            );
        }

        let instance_capacity = 1024;
        let instances = Self::make_instance_buffer(&device, instance_capacity);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            uniforms,
            bind_group,
            atlas,
            hires,
            blit_pipeline,
            hires_mip_views,
            hires_mip_bgs,
            atlas_cfg,
            instances,
            instance_capacity,
            sampler,
            blit_bgl,
            backdrop_pipeline,
            present_pipeline,
            backdrop,
        })
    }

    fn make_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (std::mem::size_of::<Instance>() * capacity) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn upload_thumb(&self, up: &crate::ThumbUpload) {
        let cfg = &self.atlas_cfg;
        let ok = up.slot < cfg.slots()
            && up.w >= 1
            && up.w <= cfg.slot_w
            && up.h >= 1
            && up.h <= cfg.slot_h
            && up.rgba.len() == (up.w * up.h * 4) as usize;
        if !ok {
            log::warn!(
                "bad thumb upload: slot {}, {}x{}, {} bytes",
                up.slot,
                up.w,
                up.h,
                up.rgba.len()
            );
            return;
        }
        let (col, row) = (up.slot as u32 % cfg.cols, up.slot as u32 / cfg.cols);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: col * cfg.slot_w,
                    y: row * cfg.slot_h,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &up.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(up.w * 4),
                rows_per_image: Some(up.h),
            },
            wgpu::Extent3d {
                width: up.w,
                height: up.h,
                depth_or_array_layers: 1,
            },
        );
    }

    fn upload_hires(&self, hf: &crate::HiresFrame) -> bool {
        let ok = hf.w >= 1
            && hf.w <= self.atlas_cfg.hires_w
            && hf.h >= 1
            && hf.h <= self.atlas_cfg.hires_h
            && hf.rgba.len() == (hf.w * hf.h * 4) as usize;
        if !ok {
            log::warn!(
                "bad hires upload: {}x{}, {} bytes",
                hf.w,
                hf.h,
                hf.rgba.len()
            );
            return false;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.hires,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &hf.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(hf.w * 4),
                rows_per_image: Some(hf.h),
            },
            wgpu::Extent3d {
                width: hf.w,
                height: hf.h,
                depth_or_array_layers: 1,
            },
        );
        true
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.backdrop = Backdrop::new(
            &self.device,
            &self.blit_bgl,
            &self.sampler,
            self.config.format,
            width,
            height,
        );
    }

    pub fn render(&mut self, frame: &Frame, viewport: Viewport) {
        for up in &frame.uploads {
            self.upload_thumb(up);
        }
        let hires_dirty = if let Some(hf) = &frame.hires_upload {
            self.upload_hires(hf)
        } else {
            false
        };

        let data: Vec<Instance> = frame
            .tiles
            .iter()
            .map(|t| Instance {
                pos: [t.x, t.y],
                size: [t.w, t.h],
                color: t.color,
                border: t.border_color,
                radius: t.corner_radius,
                border_width: t.border_width,
                tex_mix: t.tex_mix,
                frame_fade: t.frame_fade,
                uv: t.uv,
                uv2: t.uv2,
                tex_source: t.hires as u8 as f32,
                pie: t.pie,
                _pad: [0.0; 2],
            })
            .collect();

        if data.len() > self.instance_capacity {
            self.instance_capacity = data.len().next_power_of_two();
            self.instances = Self::make_instance_buffer(&self.device, self.instance_capacity);
        }
        if !data.is_empty() {
            self.queue
                .write_buffer(&self.instances, 0, bytemuck::cast_slice(&data));
        }
        self.queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::bytes_of(&Uniforms {
                viewport: [viewport.width, viewport.height],
                _pad: [0.0; 2],
            }),
        );

        let surface_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            // Minimized/hidden window — nothing to draw into, not an error.
            wgpu::CurrentSurfaceTexture::Occluded => return,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Validation => {
                log::warn!("no surface texture this frame");
                return;
            }
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        // Refresh the hires mip chain after a video-frame upload (queued
        // writes land before this command buffer executes).
        if hires_dirty {
            for i in 1..self.hires_mip_views.len() {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("hires mip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.hires_mip_views[i],
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.blit_pipeline);
                pass.set_bind_group(0, &self.hires_mip_bgs[i - 1], &[]);
                pass.draw(0..3, 0..1);
            }
        }
        // Frosted backdrop: render the grid layer offscreen, walk it down
        // the mip chain, then (in the main pass) draw the blurred mip back
        // fullscreen between the grid and the overlay tiles.
        let blur = frame
            .blur
            .filter(|b| b.split > 0 && b.split <= data.len() && b.levels > 0 && b.fade > 0.0);
        let blur = blur.map(|b| {
            let level = (b.levels as usize).min(self.backdrop.views.len() - 1);
            (b.split as u32, level, b.fade.min(1.0) as f64)
        });
        if let Some((split, level, _)) = blur {
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("backdrop grid"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.backdrop.views[0],
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: frame.clear[0] as f64,
                                g: frame.clear[1] as f64,
                                b: frame.clear[2] as f64,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..6, 0..split);
            }
            for i in 1..=level {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("backdrop mip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.backdrop.views[i],
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.backdrop_pipeline);
                pass.set_bind_group(0, &self.backdrop.down_bgs[i - 1], &[]);
                pass.draw(0..3, 0..1);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tiles"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: frame.clear[0] as f64,
                            g: frame.clear[1] as f64,
                            b: frame.clear[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            match blur {
                Some((split, level, fade)) => {
                    // Sharp grid (visible while the blur fades in), the
                    // blurred layer over it, then the overlay tiles.
                    pass.draw(0..6, 0..split);
                    pass.set_pipeline(&self.present_pipeline);
                    pass.set_bind_group(0, &self.backdrop.present_bgs[level], &[]);
                    pass.set_blend_constant(wgpu::Color {
                        r: fade,
                        g: fade,
                        b: fade,
                        a: fade,
                    });
                    pass.draw(0..3, 0..1);
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.bind_group, &[]);
                    pass.draw(0..6, split..data.len() as u32);
                }
                None => pass.draw(0..6, 0..data.len() as u32),
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(surface_tex);
    }
}
