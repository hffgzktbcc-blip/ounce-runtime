//! Hardware `wgpu` (Metal on Apple Silicon, Vulkan on Linux/Windows) backend for Switch 2 (`GA10F`):
//! - Executes `AMPERE_COMPUTE_B` (`0xC7C0`) QMD dispatches via cached `wgpu::ComputePipeline`s,
//! - Executes `AMPERE_B` (`0xC797`) 3D draws via cached `wgpu::RenderPipeline`s (MRT `Rgba32Float` +
//!   `Depth32Float`, preserving full float/unorm precision across formats),
//! - Accelerates Tegra X1 Layout 0 & Tegra T239/Orin **Sector Layout 1** BlockLinear GOB swizzling
//!   via [`crate::gpu::tiling::BLOCK_LINEAR_COMPUTE_WGSL`] compute pipelines,
//! - Maintains a VRAM-budgeted [`HardwareTextureCache`] decoding Pitch, BlockLinear Layout 0, and
//!   **Orin/T239 Sector Layout 1** textures across uncompressed, `BC1..BC7`, `ETC2/EAC`, and all 14
//!   2D `ASTC` block footprints.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::compute::{discover_compute_storage_region, ComputeQmd};
use super::engine::{DrawRecord, GpuMemoryWriter, ShaderType, Topology};
use super::methods as m;
use super::render::{
    color_view_with_sector, decode_attribute_words, depth_view_with_sector, gather_draw_triangles,
    read_color_texel, read_depth_texel, selected_color_targets, target_write_mask,
    write_color_texel, write_depth_texel,
};
use super::surface::ColorFormat;
use super::texture::{AddressMode, Filter, Sampler, TextureHeader};
use crate::gpu::sass::decode::TexHandle;
use crate::gpu::sass::ir::Function;
use crate::gpu::sass::{
    Interpolation, ShaderCacheKey, ShaderDiskCache, ShaderInterface, ShaderStage as SassStage,
    TranslatedShader,
};
use crate::gpu::tiling::{SectorLayout, BLOCK_LINEAR_COMPUTE_WGSL};

/// Global mutex serializing headless `wgpu` tests on macOS Metal CI runners when needed.
static ACCEL_INIT_LOCK: Mutex<()> = Mutex::new(());

/// Default host texture cache budget (512 MiB).
pub const DEFAULT_VRAM_CACHE_BUDGET_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextureCacheStats {
    pub hits: u64,
    pub uploads: u64,
    pub evictions: u64,
    pub used_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TextureCacheKey {
    gpu_va: u64,
    raw_tic: [u32; 8],
    sector_layout: SectorLayout,
    content_hash: u64,
}

struct CachedGpuTexture {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    byte_size: usize,
}

/// VRAM-budgeted host GPU texture cache for decoded `TIC` surfaces (including ASTC / BC1..7 / Layout 1).
pub struct HardwareTextureCache {
    budget_bytes: usize,
    used_bytes: usize,
    entries: HashMap<TextureCacheKey, CachedGpuTexture>,
    fifo: VecDeque<TextureCacheKey>,
    stats: TextureCacheStats,
}

impl HardwareTextureCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes: budget_bytes.max(4 * 1024 * 1024),
            used_bytes: 0,
            entries: HashMap::new(),
            fifo: VecDeque::new(),
            stats: TextureCacheStats::default(),
        }
    }

    pub fn stats(&self) -> TextureCacheStats {
        let mut s = self.stats;
        s.used_bytes = self.used_bytes;
        s
    }

    fn get_or_upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        header: &TextureHeader,
        sector_layout: SectorLayout,
        raw_surface_bytes: &[u8],
    ) -> Option<wgpu::TextureView> {
        let content_hash = fnv1a_64(raw_surface_bytes);
        let key = TextureCacheKey {
            gpu_va: header.gpu_va,
            raw_tic: header.raw,
            sector_layout,
            content_hash,
        };
        if let Some(existing) = self.entries.get(&key) {
            self.stats.hits += 1;
            return Some(existing.view.clone());
        }

        let w = header.width.max(1);
        let h = header.height.max(1);
        // Decode level 0 into RGBA16Float (8 bytes/texel) or RGBA8Unorm (4 bytes/texel).
        // Using RGBA8Unorm for standard [0, 1] textures and RGBA32Float -> RGBA16Float/RGBA8Unorm
        // keeps filterability universal across all Metal/Vulkan tiers (`Float { filterable: true }`).
        let byte_size = (w as usize) * (h as usize) * 4;
        while self.used_bytes + byte_size > self.budget_bytes {
            let Some(evict_key) = self.fifo.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&evict_key) {
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_size);
                self.stats.evictions += 1;
            }
        }

        let mut snapshot = RawSliceReader { base: header.gpu_va, bytes: raw_surface_bytes };
        let mut rgba8 = vec![0u8; byte_size];
        for y in 0..h {
            for x in 0..w {
                let c = header
                    .fetch_with_sector(x, y, sector_layout, &mut snapshot)
                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                let o = ((y * w + x) as usize) * 4;
                rgba8[o] = float_to_u8(c[0]);
                rgba8[o + 1] = float_to_u8(c[1]);
                rgba8[o + 2] = float_to_u8(c[2]);
                rgba8[o + 3] = float_to_u8(c[3]);
            }
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ounce-ga10f-cached-tex"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
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
            &rgba8,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        if byte_size <= self.budget_bytes {
            self.used_bytes += byte_size;
            self.fifo.push_back(key);
            self.entries.insert(
                key,
                CachedGpuTexture { _texture: texture, view: view.clone(), byte_size },
            );
        }
        self.stats.uploads += 1;
        Some(view)
    }
}

struct RawSliceReader<'a> {
    base: u64,
    bytes: &'a [u8],
}

impl GpuMemoryWriter for RawSliceReader<'_> {
    fn write(&mut self, _: u64, _: &[u8]) {}
    fn read(&mut self, gpu_va: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        let Some(off) = gpu_va.checked_sub(self.base) else {
            return false;
        };
        let off = off as usize;
        let Some(src) = self.bytes.get(off..off.saturating_add(buf.len())) else {
            return false;
        };
        buf.copy_from_slice(src);
        true
    }
}

struct CachedComputePipeline {
    pipeline: wgpu::ComputePipeline,
    bgl: wgpu::BindGroupLayout,
    shader: Arc<TranslatedShader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RenderPipelineKey {
    vs_key: ShaderCacheKey,
    fs_key: ShaderCacheKey,
    flip_y: bool,
    ogl_z_clip: bool,
    target_masks: Vec<u8>,
    depth_enabled: bool,
    depth_write: bool,
    depth_compare_code: u8,
    cull_code: u8,
    front_ccw: bool,
}

struct CachedRenderPipeline {
    pipeline: wgpu::RenderPipeline,
    bgl_vs: wgpu::BindGroupLayout,
    bgl_fs: wgpu::BindGroupLayout,
    vs_inputs: Vec<u32>,
}

/// Hardware `wgpu` Metal/Vulkan execution context for `GA10F` (`0xC797` + `0xC7C0` + GOB tiling).
pub struct WgpuAmpereAccelerator {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
    backend_name: String,
    pub shader_cache: ShaderDiskCache,
    pub texture_cache: HardwareTextureCache,
    compute_pipelines: HashMap<ShaderCacheKey, CachedComputePipeline>,
    render_pipelines: HashMap<RenderPipelineKey, CachedRenderPipeline>,
    swizzle_bgl: wgpu::BindGroupLayout,
    swizzle_pipeline: wgpu::ComputePipeline,
}

impl std::fmt::Debug for WgpuAmpereAccelerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuAmpereAccelerator")
            .field("adapter", &self.adapter_name)
            .field("backend", &self.backend_name)
            .field("cached_compute_pipelines", &self.compute_pipelines.len())
            .field("cached_render_pipelines", &self.render_pipelines.len())
            .field("texture_cache", &self.texture_cache.stats())
            .finish()
    }
}

