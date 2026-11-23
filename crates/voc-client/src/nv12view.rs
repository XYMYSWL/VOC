//! 观看屏幕共享时在显卡上转颜色（第 53-2 / 36-3 项）：硬解出来的 NV12 直接传给显卡（只有 RGBA 的 3/8 大小），
//! 在界面的着色器里转成 RGB 画出来，不在 CPU 上逐像素转换。1080p60 观看时 CPU 占用明显下降。
//! 界面用的是 wgpu 渲染（eframe 默认）；拿不到 wgpu（例如退回了 OpenGL）时 GPU_NV12 = false，解码线程照旧在 CPU 上转成 RGBA。

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, ScreenDescriptor};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 界面能在显卡上画 NV12（解码线程据此决定交 NV12 还是 RGBA）
pub static GPU_NV12: AtomicBool = AtomicBool::new(false);
/// 关掉的观看窗口：下一帧释放它的贴图
static FORGET: Mutex<Vec<u32>> = Mutex::new(Vec::new());

const SHADER: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var o: VOut;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}

@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tuv: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment fn fs(v: VOut) -> @location(0) vec4<f32> {
    // BT.601 有限范围（和原来 CPU 上的转换一样）
    let y = (textureSample(ty, smp, v.uv).r - 16.0 / 255.0) * 1.164;
    let c = textureSample(tuv, smp, v.uv).rg - vec2<f32>(0.5, 0.5);
    var rgb = vec3<f32>(y + 1.596 * c.y, y - 0.391 * c.x - 0.813 * c.y, y + 2.018 * c.x);
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (LINEAR_TARGET) {
        rgb = to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

struct Tex {
    size: (u32, u32),
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
    seq: u64,
}

struct Resources {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texs: HashMap<u32, Tex>,
}

/// 启动时调用：准备着色器。拿不到 wgpu 时什么都不做
pub fn init(cc: &eframe::CreationContext<'_>) {
    let Some(rs) = &cc.wgpu_render_state else { return };
    if std::env::var("VOC_CPU_COLOR").is_ok() {
        return; // 调试：强制用 CPU 转颜色
    }
    rs.renderer.write().callback_resources.insert(build(&rs.device, rs.target_format));
    GPU_NV12.store(true, Ordering::Relaxed);
    crate::applog::log(format!("观看：颜色转换在显卡上做（{:?}）", rs.target_format));
}

/// 着色器、绑定布局、采样器（画到 format 格式的目标上）
fn build(device: &wgpu::Device, format: wgpu::TextureFormat) -> Resources {
    let src = SHADER.replace("LINEAR_TARGET", if format.is_srgb() { "true" } else { "false" });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("voc_nv12"), source: wgpu::ShaderSource::Wgsl(src.into()) });
    let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            multisampled: false,
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("voc_nv12_layout"),
        entries: &[
            tex_entry(0),
            tex_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("voc_nv12_pl"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("voc_nv12_pipeline"),
        layout: Some(&pl),
        vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState { format: format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("voc_nv12_sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Resources { pipeline, layout, sampler, texs: HashMap::new() }
}

/// 观看窗口关了：释放它的贴图
pub fn forget(id: u32) {
    FORGET.lock().unwrap().push(id);
}

/// 一帧 NV12（宽高都是偶数，Y 平面 w×h 紧接 UV 平面 w×h/2）
#[derive(Clone)]
pub struct Nv12 {
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
    pub seq: u64,
}

/// 在 rect 里画观看窗口 id 的最新一帧
pub fn paint(painter: &egui::Painter, rect: egui::Rect, id: u32, frame: Nv12) {
    painter.add(egui_wgpu::Callback::new_paint_callback(rect, Draw { id, frame }));
}

struct Draw {
    id: u32,
    frame: Nv12,
}

fn make_tex(device: &wgpu::Device, label: &str, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn ensure_tex(device: &wgpu::Device, r: &mut Resources, id: u32, w: u32, h: u32) {
    if r.texs.get(&id).is_some_and(|t| t.size == (w, h)) {
        return;
    }
    let y = make_tex(device, "voc_nv12_y", w, h, wgpu::TextureFormat::R8Unorm);
    let uv = make_tex(device, "voc_nv12_uv", w / 2, h / 2, wgpu::TextureFormat::Rg8Unorm);
    let (yv, uvv) = (y.create_view(&Default::default()), uv.create_view(&Default::default()));
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("voc_nv12_bind"),
        layout: &r.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&yv) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&uvv) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&r.sampler) },
        ],
    });
    r.texs.insert(id, Tex { size: (w, h), y, uv, bind, seq: u64::MAX });
}

