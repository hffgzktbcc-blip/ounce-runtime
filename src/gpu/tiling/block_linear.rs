//! NVIDIA block-linear addressing across Maxwell (`GM20B`, GOB Sector Layout 0) and
//! Ampere/Orin (`T239` `GA10F`, GOB Sector Layout 1 + Turing+ `ZS` depth/stencil GOBs).
//!
//! Sources (public):
//! - Linux `include/uapi/drm/drm_fourcc.h` (`DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D`):
//!   GOBs are 64 B x 8 rows (`512` bytes), stacked vertically `2^height_log2` GOBs (`0..=5`)
//!   and `2^depth_log2` slices (`0..=5`).
//!   - Sector Layout `s = 0` (`SectorLayout::Layout0Tegrax1`): Tegra K1 through TX1/TX2
//!     (`offset = x[3:0] | y[0]<<4 | x[4]<<5 | y[2:1]<<6 | x[5]<<8`).
//!   - Sector Layout `s = 1` (`SectorLayout::Layout1OrinT239`): Desktop GPUs + Tegra Xavier,
//!     Orin, and Switch 2 T239 (`GA10F`), packing 16B x 8-row vertical columns per 128B L2
//!     sector (`offset = x[3:0] | y[2:0]<<4 | x[5:4]<<7`).
//! - Turing/Ampere `ZS` depth/stencil GOB kind (`GobKind::TuringZs`): interleaves 2x2 pixel
//!   quads inside the 512-byte GOB (`x[2:0] | y[0]<<3 | x[3]<<4 | y[1]<<5 | x[4]<<6 | y[2]<<7 | x[5]<<8`).

/// Bytes per GOB row.
pub const GOB_WIDTH_BYTES: u32 = 64;
/// Rows per GOB.
pub const GOB_HEIGHT: u32 = 8;
/// Bytes per GOB.
pub const GOB_BYTES: u32 = GOB_WIDTH_BYTES * GOB_HEIGHT;
/// Largest block height / depth exponent the hardware accepts (32 GOBs).
pub const MAX_BLOCK_LOG2: u8 = 5;

/// Intra-GOB sector layout selector (`s` field in `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SectorLayout {
    /// Tegra X1 (`GM20B` / Switch 1): `x[3:0] | y[0]<<4 | x[4]<<5 | y[2:1]<<6 | x[5]<<8`.
    #[default]
    Layout0Tegrax1 = 0,
    /// Tegra Orin / Switch 2 T239 (`GA10F`): `x[3:0] | y[2:0]<<4 | x[5:4]<<7`.
    Layout1OrinT239 = 1,
}

/// GOB internal element arrangement (`Color2d` vs Turing/Ampere `ZS` depth/stencil).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GobKind {
    #[default]
    Color2d,
    TuringZs,
}

/// Byte offset of byte column `x` (`0..64`) and row `y` (`0..8`) inside one Maxwell Layout-0 GOB.
#[inline]
pub const fn gob_offset(x: u32, y: u32) -> u32 {
    ((x % 64) / 32) * 256 + ((y % 8) / 2) * 64 + ((x % 32) / 16) * 32 + (y % 2) * 16 + (x % 16)
}

/// Byte offset inside one Orin / Switch 2 T239 (`GA10F`) **Sector Layout 1** GOB (`s = 1`).
/// Preserves 16-byte contiguous row atoms (`x[3:0]`) while grouping 8-row columns per 128B L2 sector.
#[inline]
pub const fn gob_offset_layout1(x: u32, y: u32) -> u32 {
    let xi = x & 63;
    let yi = y & 7;
    (xi & 0xF) | (yi << 4) | (((xi >> 4) & 0x3) << 7)
}