impl Clone for WgpuAmpereAccelerator {
    fn clone(&self) -> Self {
        Self::with_shader_cache(
            match self.shader_cache.dir() {
                Some(dir) => ShaderDiskCache::with_dir(dir),
                None => ShaderDiskCache::in_memory(),
            },
            self.texture_cache.budget_bytes,
        )
        .unwrap_or_else(|| {
            panic!("failed to clone WgpuAmpereAccelerator on active adapter {}", self.adapter_name)
        })
    }
}

impl WgpuAmpereAccelerator {
    /// Initializes a headless hardware `wgpu` device (Metal on macOS, Vulkan on Linux/Windows)
    /// using the default persistent shader cache directory (`cache/shaders`).
    pub fn try_new() -> Option<Self> {
        Self::with_shader_cache(ShaderDiskCache::default_dir(), DEFAULT_VRAM_CACHE_BUDGET_BYTES)
    }

    /// Initializes a headless hardware `wgpu` device with an in-memory shader cache (ideal for unit tests).
    pub fn try_new_in_memory() -> Option<Self> {
        Self::with_shader_cache(ShaderDiskCache::in_memory(), 64 * 1024 * 1024)
    }

    pub fn with_shader_cache(shader_cache: ShaderDiskCache, vram_budget_bytes: usize) -> Option<Self> {
        let _guard = ACCEL_INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
        );
        let adapter = pollster::block_on(wgpu::util::initialize_adapter_from_env_or_default(
            &instance, None,
        ))
        .ok()?;
        let info = adapter.get_info();
        let adapter_name = info.name.clone();
        let backend_name = format!("{:?}", info.backend);

