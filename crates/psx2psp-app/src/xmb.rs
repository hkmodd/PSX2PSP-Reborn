//! Live XMB preview: a wgpu custom primitive that composites the animated wave, PIC1, PIC0,
//! ICON0 and the boot image in a single full-screen pass (`xmb.wgsl`).
//!
//! The widget drives its own animation: on every `RedrawRequested` it records the frame time and
//! requests the next redraw, so the wave runs at the display refresh rate without re-running the
//! application's `update`/`view`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use iced::advanced::graphics::Viewport;
use iced::mouse;
use iced::widget::shader::{self, Action, Pipeline, Primitive};
use iced::wgpu;
use iced::{Color, Rectangle, window};

/// Decoded RGBA8 image with a process-unique id (the GPU cache key).
pub struct Pixels {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Pixels {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Arc<Self> {
        debug_assert_eq!(rgba.len(), (width * height * 4) as usize);
        Arc::new(Self { id: next_id(), width, height, rgba })
    }
}

/// Decoded ICON1.PMF: RGBA8 frames played at 29.97 fps in the ICON0 slot, as the XMB does when
/// the game is focused.
pub struct Movie {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<Vec<u8>>,
}

impl Movie {
    pub const FPS: f32 = 30000.0 / 1001.0;

    pub fn new(width: u32, height: u32, frames: Vec<Vec<u8>>) -> Arc<Self> {
        debug_assert!(!frames.is_empty() && frames.iter().all(|f| f.len() == (width * height * 4) as usize));
        Arc::new(Self { id: next_id(), width, height, frames })
    }

    fn frame_at(&self, secs: f32) -> usize {
        (secs.max(0.0) * Self::FPS) as usize % self.frames.len()
    }
}

impl fmt::Debug for Movie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Movie#{}({}x{}, {} frames)", self.id, self.width, self.height, self.frames.len())
    }
}

impl fmt::Debug for Pixels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pixels#{}({}x{})", self.id, self.width, self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Game,
    Save,
    Boot,
}

impl Screen {
    pub const ALL: [Screen; 3] = [Self::Game, Self::Save, Self::Boot];

    pub fn label(self) -> &'static str {
        match self {
            Self::Game => "Game",
            Self::Save => "Save",
            Self::Boot => "Boot",
        }
    }
}

/// Texture layers, in shader binding order.
pub const PIC1: usize = 0;
pub const PIC0: usize = 1;
pub const ICON0: usize = 2;
pub const BOOT: usize = 3;

/// What the preview shows; built by the app's `view`.
pub struct Xmb {
    pub screen: Screen,
    pub layers: [Option<Arc<Pixels>>; 4],
    /// ICON1 animation; replaces the ICON0 still while present.
    pub movie: Option<Arc<Movie>>,
    pub accent: Color,
}

impl Xmb {
    /// GPU cache key of each slot (the movie owns the ICON0 slot when present).
    fn ids(&self) -> [u64; 4] {
        let mut ids = self.layers.each_ref().map(|l| l.as_ref().map_or(0, |p| p.id));
        if let Some(m) = &self.movie {
            ids[ICON0] = m.id;
        }
        ids
    }
}

/// Per-widget animation state (lives in the widget tree).
#[derive(Default)]
pub struct State {
    start: Option<Instant>,
    now: f32,
    screen: Screen,
    prev: Screen,
    screen_at: f32,
    ids: [u64; 4],
    ids_at: [f32; 4],
    pacing: Pacing,
}

/// Opt-in frame-pacing telemetry (`PSX2PSP_FPS=1`): every 2 s, logs the mean FPS and the worst
/// frame interval to stderr. Measures real presentation cadence, not just CPU time.
#[derive(Default)]
struct Pacing {
    window_start: Option<Instant>,
    last: Option<Instant>,
    frames: u32,
    worst: Duration,
}

impl Pacing {
    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("PSX2PSP_FPS").is_some())
    }

    fn tick(&mut self, at: Instant) {
        if let Some(last) = self.last {
            self.worst = self.worst.max(at.saturating_duration_since(last));
        }
        self.last = Some(at);
        self.frames += 1;
        let span = at.saturating_duration_since(*self.window_start.get_or_insert(at));
        if span >= Duration::from_secs(2) {
            eprintln!(
                "xmb: {:.1} fps, worst frame {:.2} ms",
                f64::from(self.frames - 1) / span.as_secs_f64(),
                self.worst.as_secs_f64() * 1e3
            );
            *self = Self { window_start: Some(at), last: Some(at), frames: 1, worst: Duration::ZERO };
        }
    }
}