/// Byte offset inside one Turing/Ampere `ZS` depth/stencil GOB (`GobKind::TuringZs`).
#[inline]
pub const fn gob_offset_turing_zs(x: u32, y: u32) -> u32 {
    let xi = x & 63;
    let yi = y & 7;
    (xi & 0x7)
        | ((yi & 0x1) << 3)
        | (((xi >> 3) & 0x1) << 4)
        | (((yi >> 1) & 0x1) << 5)
        | (((xi >> 4) & 0x1) << 6)
        | (((yi >> 2) & 0x1) << 7)
        | (((xi >> 5) & 0x1) << 8)
}

/// Dispatch intra-GOB offset for `(sector_layout, gob_kind)`.
#[inline]
pub const fn gob_offset_for(x: u32, y: u32, sector: SectorLayout, kind: GobKind) -> u32 {
    match kind {
        GobKind::TuringZs => gob_offset_turing_zs(x, y),
        GobKind::Color2d => match sector {
            SectorLayout::Layout0Tegrax1 => gob_offset(x, y),
            SectorLayout::Layout1OrinT239 => gob_offset_layout1(x, y),
        },
    }
}

/// WGSL GPU compute shader that swizzles/unswizzles 32-bit words (`u32`) between tightly packed
/// pitch-linear buffers and NVIDIA Block-Linear buffers for **both** Maxwell Sector Layout 0
/// (`params.sector_layout == 0u`) and Switch 2 T239 Sector Layout 1 (`params.sector_layout == 1u`).
pub const BLOCK_LINEAR_COMPUTE_WGSL: &str = r#"
struct SwizzleParams {
    row_bytes: u32,
    rows: u32,
    gobs_wide: u32,
    height_log2: u32,
    sector_layout: u32, // 0 = Maxwell Layout 0, 1 = Orin/T239 Layout 1
    to_tiled: u32,      // 0 = detile (tiled -> linear), 1 = tile (linear -> tiled)
    pitch_stride: u32,
    _pad: u32,
};

@group(0) @binding(0) var<uniform> params: SwizzleParams;
@group(0) @binding(1) var<storage, read> src_words: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst_words: array<u32>;

fn gob_byte_offset(xi: u32, yi: u32, sector: u32) -> u32 {
    if (sector == 1u) {
        return (xi & 15u) | (yi << 4u) | (((xi >> 4u) & 3u) << 7u);
    }
    return (xi & 15u) | ((yi & 1u) << 4u) | (((xi >> 4u) & 1u) << 5u) | (((yi >> 1u) & 3u) << 6u) | (((xi >> 5u) & 1u) << 8u);
}

@compute @workgroup_size(16, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x_word = gid.x;
    let y = gid.y;
    let x = x_word * 4u;
    if (x >= params.row_bytes || y >= params.rows) {
        return;
    }

    let gob_x = x / 64u;
    let gob_row = y / 8u;
    let block_y = gob_row >> params.height_log2;
    let gob_y_in_block = gob_row & ((1u << params.height_log2) - 1u);
    let block_bytes = 512u << params.height_log2;
    let block_idx = block_y * params.gobs_wide + gob_x;
    let tiled_byte = block_idx * block_bytes + gob_y_in_block * 512u + gob_byte_offset(x & 63u, y & 7u, params.sector_layout);
    let linear_byte = y * params.pitch_stride + x;

    if (params.to_tiled == 0u) {
        dst_words[linear_byte >> 2u] = src_words[tiled_byte >> 2u];
    } else {
        dst_words[tiled_byte >> 2u] = src_words[linear_byte >> 2u];
    }
}
"#;

/// Shrinks block height/depth exponents so one block is not much larger than the surface,
/// the way NVK's `nil` clamps its tiling for small images. `rows` and `depth` are in
/// elements (rows of 4x4 blocks for BCn).
pub fn clamp_block_log2(rows: u32, depth: u32, mut height_log2: u8, mut depth_log2: u8) -> (u8, u8) {
    height_log2 = height_log2.min(MAX_BLOCK_LOG2);
    depth_log2 = depth_log2.min(MAX_BLOCK_LOG2);
    while height_log2 > 0 && rows <= GOB_HEIGHT << (height_log2 - 1) {
        height_log2 -= 1;
    }
    while depth_log2 > 0 && depth <= 1 << (depth_log2 - 1) {
        depth_log2 -= 1;
    }
    (height_log2, depth_log2)
}

