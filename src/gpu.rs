// ABOUTME: GPU compositor: wgpu surface + cosmic-text glyph atlas, one instanced
// ABOUTME: quad pass per frame (bg quads then glyph quads) driven by the Grid.

use crate::config::Config;
use crate::grid::{CursorState, Grid};
use cosmic_text::{
    Attrs, Buffer as TextBuffer, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    SwashContent, Weight,
};
use std::collections::HashMap;
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

const ATLAS_SIZE: u32 = 2048;
/// Sentinel colors a default cell carries (SGR reset / 39 / 49). The renderer
/// maps these to the configured fg/bg so theming needs no grid changes.
const SENTINEL_FG: u32 = 0xFFFFFF;
const SENTINEL_BG: u32 = 0x000000;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadInstance {
    /// x, y, w, h in physical pixels, top-left origin.
    rect: [f32; 4],
    /// u0, v0, u1, v1 into the atlas.
    uv: [f32; 4],
    /// rgba, 0..1.
    color: [f32; 4],
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct GlyphKey {
    ch: char,
    bold: bool,
    italic: bool,
}

#[derive(Clone, Copy)]
struct GlyphInfo {
    uv: [f32; 4],
    /// Bitmap size and baseline-relative bearings (swash placement).
    w: f32,
    h: f32,
    left: f32,
    top: f32,
}

/// Simple shelf allocator over a single R8 atlas texture.
struct Atlas {
    texture: wgpu::Texture,
    cursor_x: u32,
    cursor_y: u32,
    shelf_h: u32,
    /// UV of a reserved 1x1 fully-opaque texel, used by background quads.
    white_uv: [f32; 4],
}

pub struct GpuRenderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    quad_vbuf: wgpu::Buffer,
    quad_ibuf: wgpu::Buffer,
    atlas: Atlas,

    font_system: FontSystem,
    swash: SwashCache,
    glyphs: HashMap<GlyphKey, Option<GlyphInfo>>,

    /// Logical font size (pt) and line-height factor from config.
    font_pt: f32,
    line_factor: f32,
    /// Backing scale factor; glyphs rasterize at font_pt * scale (physical px).
    scale: f32,
    default_fg: u32,
    default_bg: u32,

    pub cell_w: u32,
    pub cell_h: u32,
    ascent: f32,
}

