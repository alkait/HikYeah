// render.rs — GPU video tiles: I420 planes as three R8 textures, converted to
// RGB in a WGSL shader during egui's render pass (egui-wgpu paint callbacks).
// One VideoRenderer lives in the egui renderer's callback_resources and holds
// per-tile GPU state keyed by a stable tile id (grid substream or focused
// main stream).

use crate::stream;
use eframe::egui_wgpu::{self, wgpu};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Render adapter names seen at startup, for the Settings dropdown.
/// Populated by the adapter selector in main() before the UI exists.
static ADAPTER_NAMES: OnceLock<Vec<String>> = OnceLock::new();

pub fn set_adapter_names(names: Vec<String>) {
    let _ = ADAPTER_NAMES.set(names);
}

pub fn adapter_names() -> &'static [String] {
    ADAPTER_NAMES.get().map_or(&[], Vec::as_slice)
}

const SHADER: &str = r#"
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Fullscreen triangle over the callback viewport (egui-wgpu sets the
// viewport to the tile rect before paint).
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let corner = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.pos = vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(corner.x, 1.0 - corner.y);
    return out;
}

// BT.601 limited range — same conversion the CPU path used.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * 1.164;
    let u = textureSample(tex_u, samp, in.uv).r - 0.5;
    let v = textureSample(tex_v, samp, in.uv).r - 0.5;
    let rgb = vec3<f32>(
        y + 1.596 * v,
        y - 0.391 * u - 0.813 * v,
        y + 2.018 * u,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// Long-lived GPU state, stored in the egui renderer's callback_resources.
pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    tiles: HashMap<u64, Tile>,
}

#[derive(Default)]
struct Tile {
    planes: Option<Planes>,
    uploaded_seq: u64,
}

struct Planes {
    width: usize,
    height: usize,
    tex: [wgpu::Texture; 3],
    bind: wgpu::BindGroup,
}

impl VideoRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(1),
                tex_entry(2),
                tex_entry(3),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: Default::default(),
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        VideoRenderer {
            pipeline,
            layout,
            sampler,
            tiles: HashMap::new(),
        }
    }

    fn ensure_planes(&mut self, id: u64, device: &wgpu::Device, w: usize, h: usize) {
        let tile = self.tiles.entry(id).or_default();
        if tile
            .planes
            .as_ref()
            .is_some_and(|p| p.width == w && p.height == h)
        {
            return;
        }
        let make = |pw: usize, ph: usize| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("video plane"),
                size: wgpu::Extent3d {
                    width: pw as u32,
                    height: ph as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let tex = [make(w, h), make(cw, ch), make(cw, ch)];
        let views: Vec<_> = tex
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
            ],
        });
        let tile = self.tiles.get_mut(&id).unwrap();
        tile.planes = Some(Planes {
            width: w,
            height: h,
            tex,
            bind,
        });
        tile.uploaded_seq = 0;
    }

    fn upload(&mut self, id: u64, device: &wgpu::Device, queue: &wgpu::Queue, f: &stream::Frame) {
        if self.tiles.get(&id).is_some_and(|t| t.uploaded_seq == f.seq) {
            return;
        }
        self.ensure_planes(id, device, f.width, f.height);
        let tile = self.tiles.get_mut(&id).unwrap();
        let p = tile.planes.as_ref().unwrap();
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let planes: [(&wgpu::Texture, &[u8], usize, usize); 3] = [
            (&p.tex[0], &f.yuv[..w * h], w, h),
            (&p.tex[1], &f.yuv[w * h..w * h + cw * ch], cw, ch),
            (&p.tex[2], &f.yuv[w * h + cw * ch..], cw, ch),
        ];
        for (tex, data, pw, ph) in planes {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(pw as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: pw as u32,
                    height: ph as u32,
                    depth_or_array_layers: 1,
                },
            );
        }
        tile.uploaded_seq = f.seq;
    }
}

/// Per-paint callback: upload the tile's newest frame in prepare, draw in paint.
pub struct VideoCallback {
    pub id: u64,
    pub shared: Arc<stream::Shared>,
}

impl egui_wgpu::CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let r: &mut VideoRenderer = resources.get_mut().expect("VideoRenderer registered");
        if let Some(f) = self.shared.current.lock().unwrap().as_ref() {
            r.upload(self.id, device, queue, f);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let r: &VideoRenderer = resources.get().expect("VideoRenderer registered");
        if let Some(tile) = r.tiles.get(&self.id)
            && let (Some(p), true) = (&tile.planes, tile.uploaded_seq > 0)
        {
            render_pass.set_pipeline(&r.pipeline);
            render_pass.set_bind_group(0, &p.bind, &[]);
            render_pass.draw(0..3, 0..1);
        }
    }
}