/// Geometry of one block-linear surface level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLinear {
    /// Surface width in GOBs (row bytes rounded up to 64).
    pub gobs_wide: u32,
    /// Surface height in blocks.
    pub blocks_high: u32,
    /// Surface depth in blocks.
    pub blocks_deep: u32,
    pub height_log2: u8,
    pub depth_log2: u8,
    pub sector_layout: SectorLayout,
    pub gob_kind: GobKind,
}

impl BlockLinear {
    /// `row_bytes` is one row of elements in bytes; `rows` and `depth` are in elements.
    /// Defaults to `SectorLayout::Layout0Tegrax1` + `GobKind::Color2d` for backward compatibility.
    pub fn new(row_bytes: u32, rows: u32, depth: u32, height_log2: u8, depth_log2: u8) -> Self {
        Self::with_options(
            row_bytes,
            rows,
            depth,
            height_log2,
            depth_log2,
            SectorLayout::Layout0Tegrax1,
            GobKind::Color2d,
        )
    }

    /// Constructs a `BlockLinear` geometry with explicit `SectorLayout` (e.g. `Layout1OrinT239`)
    /// and `GobKind` (`Color2d` or `TuringZs`).
    pub fn with_options(
        row_bytes: u32,
        rows: u32,
        depth: u32,
        height_log2: u8,
        depth_log2: u8,
        sector_layout: SectorLayout,
        gob_kind: GobKind,
    ) -> Self {
        let block_rows = GOB_HEIGHT << height_log2;
        let block_depth = 1u32 << depth_log2;
        Self {
            gobs_wide: row_bytes.div_ceil(GOB_WIDTH_BYTES),
            blocks_high: rows.div_ceil(block_rows),
            blocks_deep: depth.div_ceil(block_depth),
            height_log2,
            depth_log2,
            sector_layout,
            gob_kind,
        }
    }

    /// Bytes per block.
    pub fn block_bytes(&self) -> usize {
        (GOB_BYTES as usize) << (self.height_log2 + self.depth_log2)
    }

    /// Row pitch of the padded surface, as a pitch-style stride (`gobs_wide * 64`).
    pub fn stride(&self) -> u32 {
        self.gobs_wide * GOB_WIDTH_BYTES
    }

    /// Total bytes, including padding to whole blocks.
    pub fn size(&self) -> usize {
        self.gobs_wide as usize * self.blocks_high as usize * self.blocks_deep as usize * self.block_bytes()
    }

    /// Byte offset of byte column `x`, row `y`, slice `z`.
    #[inline]
    pub fn offset(&self, x: u32, y: u32, z: u32) -> usize {
        let gob_x = (x / GOB_WIDTH_BYTES) as usize;
        let gob_row = y / GOB_HEIGHT;
        let block_y = (gob_row >> self.height_log2) as usize;
        let gob_y_in_block = (gob_row & ((1 << self.height_log2) - 1)) as usize;
        let block_z = (z >> self.depth_log2) as usize;
        let gob_z_in_block = (z & ((1 << self.depth_log2) - 1)) as usize;

        let block_index =
            (block_z * self.blocks_high as usize + block_y) * self.gobs_wide as usize + gob_x;
        let gob_in_block = (gob_z_in_block << self.height_log2) + gob_y_in_block;
        block_index * self.block_bytes()
            + gob_in_block * GOB_BYTES as usize
            + gob_offset_for(x % GOB_WIDTH_BYTES, y % GOB_HEIGHT, self.sector_layout, self.gob_kind) as usize
    }
}