fn rgb(c: u32) -> [f32; 4] {
    [
        ((c >> 16) & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        (c & 0xFF) as f32 / 255.0,
        1.0,
    ]
}

impl GpuRenderer {
    /// Build the GPU context and measure cell metrics from a system monospace
    /// font (loaded by path, falling back to fontdb's monospace resolution).
    pub async fn new(window: Arc<Window>, cfg: &Config) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let scale = window.scale_factor() as f32;
        let font_pt = cfg.font.size;
        let line_factor = cfg.font.line_height;
        let default_fg = cfg.colors.foreground.0;
        let default_bg = cfg.colors.background.0;

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no suitable GPU adapter"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format so our 0..1 colors map straight through.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Font system: configured path first, then known macOS monospace
        // paths, else fontdb's monospace resolution.
        let mut font_system = FontSystem::new();
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = &cfg.font.path {
            paths.push(p.clone());
        }
        paths.extend(
            [
                "/System/Library/Fonts/Menlo.ttc",
                "/System/Library/Fonts/SFNSMono.ttf",
                "/Library/Fonts/Menlo.ttc",
            ]
            .map(String::from),
        );
        for path in &paths {
            if std::path::Path::new(path).exists() {
                font_system.db_mut().load_font_file(path).ok();
            }
        }
        let swash = SwashCache::new();

        // Measure cell metrics at the physical (DPI-scaled) font size.
        let metrics = scaled_metrics(font_pt, line_factor, scale);
        let (cell_w, cell_h, ascent) = measure_cell(&mut font_system, metrics);

        let atlas = Atlas::new(&device, &queue);

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let atlas_view = atlas.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bgl"),
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
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cells"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cells"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: 8,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<QuadInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4, 3 => Float32x4],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        // Unit quad: corners (0,0)(1,0)(0,1)(1,1) + indices.
        let quad_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-v"),
            contents: bytemuck::cast_slice(&[
                0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0,
            ]),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let quad_ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-i"),
            contents: bytemuck::cast_slice(&[0u16, 1, 2, 2, 1, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            uniform_buf,
            quad_vbuf,
            quad_ibuf,
            atlas,
            font_system,
            swash,
            glyphs: HashMap::new(),
            font_pt,
            line_factor,
            scale,
            default_fg,
            default_bg,
            cell_w,
            cell_h,
            ascent,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Columns/rows that fit the current surface (physical px / physical cell).
    pub fn grid_dims(&self) -> (usize, usize) {
        (
            (self.config.width / self.cell_w).max(1) as usize,
            (self.config.height / self.cell_h).max(1) as usize,
        )
    }

    /// Apply a new backing scale factor (HiDPI). Re-measures the cell at the
    /// new physical font size and drops cached glyphs so they re-rasterize
    /// crisply; the atlas allocator is reset (old pixels become unreferenced).
    pub fn rescale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < f32::EPSILON || scale <= 0.0 {
            return;
        }
        self.scale = scale;
        let metrics = scaled_metrics(self.font_pt, self.line_factor, scale);
        let (cw, ch, asc) = measure_cell(&mut self.font_system, metrics);
        self.cell_w = cw;
        self.cell_h = ch;
        self.ascent = asc;
        self.glyphs.clear();
        self.atlas.reset(&self.queue);
    }

    /// Resolve (rasterize + atlas) a glyph, memoized. None = no bitmap (space).
    fn glyph(&mut self, key: GlyphKey) -> Option<GlyphInfo> {
        if let Some(cached) = self.glyphs.get(&key) {
            return *cached;
        }
        let info = self.rasterize(key);
        self.glyphs.insert(key, info);
        info
    }

    fn rasterize(&mut self, key: GlyphKey) -> Option<GlyphInfo> {
        let mut attrs = Attrs::new().family(Family::Monospace);
        if key.bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        if key.italic {
            attrs = attrs.style(Style::Italic);
        }
        let metrics = scaled_metrics(self.font_pt, self.line_factor, self.scale);
        let box_px = metrics.font_size * 4.0;
        let mut buf = TextBuffer::new(&mut self.font_system, metrics);
        buf.set_size(&mut self.font_system, Some(box_px), Some(box_px));
        buf.set_text(&mut self.font_system, &key.ch.to_string(), attrs, Shaping::Advanced);
        buf.shape_until_scroll(&mut self.font_system, false);

        let phys = buf
            .layout_runs()
            .next()?
            .glyphs
            .first()?
            .physical((0.0, 0.0), 1.0);
        let img = self.swash.get_image(&mut self.font_system, phys.cache_key).clone()?;
        let (pw, ph) = (img.placement.width, img.placement.height);
        if pw == 0 || ph == 0 {
            return None;
        }

        // Normalize swash content to single-channel coverage.
        let coverage: Vec<u8> = match img.content {
            SwashContent::Mask => img.data,
            SwashContent::SubpixelMask => img
                .data
                .chunks(4)
                .map(|p| p.get(3).copied().unwrap_or(0))
                .collect(),
            // Color glyphs (emoji): approximate with the alpha channel for now.
            SwashContent::Color => img.data.chunks(4).map(|p| p.get(3).copied().unwrap_or(0)).collect(),
        };

        let (ax, ay) = self.atlas.alloc(&self.queue, pw, ph, &coverage)?;
        let uv = [
            ax as f32 / ATLAS_SIZE as f32,
            ay as f32 / ATLAS_SIZE as f32,
            (ax + pw) as f32 / ATLAS_SIZE as f32,
            (ay + ph) as f32 / ATLAS_SIZE as f32,
        ];
        Some(GlyphInfo {
            uv,
            w: pw as f32,
            h: ph as f32,
            left: img.placement.left as f32,
            top: img.placement.top as f32,
        })
    }

    pub fn render(&mut self, grid: &Grid, cursor: &CursorState) {
        let cols = grid.cols();
        let cells = grid.viewport();
        let cw = self.cell_w as f32;
        let ch = self.cell_h as f32;

        let cursor_xy = if cursor.visible {
            Some((cursor.col, cursor.row))
        } else {
            None
        };

        let mut bg: Vec<QuadInstance> = Vec::new();
        let mut fg: Vec<QuadInstance> = Vec::new();

        for (i, cell) in cells.iter().enumerate() {
            let x = i % cols;
            let y = i / cols;
            let is_cursor = cursor_xy == Some((x, y));
            // Map the grid's sentinel defaults to the configured theme colors,
            // then apply cursor inversion so the block masks/unmasks the glyph.
            let cell_fg = if cell.fg == SENTINEL_FG { self.default_fg } else { cell.fg };
            let cell_bg = if cell.bg == SENTINEL_BG { self.default_bg } else { cell.bg };
            let (fgc, bgc) = if is_cursor {
                (cell_bg, cell_fg)
            } else {
                (cell_fg, cell_bg)
            };

            let ox = x as f32 * cw;
            let oy = y as f32 * ch;

            if bgc != self.default_bg || is_cursor {
                bg.push(QuadInstance {
                    rect: [ox, oy, cw, ch],
                    uv: self.atlas.white_uv,
                    color: rgb(bgc),
                });
            }

            let c = cell.c;
            if c != ' ' && c != '\0' {
                let key = GlyphKey {
                    ch: c,
                    bold: cell.flags & crate::grid::ATTR_BOLD != 0,
                    italic: cell.flags & crate::grid::ATTR_ITALIC != 0,
                };
                if let Some(g) = self.glyph(key) {
                    fg.push(QuadInstance {
                        rect: [ox + g.left, oy + self.ascent - g.top, g.w, g.h],
                        uv: g.uv,
                        color: rgb(fgc),
                    });
                }
            }
        }

        // Background quads first, glyphs after, in one instance buffer so the
        // single instanced draw paints glyphs over their cell backgrounds.
        let bg_count = bg.len() as u32;
        bg.extend_from_slice(&fg);
        let total = bg.len() as u32;

        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen: [self.config.width as f32, self.config.height as f32],
                _pad: [0.0, 0.0],
            }),
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Err(_) => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let instance_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instances"),
            contents: if total == 0 {
                // Avoid a zero-sized buffer; we just won't draw it.
                bytemuck::cast_slice(&[QuadInstance {
                    rect: [0.0; 4],
                    uv: [0.0; 4],
                    color: [0.0; 4],
                }])
            } else {
                bytemuck::cast_slice(&bg)
            },
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: ((self.default_bg >> 16) & 0xFF) as f64 / 255.0,
                            g: ((self.default_bg >> 8) & 0xFF) as f64 / 255.0,
                            b: (self.default_bg & 0xFF) as f64 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if total > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.quad_vbuf.slice(..));
                pass.set_vertex_buffer(1, instance_buf.slice(..));
                pass.set_index_buffer(self.quad_ibuf.slice(..), wgpu::IndexFormat::Uint16);
                let _ = bg_count;
                pass.draw_indexed(0..6, 0, 0..total);
            }
        }
        self.queue.submit(Some(enc.finish()));
        self.window.pre_present_notify();
        frame.present();
    }
}

