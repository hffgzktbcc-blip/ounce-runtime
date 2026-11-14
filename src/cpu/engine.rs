//! CPU tier selection: `--cpu=tier3|tier4|nce` or `OUNCE_CPU` (default `tier3`). Tier-4 needs the
//! `native-jit` feature; `nce` executes native AArch64 `MAP_JIT` blocks on Apple Silicon / ARM64.

use super::interpreter::Cpu;
use super::nce::NceEngine;
use super::tier2_jit::BlockExit;
use super::tier3_jit::Tier3JitEngine;
#[cfg(feature = "native-jit")]
use super::tier4::Tier4JitEngine;
use crate::mmu::Memory;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuTier {
    Interp,
    Tier3,
    Tier4,
    Nce,
}

static CLI_TIER: OnceLock<CpuTier> = OnceLock::new();

impl CpuTier {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "interp" | "interpreter" => Some(Self::Interp),
            "tier3" | "3" => Some(Self::Tier3),
            "tier4" | "4" | "native" => Some(Self::Tier4),
            "nce" | "5" | "apple-nce" => Some(Self::Nce),
            _ => None,
        }
    }

    /// The `--cpu=` choice if one was given, else `OUNCE_CPU`, else [`CpuTier::Tier3`].
    pub fn from_env() -> Self {
        if let Some(tier) = CLI_TIER.get() {
            return *tier;
        }
        std::env::var("OUNCE_CPU")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or(Self::Tier3)
    }

    pub fn set_cli_override(tier: Self) {
        let _ = CLI_TIER.set(tier);
    }
}

/// The selected block engine. Same `run_slice` contract for every tier.
pub enum CpuEngine {
    Interp {
        instructions: usize,
    },
    Tier3(Tier3JitEngine),
    #[cfg(feature = "native-jit")]
    Tier4(Box<Tier4JitEngine>),
    Nce(Box<NceEngine>),
}