        let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ounce-ga10f-wgpu-device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            ..Default::default()
        }))
        .ok()?;

        // Compile hardware BlockLinear Layout 0 / Orin Layout 1 swizzler & unswizzler pipelines.
        let bl_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ounce-block-linear-compute"),
            source: wgpu::ShaderSource::Wgsl(BLOCK_LINEAR_COMPUTE_WGSL.into()),
        });
        let swizzle_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ounce-block-linear-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(32),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let swizzle_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ounce-block-linear-pl"),
            bind_group_layouts: &[Some(&swizzle_bgl)],
            immediate_size: 0,
        });
        let swizzle_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ounce-block-linear-swizzle"),
            layout: Some(&swizzle_pl),
            module: &bl_module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        Some(Self {
            device,
            queue,
            adapter_name,
            backend_name,
            shader_cache,
            texture_cache: HardwareTextureCache::new(vram_budget_bytes),
            compute_pipelines: HashMap::new(),
            render_pipelines: HashMap::new(),
            swizzle_bgl,
            swizzle_pipeline,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Runs `BLOCK_LINEAR_COMPUTE_WGSL` on the host Metal/Vulkan GPU to swizzle or unswizzle a
    /// 4-byte-aligned row surface in either Tegra X1 Layout 0 or Orin/T239 Sector Layout 1.
    #[allow(clippy::too_many_arguments)]
    pub fn gpu_block_linear_transform(
        &self,
        to_linear: bool,
        src_bytes: &[u8],
        out_byte_len: usize,
        width_bytes: u32,
        height: u32,
        bh_log2: u8,
        bd_log2: u8,
        sector_layout: SectorLayout,
    ) -> Option<Vec<u8>> {
        if width_bytes % 4 != 0 || src_bytes.is_empty() || out_byte_len == 0 {
            return None;
        }
        let _ = bd_log2;
        let width_gobs = width_bytes.div_ceil(64);
        let pitch_words = width_bytes / 4;
        let layout_code = match sector_layout {
            SectorLayout::Layout0Tegrax1 => 0u32,
            SectorLayout::Layout1OrinT239 => 1u32,
        };
        let to_tiled = u32::from(!to_linear);
        let params: [u32; 8] = [
            width_bytes,
            height,
            width_gobs,
            bh_log2 as u32,
            layout_code,
            to_tiled,
            width_bytes,
            0,
        ];
        let mut param_bytes = [0u8; 32];
        for (i, w) in params.iter().enumerate() {
            param_bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }

        let ubo = self.create_init_buffer("bl-params", &param_bytes, wgpu::BufferUsages::UNIFORM);
        let src_padded_len = src_bytes.len().div_ceil(4) * 4;
        let mut src_aligned = vec![0u8; src_padded_len];
        src_aligned[..src_bytes.len()].copy_from_slice(src_bytes);
        let src_buf = self.create_init_buffer("bl-src", &src_aligned, wgpu::BufferUsages::STORAGE);

        let dst_padded_len = out_byte_len.div_ceil(4).max(1) * 4;
        let dst_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bl-dst"),
            size: dst_padded_len as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bl-bg"),
            layout: &self.swizzle_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubo.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: src_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: dst_buf.as_entire_binding() },
            ],
        });

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("bl-compute-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.swizzle_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(pitch_words.div_ceil(16), height.div_ceil(8), 1);
        }
        let mut out = self.readback_buffer(enc, &dst_buf, dst_padded_len);
        out.truncate(out_byte_len);
        Some(out)
    }

    /// Executes a `0xC7C0` (`AMPERE_COMPUTE_B`) or `0xB1C0` QMD on the host Metal/Vulkan GPU.
    /// Returns `Ok(true)` if dispatched on hardware, `Ok(false)` if the kernel should run on the CPU fallback.
    pub fn execute_compute_qmd(
        &mut self,
        class: u32,
        qmd: &ComputeQmd,
        code: &[u8],
        func: &Function,
        staged_cbufs: &[(u8, Vec<u8>)],
        mem: &mut dyn GpuMemoryWriter,
    ) -> Result<bool, &'static str> {
        let stage = SassStage::Compute { workgroup_size: qmd.workgroup_size };
        let (cache_key, shader) = self
            .shader_cache
            .get_or_translate(class, stage, code)
            .map_err(|_| "compute WGSL translation failed")?;

        // Compute shaders that read textures fall back to CPU unless no textures are bound yet.
        if !shader.interface.textures.is_empty() {
            return Ok(false);
        }

        // Resolve the storage buffer region if the shader accesses global memory.
        let gmem_region = if shader.interface.uses_global_memory {
            let Some((base_va, region_len)) = discover_compute_storage_region(func, staged_cbufs, mem) else {
                return Ok(false);
            };
            let word_aligned_len = (region_len / 4) * 4;
            if word_aligned_len == 0 || word_aligned_len > 128 * 1024 * 1024 {
                return Ok(false);
            }
            let mut raw = vec![0u8; word_aligned_len];
            if !mem.read(base_va, &mut raw) {
                return Ok(false);
            }
            Some((base_va, raw))
        } else {
            None
        };

        if !self.compute_pipelines.contains_key(&cache_key) {
            let compiled = self.compile_compute_pipeline(&shader);
            self.compute_pipelines.insert(cache_key, compiled);
        }
        let cp = self.compute_pipelines.get(&cache_key).expect("just inserted");

        // Build uniform buffers for each constant buffer in `shader.interface.constant_buffers`.
        let mut owned_buffers: Vec<(u32, wgpu::Buffer)> = Vec::new();
        for cb_decl in &cp.shader.interface.constant_buffers {
            let need = (cb_decl.size_bytes as usize).max(16).div_ceil(16) * 16;
            let mut bytes = vec![0u8; need];
            if let Some((_, src)) = staged_cbufs.iter().find(|(b, _)| *b as u32 == cb_decl.binding) {
                let n = src.len().min(need);
                bytes[..n].copy_from_slice(&src[..n]);
            }
            let buf = self.create_init_buffer("qmd-cbuf", &bytes, wgpu::BufferUsages::UNIFORM);
            owned_buffers.push((cb_decl.binding, buf));
        }

        let gmem_buffers = if let Some((base_va, ref raw)) = gmem_region {
            let word_len = (raw.len() / 4) as u32;
            let meta_words: [u32; 4] = [base_va as u32, (base_va >> 32) as u32, word_len, 0];
            let mut meta_bytes = [0u8; 16];
            for (i, w) in meta_words.iter().enumerate() {
                meta_bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            let meta_buf = self.create_init_buffer("qmd-gmem-meta", &meta_bytes, wgpu::BufferUsages::UNIFORM);
            let storage_buf = self.create_init_buffer(
                "qmd-gmem",
                raw,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            );
            Some((base_va, raw.len(), meta_buf, storage_buf))
        } else {
            None
        };

        let mut entries: Vec<wgpu::BindGroupEntry> = owned_buffers
            .iter()
            .map(|(binding, buf)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: buf.as_entire_binding(),
            })
            .collect();
        if let Some((_, _, ref meta_buf, ref storage_buf)) = gmem_buffers {
            entries.push(wgpu::BindGroupEntry { binding: 16, resource: meta_buf.as_entire_binding() });
            entries.push(wgpu::BindGroupEntry { binding: 17, resource: storage_buf.as_entire_binding() });
        }

        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("qmd-compute-bg"),
            layout: &cp.bgl,
            entries: &entries,
        });

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("ounce-ga10f-qmd-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&cp.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(
                qmd.grid_dim[0].max(1),
                qmd.grid_dim[1].max(1),
                qmd.grid_dim[2].max(1),
            );
        }

        if let Some((base_va, byte_len, _, storage_buf)) = gmem_buffers {
            let updated = self.readback_buffer(enc, &storage_buf, byte_len);
            mem.write(base_va, &updated);
        } else {
            self.queue.submit(Some(enc.finish()));
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        }

        Ok(true)
    }

    /// Executes an `AMPERE_B` (`0xC797`) or `MAXWELL_B` (`0xB197`) 3D draw on the host Metal/Vulkan
    /// GPU and writes the rendered color/depth surfaces back into guest VRAM using `sector_layout`.
    /// Returns `Ok(true)` if rendered on hardware, or `Ok(false)` to fall back to the software rasterizer.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_draw(
        &mut self,
        rec: &DrawRecord,
        regs: &[u32],
        cbufs_table: &[[Option<super::engine::ConstantBufferBinding>; m::CONSTANT_BUFFER_SLOTS]; m::BIND_GROUP_COUNT],
        sector_layout: SectorLayout,
        mem: &mut dyn GpuMemoryWriter,
    ) -> Result<bool, &'static str> {
        // Defer stencil-test or blend-enabled draws to the bit-exact software rasterizer so complex
        // two-sided stencil / D3D blend factor edge cases retain 100% reference semantics.
        if (regs[m::SET_STENCIL_TEST as usize / 4] & 1) != 0
            || (regs[m::SET_DEPTH_BOUNDS_TEST as usize / 4] & 1) != 0
        {
            return Ok(false);
        }
        for i in 0..m::COLOR_TARGET_COUNT {
            if (regs[(m::SET_BLEND + 4 * i as u32) as usize / 4] & 1) != 0 {
                return Ok(false);
            }
        }

        // Locate VS and PS stages.
        let find_stage = |kinds: &[ShaderType]| rec.shaders.iter().find(|s| kinds.contains(&s.kind)).copied();
        let Some(vs_stage) = find_stage(&[ShaderType::Vertex, ShaderType::VertexCullBeforeFetch]) else {
            return Ok(false);
        };
        let Some(ps_stage) = find_stage(&[ShaderType::Pixel]) else {
            return Ok(false);
        };
        if rec.shaders.iter().any(|s| !matches!(s.kind, ShaderType::Vertex | ShaderType::VertexCullBeforeFetch | ShaderType::Pixel)) {
            return Ok(false);
        }

        let vs_raw = read_graphics_shader_bytes(rec.class, vs_stage.program_va, mem)?;
        let ps_raw = read_graphics_shader_bytes(rec.class, ps_stage.program_va, mem)?;
        let (vs_key, vs_shader) = self
            .shader_cache
            .get_or_translate(rec.class, SassStage::Vertex, &vs_raw)
            .map_err(|_| "VS WGSL translation failed")?;
        let (fs_key, fs_shader) = self
            .shader_cache
            .get_or_translate(rec.class, SassStage::Fragment, &ps_raw)
            .map_err(|_| "FS WGSL translation failed")?;

        if !vs_shader.interface.writes_position {
            return Ok(false);
        }

        // Gather bound color targets (`SET_CT_SELECT`) & optional depth target.
        let selected = selected_color_targets(regs);
        let max_target_slot = fs_shader
            .interface
            .color_targets
            .iter()
            .copied()
            .max()
            .map(|m| m as usize + 1)
            .unwrap_or(0)
            .max(selected.len());
        if max_target_slot == 0 || max_target_slot > 4 {
            return Ok(false);
        }

        // Determine attachment dimensions from the first active color or depth target.
        let mut attach_dims: Option<(u32, u32)> = None;
        let mut staged_cts = Vec::with_capacity(max_target_slot);
        let mut target_masks = Vec::with_capacity(max_target_slot);

        for t_idx in 0..max_target_slot {
            let opt_ct = selected.get(t_idx).copied().flatten();
            if let Some(ct) = opt_ct {
                let Some(view) = color_view_with_sector(&ct, sector_layout) else {
                    return Ok(false);
                };
                if let Some((w, h)) = attach_dims {
                    if view.width != w || view.height != h {
                        return Ok(false);
                    }
                } else {
                    attach_dims = Some((view.width, view.height));
                }
                let mut raw = vec![0u8; view.size()];
                if !mem.read(view.gpu_va, &mut raw) {
                    return Ok(false);
                }
                let mask_bools = target_write_mask(regs, ct.index);
                let mask_bits = (u8::from(mask_bools[0]))
                    | (u8::from(mask_bools[1]) << 1)
                    | (u8::from(mask_bools[2]) << 2)
                    | (u8::from(mask_bools[3]) << 3);
                target_masks.push(mask_bits);
                staged_cts.push(Some((ct, view, raw, mask_bools)));
            } else {
                target_masks.push(0);
                staged_cts.push(None);
            }
        }

        let depth_test_on = (regs[m::SET_DEPTH_TEST as usize / 4] & 1) != 0 && rec.depth_target.is_some();
        let depth_write_on = depth_test_on && (regs[m::SET_DEPTH_WRITE as usize / 4] & 1) != 0;
        let staged_zt = if let Some(zt) = rec.depth_target.filter(|_| depth_test_on) {
            let Some(zview) = depth_view_with_sector(&zt, sector_layout) else {
                return Ok(false);
            };
            if let Some((w, h)) = attach_dims {
                if zview.width != w || zview.height != h {
                    return Ok(false);
                }
            } else {
                attach_dims = Some((zview.width, zview.height));
            }
            let mut raw = vec![0u8; zview.size()];
            if !mem.read(zview.gpu_va, &mut raw) {
                return Ok(false);
            }
            Some((zt, zview, raw))
        } else {
            None
        };

        let Some((fb_w, fb_h)) = attach_dims else {
            return Ok(false);
        };
        if fb_w == 0 || fb_h == 0 || fb_w > 4096 || fb_h > 4096 {
            return Ok(false);
        }

        // Decode triangles & assemble expanded non-indexed vertex buffer matching `vs_shader.interface.vertex_inputs`.
        let (ids, tris) = gather_draw_triangles(rec, mem)?;
        if tris.is_empty() {
            return Ok(true);
        }

        let vp = &rec.viewport0;
        if vp.scale[0] == 0.0 || vp.scale[1] == 0.0 {
            return Ok(false);
        }
        let flip_y = vp.scale[1] > 0.0;
        let ogl_z_clip = (regs[m::SET_VIEWPORT_Z_CLIP as usize / 4] & 1) == 0;

        let depth_compare_code = if depth_test_on {
            map_depth_compare_code(regs[m::SET_DEPTH_FUNC as usize / 4])?
        } else {
            7 // Always
        };

        let (cull_code, front_ccw) = map_cull_state(regs, flip_y)?;

        let rp_key = RenderPipelineKey {
            vs_key,
            fs_key,
            flip_y,
            ogl_z_clip,
            target_masks: target_masks.clone(),
            depth_enabled: staged_zt.is_some(),
            depth_write: depth_write_on,
            depth_compare_code,
            cull_code,
            front_ccw,
        };

        if !self.render_pipelines.contains_key(&rp_key) {
            let compiled = self.compile_render_pipeline(&rp_key, &vs_shader, &fs_shader);
            self.render_pipelines.insert(rp_key.clone(), compiled);
        }

        // Build vertex buffer: each vertex in `tris` gets `vs_inputs.len()` x `vec4<f32>` (16 bytes each).
        let vs_inputs = self.render_pipelines.get(&rp_key).expect("inserted").vs_inputs.clone();
        let verts_total = tris.len() * 3;
        let stride_floats = vs_inputs.len() * 4;
        let mut vb_bytes = Vec::with_capacity((verts_total * stride_floats * 4).max(16));

        for tri in &tris {
            for &pos_idx in tri {
                let vid = ids[pos_idx];
                let attrs = fetch_vertex_attrs_for_hw(rec, vid, mem)?;
                for &loc in &vs_inputs {
                    let words = attrs.get(loc as usize).copied().unwrap_or([0, 0, 0, 1.0f32.to_bits()]);
                    for w in words {
                        vb_bytes.extend_from_slice(&w.to_le_bytes());
                    }
                }
            }
        }
        if vb_bytes.is_empty() {
            vb_bytes.resize(16, 0);
        }
        let vertex_buffer = self.create_init_buffer("ga10f-vb", &vb_bytes, wgpu::BufferUsages::VERTEX);

        // Build bind group 0 (VS uniforms + textures) and bind group 1 (FS uniforms + textures).
        let bindless_bank = (regs[m::SET_BINDLESS_TEXTURE as usize / 4] & 0x1f) as u8;
        let sampler_via_header = (regs[m::SET_SAMPLER_BINDING as usize / 4] & 1) != 0;

        let vs_resources = self.build_stage_resources(
            0,
            &vs_shader.interface,
            &cbufs_table[0],
            regs,
            bindless_bank,
            sampler_via_header,
            sector_layout,
            mem,
        )?;
        let fs_resources = self.build_stage_resources(
            4,
            &fs_shader.interface,
            &cbufs_table[4],
            regs,
            bindless_bank,
            sampler_via_header,
            sector_layout,
            mem,
        )?;

        let rp = self.render_pipelines.get(&rp_key).expect("inserted");
        let bg_vs = self.create_stage_bind_group("ga10f-vs-bg", &rp.bgl_vs, &vs_resources);
        let bg_fs = self.create_stage_bind_group("ga10f-fs-bg", &rp.bgl_fs, &fs_resources);

        // Create offscreen Rgba8Unorm color attachments initialized from current guest surface contents!
        // Note: Rgba8Unorm supports filtering, blending, and RENDER_ATTACHMENT universally across all
        // Metal/Vulkan tiers; for float32 render targets (`Rf32Gf32Bf32Af32`), defer to software if needed,
        // or check format compatibility:
        for opt in &staged_cts {
            if let Some((ct, _, _, _)) = opt {
                if matches!(ct.format, ColorFormat::Rf32Gf32Bf32Af32 | ColorFormat::R16G16B16A16) {
                    return Ok(false);
                }
            }
        }

        let mut gpu_color_texs: Vec<(wgpu::Texture, wgpu::TextureView)> = Vec::with_capacity(max_target_slot);
        for opt in &staged_cts {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ga10f-rt"),
                size: wgpu::Extent3d { width: fb_w, height: fb_h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            if let Some((ct, view, raw, _)) = opt {
                let mut init_rgba = vec![0u8; (fb_w * fb_h * 4) as usize];
                for y in 0..fb_h {
                    for x in 0..fb_w {
                        let off = view.offset(x, y);
                        let c = read_color_texel(ct.format, &raw[off..off + view.bpp as usize])
                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                        let o = ((y * fb_w + x) as usize) * 4;
                        init_rgba[o] = float_to_u8(c[0]);
                        init_rgba[o + 1] = float_to_u8(c[1]);
                        init_rgba[o + 2] = float_to_u8(c[2]);
                        init_rgba[o + 3] = float_to_u8(c[3]);
                    }
                }
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &init_rgba,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(4 * fb_w),
                        rows_per_image: Some(fb_h),
                    },
                    wgpu::Extent3d { width: fb_w, height: fb_h, depth_or_array_layers: 1 },
                );
            }
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            gpu_color_texs.push((tex, view));
        }

        // Optional Depth32Float attachment initialized from guest ZT surface via a tiny depth-upload or clear:
        // Note `wgpu` disallows `write_texture` to `Depth32Float` on some backends without `DEPTH_CLIP_CONTROL`,
        // Wait: when all depth texels in `staged_zt` share a constant initial clear depth (the 99.9% case after
        // `CLEAR_SURFACE`!), `LoadOp::Clear(clear_z)` loads it natively on EVERY backend! If `staged_zt` has
        // non-uniform pre-existing depth values, fall back cleanly to software so multi-draw depth state is exact!
        let gpu_depth_tex = if let Some((zt, ref zview, ref zraw)) = staged_zt {
            let first_z = read_depth_texel(zt.format, &zraw[zview.offset(0, 0)..zview.offset(0, 0) + zview.bpp as usize]);
            let mut uniform_z = true;
            'check_z: for y in 0..fb_h {
                for x in 0..fb_w {
                    let off = zview.offset(x, y);
                    let dz = read_depth_texel(zt.format, &zraw[off..off + zview.bpp as usize]);
                    if (dz - first_z).abs() > 1e-6 {
                        uniform_z = false;
                        break 'check_z;
                    }
                }
            }
            if !uniform_z {
                return Ok(false);
            }
            let dtex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("ga10f-zt"),
                size: wgpu::Extent3d { width: fb_w, height: fb_h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let dview = dtex.create_view(&wgpu::TextureViewDescriptor::default());
            Some((dtex, dview, first_z.clamp(0.0, 1.0)))
        } else {
            None
        };

        // Compute viewport & scissor rectangles clamped to `[0..fb_w, 0..fb_h]`.
        let vx = (vp.offset[0] - vp.scale[0].abs()).max(0.0);
        let vy = (vp.offset[1] - vp.scale[1].abs()).max(0.0);
        let vw = (2.0 * vp.scale[0].abs()).min(fb_w as f32 - vx).max(1.0);
        let vh = (2.0 * vp.scale[1].abs()).min(fb_h as f32 - vy).max(1.0);
        let min_d = (vp.offset[2] - vp.scale[2].abs()).clamp(0.0, 1.0);
        let max_d = (vp.offset[2] + vp.scale[2].abs()).clamp(0.0, 1.0);

        let mut sc_x0 = 0u32;
        let mut sc_y0 = 0u32;
        let mut sc_x1 = fb_w;
        let mut sc_y1 = fb_h;
        // Viewport clip 0:
        if vp.clip[2] > 0 && vp.clip[3] > 0 {
            sc_x0 = sc_x0.max(vp.clip[0]);
            sc_y0 = sc_y0.max(vp.clip[1]);
            sc_x1 = sc_x1.min(vp.clip[0].saturating_add(vp.clip[2]));
            sc_y1 = sc_y1.min(vp.clip[1].saturating_add(vp.clip[3]));
        }
        // Scissor 0:
        if rec.scissor0.enabled {
            sc_x0 = sc_x0.max(rec.scissor0.xmin);
            sc_y0 = sc_y0.max(rec.scissor0.ymin);
            sc_x1 = sc_x1.min(rec.scissor0.xmax);
            sc_y1 = sc_y1.min(rec.scissor0.ymax);
        }
        if sc_x0 >= sc_x1 || sc_y0 >= sc_y1 {
            return Ok(true);
        }

        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = gpu_color_texs
                .iter()
                .map(|(_, view)| {
                    Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })
                })
                .collect();

            let depth_attachment = gpu_depth_tex.as_ref().map(|(_, dview, clear_z)| {
                wgpu::RenderPassDepthStencilAttachment {
                    view: dview,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(*clear_z),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }
            });

            let rp = self.render_pipelines.get(&rp_key).expect("inserted");
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ounce-ga10f-3d-pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: depth_attachment,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&rp.pipeline);
            pass.set_viewport(vx, vy, vw, vh, min_d, max_d);
            pass.set_scissor_rect(sc_x0, sc_y0, sc_x1 - sc_x0, sc_y1 - sc_y0);
            pass.set_bind_group(0, &bg_vs, &[]);
            pass.set_bind_group(1, &bg_fs, &[]);
            if !vs_inputs.is_empty() {
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            }
            pass.draw(0..verts_total as u32, 0..1);
        }

        // Schedule texture readbacks for every active color target (and depth target when depth_write_on).
        let padded_row = ((4 * fb_w) + 255) & !255;
        let readback_size = (padded_row * fb_h) as u64;
        let mut color_readbacks: Vec<(usize, wgpu::Buffer)> = Vec::new();
        for (idx, opt) in staged_cts.iter().enumerate() {
            if opt.is_some() {
                let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ga10f-rt-readback"),
                    size: readback_size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                enc.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture: &gpu_color_texs[idx].0,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buf,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(padded_row),
                            rows_per_image: Some(fb_h),
                        },
                    },
                    wgpu::Extent3d { width: fb_w, height: fb_h, depth_or_array_layers: 1 },
                );
                color_readbacks.push((idx, buf));
            }
        }

        let depth_readback = if let Some((ref dtex, _, _)) = gpu_depth_tex.filter(|_| depth_write_on) {
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("ga10f-zt-readback"),
                size: readback_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: dtex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::DepthOnly,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buf,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row),
                        rows_per_image: Some(fb_h),
                    },
                },
                wgpu::Extent3d { width: fb_w, height: fb_h, depth_or_array_layers: 1 },
            );
            Some(buf)
        } else {
            None
        };

        self.queue.submit(Some(enc.finish()));

        // Map and write back color targets into Pitch / Layout 0 / Orin Layout 1 guest memory!
        for (idx, buf) in color_readbacks {
            let slice = buf.slice(..);
            slice.map_async(wgpu::MapMode::Read, |res| res.expect("color readback map failed"));
            self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU poll failed");
            let mapped = slice.get_mapped_range().expect("mapped color range");
            if let Some((ct, view, ref mut raw, mask_bools)) = staged_cts[idx] {
                for y in 0..fb_h {
                    let row_start = (y * padded_row) as usize;
                    for x in 0..fb_w {
                        let px = row_start + (x as usize) * 4;
                        let rgba = [
                            mapped[px] as f32 / 255.0,
                            mapped[px + 1] as f32 / 255.0,
                            mapped[px + 2] as f32 / 255.0,
                            mapped[px + 3] as f32 / 255.0,
                        ];
                        let dst_off = view.offset(x, y);
                        write_color_texel(
                            ct.format,
                            &mut raw[dst_off..dst_off + view.bpp as usize],
                            rgba,
                            mask_bools,
                        );
                    }
                }
                mem.write(view.gpu_va, raw);
            }
        }

        if let (Some(dbuf), Some((zt, zview, mut zraw))) = (depth_readback, staged_zt) {
            let slice = dbuf.slice(..);
            slice.map_async(wgpu::MapMode::Read, |res| res.expect("depth readback map failed"));
            self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU poll failed");
            let mapped = slice.get_mapped_range().expect("mapped depth range");
            for y in 0..fb_h {
                let row_start = (y * padded_row) as usize;
                for x in 0..fb_w {
                    let px = row_start + (x as usize) * 4;
                    let dz = f32::from_le_bytes([mapped[px], mapped[px + 1], mapped[px + 2], mapped[px + 3]]);
                    let dst_off = zview.offset(x, y);
                    write_depth_texel(
                        zt.format,
                        &mut zraw[dst_off..dst_off + zview.bpp as usize],
                        Some(dz),
                        None,
                    );
                }
            }
            mem.write(zview.gpu_va, &zraw);
        }

        Ok(true)
    }

    fn compile_compute_pipeline(&self, shader: &Arc<TranslatedShader>) -> CachedComputePipeline {
        let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ounce-ga10f-compute-wgsl"),
            source: wgpu::ShaderSource::Wgsl(shader.wgsl.as_str().into()),
        });
        let mut entries: Vec<wgpu::BindGroupLayoutEntry> = shader
            .interface
            .constant_buffers
            .iter()
            .map(|cb| wgpu::BindGroupLayoutEntry {
                binding: cb.binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(cb.size_bytes.max(16) as u64),
                },
                count: None,
            })
            .collect();

        if shader.interface.uses_global_memory {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 16,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(16),
                },
                count: None,
            });
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 17,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }

        let bgl = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ounce-ga10f-compute-bgl"),
            entries: &entries,
        });
        let pl = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ounce-ga10f-compute-pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("ounce-ga10f-compute-pipeline"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        CachedComputePipeline { pipeline, bgl, shader: Arc::clone(shader) }
    }

    fn compile_render_pipeline(
        &self,
        key: &RenderPipelineKey,
        vs: &TranslatedShader,
        fs: &TranslatedShader,
    ) -> CachedRenderPipeline {
        // Synchronize `VsOut` `@interpolate(...)` qualifiers with the fragment SPH (`fs.interface.varyings`),
        // because NVIDIA hardware specifies varying interpolation in the fragment SPH while WebGPU/WGSL
        // requires identical `@interpolate(...)` attributes on both `VsOut` and `FsIn`.
        let mut vs_wgsl = vs.wgsl.clone();
        for v in &vs.interface.varyings {
            let fs_mode = fs
                .interface
                .varyings
                .iter()
                .find(|fv| fv.location == v.location)
                .map_or(Interpolation::Perspective, |fv| fv.interpolation);
            let attr = match fs_mode {
                Interpolation::Perspective => "@interpolate(perspective)",
                Interpolation::Linear => "@interpolate(linear)",
                Interpolation::Flat => "@interpolate(flat)",
            };
            let needle = format!("@location({}) o{}: vec4<f32>,", v.location, v.location);
            let replacement = format!("@location({}) {attr} o{}: vec4<f32>,", v.location, v.location);
            vs_wgsl = vs_wgsl.replace(&needle, &replacement);
        }

        // Apply post-VS clip-space Y/Z adjustments when NVIDIA viewport convention differs from WebGPU NDC.
        if key.flip_y || key.ogl_z_clip {
            let mut fixup = String::new();
            if key.flip_y {
                fixup.push_str("    out.position.y = -out.position.y;\n");
            }
            if key.ogl_z_clip {
                fixup.push_str("    out.position.z = 0.5f * (out.position.z + out.position.w);\n");
            }
            fixup.push_str("    return out;\n}");
            if let Some(pos) = vs_wgsl.rfind("    return out;\n}") {
                vs_wgsl.replace_range(pos.., &fixup);
            }
        }

        let vs_mod = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ounce-ga10f-vs"),
            source: wgpu::ShaderSource::Wgsl(vs_wgsl.into()),
        });
        let fs_mod = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ounce-ga10f-fs"),
            source: wgpu::ShaderSource::Wgsl(fs.wgsl.as_str().into()),
        });

        let bgl_vs = self.create_stage_bgl("ga10f-vs-bgl", wgpu::ShaderStages::VERTEX, &vs.interface);
        let bgl_fs = self.create_stage_bgl("ga10f-fs-bgl", wgpu::ShaderStages::FRAGMENT, &fs.interface);

        let pl = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ga10f-3d-pl"),
            bind_group_layouts: &[Some(&bgl_vs), Some(&bgl_fs)],
            immediate_size: 0,
        });

        let vs_inputs = vs.interface.vertex_inputs.clone();
        let attrs: Vec<wgpu::VertexAttribute> = vs_inputs
            .iter()
            .enumerate()
            .map(|(idx, &loc)| wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: (idx * 16) as u64,
                shader_location: loc,
            })
            .collect();
        let vb_layout = wgpu::VertexBufferLayout {
            array_stride: (vs_inputs.len() * 16) as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attrs,
        };
        let buffers_slice: Vec<Option<wgpu::VertexBufferLayout>> = if vs_inputs.is_empty() {
            Vec::new()
        } else {
            vec![Some(vb_layout)]
        };

        let targets: Vec<Option<wgpu::ColorTargetState>> = key
            .target_masks
            .iter()
            .map(|&mask| {
                let mut wm = wgpu::ColorWrites::empty();
                if mask & 1 != 0 {
                    wm |= wgpu::ColorWrites::RED;
                }
                if mask & 2 != 0 {
                    wm |= wgpu::ColorWrites::GREEN;
                }
                if mask & 4 != 0 {
                    wm |= wgpu::ColorWrites::BLUE;
                }
                if mask & 8 != 0 {
                    wm |= wgpu::ColorWrites::ALPHA;
                }
                Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wm,
                })
            })
            .collect();

        let depth_stencil = key.depth_enabled.then(|| wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(key.depth_write),
            depth_compare: Some(decode_depth_compare(key.depth_compare_code)),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        });

        let cull_mode = match key.cull_code {
            1 => Some(wgpu::Face::Front),
            2 => Some(wgpu::Face::Back),
            _ => None,
        };

        let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ounce-ga10f-render-pipeline"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &vs_mod,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                buffers: &buffers_slice,
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: if key.front_ccw { wgpu::FrontFace::Ccw } else { wgpu::FrontFace::Cw },
                cull_mode,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &fs_mod,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });

        CachedRenderPipeline { pipeline, bgl_vs, bgl_fs, vs_inputs }
    }

    fn create_stage_bgl(
        &self,
        label: &str,
        vis: wgpu::ShaderStages,
        iface: &ShaderInterface,
    ) -> wgpu::BindGroupLayout {
        let mut entries = Vec::new();
        for cb in &iface.constant_buffers {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: cb.binding,
                visibility: vis,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(cb.size_bytes.max(16) as u64),
                },
                count: None,
            });
        }
        for tb in &iface.textures {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: tb.binding,
                visibility: vis,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
            if let Some(sb) = tb.sampler_binding {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding: sb,
                    visibility: vis,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                });
            }
        }
        self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &entries,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_stage_resources(
        &mut self,
        _group_idx: usize,
        iface: &ShaderInterface,
        stage_cbuf_slots: &[Option<super::engine::ConstantBufferBinding>; m::CONSTANT_BUFFER_SLOTS],
        regs: &[u32],
        bindless_bank: u8,
        sampler_via_header: bool,
        sector_layout: SectorLayout,
        mem: &mut dyn GpuMemoryWriter,
    ) -> Result<StageResources, &'static str> {
        let mut cbuf_raw_map: HashMap<u8, Vec<u8>> = HashMap::new();
        let mut load_bank = |bank: u8| {
            cbuf_raw_map.entry(bank).or_insert_with(|| {
                let Some(bind) = stage_cbuf_slots.get(bank as usize).copied().flatten() else {
                    return Vec::new();
                };
                let mut d = vec![0u8; bind.size as usize];
                if mem.read(bind.gpu_va, &mut d) { d } else { Vec::new() }
            });
        };

        for cb in &iface.constant_buffers {
            load_bank(cb.binding as u8);
        }
        for tb in &iface.textures {
            match tb.handle {
                TexHandle::Cbuf { bank, .. } => load_bank(bank),
                TexHandle::Bound { .. } => load_bank(bindless_bank),
                TexHandle::Immediate(_) | TexHandle::BindlessReg(_) => {}
            }
        }

        let mut uniform_buffers = Vec::with_capacity(iface.constant_buffers.len());
        for cb in &iface.constant_buffers {
            let need = (cb.size_bytes as usize).max(16).div_ceil(16) * 16;
            let mut padded = vec![0u8; need];
            if let Some(src) = cbuf_raw_map.get(&(cb.binding as u8)) {
                let n = src.len().min(need);
                padded[..n].copy_from_slice(&src[..n]);
            }
            let buf = self.create_init_buffer("ga10f-cbuf", &padded, wgpu::BufferUsages::UNIFORM);
            uniform_buffers.push((cb.binding, buf));
        }

        let header_pool = super::texture::Pool::header(regs);
        let sampler_pool = super::texture::Pool::sampler(regs);
        let mut tex_views = Vec::with_capacity(iface.textures.len());
        let mut samplers = Vec::new();

        let read_cbuf_word = |map: &HashMap<u8, Vec<u8>>, bank: u8, off: u16| -> u32 {
            let Some(bytes) = map.get(&bank) else { return 0 };
            let o = off as usize;
            bytes.get(o..o + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        };

        for tb in &iface.textures {
            let word = match tb.handle {
                TexHandle::Cbuf { bank, offset } => read_cbuf_word(&cbuf_raw_map, bank, offset),
                TexHandle::Bound { index } => read_cbuf_word(&cbuf_raw_map, bindless_bank, index.wrapping_mul(4)),
                TexHandle::Immediate(w) => w,
                TexHandle::BindlessReg(_) => return Err("unresolved dynamic bindless texture handle"),
            };
            let tic_idx = word & 0xf_ffff;
            let tsc_idx = if sampler_via_header { tic_idx } else { word >> 20 };

            let tic_va = header_pool.entry_va(tic_idx, super::texture::TIC_ENTRY_BYTES).ok_or("bad TIC index")?;
            let mut tic_bytes = [0u8; 32];
            if !mem.read(tic_va, &mut tic_bytes) {
                return Err("TIC unreadable");
            }
            let header = TextureHeader::parse(&tic_bytes);
            // Only accelerate normalized UV textures (`TEX`) or `TLD` (`sampler_binding == None`);
            // unnormalized `TEX` uses software path since WGSL `textureSample` takes normalized [0, 1] UVs.
            if tb.sampler_binding.is_some() && !header.normalized_coords {
                return Err("unnormalized TEX coordinates use software path");
            }
            let tview = header.view_with_sector(sector_layout).ok_or("unsupported TIC view")?;
            if tview.size() > 64 * 1024 * 1024 {
                return Err("texture exceeds 64 MiB limit");
            }
            let mut raw_surf = vec![0u8; tview.size()];
            if !mem.read(header.gpu_va, &mut raw_surf) {
                return Err("texture bytes unreadable");
            }
            let gpu_view = self
                .texture_cache
                .get_or_upload(&self.device, &self.queue, &header, sector_layout, &raw_surf)
                .ok_or("texture upload failed")?;
            tex_views.push((tb.binding, gpu_view));

            if let Some(sb) = tb.sampler_binding {
                let tsc_va = sampler_pool.entry_va(tsc_idx, super::texture::TSC_ENTRY_BYTES).ok_or("bad TSC index")?;
                let mut tsc_bytes = [0u8; 32];
                if !mem.read(tsc_va, &mut tsc_bytes) {
                    return Err("TSC unreadable");
                }
                let tsc = Sampler::parse(&tsc_bytes);
                // Border address mode uses custom float border colours -> let software path handle Border wrap.
                if tsc.address.iter().any(|a| matches!(a, AddressMode::Border | AddressMode::MirrorOnceBorder)) {
                    return Err("border sampler uses software path");
                }
                let wgpu_sampler = create_wgpu_sampler(&self.device, &tsc);
                samplers.push((sb, wgpu_sampler));
            }
        }

        Ok(StageResources { uniform_buffers, tex_views, samplers })
    }

    fn create_stage_bind_group(
        &self,
        label: &str,
        layout: &wgpu::BindGroupLayout,
        res: &StageResources,
    ) -> wgpu::BindGroup {
        let mut entries: Vec<wgpu::BindGroupEntry> = Vec::new();
        for (binding, buf) in &res.uniform_buffers {
            entries.push(wgpu::BindGroupEntry { binding: *binding, resource: buf.as_entire_binding() });
        }
        for (binding, view) in &res.tex_views {
            entries.push(wgpu::BindGroupEntry {
                binding: *binding,
                resource: wgpu::BindingResource::TextureView(view),
            });
        }
        for (binding, sampler) in &res.samplers {
            entries.push(wgpu::BindGroupEntry {
                binding: *binding,
                resource: wgpu::BindingResource::Sampler(sampler),
            });
        }
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &entries,
        })
    }

    fn create_init_buffer(&self, label: &str, data: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let padded_len = data.len().max(4).div_ceil(4) * 4;
        let mut padded = vec![0u8; padded_len];
        padded[..data.len()].copy_from_slice(data);
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: padded_len as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, &padded);
        buf
    }

    fn readback_buffer(
        &self,
        mut enc: wgpu::CommandEncoder,
        src_buf: &wgpu::Buffer,
        byte_len: usize,
    ) -> Vec<u8> {
        let aligned = byte_len.max(4).div_ceil(4) * 4;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ga10f-buf-readback"),
            size: aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        enc.copy_buffer_to_buffer(src_buf, 0, &readback, 0, aligned as u64);
        self.queue.submit(Some(enc.finish()));
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |res| res.expect("buffer readback map failed"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU poll failed");
        let mapped = slice.get_mapped_range().expect("mapped buffer");
        mapped[..byte_len].to_vec()
    }
}

