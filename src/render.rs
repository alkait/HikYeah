// render.rs — GPU video tiles: I420 planes as three R8 textures (or NV12 as
// R8 + RG8, straight off a hardware decoder), converted to RGB in a WGSL
// shader during egui's render pass (egui-wgpu paint callbacks).
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
// I420: U and V planes (R8 each). NV12: one RG8 plane bound to both slots.
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
struct Params {
    // Texture sub-rectangle to show (min.xy, max.xy): the whole frame for a
    // grid tile, a crop when the focused view is zoomed in. egui clamps a
    // callback's viewport to the screen, so zoom can't be an oversized rect.
    uv_rect: vec4<f32>,
    // x: 1 = NV12 chroma (interleaved UV in tex_u), 0 = planar I420.
    flags: vec4<f32>,
};
@group(0) @binding(4) var<uniform> params: Params;

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
    out.uv = mix(params.uv_rect.xy, params.uv_rect.zw, vec2<f32>(corner.x, 1.0 - corner.y));
    return out;
}

// BT.601 limited range — same conversion the CPU path used.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * 1.164;
    var u: f32;
    var v: f32;
    if (params.flags.x > 0.5) {
        let c = textureSample(tex_u, samp, in.uv).rg;
        u = c.r - 0.5;
        v = c.g - 0.5;
    } else {
        u = textureSample(tex_u, samp, in.uv).r - 0.5;
        v = textureSample(tex_v, samp, in.uv).r - 0.5;
    }
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

struct Tile {
    planes: Option<Planes>,
    /// What the textures hold: the source stream (by address — playback
    /// swaps streams under one tile id, each restarting its sequence) and
    /// the frame sequence number; 0 = nothing yet.
    uploaded: (usize, u64),
    uv_buf: wgpu::Buffer,
}

impl Tile {
    fn new(device: &wgpu::Device) -> Self {
        Tile {
            planes: None,
            uploaded: (0, 0),
            uv_buf: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("video params"),
                size: 32,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        }
    }
}

struct Planes {
    width: usize,
    height: usize,
    format: stream::PixFmt,
    /// I420: Y, U, V. NV12: Y, UV (RG8) — the third slot is the UV again.
    tex: Vec<wgpu::Texture>,
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
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
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

    fn ensure_planes(
        &mut self,
        id: u64,
        device: &wgpu::Device,
        w: usize,
        h: usize,
        format: stream::PixFmt,
    ) {
        let tile = self.tiles.entry(id).or_insert_with(|| Tile::new(device));
        if tile
            .planes
            .as_ref()
            .is_some_and(|p| p.width == w && p.height == h && p.format == format)
        {
            return;
        }
        let make = |pw: usize, ph: usize, fmt: wgpu::TextureFormat| {
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
                format: fmt,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let tex = match format {
            stream::PixFmt::I420 => vec![
                make(w, h, wgpu::TextureFormat::R8Unorm),
                make(cw, ch, wgpu::TextureFormat::R8Unorm),
                make(cw, ch, wgpu::TextureFormat::R8Unorm),
            ],
            stream::PixFmt::Nv12 => vec![
                make(w, h, wgpu::TextureFormat::R8Unorm),
                make(cw, ch, wgpu::TextureFormat::Rg8Unorm),
            ],
        };
        let mut views: Vec<_> = tex
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        if views.len() == 2 {
            views.push(tex[1].create_view(&Default::default()));
        }
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
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: tile.uv_buf.as_entire_binding(),
                },
            ],
        });
        let tile = self.tiles.get_mut(&id).unwrap();
        tile.planes = Some(Planes {
            width: w,
            height: h,
            format,
            tex,
            bind,
        });
        tile.uploaded = (0, 0);
    }

    fn upload(
        &mut self,
        id: u64,
        source: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        f: &stream::Frame,
    ) {
        if self
            .tiles
            .get(&id)
            .is_some_and(|t| t.uploaded == (source, f.seq))
        {
            return;
        }
        self.ensure_planes(id, device, f.width, f.height, f.format);
        let tile = self.tiles.get_mut(&id).unwrap();
        let p = tile.planes.as_ref().unwrap();
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // (texture, bytes, texels per row, rows, bytes per row)
        let planes: Vec<(&wgpu::Texture, &[u8], usize, usize, usize)> = match f.format {
            stream::PixFmt::I420 => vec![
                (&p.tex[0], &f.data[..w * h], w, h, w),
                (&p.tex[1], &f.data[w * h..w * h + cw * ch], cw, ch, cw),
                (&p.tex[2], &f.data[w * h + cw * ch..], cw, ch, cw),
            ],
            stream::PixFmt::Nv12 => vec![
                (&p.tex[0], &f.data[..w * h], w, h, w),
                (&p.tex[1], &f.data[w * h..], cw, ch, cw * 2),
            ],
        };
        for (tex, data, pw, ph, bpr) in planes {
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
                    bytes_per_row: Some(bpr as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: pw as u32,
                    height: ph as u32,
                    depth_or_array_layers: 1,
                },
            );
        }
        tile.uploaded = (source, f.seq);
    }

    fn set_params(&self, id: u64, queue: &wgpu::Queue, uv: eframe::egui::Rect) {
        if let Some(tile) = self.tiles.get(&id) {
            let nv12 = tile
                .planes
                .as_ref()
                .is_some_and(|p| p.format == stream::PixFmt::Nv12);
            let vals = [
                uv.min.x,
                uv.min.y,
                uv.max.x,
                uv.max.y,
                if nv12 { 1.0 } else { 0.0 },
                0.0,
                0.0,
                0.0,
            ];
            let mut bytes = [0u8; 32];
            for (i, v) in vals.iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            queue.write_buffer(&tile.uv_buf, 0, &bytes);
        }
    }
}

/// Per-paint callback: upload the tile's newest frame in prepare, draw in paint.
pub struct VideoCallback {
    pub id: u64,
    pub shared: Arc<stream::Shared>,
    /// Frame sub-rectangle (0..1 texture coords) mapped onto the paint rect.
    pub uv: eframe::egui::Rect,
}

impl VideoCallback {
    pub const FULL: eframe::egui::Rect = eframe::egui::Rect::from_min_max(
        eframe::egui::pos2(0.0, 0.0),
        eframe::egui::pos2(1.0, 1.0),
    );
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
            r.upload(
                self.id,
                Arc::as_ptr(&self.shared) as usize,
                device,
                queue,
                f,
            );
        }
        r.set_params(self.id, queue, self.uv);
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
            && let (Some(p), true) = (&tile.planes, tile.uploaded.1 > 0)
        {
            render_pass.set_pipeline(&r.pipeline);
            render_pass.set_bind_group(0, &p.bind, &[]);
            render_pass.draw(0..3, 0..1);
        }
    }
}