const SCREEN_FADE: f32 = 0.45;
const LAYER_FADE: f32 = 0.35;

fn ease_out_cubic(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

impl<M> shader::Program<M> for Xmb {
    type State = State;
    type Primitive = XmbPrimitive;

    fn update(&self, s: &mut State, event: &iced::Event, _: Rectangle, _: mouse::Cursor) -> Option<Action<M>> {
        let iced::Event::Window(window::Event::RedrawRequested(at)) = event else {
            return None;
        };
        if Pacing::enabled() {
            s.pacing.tick(*at);
        }
        let start = *s.start.get_or_insert(*at);
        s.now = at.saturating_duration_since(start).as_secs_f32();
        if s.screen != self.screen {
            // Restart from wherever the current crossfade is, so rapid toggling never pops.
            s.prev = s.screen;
            s.screen = self.screen;
            s.screen_at = s.now;
        }
        for (i, id) in self.ids().into_iter().enumerate() {
            if s.ids[i] != id {
                s.ids[i] = id;
                s.ids_at[i] = s.now;
            }
        }
        Some(Action::request_redraw())
    }

    fn draw(&self, s: &State, _: mouse::Cursor, _: Rectangle) -> XmbPrimitive {
        let mut fade = [0.0f32; 4];
        let ids = self.ids();
        for (i, f) in fade.iter_mut().enumerate() {
            if ids[i] != 0 {
                *f = ease_out_cubic((s.now - s.ids_at[i]) / LAYER_FADE);
            }
        }
        XmbPrimitive {
            uniforms: Uniforms {
                time: s.now,
                blend: ease_out_cubic((s.now - s.screen_at) / SCREEN_FADE),
                mode: s.screen as u32,
                prev: s.prev as u32,
                accent: [self.accent.r, self.accent.g, self.accent.b, 1.0],
                fade,
                size: [0.0; 2],
                srgb_target: 0.0,
                radius: 0.0,
            },
            layers: self.layers.clone(),
            movie: self.movie.as_ref().map(|m| (m.clone(), m.frame_at(s.now - s.ids_at[ICON0]))),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    time: f32,
    blend: f32,
    mode: u32,
    prev: u32,
    accent: [f32; 4],
    fade: [f32; 4],
    size: [f32; 2],
    srgb_target: f32,
    radius: f32,
}

#[derive(Debug)]
pub struct XmbPrimitive {
    uniforms: Uniforms,
    layers: [Option<Arc<Pixels>>; 4],
    movie: Option<(Arc<Movie>, usize)>,
}

/// Corner radius of the preview, in logical pixels.
const RADIUS: f32 = 14.0;

impl Primitive for XmbPrimitive {
    type Pipeline = XmbPipeline;

    fn prepare(&self, p: &mut XmbPipeline, device: &wgpu::Device, queue: &wgpu::Queue, bounds: &Rectangle, viewport: &Viewport) {
        let mut dirty = false;
        for (i, (slot, layer)) in p.slots.iter_mut().zip(&self.layers).enumerate() {
            let movie = self.movie.as_ref().filter(|_| i == ICON0);
            let (id, image) = match (movie, layer) {
                (Some((m, f)), _) => (m.id, Some((m.width, m.height, &m.frames[*f], *f))),
                (None, Some(px)) => (px.id, Some((px.width, px.height, &px.rgba, 0))),
                (None, None) => (0, None),
            };
            if slot.id != id {
                (slot.texture, slot.view) = match image {
                    Some((w, h, rgba, _)) => upload(device, queue, w, h, rgba),
                    None => (None, p.empty.clone()),
                };
                slot.id = id;
                slot.frame = image.map_or(0, |i| i.3);
                dirty = true;
            } else if let (Some((w, h, rgba, f)), Some(tex)) = (image, &slot.texture)
                && slot.frame != f
            {
                // Next movie frame: rewrite the texture in place, no reallocation or rebind.
                write(queue, tex, w, h, rgba);
                slot.frame = f;
            }
        }
        if dirty {
            p.bind_group = p.bind(device);
        }
        let scale = viewport.scale_factor();
        let u = Uniforms {
            size: [bounds.width * scale, bounds.height * scale],
            srgb_target: if p.srgb { 1.0 } else { 0.0 },
            radius: RADIUS * scale,
            ..self.uniforms
        };
        queue.write_buffer(&p.uniforms, 0, bytemuck::bytes_of(&u));
    }

    fn draw(&self, p: &XmbPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        pass.set_pipeline(&p.pipeline);
        pass.set_bind_group(0, &p.bind_group, &[]);
        pass.draw(0..3, 0..1);
        true
    }
}

struct Slot {
    id: u64,
    texture: Option<wgpu::Texture>,
    view: wgpu::TextureView,
    /// Movie frame currently in `texture`.
    frame: usize,
}

pub struct XmbPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    empty: wgpu::TextureView,
    slots: [Slot; 4],
    bind_group: wgpu::BindGroup,
    srgb: bool,
}

fn upload(device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32, rgba: &[u8]) -> (Option<wgpu::Texture>, wgpu::TextureView) {
    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("xmb.layer"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    write(queue, &texture, width, height, rgba);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (Some(texture), view)
}

fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32, rgba: &[u8]) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        rgba,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * width), rows_per_image: Some(height) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

fn bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    views: [&wgpu::TextureView; 4],
) -> wgpu::BindGroup {
    let [a, b, c, d] = views.map(wgpu::BindingResource::TextureView);
    let entries = [
        wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        wgpu::BindGroupEntry { binding: 2, resource: a },
        wgpu::BindGroupEntry { binding: 3, resource: b },
        wgpu::BindGroupEntry { binding: 4, resource: c },
        wgpu::BindGroupEntry { binding: 5, resource: d },
    ];
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("xmb.bind"), layout, entries: &entries })
}