/// 把一帧 NV12 传到显卡（Y 平面 + 交错的 UV 平面）
fn upload(queue: &wgpu::Queue, t: &Tex, d: &[u8]) {
    let (w, h) = t.size;
    let ysz = (w * h) as usize;
    let write = |tex: &wgpu::Texture, data: &[u8], bpr: u32, tw: u32, th: u32| {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(th) },
            wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
        );
    };
    write(&t.y, &d[..ysz], w, w, h);
    write(&t.uv, &d[ysz..ysz + ysz / 2], w, w / 2, h / 2);
}

impl CallbackTrait for Draw {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _sd: &ScreenDescriptor,
        _enc: &mut wgpu::CommandEncoder,
        res: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = res.get_mut::<Resources>() else { return Vec::new() };
        for id in FORGET.lock().unwrap().drain(..) {
            r.texs.remove(&id);
        }
        let (w, h) = (self.frame.width, self.frame.height);
        if w == 0 || h == 0 || self.frame.data.len() < (w * h * 3 / 2) as usize {
            return Vec::new();
        }
        ensure_tex(device, r, self.id, w, h);
        let t = r.texs.get_mut(&self.id).unwrap();
        if t.seq != self.frame.seq {
            t.seq = self.frame.seq;
            upload(queue, t, &self.frame.data);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, res: &CallbackResources) {
        let Some(r) = res.get::<Resources>() else { return };
        let Some(t) = r.texs.get(&self.id) else { return };
        pass.set_pipeline(&r.pipeline);
        pass.set_bind_group(0, &t.bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 显卡上转出来的颜色和原来 CPU 上转的一样（误差 ±3 以内）
    #[test]
    fn gpu_matches_cpu() {
        let instance = wgpu::Instance::default();
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            println!("没有可用的显卡，跳过");
            return;
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("device");
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut r = build(&device, format);
        let (w, h) = (64u32, 32u32);
        // Y 横向渐变；UV 每次整张一样（线性采样不会把相邻色度混在一起，和 CPU 的逐块取值可比）
        for (u, v) in [(128u8, 128u8), (90, 200), (200, 60), (16, 240)] {
            let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
            for y in 0..h {
                for x in 0..w {
                    nv12[(y * w + x) as usize] = (16 + x * 219 / (w - 1)) as u8;
                }
            }
            for c in nv12[(w * h) as usize..].chunks_mut(2) {
                c[0] = u;
                c[1] = v;
            }
            ensure_tex(&device, &mut r, 1, w, h);
            upload(&queue, &r.texs[&1], &nv12);
            let target = device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = target.create_view(&Default::default());
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (w * h * 4) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&r.pipeline);
                pass.set_bind_group(0, &r.texs[&1].bind, &[]);
                pass.draw(0..3, 0..1);
            }
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) } },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            queue.submit([enc.finish()]);
            buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).unwrap();
            let gpu = buf.slice(..).get_mapped_range().expect("map").to_vec();
            let mut cpu = Vec::new();
            crate::video::nv12_to_rgba(&nv12, w as usize, h as usize, &mut cpu);
            let worst = gpu.iter().zip(&cpu).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            println!("UV ({u},{v})：最大误差 {worst}；第一个像素 显卡 {:?} CPU {:?}", &gpu[..4], &cpu[..4]);
            assert!(worst <= 3, "UV ({u},{v}) 误差 {worst}");
        }
    }
}
