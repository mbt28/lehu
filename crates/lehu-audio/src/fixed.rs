//! Integer arithmetic for the volume scale. The ARM926 has no FPU, and the
//! few logarithms a volume key needs are done here in Q16 rather than on
//! soft-float: a 2^x polynomial and a log2 by squaring, each within a few
//! parts in a million, which is far below a hundredth of a decibel.

/// log2(10) in Q16.
const LOG2_10_Q16: i64 = 217_706;

/// Taylor coefficients of 2^x on [0, 1), ln(2)^k / k!, in Q30. The seventh
/// term bounds the error at 1.5e-6.
const EXP2_Q30: [i64; 8] = [
    1_073_741_824,
    744_261_118,
    257_941_248,
    59_597_083,
    10_327_387,
    1_431_680,
    165_394,
    16_377,
];

/// log2 of `x` (Q16, treated as at least 1/65536) in Q16.
pub fn log2_q16(x: u64) -> i64 {
    let x = x.max(1);
    let top = i64::from(63 - x.leading_zeros());
    // The mantissa in [1, 2) as Q31; it is below 2^32, so its square fits.
    let mut m = if top >= 31 {
        x >> (top - 31)
    } else {
        x << (31 - top)
    };
    let mut frac: i64 = 0;
    for i in 0..16 {
        let sq = m * m; // Q62
        if sq >= 1u64 << 63 {
            m = sq >> 32;
            frac |= 1 << (15 - i);
        } else {
            m = sq >> 31;
        }
    }
    ((top - 16) << 16) + frac
}

/// 2^(y / 65536) in Q16; saturates at the ends.
pub fn exp2_q16(y: i64) -> u64 {
    let int = y >> 16;
    let frac = y & 0xFFFF;
    if int >= 47 {
        return u64::MAX;
    }
    if int < -40 {
        return 0;
    }
    // Horner in Q30; frac goes from Q16 to Q30.
    let f = frac << 14;
    let mut r = EXP2_Q30[7];
    for c in EXP2_Q30[..7].iter().rev() {
        r = c + ((r * f) >> 30);
    }
    // r is 2^frac in Q30, in [1, 2): to Q16, rounded, then the integer
    // part.
    let r = ((r + (1 << 13)) >> 14) as u64;
    if int >= 0 { r << int } else { r >> -int }
}

