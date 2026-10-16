//! What this CPU offers mdma's kernels, detected once at startup by
//! [`super::init`], so one binary built for a target's baseline still runs
//! AVX2 or AVX-512 where the machine has it. The exact path's variants give
//! the same bits at every level: detection changes speed, never results.
use std::sync::OnceLock;

/// The best variant of the exact kernels this CPU runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// What the build target guarantees: SSE2 on x86_64, NEON on aarch64.
    Baseline,
    /// AVX2, FMA and F16C (the vector part of x86-64-v3).
    X86V3,
    /// x86-64-v3 plus AVX-512 F, BW, DQ and VL.
    X86V4,
}

impl Level {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::X86V3 => "x86-64-v3",
            Self::X86V4 => "x86-64-v4",
        }
    }
}

#[derive(Debug)]
pub struct Isa {
    pub level: Level,
    /// Instruction sets present that a kernel could use, in a fixed order.
    pub features: Vec<&'static str>,
}

/// This CPU's instruction sets. [`super::init`] detects them at startup; a
/// library caller that skips it detects them on first use.
pub fn isa() -> &'static Isa {
    static ISA: OnceLock<Isa> = OnceLock::new();
    ISA.get_or_init(detect)
}

fn detect() -> Isa {
    cfg_select! {
        target_arch = "x86_64" => {
            macro_rules! present {
                ($($feature:tt),*) => {
                    [$(($feature, std::arch::is_x86_feature_detected!($feature))),*]
                };
            }
            let found = present!(
                "sse4.2", "avx", "avx2", "fma", "f16c", "avx512f", "avx512bw", "avx512dq",
                "avx512vl", "avx512vnni", "avx512bf16", "avxvnni"
            );
            let has = |name: &str| found.iter().any(|&(feature, on)| on && feature == name);
            let v3 = has("avx2") && has("fma") && has("f16c");
            let v4 = v3 && ["avx512f", "avx512bw", "avx512dq", "avx512vl"].into_iter().all(has);
            Isa {
                level: if v4 { Level::X86V4 } else if v3 { Level::X86V3 } else { Level::Baseline },
                features: found.iter().filter(|(_, on)| *on).map(|&(name, _)| name).collect(),
            }
        }
        target_arch = "aarch64" => {
            macro_rules! present {
                ($($feature:tt),*) => {
                    [$(($feature, std::arch::is_aarch64_feature_detected!($feature))),*]
                };
            }
            let found = present!("neon", "dotprod", "i8mm", "fp16", "bf16", "sve", "sve2");
            Isa {
                level: Level::Baseline,
                features: found.iter().filter(|(_, on)| *on).map(|&(name, _)| name).collect(),
            }
        }
        _ => {
            Isa {
                level: Level::Baseline,
                features: Vec::new(),
            }
        }
    }
}

/// `fn name(args) -> T = body;` defines `name(level)`: `body` compiled for
/// `level` as an `unsafe fn` pointer, or `None` when this CPU lacks it. Only
/// [`super::Kernels`] holds the pointers. `body` must be `#[inline(always)]`
/// so each variant compiles it with its own instruction sets.
macro_rules! multiversion {
    ($(#[$meta:meta])* $vis:vis fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $body:path;) => {
        $(#[$meta])*
        $vis fn $name(
            level: $crate::backend::cpu::isa::Level,
        ) -> Option<unsafe fn($($ty),*) -> $ret> {
            use $crate::backend::cpu::isa::{Level, isa};
            if level > isa().level {
                return None;
            }
            match level {
                Level::Baseline => Some($body as fn($($ty),*) -> $ret),
                #[cfg(target_arch = "x86_64")]
                Level::X86V3 => {
                    #[target_feature(enable = "avx2,fma,f16c")]
                    fn v3($($arg: $ty),*) -> $ret {
                        $body($($arg),*)
                    }
                    Some(v3)
                }
                #[cfg(target_arch = "x86_64")]
                Level::X86V4 => {
                    #[target_feature(enable = "avx2,fma,f16c,avx512f,avx512bw,avx512dq,avx512vl")]
                    fn v4($($arg: $ty),*) -> $ret {
                        $body($($arg),*)
                    }
                    Some(v4)
                }
                #[cfg(not(target_arch = "x86_64"))]
                Level::X86V3 | Level::X86V4 => None,
            }
        }
    };
}
pub(super) use multiversion;
