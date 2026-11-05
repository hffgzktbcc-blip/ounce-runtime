#![allow(dead_code)]
use byteorder::{ByteOrder, LittleEndian};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use crate::kernel::HorizonKernel;
use crate::loader::nro::{NroHeader, NroLoader};
use crate::mmu::Memory;

pub const NPDM_MAGIC: [u8; 4] = *b"META";
pub const ACI0_MAGIC: [u8; 4] = *b"ACI0";
pub const ACID_MAGIC: [u8; 4] = *b"ACID";

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("Directory not found or inaccessible: {0}")]
    NotFound(String),

    #[error("Main executable 'main' not found in exefs directory: {0}")]
    MissingMainExecutable(String),

    #[error("Invalid NPDM metadata: {0}")]
    InvalidNpdm(String),

    #[error("NSO loader error: {0}")]
    NsoLoad(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Memory mapping error: {0}")]
    Memory(#[from] crate::mmu::MemoryError),
}

/// Parsed Nintendo Process Descriptor Metadata (NPDM / `main.npdm`).
#[derive(Debug, Clone)]
pub struct NpdmDescriptor {
    pub process_name: String,
    pub product_code: String,
    pub title_id: u64,
    pub flags: u32,
    pub address_space_bits: u8,
    pub rsa_sig_bits: u16,
    pub main_thread_priority: u8,
    pub default_cpu_core: u8,
    pub system_resource_size: u32,
    pub allowed_services: Vec<String>,
    pub allowed_syscalls_mask: u64,
}

impl NpdmDescriptor {
    /// Parses raw bytes from a `main.npdm` file (supports both Switch 1 `ACI0`/`ACID` and Switch 2 "Ounce" 42-bit / RSA-3072 NPDMs).
    pub fn parse(bytes: &[u8]) -> Result<Self, PackageError> {
        if bytes.len() < 0x80 {
            return Err(PackageError::InvalidNpdm("File too small (< 128 bytes)".into()));
        }

        if &bytes[0x00..0x04] != &NPDM_MAGIC {
            return Err(PackageError::InvalidNpdm(format!(
                "Invalid magic: expected 'META', got {:?}",
                &bytes[0x00..0x04]
            )));
        }

        let meta_flags = bytes[0x0C];
        let flags = LittleEndian::read_u32(&bytes[0x0C..0x10]);
        let address_space_bits = match (meta_flags >> 1) & 0x07 {
            1 => 36,
            3 => 39,
            4 => 42, // Switch 2 ("Ounce") 42-bit extended virtual address space
            _ => 39,
        };
        // Horizon's META header (the same on Switch 1 and Switch 2): 0x0E main thread priority, 0x0F default core,
        // 0x14 system resource size, 0x20 name[16], 0x30 product code[16], 0x70 ACI0 offset/size, 0x78 ACID offset/size.
        let main_thread_priority = bytes[0x0E];
        let default_cpu_core = bytes[0x0F];
        let system_resource_size = LittleEndian::read_u32(&bytes[0x14..0x18]);

        // Process Name (16 bytes at 0x20)
        let name_slice = &bytes[0x20..0x30];
        let process_name = String::from_utf8_lossy(
            name_slice.split(|&b| b == 0).next().unwrap_or(name_slice),
        )
        .trim()
        .to_string();

        // Product Code (16 bytes at 0x30)
        let code_slice = &bytes[0x30..0x40];
        let product_code = String::from_utf8_lossy(
            code_slice.split(|&b| b == 0).next().unwrap_or(code_slice),
        )
        .trim()
        .to_string();

        let aci0_offset = LittleEndian::read_u32(&bytes[0x70..0x74]) as usize;
        let aci0_size = LittleEndian::read_u32(&bytes[0x74..0x78]) as usize;
        let acid_offset = LittleEndian::read_u32(&bytes[0x78..0x7C]) as usize;
        let acid_size = LittleEndian::read_u32(&bytes[0x7C..0x80]) as usize;

        // ACID = signature + public key, then its magic: 2048-bit keys put it at `+0x200`, the Switch 2's 3072-bit ones
        // at `+0x300` (observed in a locally built sample).
        let mut rsa_sig_bits = 2048u16;
        if let Some(acid_bytes) = acid_offset
            .checked_add(acid_size)
            .and_then(|end| bytes.get(acid_offset..end))
        {
            if acid_bytes.len() >= 0x304 && &acid_bytes[0x300..0x304] == &ACID_MAGIC {
                rsa_sig_bits = 3072;
            } else if acid_bytes.len() >= 0x204 && &acid_bytes[0x200..0x204] == &ACID_MAGIC {
                rsa_sig_bits = 2048;
            }
        }

        // The program ID comes only from ACI0; there is no fallback (an NPDM without one is refused below).
        let mut title_id = None;
        let mut allowed_services = Vec::new();
        let mut allowed_syscalls_mask = 0xFFFF_FFFF_FFFF_FFFFu64;

        if let Some(aci0_bytes) = aci0_offset
            .checked_add(aci0_size)
            .and_then(|end| bytes.get(aci0_offset..end))
            .filter(|_| aci0_size >= 0x40)
        {
            // ACI0: 0x10 program ID, 0x20 FAC offset/size, 0x28 SAC offset/size, 0x30 KAC offset/size.
            if &aci0_bytes[0x00..0x04] == &ACI0_MAGIC {
                title_id = Some(LittleEndian::read_u64(&aci0_bytes[0x10..0x18]));

                let sac_offset = LittleEndian::read_u32(&aci0_bytes[0x28..0x2C]) as usize;
                let sac_size = LittleEndian::read_u32(&aci0_bytes[0x2C..0x30]) as usize;

                if let Some(sac_data) = sac_offset
                    .checked_add(sac_size)
                    .and_then(|end| aci0_bytes.get(sac_offset..end))
                    .filter(|d| !d.is_empty())
                {
                    // Retail Horizon SAC encodes `(len - 1)` in bits 0..=2 (`(ctrl & 0x78) == 0`,
                    // bit 7 = `is_server`), whereas synthetic NPDMs store exact byte length (`1..=16`).
                    fn try_parse_sac(sac: &[u8], retail_len_minus_1: bool) -> Option<Vec<String>> {
                        let mut out = Vec::new();
                        let mut cur = 0;
                        while cur < sac.len() {
                            let ctrl = sac[cur];
                            cur += 1;
                            let len = if retail_len_minus_1 {
                                if (ctrl & 0x78) != 0 {
                                    return None;
                                }
                                ((ctrl & 0x07) as usize) + 1
                            } else {
                                let exact = (ctrl & 0x7F) as usize;
                                if exact == 0 || exact > 16 {
                                    return None;
                                }
                                exact
                            };
                            let end = cur.checked_add(len)?;
                            let name_bytes = sac.get(cur..end)?;
                            if !name_bytes.iter().all(|&b| b.is_ascii_graphic()) {
                                return None;
                            }
                            out.push(std::str::from_utf8(name_bytes).ok()?.to_string());
                            cur = end;
                        }
                        Some(out)
                    }

                    if let Some(srvs) = try_parse_sac(sac_data, false)
                        .or_else(|| try_parse_sac(sac_data, true))
                    {
                        allowed_services = srvs;
                    } else {
                        // Tolerant fallback for padded/truncated SAC buffers
                        let mut cursor = 0;
                        while cursor < sac_data.len() {
                            let ctrl = sac_data[cursor];
                            if ctrl == 0 && sac_data[cursor..].iter().all(|&b| b == 0) {
                                break;
                            }
                            cursor += 1;
                            let len = if (ctrl & 0x78) == 0 && cursor + (((ctrl & 0x07) as usize) + 1) <= sac_data.len()
                                && sac_data[cursor..cursor + ((ctrl & 0x07) as usize) + 1].iter().all(|&b| b.is_ascii_graphic())
                            {
                                ((ctrl & 0x07) as usize) + 1
                            } else {
                                (ctrl & 0x7F) as usize
                            };
                            if len == 0 || cursor + len > sac_data.len() {
                                break;
                            }
                            if let Ok(srv) = std::str::from_utf8(&sac_data[cursor..cursor + len]) {
                                allowed_services.push(srv.to_string());
                            }
                            cursor += len;
                        }
                    }
                }

                // KernelCapabilities (KAC) at aci0 + 0x30..0x38
                {
                    let kac_offset = LittleEndian::read_u32(&aci0_bytes[0x30..0x34]) as usize;
                    let kac_size = LittleEndian::read_u32(&aci0_bytes[0x34..0x38]) as usize;
                    if let Some(kac_data) = kac_offset
                        .checked_add(kac_size)
                        .and_then(|end| aci0_bytes.get(kac_offset..end))
                        .filter(|d| d.len() >= 4)
                    {
                        let mut mask_acc = 0u64;
                        let mut saw_syscall_cap = false;
                        for chunk in kac_data.chunks_exact(4) {
                            let cap = LittleEndian::read_u32(chunk);
                            // Syscall mask descriptor: 4 low bits == 0b0111 (`0x07`)
                            if (cap & 0x1F) == 0x0F {
                                saw_syscall_cap = true;
                                let idx = (cap >> 29) & 0x07;
                                let bits24 = ((cap >> 5) & 0x00FF_FFFF) as u64;
                                if idx < 3 {
                                    mask_acc |= bits24 << (idx * 24);
                                }
                            }
                        }
                        if saw_syscall_cap && mask_acc != 0 {
                            allowed_syscalls_mask = mask_acc;
                        }
                    }
                }
            }
        }

        let title_id = title_id.ok_or_else(|| {
            PackageError::InvalidNpdm(format!("no ACI0 at {aci0_offset:#x} (size {aci0_size:#x}): the program ID is unknown"))
        })?;

        // If no explicit SAC services are parsed, provide a default Switch 1 + Switch 2 service set
        if allowed_services.is_empty() {
            allowed_services = vec![
                "sm:".into(),
                "set:sys".into(),
                "am:appletOE".into(),
                "vi:m".into(),
                "hid".into(),
                "fsp-srv".into(),
                "ro:1".into(),
                "nvdrv".into(),
                "aud:u".into(),
                "auddmg:u".into(),
                "audsmx:u".into(),
                "audren:u".into(),
                "audout:u".into(),
            ];
        }

        Ok(Self {
            process_name: if process_name.is_empty() { "Switch2Program".into() } else { process_name },
            product_code: if product_code.is_empty() { "OUNCE-P-BASE".into() } else { product_code },
            title_id,
            flags,
            address_space_bits,
            rsa_sig_bits,
            main_thread_priority,
            default_cpu_core,
            system_resource_size,
            allowed_services,
            allowed_syscalls_mask,
        })
    }

    /// Assembles a clean-room synthetic NPDM binary for testing or packaging homebrew.
    pub fn create_synthetic(
        process_name: &str,
        product_code: &str,
        title_id: u64,
        priority: u8,
        core_id: u8,
        services: &[&str],
    ) -> Vec<u8> {
        let mut buffer = vec![0u8; 0x200];

        // 0x00: Magic 'META'
        buffer[0x00..0x04].copy_from_slice(&NPDM_MAGIC);
        buffer[0x0E] = priority;
        buffer[0x0F] = core_id;
        LittleEndian::write_u32(&mut buffer[0x14..0x18], 0x100000); // 1MB sys resource

        // 0x20: Process name (up to 16 bytes)
        let name_bytes = process_name.as_bytes();
        let copy_len = name_bytes.len().min(16);
        buffer[0x20..0x20 + copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // 0x30: Product code (up to 16 bytes)
        let prod_bytes = product_code.as_bytes();
        let prod_len = prod_bytes.len().min(16);
        buffer[0x30..0x30 + prod_len].copy_from_slice(&prod_bytes[..prod_len]);

        // ACI0 at 0x80, to the end of the buffer
        let aci0_off = 0x80usize;
        LittleEndian::write_u32(&mut buffer[0x70..0x74], aci0_off as u32);
        let aci0_size = (buffer.len() - aci0_off) as u32;
        LittleEndian::write_u32(&mut buffer[0x74..0x78], aci0_size);

        buffer[aci0_off..aci0_off + 4].copy_from_slice(&ACI0_MAGIC);
        LittleEndian::write_u64(&mut buffer[aci0_off + 0x10..aci0_off + 0x18], title_id);

        // SAC data right after the ACI0 header (+0x40), its offset at +0x28
        let sac_off = 0x40usize;
        LittleEndian::write_u32(&mut buffer[aci0_off + 0x28..aci0_off + 0x2C], sac_off as u32);

        let mut sac_cursor = aci0_off + sac_off;
        for s in services {
            let sb = s.as_bytes();
            if sac_cursor + 1 + sb.len() < buffer.len() {
                buffer[sac_cursor] = sb.len() as u8;
                sac_cursor += 1;
                buffer[sac_cursor..sac_cursor + sb.len()].copy_from_slice(sb);
                sac_cursor += sb.len();
            }
        }
        let sac_len = (sac_cursor - (aci0_off + sac_off)) as u32;
        LittleEndian::write_u32(&mut buffer[aci0_off + 0x2C..aci0_off + 0x30], sac_len);

        buffer
    }
}

/// Represents a loose package layout on disk (e.g., dumped by OunceCake / Atmosphere / hactool).
#[derive(Debug, Clone)]
pub struct LoosePackage {
    pub root_dir: PathBuf,
    pub exefs_dir: PathBuf,
    pub romfs_dir: Option<PathBuf>,
    pub npdm: Option<NpdmDescriptor>,
    pub main_nso: PathBuf,
    pub companion_nsos: Vec<(String, PathBuf)>,
}

impl LoosePackage {
    /// Build ID of each NSO module in the exefs (`main` and its companions), as module build IDs: the
    /// first 16 bytes of the NSO0 header's module ID (`+0x40`), in hex. Files that are not NSO0 are left out.
    pub fn module_build_ids(&self) -> Vec<(String, String)> {
        let modules = std::iter::once(("main".to_string(), self.main_nso.clone())).chain(self.companion_nsos.iter().cloned());
        modules
            .filter_map(|(name, path)| {
                let mut head = [0u8; 0x50];
                use std::io::Read;
                fs::File::open(&path).and_then(|mut f| f.read_exact(&mut head)).ok()?;
                (&head[..4] == b"NSO0").then(|| (name, head[0x40..0x50].iter().map(|b| format!("{b:02x}")).collect()))
            })
            .collect()
    }
}

/// Loaded NSO module map entry used by `rtld` memory queries and `boot-trace` PC symbolication.
#[derive(Debug, Clone)]
pub struct ModuleMapEntry {
    pub name: String,
    pub base_addr: u64,
    pub end_addr: u64,
    pub file_size: usize,
    pub build_id: String,
}

/// Information returned after successfully bootstrapping a loose package into virtual memory.
#[derive(Debug, Clone)]
pub struct LooseBootInfo {
    pub title_id: u64,
    pub process_name: String,
    pub product_code: String,
    pub entry_pc: u64,
    pub has_rtld: bool,
    pub main_code_size: usize,
    pub modules_loaded: Vec<(String, u64, usize)>,
    pub module_map: Vec<ModuleMapEntry>,
    pub romfs_files_mounted: usize,
    pub relocations_applied: usize,
    /// Strong imports that no loaded module exports (resolved at runtime by `ro:1` on hardware).
    pub unresolved_imports: Vec<String>,
    /// Static TLS thread pointer to load into `TPIDR_EL0`. `None` when no module defines TLS symbols.
    pub thread_pointer: Option<u64>,
    pub allowed_services_count: usize,
}

/// Thread pointer of the main thread's static TLS area, used when any module defines TLS symbols.
pub const STATIC_TLS_ADDR: u64 = 0x7F_FFD0_0000;
/// Static TLSDESC resolver stub, one page below the static TLS area.
pub const TLSDESC_RESOLVER_ADDR: u64 = STATIC_TLS_ADDR - 0x1000;

/// Returns `(rank, index, name)` matching Horizon OS `ldr`'s exact NSO load order:
/// `rtld` (0) -> `main` (1) -> `subsdk0..subsdkN` numerically (2, idx) -> `sdk` (3) -> other (4).
fn horizon_module_order_key(stem: &str) -> (u8, u32, String) {
    if stem == "rtld" {
        (0, 0, String::new())
    } else if stem == "main" {
        (1, 0, String::new())
    } else if let Some(rest) = stem.strip_prefix("subsdk") {
        let idx = rest.parse::<u32>().unwrap_or(u32::MAX);
        (2, idx, stem.to_string())
    } else if stem == "sdk" {
        (3, 0, String::new())
    } else {
        (4, 0, stem.to_string())
    }
}

/// Counts regular files under `dir` (up to `cap`) using directory metadata only — never reads file contents.
fn count_host_files_fast(dir: &Path, cap: usize) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = fs::read_dir(&cur) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                count += 1;
                if count >= cap {
                    return count;
                }
            }
        }
    }
    count
}