impl Atlas {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Reserve a 1x1 opaque texel at (0,0) for background quads.
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(1),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let half = 0.5 / ATLAS_SIZE as f32;
        Self {
            texture,
            cursor_x: 2,
            cursor_y: 0,
            shelf_h: 1,
            white_uv: [half, half, half, half],
        }
    }

    /// Reset the shelf allocator (used on DPI change). Stale pixels remain
    /// in the texture but are simply no longer referenced by any glyph.
    fn reset(&mut self, queue: &wgpu::Queue) {
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(1),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        self.cursor_x = 2;
        self.cursor_y = 0;
        self.shelf_h = 1;
    }

    /// Pack a coverage bitmap into the atlas; returns its top-left origin.
    fn alloc(&mut self, queue: &wgpu::Queue, w: u32, h: u32, data: &[u8]) -> Option<(u32, u32)> {
        if w > ATLAS_SIZE || h > ATLAS_SIZE {
            return None;
        }
        if self.cursor_x + w > ATLAS_SIZE {
            self.cursor_x = 0;
            self.cursor_y += self.shelf_h;
            self.shelf_h = 0;
        }
        if self.cursor_y + h > ATLAS_SIZE {
            return None; // Atlas full; caller falls back to no glyph.
        }
        let (x, y) = (self.cursor_x, self.cursor_y);
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.cursor_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Some((x, y))
    }
}

/// Physical-pixel metrics: logical pt * backing scale, line height scaled too.
fn scaled_metrics(font_pt: f32, line_factor: f32, scale: f32) -> Metrics {
    let px = (font_pt * scale).max(1.0);
    Metrics::new(px, (px * line_factor).round().max(1.0))
}

/// Shape a representative glyph to derive integer cell width/height + ascent.
fn measure_cell(fs: &mut FontSystem, metrics: Metrics) -> (u32, u32, f32) {
    let mut buf = TextBuffer::new(fs, metrics);
    buf.set_size(fs, Some(1000.0), Some(1000.0));
    buf.set_text(fs, "M", Attrs::new().family(Family::Monospace), Shaping::Advanced);
    buf.shape_until_scroll(fs, false);
    let mut adv = metrics.font_size * 0.6;
    let mut ascent = metrics.line_height * 0.8;
    if let Some(run) = buf.layout_runs().next() {
        ascent = run.line_y;
        if let Some(g) = run.glyphs.first() {
            adv = g.w;
        }
    }
    let cell_w = adv.ceil().max(1.0) as u32;
    let cell_h = metrics.line_height.ceil().max(1.0) as u32;
    (cell_w, cell_h, ascent)
}

const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct VsIn {
  @location(0) corner: vec2<f32>,
  @location(1) rect: vec4<f32>,
  @location(2) uv: vec4<f32>,
  @location(3) color: vec4<f32>,
};
struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
};

@vertex
fn vs(in: VsIn) -> VsOut {
  let px = in.rect.xy + in.corner * in.rect.zw;
  let ndc = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
  var o: VsOut;
  o.pos = vec4<f32>(ndc, 0.0, 1.0);
  o.uv = mix(in.uv.xy, in.uv.zw, in.corner);
  o.color = in.color;
  return o;
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4<f32> {
  let cov = textureSample(atlas_tex, atlas_smp, i.uv).r;
  return vec4<f32>(i.color.rgb, i.color.a * cov);
}
"#;