impl CpuEngine {
    /// Builds the engine for `tier`, printing why if Tier-4 or NCE cannot run here.
    pub fn new(tier: CpuTier) -> Self {
        match tier {
            CpuTier::Interp => Self::Interp { instructions: 0 },
            CpuTier::Tier3 => CpuEngine::Tier3(Tier3JitEngine::new()),
            #[cfg(feature = "native-jit")]
            CpuTier::Tier4 => {
                let engine = Tier4JitEngine::new();
                match &engine.unavailable {
                    None => CpuEngine::Tier4(Box::new(engine)),
                    Some(why) => {
                        eprintln!("[Tier-4] Not available on this host ({why}); running Tier-3.");
                        CpuEngine::Tier3(Tier3JitEngine::new())
                    }
                }
            }
            #[cfg(not(feature = "native-jit"))]
            CpuTier::Tier4 => {
                eprintln!(
                    "[Tier-4] Not available: rebuild with `--features native-jit`; running Tier-3."
                );
                CpuEngine::Tier3(Tier3JitEngine::new())
            }
            CpuTier::Nce => {
                let engine = NceEngine::new();
                match &engine.unavailable {
                    None => CpuEngine::Nce(Box::new(engine)),
                    Some(why) => {
                        eprintln!(
                            "[NCE] Not available on this host ({why}); falling back to Tier-4."
                        );
                        Self::new(CpuTier::Tier4)
                    }
                }
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            CpuEngine::Interp { .. } => "Interpreter",
            CpuEngine::Tier3(_) => "Tier-3",
            #[cfg(feature = "native-jit")]
            CpuEngine::Tier4(_) => "Tier-4",
            CpuEngine::Nce(_) => "NCE",
        }
    }

    pub fn run_slice(
        &mut self,
        cpu: &mut Cpu,
        mmu: &mut Memory,
        max_instructions: usize,
    ) -> (usize, BlockExit) {
        // A pending BTI check (indirect branch into a guarded page) runs before any native block;
        // Tier-4 does not check landing pads itself yet.
        if cpu.check_landing_pad(mmu).is_err() {
            return (0, BlockExit::Fault { pc: cpu.regs.pc });
        }
        match self {
            CpuEngine::Interp { instructions } => {
                let mut n = 0;
                while n < max_instructions {
                    let exit = match cpu.step(mmu) {
                        Ok(super::StepResult::Continue) => None,
                        Ok(super::StepResult::Svc(svc_id)) => Some(BlockExit::Svc { svc_id }),
                        _ => return (n, BlockExit::Fault { pc: cpu.regs.pc }),
                    };
                    n += 1;
                    *instructions += 1;
                    if let Some(exit) = exit {
                        return (n, exit);
                    }
                }
                (n, BlockExit::BudgetExceeded)
            }
            CpuEngine::Tier3(e) => e.run_slice(cpu, mmu, max_instructions),
            #[cfg(feature = "native-jit")]
            CpuEngine::Tier4(e) => e.run_slice(cpu, mmu, max_instructions),
            CpuEngine::Nce(e) => e.run_slice(cpu, mmu, max_instructions),
        }
    }

    pub fn mips_rate(&self) -> f64 {
        match self {
            CpuEngine::Interp { .. } => 0.0,
            CpuEngine::Tier3(e) => e.mips_rate,
            #[cfg(feature = "native-jit")]
            CpuEngine::Tier4(e) => e.mips_rate,
            CpuEngine::Nce(e) => e.mips_rate,
        }
    }

    /// One line of engine counters for logs.
    pub fn stats(&self) -> String {
        match self {
            CpuEngine::Interp { instructions } => {
                format!("{instructions} interpreted instructions")
            }
            CpuEngine::Tier3(e) => format!("{} Super-Blocks fused", e.total_super_block_executions),
            #[cfg(feature = "native-jit")]
            CpuEngine::Tier4(e) => format!(
                "{} native blocks compiled, {} native runs, {} native / {} Tier-3 insns",
                e.blocks_compiled, e.native_block_runs, e.native_insns, e.fallback_insns
            ),
            CpuEngine::Nce(e) => format!(
                "{} NCE blocks compiled, {} NCE runs, {} NCE / {} fallback insns",
                e.blocks_compiled, e.nce_block_runs, e.nce_insns, e.fallback_insns
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tier_names() {
        assert_eq!(CpuTier::parse("tier3"), Some(CpuTier::Tier3));
        assert_eq!(CpuTier::parse(" Tier4 "), Some(CpuTier::Tier4));
        assert_eq!(CpuTier::parse("nce"), Some(CpuTier::Nce));
        assert_eq!(CpuTier::parse("tier5"), None);
    }
    #[test]
    fn slices_obey_exact_budgets_and_count_svc() {
        for tier in [
            CpuTier::Interp,
            CpuTier::Tier3,
            CpuTier::Tier4,
            CpuTier::Nce,
        ] {
            let mut engine = CpuEngine::new(tier);
            let mut cpu = Cpu::new();
            let mut mmu = Memory::new();
            mmu.map_range(0x1000, 4096, crate::mmu::Permission::READ_WRITE_EXECUTE);
            for i in 0..31 {
                mmu.write_u32(0x1000 + i * 4, 0x91000400).unwrap();
            }
            mmu.write_u32(0x107c, 0xd40000e1).unwrap(); // svc #7
            cpu.regs.pc = 0x1000;
            for budget in [1, 3, 7, 13, 7] {
                let (n, exit) = engine.run_slice(&mut cpu, &mut mmu, budget);
                assert_eq!(n, budget, "{}", engine.name());
                assert!(!matches!(exit, BlockExit::Fault { .. }));
            }
            assert_eq!(cpu.regs.x[0], 31);
            assert_eq!(
                engine.run_slice(&mut cpu, &mut mmu, 1),
                (1, BlockExit::Svc { svc_id: 7 })
            );
        }
    }
}