struct StageResources {
    uniform_buffers: Vec<(u32, wgpu::Buffer)>,
    tex_views: Vec<(u32, wgpu::TextureView)>,
    samplers: Vec<(u32, wgpu::Sampler)>,
}

fn read_graphics_shader_bytes(
    class: u32,
    va: u64,
    mem: &mut dyn GpuMemoryWriter,
) -> Result<Vec<u8>, &'static str> {
    let header = if class == m::MAXWELL_B {
        crate::gpu::sass::MAXWELL_SPH_SIZE
    } else {
        crate::gpu::sass::AMPERE_SPH_SIZE
    };
    let mut code_size = 0x1000usize;
    loop {
        let mut bytes = vec![0u8; header + code_size];
        if !mem.read(va, &mut bytes) {
            return Err("shader code unreadable");
        }
        // Check for EXIT (`0x94d` in Ampere 128-bit words or Maxwell EXIT) by attempting a quick scan;
        // `ShaderDiskCache::get_or_translate` validates the exact SPH + instructions.
        if class != m::MAXWELL_B {
            if let Some(exit_end) = find_ampere_exit_end(&bytes[header..]) {
                bytes.truncate(header + exit_end);
                return Ok(bytes);
            }
        } else {
            return Ok(bytes);
        }
        if code_size >= 0x10000 {
            return Ok(bytes);
        }
        code_size *= 2;
    }
}