/// `n / d` rounded to the nearest, half away from zero; `d` positive.
fn div_round(n: i64, d: i64) -> i64 {
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

/// alsa-lib's "mute" bottom of a dB scale.
pub const DB_GAIN_MUTE: i64 = -9_999_999;

/// dB ranges up to this wide (0.01 dB units) map linearly; wider ones by
/// amplitude, cubed, which is alsamixer's and PulseAudio's rule, so a step
/// sounds about the same anywhere on the scale.
pub const LINEAR_DB_RANGE: i64 = 24 * 100;

/// 10^(centi / 6000): the amplitude of a cubic-scale value, Q16.
fn amp_of_centidb(centi: i64) -> u64 {
    exp2_q16(centi * LOG2_10_Q16 / 6000)
}

/// 6000 * log10(amp) for a Q16 amplitude, to the nearest 0.01 dB.
fn centidb_of_amp(amp: u64) -> i64 {
    div_round(log2_q16(amp) * 6000, LOG2_10_Q16)
}

/// A level (1..=100) on a control's dB scale `min..=max` (0.01 dB).
pub fn db_of_level(level: u32, min: i64, max: i64) -> i64 {
    let level = i64::from(level.clamp(1, 100));
    if max - min <= LINEAR_DB_RANGE {
        return div_round(level * (max - min), 100) + min;
    }
    let mut amp = (level as u64) * 65536 / 100;
    if min != DB_GAIN_MUTE {
        // The scale's floor is not silence: the curve lands on it at 0.
        let floor = amp_of_centidb(min - max);
        amp = amp * (65536 - floor.min(65536)) / 65536 + floor;
    }
    centidb_of_amp(amp) + max
}

/// The level a dB value stands for: the inverse of `db_of_level`.
pub fn level_of_db(value: i64, min: i64, max: i64) -> u32 {
    if max <= min {
        return 0;
    }
    let v_q16 = if max - min <= LINEAR_DB_RANGE {
        (value - min).clamp(0, max - min) * 65536 / (max - min)
    } else {
        let mut amp = amp_of_centidb(value.min(max) - max) as i64;
        if min != DB_GAIN_MUTE {
            let floor = amp_of_centidb(min - max) as i64;
            amp = (amp - floor).max(0) * 65536 / (65536 - floor).max(1);
        }
        amp
    };
    (div_round(v_q16 * 100, 65536).clamp(0, 100)) as u32
}

/// A level on a raw (no dB) range, rounded up so no level above 0 lands on
/// a codec's silent bottom step.
pub fn raw_of_level(level: u32, min: i64, max: i64) -> i64 {
    let level = i64::from(level.min(100));
    (level * (max - min) + 99) / 100 + min
}

/// The level a raw value stands for.
pub fn level_of_raw(value: i64, min: i64, max: i64) -> u32 {
    if max <= min {
        return 0;
    }
    div_round((value - min).clamp(0, max - min) * 100, max - min) as u32
}

/// The software gain for a level: the same cubic curve as the dB scale,
/// Q15 with 32768 at 100.
pub fn software_gain_q15(level: u32) -> i32 {
    let l = u64::from(level.min(100));
    ((32768 * l * l * l + 500_000) / 1_000_000) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_and_exp2_match_f64() {
        for &x in &[
            1u64,
            2,
            3,
            65536,
            65537,
            100_000,
            1 << 20,
            (1 << 32) + 12345,
            1 << 50,
        ] {
            let want = (x as f64 / 65536.0).log2();
            let got = log2_q16(x) as f64 / 65536.0;
            assert!((want - got).abs() < 2e-5, "log2 {x}: {want} vs {got}");
        }
        for y in (-20 * 65536..=20 * 65536).step_by(12345) {
            let want = 2f64.powf(y as f64 / 65536.0);
            let got = exp2_q16(y) as f64 / 65536.0;
            let tol = want * 1.6e-5 + 2.0 / 65536.0;
            assert!((want - got).abs() < tol, "exp2 {y}: {want} vs {got}");
        }
        assert_eq!(exp2_q16(0), 65536);
        assert_eq!(exp2_q16(65536), 131072);
        assert_eq!(log2_q16(0), log2_q16(1));
    }

    #[test]
    fn cubic_db_mapping_matches_the_float_rule() {
        // The F1C codec's Headphone: -62.5 .. 0 dB, no mute step.
        let (min, max) = (-6250, 0);
        for level in 1..=100u32 {
            let v = level as f64 / 100.0;
            let min_amp = 10f64.powf((min - max) as f64 / 6000.0);
            let amp = v * (1.0 - min_amp) + min_amp;
            let want = (6000.0 * amp.log10()).round() as i64 + max;
            let got = db_of_level(level, min, max);
            assert!((want - got).abs() <= 1, "level {level}: {want} vs {got}");
            assert_eq!(level_of_db(got, min, max), level, "inverse of {level}");
        }
        assert_eq!(db_of_level(100, min, max), 0);
        assert!(db_of_level(1, min, max) > min, "1% is above the floor");
        // A scale that bottoms out in mute: the plain cube.
        let (min, max) = (DB_GAIN_MUTE, 0);
        assert_eq!(db_of_level(100, min, max), 0);
        assert_eq!(db_of_level(50, min, max), -1806);
        assert_eq!(db_of_level(10, min, max), -6000);
        assert_eq!(level_of_db(-1806, min, max), 50);
        assert_eq!(level_of_db(-6000, min, max), 10);
        assert_eq!(level_of_db(500, min, max), 100);
    }

    #[test]
    fn narrow_and_raw_ranges_are_linear() {
        assert_eq!(db_of_level(50, -2000, 0), -1000);
        assert_eq!(db_of_level(100, -2000, 0), 0);
        assert_eq!(db_of_level(1, -2000, 0), -1980);
        assert_eq!(level_of_db(-1000, -2000, 0), 50);
        assert_eq!(level_of_db(-5000, -2000, 0), 0);
        assert_eq!(raw_of_level(50, 0, 31), 16);
        assert_eq!(raw_of_level(1, 0, 31), 1);
        assert_eq!(raw_of_level(0, 0, 31), 0);
        assert_eq!(level_of_raw(16, 0, 31), 52);
        assert_eq!(level_of_raw(0, 0, 0), 0);
    }

    #[test]
    fn software_gain_is_the_cube() {
        assert_eq!(software_gain_q15(100), 32768);
        assert_eq!(software_gain_q15(50), 4096);
        assert_eq!(software_gain_q15(0), 0);
        assert_eq!(software_gain_q15(10), 33);
        assert_eq!(software_gain_q15(200), 32768);
    }
}
