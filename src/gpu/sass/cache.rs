//! Persistent disk + in-memory WGSL translation cache for Switch 2 (`GA10F`) shaders.
//!
//! Avoids re-decoding and re-lowering identical Ampere SM86 / Maxwell SASS binaries across
//! draws, compute dispatches, and emulator restarts. Disk entries are written atomically via a
//! sibling temporary file + `rename` so interrupted runs never leave truncated cache files.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::decode::TexHandle;
use super::{
    translate_compute_for_class, translate_for_class, ConstantBuffer, Interpolation,
    ShaderInterface, ShaderStage, TextureBinding, TranslatedShader, Varying, XlateError,
};

const CACHE_MAGIC: &[u8; 8] = b"ONX2WGSL";
const CACHE_VERSION: u32 = 1;

/// Deterministic 128-bit content hash for `(class, stage, shader_bytes)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShaderCacheKey(pub u128);

impl ShaderCacheKey {
    pub fn compute(class: u32, stage: ShaderStage, bytes: &[u8]) -> Self {
        // Dual 64-bit FNV-1a lanes with independent seeds -> stable 128-bit digest across runs.
        let mut h_lo: u64 = 0xcbf2_9ce4_8422_2325;
        let mut h_hi: u64 = 0x8422_2325_cbf2_9ce4 ^ 0x4f55_4e43_455f_4e58;

        let mut feed = |slice: &[u8]| {
            for &b in slice {
                h_lo ^= b as u64;
                h_lo = h_lo.wrapping_mul(0x0000_0100_0000_01B3);
                h_hi ^= (b.rotate_left(3)) as u64;
                h_hi = h_hi.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };

        feed(CACHE_MAGIC);
        feed(&CACHE_VERSION.to_le_bytes());
        feed(&class.to_le_bytes());
        match stage {
            ShaderStage::Vertex => feed(&[1, 0, 0, 0]),
            ShaderStage::Fragment => feed(&[2, 0, 0, 0]),
            ShaderStage::Compute { workgroup_size } => {
                feed(&[3]);
                for d in workgroup_size {
                    feed(&d.to_le_bytes());
                }
            }
        }
        feed(&(bytes.len() as u64).to_le_bytes());
        feed(bytes);

        ShaderCacheKey(((h_hi as u128) << 64) | (h_lo as u128))
    }

    pub fn filename(self) -> String {
        format!("{:032x}.wgslbin", self.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShaderCacheStats {
    pub mem_hits: u64,
    pub disk_hits: u64,
    pub misses: u64,
}

#[derive(Debug, Default)]
pub struct ShaderDiskCache {
    dir: Option<PathBuf>,
    entries: HashMap<ShaderCacheKey, Arc<TranslatedShader>>,
    stats: ShaderCacheStats,
}

impl ShaderDiskCache {
    pub fn in_memory() -> Self {
        Self { dir: None, entries: HashMap::new(), stats: ShaderCacheStats::default() }
    }

    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: Some(dir.into()), entries: HashMap::new(), stats: ShaderCacheStats::default() }
    }

    pub fn default_dir() -> Self {
        Self::with_dir("cache/shaders")
    }

    pub fn stats(&self) -> ShaderCacheStats {
        self.stats
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn get_or_translate(
        &mut self,
        class: u32,
        stage: ShaderStage,
        bytes: &[u8],
    ) -> Result<(ShaderCacheKey, Arc<TranslatedShader>), XlateError> {
        let key = ShaderCacheKey::compute(class, stage, bytes);
        if let Some(hit) = self.entries.get(&key) {
            self.stats.mem_hits += 1;
            return Ok((key, Arc::clone(hit)));
        }

        if let Some(dir) = &self.dir {
            let path = dir.join(key.filename());
            if let Ok(raw) = fs::read(&path) {
                if let Some(loaded) = deserialize_shader(&raw) {
                    if loaded.stage == stage {
                        let arc = Arc::new(loaded);
                        self.entries.insert(key, Arc::clone(&arc));
                        self.stats.disk_hits += 1;
                        return Ok((key, arc));
                    }
                }
            }
        }

        self.stats.misses += 1;
        let translated = match stage {
            ShaderStage::Compute { workgroup_size } => {
                translate_compute_for_class(class, bytes, workgroup_size)?
            }
            ShaderStage::Vertex | ShaderStage::Fragment => translate_for_class(class, stage, bytes)?,
        };

        if let Some(dir) = &self.dir {
            let _ = write_entry_atomic(dir, key, &translated);
        }

        let arc = Arc::new(translated);
        self.entries.insert(key, Arc::clone(&arc));
        Ok((key, arc))
    }
}

fn write_entry_atomic(dir: &Path, key: ShaderCacheKey, shader: &TranslatedShader) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let final_path = dir.join(key.filename());
    let tmp_path = dir.join(format!("{}.tmp.{}", key.filename(), std::process::id()));
    let payload = serialize_shader(shader);
    fs::write(&tmp_path, &payload)?;
    fs::rename(&tmp_path, &final_path)
}

fn serialize_shader(s: &TranslatedShader) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + s.wgsl.len());
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_VERSION.to_le_bytes());

    match s.stage {
        ShaderStage::Vertex => {
            out.push(1);
            out.extend_from_slice(&[0u8; 12]);
        }
        ShaderStage::Fragment => {
            out.push(2);
            out.extend_from_slice(&[0u8; 12]);
        }
        ShaderStage::Compute { workgroup_size } => {
            out.push(3);
            for d in workgroup_size {
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
    }

    put_str(&mut out, &s.wgsl);

    let iface = &s.interface;
    put_u32_slice(&mut out, &iface.vertex_inputs);
    put_u32(&mut out, iface.varyings.len() as u32);
    for v in &iface.varyings {
        put_u32(&mut out, v.location);
        out.push(match v.interpolation {
            Interpolation::Perspective => 0,
            Interpolation::Linear => 1,
            Interpolation::Flat => 2,
        });
    }
    put_u32_slice(&mut out, &iface.color_targets);
    put_u32(&mut out, iface.constant_buffers.len() as u32);
    for cb in &iface.constant_buffers {
        put_u32(&mut out, cb.group);
        put_u32(&mut out, cb.binding);
        put_u32(&mut out, cb.size_bytes);
    }
    put_u32(&mut out, iface.textures.len() as u32);
    for tb in &iface.textures {
        put_u32(&mut out, tb.group);
        put_u32(&mut out, tb.binding);
        match tb.sampler_binding {
            Some(sb) => {
                out.push(1);
                put_u32(&mut out, sb);
            }
            None => out.push(0),
        }
        match tb.handle {
            TexHandle::Cbuf { bank, offset } => {
                out.push(0);
                out.push(bank);
                out.extend_from_slice(&offset.to_le_bytes());
            }
            TexHandle::Bound { index } => {
                out.push(1);
                out.extend_from_slice(&index.to_le_bytes());
            }
            TexHandle::BindlessReg(r) => {
                out.push(2);
                out.push(r);
            }
            TexHandle::Immediate(w) => {
                out.push(3);
                put_u32(&mut out, w);
            }
        }
    }
    out.push(u8::from(iface.writes_position));
    out.push(u8::from(iface.uses_global_memory));

    put_u32(&mut out, s.listing.len() as u32);
    for line in &s.listing {
        put_str(&mut out, line);
    }
    out
}

fn deserialize_shader(buf: &[u8]) -> Option<TranslatedShader> {
    let mut cur = 0usize;
    if buf.get(cur..cur + 8)? != CACHE_MAGIC {
        return None;
    }
    cur += 8;
    let ver = take_u32(buf, &mut cur)?;
    if ver != CACHE_VERSION {
        return None;
    }
    let stage_tag = take_u8(buf, &mut cur)?;
    let wgx = take_u32(buf, &mut cur)?;
    let wgy = take_u32(buf, &mut cur)?;
    let wgz = take_u32(buf, &mut cur)?;
    let stage = match stage_tag {
        1 => ShaderStage::Vertex,
        2 => ShaderStage::Fragment,
        3 => ShaderStage::Compute { workgroup_size: [wgx, wgy, wgz] },
        _ => return None,
    };

    let wgsl = take_str(buf, &mut cur)?;
    let vertex_inputs = take_u32_vec(buf, &mut cur)?;
    let vary_len = take_u32(buf, &mut cur)? as usize;
    let mut varyings = Vec::with_capacity(vary_len.min(64));
    for _ in 0..vary_len {
        let location = take_u32(buf, &mut cur)?;
        let interpolation = match take_u8(buf, &mut cur)? {
            0 => Interpolation::Perspective,
            1 => Interpolation::Linear,
            2 => Interpolation::Flat,
            _ => return None,
        };
        varyings.push(Varying { location, interpolation });
    }
    let color_targets = take_u32_vec(buf, &mut cur)?;
    let cb_len = take_u32(buf, &mut cur)? as usize;
    let mut constant_buffers = Vec::with_capacity(cb_len.min(32));
    for _ in 0..cb_len {
        let group = take_u32(buf, &mut cur)?;
        let binding = take_u32(buf, &mut cur)?;
        let size_bytes = take_u32(buf, &mut cur)?;
        constant_buffers.push(ConstantBuffer { group, binding, size_bytes });
    }
    let tex_len = take_u32(buf, &mut cur)? as usize;
    let mut textures = Vec::with_capacity(tex_len.min(64));
    for _ in 0..tex_len {
        let group = take_u32(buf, &mut cur)?;
        let binding = take_u32(buf, &mut cur)?;
        let sampler_binding = match take_u8(buf, &mut cur)? {
            0 => None,
            1 => Some(take_u32(buf, &mut cur)?),
            _ => return None,
        };
        let handle = match take_u8(buf, &mut cur)? {
            0 => {
                let bank = take_u8(buf, &mut cur)?;
                let lo = take_u8(buf, &mut cur)?;
                let hi = take_u8(buf, &mut cur)?;
                TexHandle::Cbuf { bank, offset: u16::from_le_bytes([lo, hi]) }
            }
            1 => {
                let lo = take_u8(buf, &mut cur)?;
                let hi = take_u8(buf, &mut cur)?;
                TexHandle::Bound { index: u16::from_le_bytes([lo, hi]) }
            }
            2 => TexHandle::BindlessReg(take_u8(buf, &mut cur)?),
            3 => TexHandle::Immediate(take_u32(buf, &mut cur)?),
            _ => return None,
        };
        textures.push(TextureBinding { group, binding, sampler_binding, handle });
    }
    let writes_position = take_u8(buf, &mut cur)? != 0;
    let uses_global_memory = take_u8(buf, &mut cur)? != 0;
    let list_len = take_u32(buf, &mut cur)? as usize;
    let mut listing = Vec::with_capacity(list_len.min(4096));
    for _ in 0..list_len {
        listing.push(take_str(buf, &mut cur)?);
    }

    Some(TranslatedShader {
        stage,
        wgsl,
        interface: ShaderInterface {
            vertex_inputs,
            varyings,
            color_targets,
            constant_buffers,
            textures,
            writes_position,
            uses_global_memory,
        },
        listing,
    })
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32_slice(out: &mut Vec<u8>, xs: &[u32]) {
    put_u32(out, xs.len() as u32);
    for &x in xs {
        put_u32(out, x);
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn take_u8(buf: &[u8], cur: &mut usize) -> Option<u8> {
    let b = *buf.get(*cur)?;
    *cur += 1;
    Some(b)
}

fn take_u32(buf: &[u8], cur: &mut usize) -> Option<u32> {
    let slice = buf.get(*cur..*cur + 4)?;
    *cur += 4;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn take_u32_vec(buf: &[u8], cur: &mut usize) -> Option<Vec<u32>> {
    let len = take_u32(buf, cur)? as usize;
    if len > 4096 {
        return None;
    }
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        v.push(take_u32(buf, cur)?);
    }
    Some(v)
}

fn take_str(buf: &[u8], cur: &mut usize) -> Option<String> {
    let len = take_u32(buf, cur)? as usize;
    let slice = buf.get(*cur..*cur + len)?;
    *cur += len;
    std::str::from_utf8(slice).ok().map(str::to_owned)
}
