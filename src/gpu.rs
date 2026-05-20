// ABOUTME: GPU compositor: wgpu surface + cosmic-text glyph atlas, one instanced
// ABOUTME: quad pass per frame (bg quads then glyph quads) driven by the Grid.

use crate::config::Config;
use crate::effects::EffectsUniformData;
use crate::grid::{AbsCoord, CursorState, CursorStyle, Grid, Selection};

/// Per-frame data the orchestrator hands the renderer to draw the find UI.
pub struct FindRender<'a> {
    pub query: &'a str,
    pub ranges: &'a [(AbsCoord, AbsCoord)],
    pub current: usize,
}
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

#[derive(Hash, PartialEq, Eq, Clone)]
struct GlyphKey {
    /// Full grapheme-cluster string (base + combining marks). Heap allocation
    /// is one-time-per-unique-cluster since the result is cached in `glyphs`.
    cluster: String,
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
    /// Configured base size, retained for the "reset" zoom keybind.
    font_pt_default: f32,
    line_factor: f32,
    /// Backing scale factor; glyphs rasterize at font_pt * scale (physical px).
    scale: f32,
    /// Logical padding (px) between window edge and the cell grid.
    padding_logical: u32,
    default_fg: u32,
    default_bg: u32,
    /// "Bold = bright" remap of the basic palette at draw time.
    bold_is_bright: bool,

    pub cell_w: u32,
    pub cell_h: u32,
    ascent: f32,

    // Two-pass post-process pipeline.
    offscreen: OffscreenTarget,
    effects_pipeline: wgpu::RenderPipeline,
    effects_bgl: wgpu::BindGroupLayout,
    effects_bind_group: wgpu::BindGroup,
    effects_uniform_buf: wgpu::Buffer,
    effects_sampler: wgpu::Sampler,
}

struct OffscreenTarget {
    view: wgpu::TextureView,
}