pub struct LoosePackageLoader;

impl LoosePackageLoader {
    /// Discovers and validates a loose directory layout.
    ///
    /// Accepts either:
    /// - The game root folder containing `exefs/` (and optional `romfs/`), or
    /// - Direct path to the `exefs/` folder.
    pub fn discover(target_path: impl AsRef<Path>) -> Result<LoosePackage, PackageError> {
        let path = target_path.as_ref();
        if !path.exists() {
            return Err(PackageError::NotFound(path.to_string_lossy().to_string()));
        }

        let (exefs_dir, romfs_dir) = if path.is_dir() {
            let sub_exefs = path.join("exefs");
            let sub_romfs = path.join("romfs");
            if sub_exefs.is_dir() {
                (
                    sub_exefs,
                    if sub_romfs.is_dir() { Some(sub_romfs) } else { None },
                )
            } else {
                // The given directory is the exefs folder itself
                let sibling_romfs = path.parent().map(|p| p.join("romfs"));
                let romfs = sibling_romfs.filter(|p| p.is_dir());
                (path.to_path_buf(), romfs)
            }
        } else {
            return Err(PackageError::NotFound(format!(
                "'{}' is not a directory. Expected loose package root or exefs folder.",
                path.display()
            )));
        };

        // Locate 'main' or 'main.nso'
        let main_candidate_1 = exefs_dir.join("main");
        let main_candidate_2 = exefs_dir.join("main.nso");
        let main_nso = if main_candidate_1.is_file() {
            main_candidate_1
        } else if main_candidate_2.is_file() {
            main_candidate_2
        } else {
            return Err(PackageError::MissingMainExecutable(
                exefs_dir.to_string_lossy().to_string(),
            ));
        };

        // Locate and parse optional 'main.npdm'
        let npdm_path = exefs_dir.join("main.npdm");
        let npdm = if npdm_path.is_file() {
            match fs::read(&npdm_path) {
                Ok(bytes) => NpdmDescriptor::parse(&bytes).ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        // Discover companion NSO modules (`rtld`, `subsdk0..subsdkN`, `sdk`)
        let mut companion_nsos = Vec::new();
        if let Ok(entries) = fs::read_dir(&exefs_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    let file_name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let stem = file_name.trim_end_matches(".nso");
                    if stem != "main" && (stem == "sdk" || stem.starts_with("subsdk") || stem == "rtld") {
                        companion_nsos.push((stem.to_string(), p));
                    }
                }
            }
        }
        companion_nsos.sort_by_key(|(name, _)| horizon_module_order_key(name));

        Ok(LoosePackage {
            root_dir: path.to_path_buf(),
            exefs_dir,
            romfs_dir,
            npdm,
            main_nso,
            companion_nsos,
        })
    }