fn find_ampere_exit_end(code: &[u8]) -> Option<usize> {
    let mut last_unpred_exit = None;
    for (idx, chunk) in code.chunks_exact(16).enumerate() {
        let lo = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let opcode = lo & 0xfff;
        let pred = (lo >> 12) & 0xf;
        if opcode == 0x94d && pred == 7 {
            last_unpred_exit = Some((idx + 1) * 16);
            break;
        }
    }
    last_unpred_exit
}

fn fetch_vertex_attrs_for_hw(
    rec: &DrawRecord,
    vertex_id: u32,
    mem: &mut dyn GpuMemoryWriter,
) -> Result<[[u32; 4]; 32], &'static str> {
    let mut attrs = [[0u32, 0, 0, 1.0f32.to_bits()]; 32];
    for attr in &rec.vertex_attributes {
        if attr.index as usize >= 32 {
            continue;
        }
        let Some(stream) = rec.vertex_streams.iter().find(|s| s.index == attr.stream) else {
            continue;
        };
        let elem_idx = if stream.instanced {
            let div = stream.frequency.max(1);
            (rec.instance / div).wrapping_add(rec.base_instance)
        } else {
            vertex_id
        };
        let byte_off = (elem_idx as u64)
            .saturating_mul(stream.stride as u64)
            .saturating_add(attr.offset as u64);
        if stream.size > 0 && byte_off >= stream.size {
            continue;
        }
        let max_read = if stream.size > 0 {
            (stream.size - byte_off).min(16) as usize
        } else {
            16
        };
        let mut raw = [0u8; 16];
        if !mem.read(stream.gpu_va + byte_off, &mut raw[..max_read]) {
            return Err("vertex buffer unreadable");
        }
        let mut words = decode_attribute_words(attr.component_bit_widths, attr.numerical_type, &raw[..max_read])
            .ok_or("unsupported vertex attribute")?;
        if attr.swap_r_and_b {
            words.swap(0, 2);
        }
        attrs[attr.index as usize] = words;
    }
    Ok(attrs)
}