impl OffscreenTarget {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, w: u32, h: u32) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        Self {
            view: tex.create_view(&wgpu::TextureViewDescriptor::default()),
        }
    }
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
        let padding_logical = cfg.chrome.padding;
        let default_fg = cfg.colors.foreground.0;
        let default_bg = cfg.colors.background.0;
        let bold_is_bright = cfg.colors.bold_is_bright;

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

        // ---- Two-pass effect pipeline ----
        let offscreen = OffscreenTarget::new(&device, format, w, h);
        let effects_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fx-uniform"),
            size: std::mem::size_of::<EffectsUniformData>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let effects_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let effects_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fx-bgl"),
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
        let effects_bind_group = make_effects_bind_group(
            &device,
            &effects_bgl,
            &effects_uniform_buf,
            &offscreen.view,
            &effects_sampler,
        );
        let fx_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fx"),
            source: wgpu::ShaderSource::Wgsl(EFFECTS_SHADER.into()),
        });
        let fx_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fx-layout"),
            bind_group_layouts: &[&effects_bgl],
            push_constant_ranges: &[],
        });
        let effects_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fx"),
            layout: Some(&fx_layout),
            vertex: wgpu::VertexState {
                module: &fx_shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &fx_shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
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
            font_pt_default: font_pt,
            line_factor,
            scale,
            padding_logical,
            default_fg,
            default_bg,
            bold_is_bright,
            cell_w,
            cell_h,
            ascent,
            offscreen,
            effects_pipeline,
            effects_bgl,
            effects_bind_group,
            effects_uniform_buf,
            effects_sampler,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        // The offscreen RT and its bind group are sized to the surface.
        self.offscreen = OffscreenTarget::new(&self.device, self.config.format, width, height);
        self.effects_bind_group = make_effects_bind_group(
            &self.device,
            &self.effects_bgl,
            &self.effects_uniform_buf,
            &self.offscreen.view,
            &self.effects_sampler,
        );
    }

    pub fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Padding in physical pixels (logical * scale, rounded).
    #[inline]
    pub fn padding_phys(&self) -> u32 {
        (self.padding_logical as f32 * self.scale).round() as u32
    }

    /// Columns/rows that fit the current surface (physical px / physical cell),
    /// accounting for padding on both sides.
    pub fn grid_dims(&self) -> (usize, usize) {
        let p = self.padding_phys() * 2;
        let usable_w = self.config.width.saturating_sub(p);
        let usable_h = self.config.height.saturating_sub(p);
        (
            (usable_w / self.cell_w).max(1) as usize,
            (usable_h / self.cell_h).max(1) as usize,
        )
    }

    /// Live font zoom. Clamped to a sensible range; same re-measure path
    /// used by `rescale`. Triggers a glyph cache + atlas reset.
    pub fn set_font_pt(&mut self, new_pt: f32) {
        let clamped = new_pt.clamp(6.0, 72.0);
        if (clamped - self.font_pt).abs() < f32::EPSILON {
            return;
        }
        self.font_pt = clamped;
        let metrics = scaled_metrics(self.font_pt, self.line_factor, self.scale);
        let (cw, ch, asc) = measure_cell(&mut self.font_system, metrics);
        self.cell_w = cw;
        self.cell_h = ch;
        self.ascent = asc;
        self.glyphs.clear();
        self.atlas.reset(&self.queue);
    }

    pub fn font_pt(&self) -> f32 {
        self.font_pt
    }
    pub fn font_pt_default(&self) -> f32 {
        self.font_pt_default
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
        let info = self.rasterize(&key);
        self.glyphs.insert(key, info);
        info
    }

    fn rasterize(&mut self, key: &GlyphKey) -> Option<GlyphInfo> {
        // Try monospace first (the natural family for a terminal); if that
        // yields no glyph (codepoint missing from any monospace face we have),
        // retry without the family constraint so fontdb can resolve from any
        // installed font (Nerd Fonts, system emoji, symbol fonts).
        self.try_rasterize(key, true)
            .or_else(|| self.try_rasterize(key, false))
    }

    fn try_rasterize(&mut self, key: &GlyphKey, monospace: bool) -> Option<GlyphInfo> {
        let mut attrs = Attrs::new();
        if monospace {
            attrs = attrs.family(Family::Monospace);
        }
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
        buf.set_text(&mut self.font_system, &key.cluster, attrs, Shaping::Advanced);
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

    pub fn render(
        &mut self,
        grid: &Grid,
        cursor: &CursorState,
        cursor_on_this_frame: bool,
        selection: Option<Selection>,
        find: Option<&FindRender<'_>>,
        effects: &EffectsUniformData,
    ) {
        let rows = grid.rows();
        let cw = self.cell_w as f32;
        let ch = self.cell_h as f32;
        let pad = self.padding_phys() as f32;
        // Absolute row index of displayed row 0 (== viewport top when at bottom).
        let top_abs = grid.viewport_first_abs_row().saturating_sub(grid.view_offset);

        // Cursor only shows when looking at the live viewport and the blink
        // clock has its "on" half active.
        let cursor_xy = if cursor.visible && cursor_on_this_frame && grid.at_bottom() {
            Some((cursor.col, cursor.row))
        } else {
            None
        };

        let mut bg: Vec<QuadInstance> = Vec::new();
        let mut fg: Vec<QuadInstance> = Vec::new();

        for y in 0..rows {
            let row_cells = grid.display_row(y);
            for (x, cell) in row_cells.iter().enumerate() {
                let cursor_here = cursor_xy == Some((x, y));
                let abs_coord = AbsCoord { abs_row: top_abs + y, col: x };
                let selected = selection
                    .as_ref()
                    .map(|s| s.contains(abs_coord))
                    .unwrap_or(false);
                let in_find = find
                    .map(|f| range_contains(f.ranges, abs_coord))
                    .unwrap_or(false);
                // Treat find matches as additional XOR like selection.
                let selected = selected ^ in_find;
                // Map sentinel defaults to configured theme colors, then
                // XOR-invert for block-cursor + selection (no Cell mutation).
                let cell_fg = if cell.fg == SENTINEL_FG { self.default_fg } else { cell.fg };
                // Bold-as-bright: dim navy/red/green on a dark background
                // is unreadable, so map basic palette to bright counterpart.
                let cell_fg = if self.bold_is_bright && cell.flags & crate::grid::ATTR_BOLD != 0 {
                    crate::grid::brighten_palette_color(cell_fg)
                } else {
                    cell_fg
                };
                let cell_bg = if cell.bg == SENTINEL_BG { self.default_bg } else { cell.bg };
                let block_invert = cursor_here && cursor.style == CursorStyle::Block;
                let invert = block_invert ^ selected;
                let (fgc, bgc) = if invert { (cell_bg, cell_fg) } else { (cell_fg, cell_bg) };

                let is_wide_lead = cell.flags & crate::grid::ATTR_WIDE != 0;
                let is_wide_trail = cell.flags & crate::grid::ATTR_WIDE_TRAILING != 0;
                let ox = pad + x as f32 * cw;
                let oy = pad + y as f32 * ch;
                // Leading wide cell paints its bg across two columns; the
                // trailing cell skips bg unless it's been individually inverted.
                if !is_wide_trail {
                    let rect_w = if is_wide_lead { 2.0 * cw } else { cw };
                    if bgc != self.default_bg || invert {
                        bg.push(QuadInstance {
                            rect: [ox, oy, rect_w, ch],
                            uv: self.atlas.white_uv,
                            color: rgb(bgc),
                        });
                    }
                } else if invert {
                    bg.push(QuadInstance {
                        rect: [ox, oy, cw, ch],
                        uv: self.atlas.white_uv,
                        color: rgb(bgc),
                    });
                }

                // Underline/Bar cursor accents (leading cells only).
                if cursor_here && !is_wide_trail {
                    let span_w = if is_wide_lead { 2.0 * cw } else { cw };
                    match cursor.style {
                        CursorStyle::Block => {} // handled via inversion above
                        CursorStyle::Underline => {
                            let bar_h = (ch * 0.10).round().max(2.0);
                            bg.push(QuadInstance {
                                rect: [ox, oy + ch - bar_h, span_w, bar_h],
                                uv: self.atlas.white_uv,
                                color: rgb(cell_fg),
                            });
                        }
                        CursorStyle::Bar => {
                            let bar_w = (cw * 0.10).round().max(2.0);
                            bg.push(QuadInstance {
                                rect: [ox, oy, bar_w, ch],
                                uv: self.atlas.white_uv,
                                color: rgb(cell_fg),
                            });
                        }
                    }
                }

                // Glyph: trailing wide cells contribute nothing; the leading
                // cell's cosmic-text image already spans both columns.
                if !is_wide_trail {
                    let cluster = grid.cluster_string(cell);
                    let visible = !cluster.is_empty() && cluster != " ";
                    if visible {
                        let key = GlyphKey {
                            cluster,
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
            }
        }

        // Find-overlay row at the bottom of the grid: inverse-color strip
        // showing "find: <query>  N/M". Drawn after main cells so it paints
        // on top within the same passes.
        if let Some(f) = find {
            self.push_find_overlay(&mut bg, &mut fg, f, grid.cols(), grid.rows(), pad);
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
        self.queue
            .write_buffer(&self.effects_uniform_buf, 0, bytemuck::bytes_of(effects));

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Err(_) => return,
        };
        let surface_view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());

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

        // ---- Pass 1: render cells into the offscreen RT. ----
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.offscreen.view,
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

        // ---- Pass 2: post-process the RT onto the surface. ----
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fx"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.effects_pipeline);
            pass.set_bind_group(0, &self.effects_bind_group, &[]);
            // Fullscreen triangle: vertex shader fabricates positions from
            // builtin(vertex_index); no vertex buffer needed.
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(enc.finish()));
        self.window.pre_present_notify();
        frame.present();
    }

    /// Draw the find-mode overlay strip across the bottom row. The strip
    /// paints `default_fg` as bg and `default_bg` as fg (an inverse block)
    /// so it's visibly distinct from the underlying terminal output.
    fn push_find_overlay(
        &mut self,
        bg: &mut Vec<QuadInstance>,
        fg: &mut Vec<QuadInstance>,
        f: &FindRender<'_>,
        cols: usize,
        rows: usize,
        pad: f32,
    ) {
        let cw = self.cell_w as f32;
        let ch = self.cell_h as f32;
        let oy = pad + (rows.saturating_sub(1) as f32) * ch;

        // Inverse-color strip across the row.
        bg.push(QuadInstance {
            rect: [pad, oy, cols as f32 * cw, ch],
            uv: self.atlas.white_uv,
            color: rgb(self.default_fg),
        });

        let total = f.ranges.len();
        let label = if total == 0 {
            format!("find: {}  no matches", f.query)
        } else {
            format!("find: {}  {}/{}", f.query, f.current + 1, total)
        };

        // Truncate label to the grid width.
        for (i, c) in label.chars().enumerate() {
            if i >= cols {
                break;
            }
            let ox = pad + (i as f32) * cw;
            if c == ' ' {
                continue;
            }
            let key = GlyphKey { cluster: c.to_string(), bold: false, italic: false };
            if let Some(g) = self.glyph(key) {
                fg.push(QuadInstance {
                    rect: [ox + g.left, oy + self.ascent - g.top, g.w, g.h],
                    uv: g.uv,
                    color: rgb(self.default_bg),
                });
            }
        }
    }
}

/// Does any (start, end) inclusive range contain `p` in reading order?
fn range_contains(ranges: &[(AbsCoord, AbsCoord)], p: AbsCoord) -> bool {
    ranges.iter().any(|(s, e)| p >= *s && p <= *e)
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

fn make_effects_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("fx-bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Post-process effect pass: samples the off-screen RT and applies any
/// combination of CRT barrel + scanlines, keystroke ripple, wobble, and a
/// page-turn "cube" squeeze. Effect bits must match `effects.rs`:
/// CRT=1, KEYSTROKE=2, WOBBLE=4, CUBE=8.
const EFFECTS_SHADER: &str = r#"
struct EU {
  surface: vec2<f32>,
  time_ms: f32,
  effect_mask: u32,
  keystroke_xy: vec2<f32>,
  keystroke_age_ms: f32,
  wobble_age_ms: f32,
  cube_age_ms: f32,
  cube_direction: f32,
  cursor_vel: vec2<f32>,
  bell_age_ms: f32,
  pad0: f32,
  pad1: f32,
  pad2: f32,
};

@group(0) @binding(0) var<uniform> u: EU;
@group(0) @binding(1) var src_tex: texture_2d<f32>;
@group(0) @binding(2) var src_smp: sampler;

@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
  // Oversized triangle covering the viewport, no vertex buffer needed.
  var pts = array<vec2<f32>, 3>(
    vec2<f32>(-1.0, -1.0),
    vec2<f32>( 3.0, -1.0),
    vec2<f32>(-1.0,  3.0),
  );
  return vec4<f32>(pts[vi], 0.0, 1.0);
}

fn crt_warp(uv: vec2<f32>) -> vec2<f32> {
  let k: f32 = 0.18;
  var p = uv * 2.0 - 1.0;
  let r2 = dot(p, p);
  p = p * (1.0 + k * r2);
  return (p + 1.0) * 0.5;
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  var uv = pos.xy / u.surface;

  // Wobble: gentle sinusoidal offset, amplitude decays over WOBBLE_MS.
  if ((u.effect_mask & 4u) != 0u) {
    let t = clamp(u.wobble_age_ms / 600.0, 0.0, 1.0);
    let amp = (1.0 - t) * 0.02;
    uv.x = uv.x + amp * sin(uv.y * 18.0 + u.time_ms * 0.020);
    uv.y = uv.y + amp * cos(uv.x * 14.0 + u.time_ms * 0.018);
  }

  // Cube: horizontal "page turn" squeeze that closes then opens.
  if ((u.effect_mask & 8u) != 0u) {
    let t = clamp(u.cube_age_ms / 300.0, 0.0, 1.0);
    let edge = abs(0.5 - t);
    let denom = 0.5 + 0.5 * edge * u.cube_direction;
    let center = (uv.x - 0.5) / max(denom, 0.05);
    uv.x = clamp(center * 0.5 + 0.5, 0.0, 1.0);
  }

  // CRT barrel.
  if ((u.effect_mask & 1u) != 0u) {
    uv = crt_warp(uv);
  }

  if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
    return vec4<f32>(0.0, 0.0, 0.0, 1.0);
  }
  var col = textureSample(src_tex, src_smp, uv);

  // CRT scanlines.
  if ((u.effect_mask & 1u) != 0u) {
    let scan = 0.92 + 0.08 * sin(pos.y * 3.14159);
    col = vec4<f32>(col.rgb * scan, col.a);
  }

  // Visual bell: brief full-screen inversion that fades out.
  if ((u.effect_mask & 16u) != 0u) {
    let t = clamp(u.bell_age_ms / 120.0, 0.0, 1.0);
    let amt = 1.0 - t;
    col = vec4<f32>(mix(col.rgb, vec3<f32>(1.0) - col.rgb, amt), col.a);
  }

  // Keystroke ripple: an expanding glowing ring around the cursor cell.
  if ((u.effect_mask & 2u) != 0u) {
    let t = clamp(u.keystroke_age_ms / 400.0, 0.0, 1.0);
    let center = u.keystroke_xy / u.surface;
    let d = distance(uv, center);
    let radius = 0.04 + t * 0.30;
    let band = 0.04;
    let n = (d - radius) / band;
    let intensity = exp(-n * n) * (1.0 - t);
    col = vec4<f32>(col.rgb + vec3<f32>(0.40, 0.65, 1.00) * intensity * 0.85, col.a);
  }

  return col;
}
"#;