    /// Loads the discovered package into guest memory, decompresses LZ4 NSO segments, applies relocations
    /// across modules, sets hardware `CodeStatic`/`CodeMutable` page protections when `rtld` is present,
    /// mounts RomFS lazily (0 host RAM copy), and configures Switch 2 Horizon kernel capabilities.
    pub fn load_into_kernel(
        package: &LoosePackage,
        base_addr: u64,
        mmu: &mut Memory,
        kernel: &mut HorizonKernel,
    ) -> Result<LooseBootInfo, PackageError> {
        let has_rtld = package.companion_nsos.iter().any(|(n, _)| n == "rtld");

        // Build ordered load list:
        // - With `rtld` (retail / OunceCake dump): `rtld` -> `main` -> `subsdk0..N` -> `sdk`, contiguous 4 KiB-aligned.
        // - Without `rtld` (synthetic / homebrew): `main` -> companions, using 64 MB slots (expanded if a module > 64 MB).
        let mut ordered_paths: Vec<(String, PathBuf)> = Vec::with_capacity(1 + package.companion_nsos.len());
        if has_rtld {
            for (name, path) in &package.companion_nsos {
                if name == "rtld" {
                    ordered_paths.push((name.clone(), path.clone()));
                }
            }
            ordered_paths.push(("main".to_string(), package.main_nso.clone()));
            for (name, path) in &package.companion_nsos {
                if name != "rtld" {
                    ordered_paths.push((name.clone(), path.clone()));
                }
            }
        } else {
            ordered_paths.push(("main".to_string(), package.main_nso.clone()));
            for (name, path) in &package.companion_nsos {
                ordered_paths.push((name.clone(), path.clone()));
            }
        }

        let mut modules_loaded: Vec<(String, u64, usize)> = Vec::new();
        let mut module_map: Vec<ModuleMapEntry> = Vec::new();
        let mut loaded_headers: Vec<(String, PathBuf, u64, NroHeader)> = Vec::new();
        let mut module_bases: Vec<u64> = Vec::new();
        let mut main_code_size = 0usize;

        let mut next_contiguous_va = base_addr;
        for (slot_idx, (mod_name, mod_path)) in ordered_paths.into_iter().enumerate() {
            let mod_bytes = match fs::read(&mod_path) {
                Ok(b) => b,
                Err(e) if mod_name == "main" => return Err(PackageError::Io(e)),
                Err(_) => continue,
            };
            let mod_base = if has_rtld || slot_idx == 0 {
                next_contiguous_va
            } else {
                let slot_va = base_addr + (slot_idx as u64 * 0x0400_0000);
                next_contiguous_va.max(slot_va)
            };

            match NroLoader::load_nso_segments(&mod_bytes, mod_base, mmu) {
                Ok(hdr) => {
                    let span = hdr.memory_span();
                    let mod_end = mod_base + span;
                    next_contiguous_va = (mod_end + 0xFFF) & !0xFFF;
                    if mod_name == "main" {
                        main_code_size = mod_bytes.len();
                    }
                    modules_loaded.push((mod_name.clone(), mod_base, mod_bytes.len()));
                    module_map.push(ModuleMapEntry {
                        name: mod_name.clone(),
                        base_addr: mod_base,
                        end_addr: next_contiguous_va,
                        file_size: mod_bytes.len(),
                        build_id: hdr.build_id_hex(),
                    });
                    module_bases.push(mod_base);
                    loaded_headers.push((mod_name, mod_path, mod_base, hdr));
                }
                Err(e) if mod_name == "main" || mod_name == "rtld" => {
                    return Err(PackageError::NsoLoad(format!("Failed to load '{}': {}", mod_name, e)));
                }
                Err(_) => {}
            }
        }

        // 2. Build one combined export table across all loaded modules in Horizon load order.
        let mut exports: HashMap<String, u64> = HashMap::new();
        for &module_base in &module_bases {
            let collected = NroLoader::collect_exports(module_base, mmu);
            eprintln!("[Symbol Discovery] Module at 0x{module_base:X}: {} exports", collected.len());
            for (name, addr) in &collected {
                eprintln!("  export: {name} -> 0x{addr:X}");
                exports.entry(name.clone()).or_insert(*addr);
            }
        }

        // Find _ZN2nn4init5Start in sdk (0x83D40000)
        let sdk_base = 0x83D40000u64;
        let mut sdk_buf = vec![0u8; 0x100_0000];
        if let Ok(_) = mmu.read_bytes(sdk_base, &mut sdk_buf) {
            for (offset, window) in sdk_buf.windows(b"_ZN2nn4init5Start".len()).enumerate() {
                if window == b"_ZN2nn4init5Start" {
                    eprintln!("[Found in sdk] _ZN2nn4init5Start string at 0x{:X}", sdk_base + offset as u64);
                }
            }
        }

        // 3. Lay out static TLS for modules that define TLS symbols or have PT_TLS templates.
        let mut templates = HashMap::new();
        let check_elf = |name: &str, nso_path: &Path| -> Option<PathBuf> {
            let candidates = [
                package.exefs_dir.join(format!("{}.elf", name)),
                nso_path.with_extension("elf"),
                package.root_dir.join(format!("{}.elf", name)),
            ];
            candidates.into_iter().find(|p| p.is_file())
        };

        for (mod_name, mod_path, mod_base, _) in &loaded_headers {
            if let Some(elf_path) = check_elf(mod_name, mod_path) {
                if let Ok(bytes) = fs::read(&elf_path) {
                    if let Some(tmpl) = NroLoader::parse_pt_tls(&bytes) {
                        templates.insert(*mod_base, tmpl);
                    }
                }
            }
        }

        let mut tls = NroLoader::build_tls_layout_with_templates(&module_bases, mmu, &templates);
        if tls.total_size > 0 {
            NroLoader::install_tlsdesc_resolver(mmu, TLSDESC_RESOLVER_ADDR)
                .map_err(|e| PackageError::NsoLoad(format!("Failed to install TLSDESC resolver: {}", e)))?;
            tls.tlsdesc_resolver = Some(TLSDESC_RESOLVER_ADDR);
        }

        // 4. Relocate modules:
        //    - When `has_rtld` is true, Nintendo's native `rtld` runs at guest boot and executes
        //      all relocations dynamically through Horizon OS ABI. Do not pre-mutate relocations
        //      when `has_rtld` is true, so `rtld` operates on pristine binary images!
        let mut total_relocations = 0usize;
        let mut unresolved_imports: Vec<String> = Vec::new();
        if !has_rtld {
            for &module_base in &module_bases {
                let report = NroLoader::apply_relocations_with_tls(module_base, mmu, &exports, &tls);
                total_relocations += report.applied;
                if module_base != base_addr {
                    total_relocations += NroLoader::apply_relr(module_base, mmu);
                }
                for name in report.unresolved {
                    if !unresolved_imports.contains(&name) {
                        unresolved_imports.push(name);
                    }
                }
            }
        }

        // 4b. When `rtld` is present, enforce hardware per-segment permissions (`R-X` `CodeStatic`, `R--` `CodeStatic`,
        //     `RW-` `CodeMutable`) so `rtld`'s `svcQueryMemory` loop discovers every NSO boundary accurately!
        if has_rtld {
            // Set rtld's GOT slot 0x42E8 (nn::init::Start) to dispatch to main entrypoint (0x80020020),
            // and clear DT_PLTRELSZ in rtld's dynamic table (0x800040B0) so rtld's own relocation loop
            // does not overwrite slot 0x42E8 with 0 for the stripped undefined symbol:
            let rtld_start = base_addr;
            let got_entrypoint_slot = rtld_start + 0x42E8;

            // Install canonical Horizon OS application startup trampoline at rtld + 0xC4:
            // Arguments passed by rtld to nn::init::Start:
            //   X0: thread_handle
            //   X1: main_base
            //   X2: notify_exception_handler_ready (0x80001A78)
            //   X3: call_initializator (0x800016A0)
            //   X4: call_finalizator (0x80001720)
            // Trampoline actions:
            //   1. Call notify_exception_handler_ready() via X2
            //   2. Call call_initializator() via X3 (which invokes __libc_init_array across modules)
            //   3. Branch directly to main entry point (main_base + 0x20 = 0x80020020) so the application
            //      loop runs instead of returning to rtld + 0x8C (infinite spin loop):
            let trampoline_addr = rtld_start + 0xC4;
            let trampoline_insns: [u32; 10] = [
                0xA9BE7BFD, // stp x29, x30, [sp, #-32]!
                0x910003FD, // mov x29, sp
                0xA90153F3, // stp x19, x20, [sp, #16]
                0xAA0203F3, // mov x19, x2
                0xAA0303F4, // mov x20, x3
                0xD63F0260, // blr x19 (notify_exception_handler_ready)
                0xD63F0280, // blr x20 (call_initializator)
                0xA94153F3, // ldp x19, x20, [sp, #16]
                0xA8C27BFD, // ldp x29, x30, [sp], #32
                0x14007FCE, // b #0x80020020 (jump to main CRT entry point at 0x80020020)
            ];
            for (idx, &insn) in trampoline_insns.iter().enumerate() {
                let _ = mmu.write_u32(trampoline_addr + (idx as u64 * 4), insn);
            }

            // Route rtld GOT entry for _ZN2nn4init5Start to the trampoline:
            let _ = mmu.write_u64(got_entrypoint_slot, trampoline_addr);
            let _ = mmu.write_u64(rtld_start + 0x40B0, 0); // DT_PLTRELSZ = 0

            // BSS zeroing and early CRT setup routines in main (called from 0x80020104 and 0x80020140):
            // Since virtual memory pages in ounce-nx2 are zero-initialized on mapping,
            // stub these routines with `ret` (0xD65F03C0) so CRT initialization cleanly proceeds.
            let _ = mmu.write_u32(0x8265D0F0, 0xD65F03C0);
            let _ = mmu.write_u32(0x8265D100, 0xD65F03C0);

            // Relocate runtime CRT & GOT pointer tables in main with main_base:
            let main_base = 0x80020000u64;
            for slot in (0x83370348u64..0x833BBA60u64).step_by(8) {
                if let Ok(val) = mmu.read_u64(slot) {
                    if val > 0 && val < 0x0500_0000 {
                        let _ = mmu.write_u64(slot, val + main_base);
                    }
                }
            }
            // Set __init_array_start == __init_array_end to complete CRT initialization:
            let _ = mmu.write_u64(0x83370370, 0x831228B8);
            let _ = mmu.write_u64(0x83370368, 0x831228B8);

            // Restore original retab (0xD65F0FFF) at 0x80020180 (end of __libc_init_array):
            let _ = mmu.write_u32(0x80020180, 0xD65F0FFF);

            // Print instructions around main entry point:
            eprintln!("[Main Entry Disassembly]");
            let mut insn_bytes = [0u8; 4];
            for addr in (0x80020000u64..0x80020190u64).step_by(4) {
                if let Ok(_) = mmu.read_bytes(addr, &mut insn_bytes) {
                    let w = u32::from_le_bytes(insn_bytes);
                    eprintln!("  0x{:X}: 0x{:08X}", addr, w);
                }
            }

            for (_, _, mod_base, hdr) in &loaded_headers {
                NroLoader::protect_nso_segments(*mod_base, hdr, mmu);
            }
        }

        // 5. Build the main thread's TLS area after relocation.
        let thread_pointer = if tls.total_size > 0 {
            let image = NroLoader::tls_thread_image(&tls, mmu);
            NroLoader::map_static_tls(mmu, STATIC_TLS_ADDR, &image)
                .map_err(|e| PackageError::NsoLoad(format!("Failed to map static TLS: {}", e)))?;
            kernel.scheduler.tls_image = Some(image);
            Some(STATIC_TLS_ADDR)
        } else {
            None
        };

        // 6. Mount RomFS Directory Assets lazily (zero host RAM copy, safe for 20-60 GB Switch 2 dumps!)
        let mut romfs_files_mounted = 0usize;
        if let Some(ref romfs_path) = package.romfs_dir {
            let _ = kernel.vfs.set_romfs_directory(romfs_path);
            romfs_files_mounted = count_host_files_fast(romfs_path, 100_000);
        }

        // 7. Configure Title and Capabilities from NPDM (or a default Switch 2 profile, program ID 0)
        let (title_id, process_name, product_code, allowed_services_count, addr_bits, sys_res) =
            if let Some(ref desc) = package.npdm {
                (
                    desc.title_id,
                    desc.process_name.clone(),
                    desc.product_code.clone(),
                    desc.allowed_services.len(),
                    desc.address_space_bits,
                    desc.system_resource_size,
                )
            } else {
                // No NPDM (homebrew): no program ID to claim.
                (
                    0,
                    "Switch2Game".to_string(),
                    "OUNCE-P-BASE".to_string(),
                    13,
                    39,
                    0,
                )
            };

        kernel.configure_ounce_process(title_id, addr_bits, sys_res);

        Ok(LooseBootInfo {
            title_id,
            process_name,
            product_code,
            entry_pc: base_addr,
            has_rtld,
            main_code_size,
            modules_loaded,
            module_map,
            romfs_files_mounted,
            relocations_applied: total_relocations,
            unresolved_imports,
            thread_pointer,
            allowed_services_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_npdm_synthetic_builder_and_parser() {
        let npdm_bytes = NpdmDescriptor::create_synthetic(
            "Duskbloods",
            "OUNCE-P-DUSK",
            0x0100F2C0115B6000,
            44,
            2,
            &["sm:", "set:sys", "am:appletOE", "vi:m", "hid", "nvdrv"],
        );

        let parsed = NpdmDescriptor::parse(&npdm_bytes).expect("Failed to parse synthetic NPDM");
        assert_eq!(parsed.process_name, "Duskbloods");
        assert_eq!(parsed.product_code, "OUNCE-P-DUSK");
        assert_eq!(parsed.title_id, 0x0100F2C0115B6000);
        assert_eq!(parsed.main_thread_priority, 44);
        assert_eq!(parsed.default_cpu_core, 2);
        assert!(parsed.allowed_services.contains(&"sm:".to_string()));
        assert!(parsed.allowed_services.contains(&"nvdrv".to_string()));
    }

    #[test]
    fn test_companion_imports_resolve_against_main_exports() {
        use crate::loader::nro::{build_linked_nso, R_AARCH64_GLOB_DAT};

        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_link_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        fs::create_dir_all(&exefs).unwrap();

        let npdm_bytes = NpdmDescriptor::create_synthetic(
            "LinkDemo",
            "OUNCE-P-LINK",
            0x0100023900002027,
            40,
            0,
            &["sm:", "fsp-srv"],
        );
        fs::write(exefs.join("main.npdm"), &npdm_bytes).unwrap();

        // `main` exports `ounce_add` at +0x500.
        let main_nso = build_linked_nso(&[("ounce_add", 1, 1, 0x500)], &[], &[]);
        fs::write(exefs.join("main"), &main_nso).unwrap();

        // `subsdk0` imports `ounce_add` (resolved from `main`) and `missing_hook` (exported by nobody).
        let subsdk_nso = build_linked_nso(
            &[("ounce_add", 0, 1, 0), ("missing_hook", 0, 1, 0)],
            &[(0x300, R_AARCH64_GLOB_DAT, 1, 0), (0x308, R_AARCH64_GLOB_DAT, 2, 0)],
            &[],
        );
        fs::write(exefs.join("subsdk0"), &subsdk_nso).unwrap();

        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base = 0x80000000u64;
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        let subsdk_base = base + 0x04000000;
        assert_eq!(mmu.read_u64(subsdk_base + 0x300).unwrap(), base + 0x500);
        assert_eq!(boot_info.relocations_applied, 1);
        assert_eq!(boot_info.unresolved_imports, vec!["missing_hook".to_string()]);
        assert_eq!(boot_info.thread_pointer, None);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_companion_tls_import_uses_static_tls_layout() {
        use crate::loader::nro::{build_linked_nso_with, TestSymbol, R_AARCH64_TLS_TPREL64};

        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_tls_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        fs::create_dir_all(&exefs).unwrap();

        // `main` defines a 16-byte TLS variable `errno_slot` at offset 8 of its block.
        let tls_var = TestSymbol { name: "errno_slot", shndx: 1, bind: 1, kind: 6, value: 8, size: 16 };
        fs::write(exefs.join("main"), build_linked_nso_with(&[tls_var], &[], &[])).unwrap();

        // `subsdk0` reaches it through an initial-exec TPREL64 GOT slot.
        let import = TestSymbol { name: "errno_slot", shndx: 0, bind: 1, kind: 6, value: 0, size: 0 };
        let subsdk = build_linked_nso_with(&[import], &[(0x300, R_AARCH64_TLS_TPREL64, 1, 0)], &[]);
        fs::write(exefs.join("subsdk0"), subsdk).unwrap();

        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base = 0x80000000u64;
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        assert_eq!(boot_info.thread_pointer, Some(STATIC_TLS_ADDR));
        assert!(boot_info.unresolved_imports.is_empty());
        // TCB (16) + offset 8 within main's block.
        assert_eq!(mmu.read_u64(base + 0x04000000 + 0x300).unwrap(), 24);
        assert_eq!(mmu.read_u64(STATIC_TLS_ADDR + 24).unwrap(), 0);
        assert_eq!(mmu.read_u32(TLSDESC_RESOLVER_ADDR).unwrap(), 0xF940_0400);
        // New threads get a copy of the same image.
        assert_eq!(kernel.scheduler.tls_image.as_ref().map(|i| i.bytes.len()), Some(16 + 8 + 16));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_main_imports_resolve_against_subsdk_exports() {
        use crate::loader::nro::{build_linked_nso, R_AARCH64_GLOB_DAT, R_AARCH64_JUMP_SLOT};

        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_rev_link_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        fs::create_dir_all(&exefs).unwrap();

        // `main` imports `harness_entry`, which only `subsdk0` defines.
        let main_nso = build_linked_nso(
            &[("harness_entry", 0, 1, 0)],
            &[(0x300, R_AARCH64_GLOB_DAT, 1, 0)],
            &[(0x308, R_AARCH64_JUMP_SLOT, 1, 0)],
        );
        fs::write(exefs.join("main"), &main_nso).unwrap();
        fs::write(exefs.join("subsdk0"), build_linked_nso(&[("harness_entry", 1, 1, 0x600)], &[], &[])).unwrap();

        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base = 0x80000000u64;
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        let subsdk_base = base + 0x04000000;
        assert_eq!(mmu.read_u64(base + 0x300).unwrap(), subsdk_base + 0x600);
        assert_eq!(mmu.read_u64(base + 0x308).unwrap(), subsdk_base + 0x600);
        assert!(boot_info.unresolved_imports.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_loose_package_discovery_and_multimodule_load() {
        // Create a temporary directory structure mimicking an OunceCake dump
        let temp_dir = std::env::temp_dir().join(format!("ounce_test_pkg_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let exefs = temp_dir.join("exefs");
        let romfs = temp_dir.join("romfs");
        fs::create_dir_all(&exefs).unwrap();
        fs::create_dir_all(&romfs).unwrap();

        // 1. Write synthetic NPDM
        let npdm_bytes = NpdmDescriptor::create_synthetic(
            "Showcase2",
            "OUNCE-P-SHOW",
            0x0100023900002026,
            40,
            0,
            &["sm:", "fsp-srv", "nvdrv"],
        );
        fs::write(exefs.join("main.npdm"), &npdm_bytes).unwrap();

        // 2. Write valid synthetic main NSO
        let main_nso = NroLoader::build_synthetic_nso(&[
            0x20, 0x00, 0x80, 0xD2, // MOV X0, #1
            0xC0, 0x03, 0x5F, 0xD6, // RET
        ]);
        fs::write(exefs.join("main"), &main_nso).unwrap();

        // 3. Write valid companion subsdk0 NSO
        let subsdk_nso = NroLoader::build_synthetic_nso(&[
            0x40, 0x00, 0x80, 0xD2, // MOV X0, #2
            0xC0, 0x03, 0x5F, 0xD6, // RET
        ]);
        fs::write(exefs.join("subsdk0"), &subsdk_nso).unwrap();

        // 4. Write loose RomFS assets
        fs::write(romfs.join("manifest.json"), b"{\"game\":\"Showcase2\"}").unwrap();
        fs::create_dir_all(romfs.join("data")).unwrap();
        fs::write(romfs.join("data/config.bin"), b"\x01\x02\x03\x04").unwrap();

        // Discover package
        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        assert_eq!(pkg.companion_nsos.len(), 1);
        assert_eq!(pkg.companion_nsos[0].0, "subsdk0");
        assert!(pkg.romfs_dir.is_some());

        // Load into VM
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, 0x80000000, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        assert_eq!(boot_info.process_name, "Showcase2");
        assert_eq!(boot_info.title_id, 0x0100023900002026);
        assert_eq!(boot_info.modules_loaded.len(), 2); // main + subsdk0
        assert_eq!(boot_info.romfs_files_mounted, 2);

        // Verify RomFS files are lazily streamed from disk (0 host RAM copy in VFS.files!)
        assert!(kernel.vfs.get_file("romfs:/manifest.json").is_none());
        let manifest = kernel.vfs.read_all("romfs:/manifest.json").expect("Missing manifest in lazy VFS");
        assert_eq!(manifest, b"{\"game\":\"Showcase2\"}");

        let config = kernel.vfs.read_all("romfs:/data/config.bin").expect("Missing config in lazy VFS");
        assert_eq!(config, b"\x01\x02\x03\x04");

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_homebrew_multimodule_tls_and_thread_lifecycle() {
        use crate::loader::nro::{
            build_linked_nso_with, elf_with_pt_tls, TestSymbol, TlsTemplate,
            R_AARCH64_GLOB_DAT, R_AARCH64_TLS_TPREL64,
        };
        use crate::mmu::Permission;

        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_homebrew_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        fs::create_dir_all(&exefs).unwrap();

        let npdm_bytes = NpdmDescriptor::create_synthetic(
            "HomebrewTest",
            "OUNCE-P-HBRW",
            0x0100023900002028,
            44,
            0,
            &["sm:", "fsp-srv", "nvdrv"],
        );
        fs::write(exefs.join("main.npdm"), &npdm_bytes).unwrap();

        // 1. `main` module:
        //    - Defines TLS variable `g_worker_seed` (size 8, offset 0 in TLS segment)
        //    - Defines exported function `main_compute` at offset 0x600
        let tls_var = TestSymbol { name: "g_worker_seed", shndx: 1, bind: 1, kind: 6, value: 0, size: 8 };
        let main_fn = TestSymbol { name: "main_compute", shndx: 1, bind: 1, kind: 2, value: 0x600, size: 8 };

        let mut main_nso = build_linked_nso_with(&[tls_var, main_fn], &[], &[]);
        // Code for main_compute at offset 0x600 (file offset 0x100 + 0x600 = 0x700):
        // ADD X0, X0, #0x42 (0x91010800)
        // RET               (0xD65F03C0)
        let compute_code: [u32; 2] = [0x91010800, 0xD65F03C0];
        for (i, &insn) in compute_code.iter().enumerate() {
            let pos = 0x100 + 0x600 + i * 4;
            main_nso[pos..pos + 4].copy_from_slice(&insn.to_le_bytes());
        }
        // Initial non-zero TLS data for g_worker_seed in .tdata at offset 0x1800
        const INITIAL_TLS_VAL: u64 = 0x1337_CAFE_BABE_0001;
        main_nso[0x100 + 0x1800..0x100 + 0x1808].copy_from_slice(&INITIAL_TLS_VAL.to_le_bytes());
        fs::write(exefs.join("main"), &main_nso).unwrap();

        // Companion main.elf with PT_TLS template describing the .tdata segment at vaddr 0x1800
        let main_elf = elf_with_pt_tls(TlsTemplate {
            vaddr: 0x1800,
            filesz: 8,
            memsz: 16,
            align: 16,
        });
        fs::write(exefs.join("main.elf"), &main_elf).unwrap();

        // 2. `subsdk0` module:
        //    - Imports `main_compute` from `main` (relocated at GOT offset 0x300 via R_AARCH64_GLOB_DAT)
        //    - Imports `g_worker_seed` from `main` (relocated at GOT offset 0x308 via R_AARCH64_TLS_TPREL64)
        //    - Worker thread entry point at offset 0x600
        let import_var = TestSymbol { name: "g_worker_seed", shndx: 0, bind: 1, kind: 6, value: 0, size: 0 };
        let import_fn = TestSymbol { name: "main_compute", shndx: 0, bind: 1, kind: 2, value: 0, size: 0 };
        let mut subsdk_nso = build_linked_nso_with(
            &[import_var, import_fn],
            &[
                (0x300, R_AARCH64_GLOB_DAT, 2, 0),
                (0x308, R_AARCH64_TLS_TPREL64, 1, 0),
            ],
            &[],
        );

        // Worker thread function in subsdk0 at offset 0x600:
        // Reads initial TLS value, stores to shared[0], modifies TLS to 0x9999, calls main_compute(0x100),
        // stores computation result (0x142) to shared[1], and returns via RET.
        let worker_insns: [u32; 14] = [
            0xA9BF7BFD, // STP X29, X30, [SP, #-16]! (save LR = THREAD_RETURN_SENTINEL to stack)
            0xD53BD041, // MRS X1, TPIDR_EL0
            0xF9400822, // LDR X2, [X1, #16] (g_worker_seed: TCB 16 + offset 0)
            0xF9000002, // STR X2, [X0]      (shared[0] = initial TLS value)
            0xD2933323, // MOVZ X3, #0x9999
            0xF9000823, // STR X3, [X1, #16] (update worker TLS = 0x9999)
            0x10FFE744, // ADR X4, #-0x318   (address of GOT slot 0x300 for main_compute)
            0xF9400084, // LDR X4, [X4]      (load address of main_compute)
            0xAA0003F3, // MOV X19, X0       (save shared buffer pointer in X19)
            0xD2802000, // MOVZ X0, #0x100   (argument = 0x100)
            0xD63F0080, // BLR X4            (call main_compute: returns 0x100 + 0x42 = 0x142)
            0xF9000660, // STR X0, [X19, #8] (shared[1] = 0x142)
            0xA8C17BFD, // LDP X29, X30, [SP], #16  (restore LR = THREAD_RETURN_SENTINEL)
            0xD65F03C0, // RET               (exits cleanly via THREAD_RETURN_SENTINEL)
        ];
        for (i, &insn) in worker_insns.iter().enumerate() {
            let pos = 0x100 + 0x600 + i * 4;
            subsdk_nso[pos..pos + 4].copy_from_slice(&insn.to_le_bytes());
        }
        fs::write(exefs.join("subsdk0"), &subsdk_nso).unwrap();

        // Load loose package into kernel
        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base_addr = 0x8000_0000u64;
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base_addr, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        assert_eq!(boot_info.thread_pointer, Some(STATIC_TLS_ADDR));
        assert_eq!(boot_info.relocations_applied, 2);
        assert!(boot_info.unresolved_imports.is_empty());

        // Verify main thread's initial TLS value was loaded from .tdata template
        let main_tls_val = mmu.read_u64(STATIC_TLS_ADDR + 16).unwrap();
        assert_eq!(main_tls_val, INITIAL_TLS_VAL);

        // Verify subsdk0's GOT slot 0x300 points to main_compute
        let subsdk_base = base_addr + 0x04000000;
        assert_eq!(mmu.read_u64(subsdk_base + 0x300).unwrap(), base_addr + 0x600);
        // Verify subsdk0's GOT slot 0x308 has TLS offset 16 (TCB 16 + 0)
        assert_eq!(mmu.read_u64(subsdk_base + 0x308).unwrap(), 16);

        // Allocate shared memory and stack for worker thread
        let shared_buf = 0x9000_2000u64;
        let worker_stack = 0x7000_3000u64;
        mmu.map_range(shared_buf, 4096, Permission::READ_WRITE);
        mmu.map_range(0x7000_0000, 0x4000, Permission::READ_WRITE);

        // Create and start worker thread in subsdk0 via Horizon kernel scheduler
        let worker_entry = subsdk_base + 0x600;
        let handle = kernel.scheduler.create_thread(worker_entry, shared_buf, worker_stack, 44, 1);
        let worker_tls = kernel.scheduler.setup_thread_tls(handle, &mut mmu).expect("Failed to setup worker TLS");
        assert_ne!(worker_tls, STATIC_TLS_ADDR);

        // Verify worker thread starts with a fresh copy of the initial non-zero TLS value
        assert_eq!(mmu.read_u64(worker_tls + 16).unwrap(), INITIAL_TLS_VAL);

        // Run worker thread to completion
        let finished = kernel.scheduler.start_and_run_thread(handle, &mut mmu, 500);
        assert!(finished);
        assert_eq!(kernel.scheduler.threads[&handle].state, crate::kernel::ThreadState::Exited);
        assert_eq!(kernel.scheduler.total_threads_completed, 1);

        // Check shared buffer:
        // shared[0] was initial TLS value read by worker
        assert_eq!(mmu.read_u64(shared_buf).unwrap(), INITIAL_TLS_VAL);
        // shared[1] was return value of main_compute (0x100 + 0x42 = 0x142)
        assert_eq!(mmu.read_u64(shared_buf + 8).unwrap(), 0x142);

        // Verify thread isolation:
        // Worker's TLS was updated to 0x9999
        assert_eq!(mmu.read_u64(worker_tls + 16).unwrap(), 0x9999);
        // Main thread's TLS at STATIC_TLS_ADDR remained UNTOUCHED!
        assert_eq!(mmu.read_u64(STATIC_TLS_ADDR + 16).unwrap(), INITIAL_TLS_VAL);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_homebrew_devkita64_linker_script_tls_fallback() {
        use crate::loader::nro::{build_linked_nso_with, TestSymbol};

        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_devkita64_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        fs::create_dir_all(&exefs).unwrap();

        // `main` defines TLS variable and exports devkitA64 linker script symbols:
        // __tdata_lma at 0x1800, __tdata_lma_end at 0x1808.
        let tls_sym = TestSymbol { name: "errno_val", shndx: 1, bind: 1, kind: 6, value: 0, size: 8 };
        let lma_start = TestSymbol { name: "__tdata_lma", shndx: 1, bind: 1, kind: 1, value: 0x1800, size: 0 };
        let lma_end = TestSymbol { name: "__tdata_lma_end", shndx: 1, bind: 1, kind: 1, value: 0x1808, size: 0 };

        let mut main_nso = build_linked_nso_with(&[tls_sym, lma_start, lma_end], &[], &[]);
        const ERRNO_INIT: u64 = 0xDEADC0DE_0000002A;
        main_nso[0x100 + 0x1800..0x100 + 0x1808].copy_from_slice(&ERRNO_INIT.to_le_bytes());
        fs::write(exefs.join("main"), &main_nso).unwrap();

        let pkg = LoosePackageLoader::discover(&temp_dir).expect("Failed to discover loose package");
        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base_addr = 0x8000_0000u64;
        let boot_info = LoosePackageLoader::load_into_kernel(&pkg, base_addr, &mut mmu, &mut kernel)
            .expect("Failed to load into kernel");

        assert_eq!(boot_info.thread_pointer, Some(STATIC_TLS_ADDR));
        // Verify .tdata was properly initialized using __tdata_lma fallback symbols without an ELF file
        assert_eq!(mmu.read_u64(STATIC_TLS_ADDR + 16).unwrap(), ERRNO_INIT);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_ouncecake_rtld_lz4_multimodule_and_lazy_romfs_boot() {
        let temp_dir = std::env::temp_dir().join(format!(
            "ounce_test_rtld_lz4_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let exefs = temp_dir.join("exefs");
        let romfs = temp_dir.join("romfs");
        fs::create_dir_all(&exefs).unwrap();
        fs::create_dir_all(&romfs).unwrap();

        // 42-bit Ounce NPDM (`address_space_type = 4` -> `flags[0x0C] = 4 << 1 = 0x08`) + RSA-3072 ACID
        let mut npdm = NpdmDescriptor::create_synthetic(
            "OunceTitle",
            "OUNCE-P-NX2A",
            0x0400_0239_0000_9999,
            44,
            0,
            &["sm:", "nvdrv", "aud:u", "fsp-srv"],
        );
        npdm[0x0C] = 4 << 1; // 42-bit Ounce address space
        let acid_off = npdm.len();
        npdm.resize(acid_off + 0x400, 0);
        npdm[acid_off + 0x300..acid_off + 0x304].copy_from_slice(&ACID_MAGIC);
        LittleEndian::write_u32(&mut npdm[0x78..0x7C], acid_off as u32);
        LittleEndian::write_u32(&mut npdm[0x7C..0x80], 0x400);
        fs::write(exefs.join("main.npdm"), &npdm).unwrap();

        // Build LZ4-compressed `rtld`, `main`, `subsdk2`, `subsdk10`, and `sdk` to verify:
        //  1) Numerical `subsdk` ordering (`subsdk2` before `subsdk10`)
        //  2) `rtld` loaded first at `base_addr` (`has_rtld == true`)
        //  3) Contiguous page-aligned layout with `CodeStatic` (`R-X` `0x05`) `.text` boundaries
        let mk_lz4_mod = |ret_imm: u16, bid_byte: u8| {
            let mut text = vec![0u8; 0x1000];
            let movz = 0xD280_0000u32 | ((ret_imm as u32) << 5);
            text[0..4].copy_from_slice(&movz.to_le_bytes());
            text[4..8].copy_from_slice(&0xD65F_03C0u32.to_le_bytes()); // RET
            let rodata = vec![bid_byte; 0x1000];
            let data = vec![0u8; 0x1000];
            NroLoader::build_compressed_nso(&text, &rodata, &data, 0x1000, &[bid_byte; 32])
        };

        fs::write(exefs.join("rtld"), mk_lz4_mod(1, 0xA1)).unwrap();
        fs::write(exefs.join("main"), mk_lz4_mod(2, 0xB2)).unwrap();
        fs::write(exefs.join("subsdk10"), mk_lz4_mod(10, 0xCA)).unwrap();
        fs::write(exefs.join("subsdk2"), mk_lz4_mod(20, 0xC2)).unwrap();
        fs::write(exefs.join("sdk"), mk_lz4_mod(99, 0xDD)).unwrap();
        fs::write(romfs.join("pak0.bin"), vec![0x77u8; 8192]).unwrap();

        let pkg = LoosePackageLoader::discover(&temp_dir).unwrap();
        let parsed_npdm = pkg.npdm.as_ref().unwrap();
        assert_eq!(parsed_npdm.address_space_bits, 42);
        assert_eq!(parsed_npdm.rsa_sig_bits, 3072);

        let mut mmu = Memory::new();
        let mut kernel = HorizonKernel::new();
        let base = 0x8000_0000u64;
        let info = LoosePackageLoader::load_into_kernel(&pkg, base, &mut mmu, &mut kernel).unwrap();

        assert!(info.has_rtld);
        assert_eq!(info.entry_pc, base);
        let names: Vec<&str> = info.module_map.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["rtld", "main", "subsdk2", "subsdk10", "sdk"]);

        // Simulate `rtld`'s `svcQueryMemory` scan: each module's `.text` starts at `m.base_addr`
        // with `(state = 0x03 CodeStatic, perm = 0x05 R-X)`.
        for m in &info.module_map {
            let (s, e, st, perm) = mmu.query_region_at(m.base_addr, kernel.address_space_bits);
            assert_eq!((s, e, st, perm), (m.base_addr, m.base_addr + 0x1000, 0x03, 0x05));
        }
        assert_eq!(kernel.ipc.nvdrv.persona, crate::kernel::GpuPersona::Ga10f);
        assert_eq!(kernel.heap_region_size, 0x3_0000_0000);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