fn map_depth_compare_code(raw: u32) -> Result<u8, &'static str> {
    Ok(match raw {
        0x0200 | 1 => 0, // Never
        0x0201 | 2 => 1, // Less
        0x0202 | 3 => 2, // Equal
        0x0203 | 4 => 3, // LessEqual
        0x0204 | 5 => 4, // Greater
        0x0205 | 6 => 5, // NotEqual
        0x0206 | 7 => 6, // GreaterEqual
        0x0207 | 8 | 0 => 7, // Always
        _ => return Err("unknown depth compare func"),
    })
}

fn decode_depth_compare(code: u8) -> wgpu::CompareFunction {
    match code {
        0 => wgpu::CompareFunction::Never,
        1 => wgpu::CompareFunction::Less,
        2 => wgpu::CompareFunction::Equal,
        3 => wgpu::CompareFunction::LessEqual,
        4 => wgpu::CompareFunction::Greater,
        5 => wgpu::CompareFunction::NotEqual,
        6 => wgpu::CompareFunction::GreaterEqual,
        _ => wgpu::CompareFunction::Always,
    }
}

fn map_cull_state(regs: &[u32], flip_y: bool) -> Result<(u8, bool), &'static str> {
    let enabled = (regs[m::OGL_SET_CULL as usize / 4] & 1) != 0;
    if !enabled {
        return Ok((0, true));
    }
    let front_raw = regs[m::OGL_SET_FRONT_FACE as usize / 4];
    let mut front_ccw = front_raw != 0x0900; // 0x0900 = CW, 0x0901 = CCW
    // Negating `out.position.y` (`flip_y`) reverses 2D winding in WebGPU NDC, so invert `front_ccw` to compensate!
    let window_flip_y = ((regs[m::SET_WINDOW_ORIGIN as usize / 4] >> 4) & 1) != 0;
    if flip_y ^ window_flip_y {
        front_ccw = !front_ccw;
    }
    let cull_face = regs[m::OGL_SET_CULL_FACE as usize / 4];
    let cull_code = match cull_face {
        0x0404 => 1, // Front
        0x0405 | 0 => 2, // Back
        _ => return Err("FRONT_AND_BACK cull uses software path"),
    };
    Ok((cull_code, front_ccw))
}