impl XmbPipeline {
    fn bind(&self, device: &wgpu::Device) -> wgpu::BindGroup {
        bind_group(device, &self.layout, &self.uniforms, &self.sampler, self.slots.each_ref().map(|s| &s.view))
    }
}
impl Pipeline for XmbPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xmb.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("xmb.wgsl").into()),
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
            label: Some("xmb.layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
                tex_entry(5),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("xmb.pipeline_layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("xmb.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("xmb.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xmb.uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (_, empty) = upload(device, queue, 1, 1, &[0; 4]);
        let slots = std::array::from_fn(|_| Slot { id: 0, texture: None, view: empty.clone(), frame: 0 });
        Self {
            bind_group: bind_group(device, &layout, &uniforms, &sampler, [&empty; 4]),
            pipeline,
            layout,
            sampler,
            uniforms,
            empty,
            slots,
            srgb: format.is_srgb(),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Off-screen GPU test: renders every screen through the real pipeline and reads pixels back.
    use super::*;
    use iced::{Point, Size};

    const W: u32 = 512; // 512×4 B = 2048: satisfies the 256-byte row alignment for readback.
    const H: u32 = 272;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Arc<Pixels> {
        Pixels::new(w, h, rgba.repeat((w * h) as usize))
    }

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    /// Renders one frame and returns tightly packed RGBA8 (sRGB-encoded) pixels.
    fn render(device: &wgpu::Device, queue: &wgpu::Queue, mode: Screen, layers: [Option<Arc<Pixels>>; 4]) -> Vec<u8> {
        let mut pipeline = XmbPipeline::new(device, queue, wgpu::TextureFormat::Rgba8UnormSrgb);
        render_with(device, queue, &mut pipeline, mode, layers, None)
    }

    fn render_with(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &mut XmbPipeline,
        mode: Screen,
        layers: [Option<Arc<Pixels>>; 4],
        movie: Option<(Arc<Movie>, usize)>,
    ) -> Vec<u8> {
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let primitive = XmbPrimitive {
            uniforms: Uniforms {
                time: 0.0,
                blend: 1.0,
                mode: mode as u32,
                prev: mode as u32,
                accent: [0.16, 0.33, 0.78, 1.0],
                fade: [1.0; 4],
                size: [0.0; 2],
                srgb_target: 0.0,
                radius: 0.0,
            },
            layers,
            movie,
        };
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(W as f32, H as f32));
        primitive.prepare(pipeline, device, queue, &bounds, &Viewport::with_physical_size(Size::new(W, H), 1.0));

        let size = wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(W * H * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            assert!(primitive.draw(pipeline, &mut pass));
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
            },
            size,
        );
        queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        slice.get_mapped_range().to_vec()
    }

    /// Pixel at PSP-screen coordinates (480×272 space).
    fn at(px: &[u8], x: f32, y: f32) -> [u8; 4] {
        let (tx, ty) = ((x / 480.0 * W as f32) as u32, y as u32);
        let i = ((ty * W + tx) * 4) as usize;
        px[i..i + 4].try_into().unwrap()
    }

    /// ICON1 frames replace ICON0 and advance in place on the same pipeline (texture rewrite path).
    #[test]
    fn movie_frames_play_in_icon0_slot() {
        let Some((device, queue)) = gpu() else { return };
        let mut pipeline = XmbPipeline::new(&device, &queue, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut layers: [Option<Arc<Pixels>>; 4] = Default::default();
        layers[ICON0] = Some(solid(144, 80, [0, 255, 0, 255]));
        let frame = |c: [u8; 4]| c.repeat(144 * 80);
        let movie = Movie::new(144, 80, vec![frame([255, 0, 0, 255]), frame([0, 0, 255, 255])]);
        assert_eq!((movie.frame_at(0.0), movie.frame_at(0.04), movie.frame_at(0.07)), (0, 1, 0));
        let icon = (92.0, 136.0);
        for (f, rgba) in [(0, [255, 0, 0, 255]), (1, [0, 0, 255, 255]), (0, [255, 0, 0, 255])] {
            let px = render_with(&device, &queue, &mut pipeline, Screen::Game, layers.clone(), Some((movie.clone(), f)));
            assert_eq!(at(&px, icon.0, icon.1), rgba, "frame {f}");
        }
        let px = render_with(&device, &queue, &mut pipeline, Screen::Game, layers, None);
        assert_eq!(at(&px, icon.0, icon.1), [0, 255, 0, 255], "ICON0 returns when the movie is removed");
    }

    #[test]
    fn screens_composite_layers_correctly() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter: skipping off-screen render test");
            return;
        };
        let mut layers: [Option<Arc<Pixels>>; 4] = Default::default();
        layers[PIC1] = Some(solid(480, 272, [255, 0, 0, 255]));
        layers[ICON0] = Some(solid(144, 80, [0, 255, 0, 255]));
        layers[BOOT] = Some(solid(480, 272, [0, 0, 255, 255]));
        let (screen_mid, icon_mid) = ((300.0, 40.0), (92.0, 136.0));

        let game = render(&device, &queue, Screen::Game, layers.clone());
        assert_eq!(at(&game, screen_mid.0, screen_mid.1), [255, 0, 0, 255], "Game: PIC1 fills the screen");
        assert_eq!(at(&game, icon_mid.0, icon_mid.1), [0, 255, 0, 255], "Game: ICON0 on top");
        assert_eq!(at(&game, 0.0, 0.0)[3], 0, "rounded corner is transparent");

        // Save: PIC1 at 40% linear → sRGB ≈ 170.
        let save = render(&device, &queue, Screen::Save, layers.clone());
        let [r, g, b, a] = at(&save, screen_mid.0, screen_mid.1);
        assert!((160..=180).contains(&r) && g < 8 && b < 8 && a == 255, "Save: dimmed PIC1, got {:?}", [r, g, b, a]);
        assert_eq!(at(&save, icon_mid.0, icon_mid.1), [0, 255, 0, 255], "Save: ICON0 undimmed");

        let boot = render(&device, &queue, Screen::Boot, layers.clone());
        assert_eq!(at(&boot, screen_mid.0, screen_mid.1), [0, 0, 255, 255], "Boot: boot image only");
        assert_eq!(at(&boot, icon_mid.0, icon_mid.1), [0, 0, 255, 255], "Boot: no ICON0");

        layers[BOOT] = None;
        let empty = render(&device, &queue, Screen::Boot, layers);
        assert_eq!(at(&empty, screen_mid.0, screen_mid.1), [0, 0, 0, 255], "Boot without image: black");
    }
}
