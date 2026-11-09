// SPDX-License-Identifier: GPL-3.0-or-later
//! What the CPU can do, read once at start.
//!
//! The fixed-point audio path and the software crypto are always there and are
//! what the F1C200s runs. A build for a target with a float unit, SIMD or the
//! crypto extensions picks faster paths at run time where it finds them, never
//! at compile time alone, so one binary per architecture serves every board of
//! that architecture. Detect once in `main` and pass the result down; there is
//! no global.

use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// A hardware float unit: VFP on ARM, FP on AArch64, SSE2 on x86-64.
    pub fpu: bool,
    /// SIMD: NEON on ARM, ASIMD on AArch64, AVX2 on x86-64.
    pub simd: bool,
    /// AES instructions.
    pub aes: bool,
    /// SHA-2 instructions.
    pub sha2: bool,
    /// Carry-less multiply (GHASH, CRC).
    pub pmull: bool,
    /// Cores the process may run on.
    pub cores: u32,
}

/// Keys of the ELF auxiliary vector that carry the kernel's capability words.
const AT_HWCAP: usize = 16;
const AT_HWCAP2: usize = 26;

impl Features {
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let (hwcap, hwcap2) = std::fs::read("/proc/self/auxv")
            .map(|bytes| parse_auxv(&bytes))
            .unwrap_or((0, 0));
        Self {
            cores,
            ..from_hwcap(hwcap, hwcap2)
        }
    }
}

/// AT_HWCAP and AT_HWCAP2 out of an auxiliary vector: native-endian pairs of
/// machine words, ended by a zero key. Missing keys read as 0.
pub fn parse_auxv(bytes: &[u8]) -> (usize, usize) {
    const W: usize = size_of::<usize>();
    let mut hwcap = 0;
    let mut hwcap2 = 0;
    for pair in bytes.as_chunks::<{ 2 * W }>().0 {
        let key = usize::from_ne_bytes(pair[..W].try_into().unwrap());
        let value = usize::from_ne_bytes(pair[W..].try_into().unwrap());
        match key {
            0 => break,
            AT_HWCAP => hwcap = value,
            AT_HWCAP2 => hwcap2 = value,
            _ => {}
        }
    }
    (hwcap, hwcap2)
}

#[cfg(target_arch = "arm")]
fn from_hwcap(hwcap: usize, hwcap2: usize) -> Features {
    // arch/arm/include/uapi/asm/hwcap.h
    const VFP: usize = 1 << 6;
    const NEON: usize = 1 << 12;
    const VFPV3: usize = 1 << 13;
    const VFPV4: usize = 1 << 16;
    const AES: usize = 1 << 0;
    const PMULL: usize = 1 << 1;
    const SHA2: usize = 1 << 3;
    Features {
        fpu: hwcap & (VFP | VFPV3 | VFPV4) != 0,
        simd: hwcap & NEON != 0,
        aes: hwcap2 & AES != 0,
        sha2: hwcap2 & SHA2 != 0,
        pmull: hwcap2 & PMULL != 0,
        cores: 0,
    }
}

#[cfg(target_arch = "aarch64")]
fn from_hwcap(hwcap: usize, _hwcap2: usize) -> Features {
    // arch/arm64/include/uapi/asm/hwcap.h
    const FP: usize = 1 << 0;
    const ASIMD: usize = 1 << 1;
    const AES: usize = 1 << 3;
    const PMULL: usize = 1 << 4;
    const SHA2: usize = 1 << 6;
    Features {
        fpu: hwcap & FP != 0,
        simd: hwcap & ASIMD != 0,
        aes: hwcap & AES != 0,
        sha2: hwcap & SHA2 != 0,
        pmull: hwcap & PMULL != 0,
        cores: 0,
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn from_hwcap(_hwcap: usize, _hwcap2: usize) -> Features {
    // The kernel's words say little on x86; cpuid does.
    Features {
        fpu: std::arch::is_x86_feature_detected!("sse2"),
        simd: std::arch::is_x86_feature_detected!("avx2"),
        aes: std::arch::is_x86_feature_detected!("aes"),
        sha2: std::arch::is_x86_feature_detected!("sha"),
        pmull: std::arch::is_x86_feature_detected!("pclmulqdq"),
        cores: 0,
    }
}

#[cfg(not(any(
    target_arch = "arm",
    target_arch = "aarch64",
    target_arch = "x86",
    target_arch = "x86_64"
)))]
fn from_hwcap(_hwcap: usize, _hwcap2: usize) -> Features {
    Features::default()
}

impl fmt::Display for Features {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let yn = |b: bool| if b { "yes" } else { "no" };
        write!(
            f,
            "fpu={} simd={} aes={} sha2={} pmull={} cores={}",
            yn(self.fpu),
            yn(self.simd),
            yn(self.aes),
            yn(self.sha2),
            yn(self.pmull),
            self.cores
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auxv(pairs: &[(usize, usize)]) -> Vec<u8> {
        let mut v = Vec::new();
        for &(k, val) in pairs {
            v.extend_from_slice(&k.to_ne_bytes());
            v.extend_from_slice(&val.to_ne_bytes());
        }
        v
    }

    #[test]
    fn reads_both_capability_words() {
        let bytes = auxv(&[
            (33, 7),
            (AT_HWCAP, 0x1234),
            (AT_HWCAP2, 0x9),
            (0, 0),
            (AT_HWCAP, 1),
        ]);
        assert_eq!(parse_auxv(&bytes), (0x1234, 0x9));
    }

    #[test]
    fn missing_words_read_as_zero() {
        assert_eq!(parse_auxv(&auxv(&[(6, 4096), (0, 0)])), (0, 0));
        assert_eq!(parse_auxv(&[1, 2, 3]), (0, 0)); // a torn tail is ignored
    }

    #[test]
    fn detect_counts_a_core() {
        assert!(Features::detect().cores >= 1);
    }
}