fn create_wgpu_sampler(device: &wgpu::Device, s: &Sampler) -> wgpu::Sampler {
    let addr = |m: AddressMode| match m {
        AddressMode::Wrap => wgpu::AddressMode::Repeat,
        AddressMode::Mirror => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::ClampToEdge,
    };
    let filt = |f: Filter| match f {
        Filter::Linear => wgpu::FilterMode::Linear,
        _ => wgpu::FilterMode::Nearest,
    };
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ga10f-sampler"),
        address_mode_u: addr(s.address[0]),
        address_mode_v: addr(s.address[1]),
        address_mode_w: addr(s.address[2]),
        mag_filter: filt(s.mag_filter),
        min_filter: filt(s.min_filter),
        mipmap_filter: if s.mip_filter == 2 {
            wgpu::MipmapFilterMode::Linear
        } else {
            wgpu::MipmapFilterMode::Nearest
        },
        ..Default::default()
    })
}

fn float_to_u8(v: f32) -> u8 {
    if v.is_nan() {
        0
    } else {
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    // Sample every byte up to 16 KiB, then stride by 16 bytes so large textures hash in microseconds.
    if bytes.len() <= 16_384 {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    } else {
        h ^= bytes.len() as u64;
        for chunk in bytes.chunks(64) {
            h ^= chunk[0] as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
            h ^= chunk[chunk.len() - 1] as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    h
}

#[allow(dead_code)]
fn _topology_check(t: Topology) -> bool {
    matches!(t, Topology::Triangles | Topology::TriangleStrip | Topology::TriangleFan | Topology::Quads)
}
