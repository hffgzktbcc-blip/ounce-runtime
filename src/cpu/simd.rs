#![allow(dead_code)]

//! Lane arithmetic for the AdvSIMD integer instructions, on 128-bit register values.
//! `esize` is the element size in bits (8, 16, 32 or 64). With `q == false` only the low 64 bits are
//! used and the result's upper 64 bits are zero, as the architecture requires.

use super::instructions::{FpOp2, SimdAcrossOp, SimdOp2, SimdOp3, SimdPermuteOp};

#[inline]
fn mask(esize: u32) -> u64 {
    if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 }
}

#[inline]
pub fn lane(v: u128, esize: u32, i: usize) -> u64 {
    (v >> (i as u32 * esize)) as u64 & mask(esize)
}

#[inline]
pub fn set_lane(v: &mut u128, esize: u32, i: usize, val: u64) {
    let shift = i as u32 * esize;
    *v &= !((mask(esize) as u128) << shift);
    *v |= ((val & mask(esize)) as u128) << shift;
}

/// Across-lanes reduction; the scalar result lands in lane 0 and the rest of the register is zero.
pub fn across(op: SimdAcrossOp, n: u128, esize: u32, q: bool) -> u128 {
    let lanes = (if q { 128 } else { 64 } / esize) as usize;
    let vals = (0..lanes).map(|i| lane(n, esize, i));
    let signed = |v: u64| sext(v, esize);
    let res = match op {
        SimdAcrossOp::Addv => vals.fold(0u64, |a, v| a.wrapping_add(v)) & mask(esize),
        SimdAcrossOp::Uaddlv => vals.fold(0u64, |a, v| a.wrapping_add(v)) & mask(esize * 2),
        SimdAcrossOp::Saddlv => (vals.fold(0i64, |a, v| a.wrapping_add(signed(v))) as u64) & mask(esize * 2),
        SimdAcrossOp::Umaxv => vals.max().unwrap_or(0),
        SimdAcrossOp::Uminv => vals.min().unwrap_or(0),
        SimdAcrossOp::Smaxv => vals.map(signed).max().unwrap_or(0) as u64 & mask(esize),
        SimdAcrossOp::Sminv => vals.map(signed).min().unwrap_or(0) as u64 & mask(esize),
    };
    res as u128
}

#[inline]
fn sext(val: u64, esize: u32) -> i64 {
    let s = 64 - esize;
    ((val << s) as i64) >> s
}

#[inline]
fn lanes(esize: u32, q: bool) -> usize {
    (if q { 128 } else { 64 }) / esize as usize
}

#[inline]
fn width_mask(q: bool) -> u128 {
    if q { u128::MAX } else { u64::MAX as u128 }
}

/// `DUP Vd.<T>, Rn`: every lane gets the low `esize` bits of `val`.
pub fn dup(val: u64, esize: u32, q: bool) -> u128 {
    let mut out = 0u128;
    for i in 0..lanes(esize, q) {
        set_lane(&mut out, esize, i, val);
    }
    out
}

/// Three-register same-type operations. `d` is the destination's old value (used by `BSL`/`BIT`/`BIF`).
/// Also returns whether sticky saturation occurred (`FPSR.QC`).
pub fn three_same_with_qc(op: SimdOp3, d: u128, n: u128, m: u128, esize: u32, q: bool) -> (u128, bool) {
    let bitwise = match op {
        SimdOp3::And => Some(n & m),
        SimdOp3::Bic => Some(n & !m),
        SimdOp3::Orr => Some(n | m),
        SimdOp3::Orn => Some(n | !m),
        SimdOp3::Eor => Some(n ^ m),
        SimdOp3::Bsl => Some((d & n) | (!d & m)),
        SimdOp3::Bit => Some((d & !m) | (n & m)),
        SimdOp3::Bif => Some((d & m) | (n & !m)),
        _ => None,
    };
    if let Some(v) = bitwise {
        return (v & width_mask(q), false);
    }

    let count = lanes(esize, q);
    let all = mask(esize);
    let mut out = 0u128;
    let mut qc = false;
    let pairwise = matches!(op, SimdOp3::Addp | SimdOp3::Uminp | SimdOp3::Umaxp | SimdOp3::Sminp | SimdOp3::Smaxp);
    for i in 0..count {
        let (a, b) = if pairwise {
            // Lanes of `n` then `m`, taken two at a time.
            let pick = |j: usize| if j < count { lane(n, esize, j) } else { lane(m, esize, j - count) };
            (pick(2 * i), pick(2 * i + 1))
        } else {
            (lane(n, esize, i), lane(m, esize, i))
        };
        let (sa, sb) = (sext(a, esize), sext(b, esize));
        let r = match op {
            SimdOp3::Add | SimdOp3::Addp => a.wrapping_add(b),
            SimdOp3::Sub => a.wrapping_sub(b),
            SimdOp3::Sqadd | SimdOp3::Sqsub => {
                let sum = if matches!(op, SimdOp3::Sqsub) {
                    (sa as i128) - (sb as i128)
                } else {
                    (sa as i128) + (sb as i128)
                };
                let min_val = -(1i128 << (esize - 1));
                let max_val = (1i128 << (esize - 1)) - 1;
                if sum > max_val {
                    qc = true;
                    max_val as u64
                } else if sum < min_val {
                    qc = true;
                    min_val as u64
                } else {
                    sum as u64
                }
            }
            SimdOp3::Uqadd => {
                let max_val = (1u128 << esize) - 1;
                let sum = (a as u128) + (b as u128);
                if sum > max_val {
                    qc = true;
                    max_val as u64
                } else {
                    sum as u64
                }
            }
            SimdOp3::Uqsub => {
                if a < b {
                    qc = true;
                    0
                } else {
                    a - b
                }
            }
            SimdOp3::Cmeq => if a == b { all } else { 0 },
            SimdOp3::Cmtst => if a & b != 0 { all } else { 0 },
            SimdOp3::Cmhi => if a > b { all } else { 0 },
            SimdOp3::Cmhs => if a >= b { all } else { 0 },
            SimdOp3::Cmgt => if sa > sb { all } else { 0 },
            SimdOp3::Cmge => if sa >= sb { all } else { 0 },
            SimdOp3::Umax | SimdOp3::Umaxp => a.max(b),
            SimdOp3::Umin | SimdOp3::Uminp => a.min(b),
            SimdOp3::Smax | SimdOp3::Smaxp => sa.max(sb) as u64,
            SimdOp3::Smin | SimdOp3::Sminp => sa.min(sb) as u64,
            _ => unreachable!("bitwise ops return above"),
        };
        set_lane(&mut out, esize, i, r);
    }
    (out, qc)
}

/// Three-register same-type operations. `d` is the destination's old value (used by `BSL`/`BIT`/`BIF`).
pub fn three_same(op: SimdOp3, d: u128, n: u128, m: u128, esize: u32, q: bool) -> u128 {
    three_same_with_qc(op, d, n, m, esize, q).0
}

/// Two-register miscellaneous operations.
pub fn two_misc(op: SimdOp2, n: u128, esize: u32, q: bool) -> u128 {
    if op == SimdOp2::Not {
        return !n & width_mask(q);
    }
    let all = mask(esize);
    let mut out = 0u128;
    for i in 0..lanes(esize, q) {
        let a = lane(n, esize, i);
        let s = sext(a, esize);
        let r = match op {
            SimdOp2::Cmeq0 => if a == 0 { all } else { 0 },
            SimdOp2::Cmgt0 => if s > 0 { all } else { 0 },
            SimdOp2::Cmge0 => if s >= 0 { all } else { 0 },
            SimdOp2::Cmle0 => if s <= 0 { all } else { 0 },
            SimdOp2::Cmlt0 => if s < 0 { all } else { 0 },
            SimdOp2::Cnt => a.count_ones() as u64,
            SimdOp2::Abs => s.unsigned_abs(),
            SimdOp2::Neg => (s.wrapping_neg()) as u64,
            SimdOp2::Scvtf => {
                if esize == 32 {
                    (s as i32 as f32).to_bits() as u64
                } else {
                    (s as f64).to_bits()
                }
            }
            SimdOp2::Ucvtf => {
                if esize == 32 {
                    (a as u32 as f32).to_bits() as u64
                } else {
                    (a as f64).to_bits()
                }
            }
            SimdOp2::Fcvtzs => {
                if esize == 32 {
                    let f = f32::from_bits(a as u32);
                    if f.is_nan() { 0 } else { f as i32 as u32 as u64 }
                } else {
                    let f = f64::from_bits(a);
                    if f.is_nan() { 0 } else { f as i64 as u64 }
                }
            }
            SimdOp2::Fcvtzu => {
                if esize == 32 {
                    let f = f32::from_bits(a as u32);
                    if f.is_nan() || f < 0.0 { 0 } else { f as u32 as u64 }
                } else {
                    let f = f64::from_bits(a);
                    if f.is_nan() || f < 0.0 { 0 } else { f as u64 }
                }
            }
            SimdOp2::Not => unreachable!(),
        };
        set_lane(&mut out, esize, i, r);
    }
    out
}

/// `SSHR`/`USHR`/`SHL` by `shift` on `esize` lanes.
pub fn shift_imm(n: u128, esize: u32, shift: u32, q: bool, signed: bool, left: bool) -> u128 {
    let mut out = 0u128;
    for i in 0..lanes(esize, q) {
        let a = lane(n, esize, i);
        let r = if left {
            if shift >= 64 { 0 } else { a << shift }
        } else if signed {
            (sext(a, esize) >> shift.min(63)) as u64
        } else if shift >= 64 {
            0
        } else {
            a >> shift
        };
        set_lane(&mut out, esize, i, r);
    }
    out
}

/// `SHRN`/`SHRN2`: shifts each `2 * esize` lane of `n` right and keeps the low `esize` bits.
/// `SHRN` writes the low half and zeroes the rest; `SHRN2` (`q`) writes the high half and keeps `d`'s low half.
pub fn shrn(d: u128, n: u128, esize: u32, shift: u32, q: bool) -> u128 {
    let count = 64 / esize as usize;
    let mut half = 0u128;
    for i in 0..count {
        set_lane(&mut half, esize, i, lane(n, esize * 2, i) >> shift);
    }
    if q { (d & u64::MAX as u128) | (half << 64) } else { half }
}

/// `UZP1/UZP2/TRN1/TRN2/ZIP1/ZIP2` on `esize` lanes.
pub fn permute(op: SimdPermuteOp, n: u128, m: u128, esize: u32, q: bool) -> u128 {
    let count = lanes(esize, q);
    let half = count / 2;
    let mut out = 0u128;
    for i in 0..count {
        let v = match op {
            // Even (UZP1) or odd (UZP2) lanes of n then m.
            SimdPermuteOp::Uzp1 | SimdPermuteOp::Uzp2 => {
                let j = 2 * i + (op == SimdPermuteOp::Uzp2) as usize;
                if j < count { lane(n, esize, j) } else { lane(m, esize, j - count) }
            }
            // Pairs: n[2k + p], m[2k + p] with p = 0 (TRN1) or 1 (TRN2).
            SimdPermuteOp::Trn1 | SimdPermuteOp::Trn2 => {
                let j = (i & !1) + (op == SimdPermuteOp::Trn2) as usize;
                if i % 2 == 0 { lane(n, esize, j) } else { lane(m, esize, j) }
            }
            // Interleave the low (ZIP1) or high (ZIP2) halves.
            SimdPermuteOp::Zip1 | SimdPermuteOp::Zip2 => {
                let j = i / 2 + if op == SimdPermuteOp::Zip2 { half } else { 0 };
                if i % 2 == 0 { lane(n, esize, j) } else { lane(m, esize, j) }
            }
        };
        set_lane(&mut out, esize, i, v);
    }
    out
}

/// ARMv8.2 `SDOT` / `UDOT`: accumulates 4-way byte dot products into 32-bit lanes of `d`.
/// When `index` is `Some(idx)`, every lane dots against the 4-byte group `idx` (`0..4`) of `m`.
pub fn dot_prod(d: u128, n: u128, m: u128, index: Option<u8>, is_unsigned: bool, q: bool) -> u128 {
    let count = if q { 4 } else { 2 };
    let mut out = 0u128;
    for i in 0..count {
        let acc = lane(d, 32, i) as u32;
        let m_grp = index.map(|idx| (idx as usize) & 3).unwrap_or(i);
        let sum = if is_unsigned {
            let mut s = acc;
            for k in 0..4 {
                let a = lane(n, 8, 4 * i + k) as u32;
                let b = lane(m, 8, 4 * m_grp + k) as u32;
                s = s.wrapping_add(a * b);
            }
            s
        } else {
            let mut s = acc as i32;
            for k in 0..4 {
                let a = (lane(n, 8, 4 * i + k) as u8 as i8) as i32;
                let b = (lane(m, 8, 4 * m_grp + k) as u8 as i8) as i32;
                s = s.wrapping_add(a * b);
            }
            s as u32
        };
        set_lane(&mut out, 32, i, sum as u64);
    }
    out
}

/// Converts IEEE 754 `binary16` (`u16`) to `f32` (exact).
#[inline]
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = ((bits as u32) & 0x8000) << 16;
    let exp = ((bits >> 10) & 0x1F) as u32;
    let mant = (bits & 0x03FF) as u32;

    let f32_bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal binary16 -> normalized binary32
            let mut m = mant;
            let mut e = 127u32 - 14;
            while (m & 0x0400) == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03FF;
            sign | (e << 23) | (m << 13)
        }
    } else if exp == 0x1F {
        if mant == 0 {
            sign | 0x7F80_0000
        } else {
            // Quiet NaN (set quiet bit 22 + preserve payload)
            sign | 0x7FC0_0000 | (mant << 13)
        }
    } else {
        let e = exp + (127 - 15);
        sign | (e << 23) | (mant << 13)
    };
    f32::from_bits(f32_bits)
}

/// Converts `f32` to IEEE 754 `binary16` (`u16`) using round-to-nearest-even.
#[inline]
pub fn f32_to_f16(val: f32) -> u16 {
    let bits = val.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x007F_FFFF;

    if exp == 0xFF {
        if mant == 0 {
            return sign | 0x7C00; // +-Inf
        }
        let payload = ((mant >> 13) as u16) & 0x03FF;
        return sign | 0x7E00 | payload.max(1); // Canonical / quiet NaN
    }

    let unbiased = exp - 127;
    if unbiased > 15 {
        return sign | 0x7C00; // Overflow to +-Inf
    }
    if unbiased < -24 {
        return sign; // Underflow to +-0
    }

    if unbiased < -14 {
        // Subnormal binary16 result: shift implicit 1-bit + mantissa right
        let full_mant = mant | 0x0080_0000;
        let shift = (-14 - unbiased) as u32 + 13; // 14..24
        let half_mant = (full_mant >> shift) as u16;
        let rem = full_mant & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        let round_up = rem > halfway || (rem == halfway && (half_mant & 1) != 0);
        return sign | half_mant.wrapping_add(u16::from(round_up));
    }

    let half_exp = ((unbiased + 15) as u16) << 10;
    let half_mant = (mant >> 13) as u16;
    let rem = mant & 0x1FFF;
    let round_up = rem > 0x1000 || (rem == 0x1000 && (half_mant & 1) != 0);
    // Carry out of `half_mant == 0x03FF` increments `half_exp` naturally (into Inf if exp == 30).
    sign | (half_exp + half_mant).wrapping_add(u16::from(round_up))
}

#[inline]
pub fn f16_to_f64(bits: u16) -> f64 {
    f16_to_f32(bits) as f64
}

/// Converts `f64` to IEEE 754 `binary16` (`u16`) without double-rounding hazard via round-to-odd staging.
#[inline]
pub fn f64_to_f16(val: f64) -> u16 {
    let bits = val.to_bits();
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let mant = bits & 0x000F_FFFF_FFFF_FFFF;
    if exp == 0x7FF {
        let sign = ((bits >> 48) & 0x8000) as u16;
        return if mant == 0 { sign | 0x7C00 } else { sign | 0x7E00 };
    }
    // Round-to-odd into f32 preserves exact ties for subsequent round-to-nearest-even into f16!
    let mut f32_val = val as f32;
    let back = f32_val as f64;
    if back != val && f32_val.is_finite() {
        let mut fb = f32_val.to_bits();
        if (fb & 1) == 0 {
            if back.abs() > val.abs() {
                fb = fb.wrapping_sub(1);
            } else {
                fb = fb.wrapping_add(1);
            }
            f32_val = f32::from_bits(fb);
        }
    }
    f32_to_f16(f32_val)
}

/// Vector FP16 (`Vd.4H` / `Vd.8H`) binary arithmetic (`FADD`, `FSUB`, `FMUL`).
pub fn fp16_vec2(op: FpOp2, n: u128, m: u128, q: bool) -> u128 {
    let count = if q { 8 } else { 4 };
    let mut out = 0u128;
    for i in 0..count {
        let a = f16_to_f32(lane(n, 16, i) as u16);
        let b = f16_to_f32(lane(m, 16, i) as u16);
        let r = match op {
            FpOp2::Fadd => a + b,
            FpOp2::Fsub => a - b,
            FpOp2::Fmul => a * b,
            FpOp2::Fdiv => a / b,
            FpOp2::Fmax => a.max(b),
            FpOp2::Fmin => a.min(b),
            FpOp2::Fmaxnm => if a.is_nan() { b } else if b.is_nan() { a } else { a.max(b) },
            FpOp2::Fminnm => if a.is_nan() { b } else if b.is_nan() { a } else { a.min(b) },
            FpOp2::Fnmul => -(a * b),
        };
        set_lane(&mut out, 16, i, f32_to_f16(r) as u64);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dup_and_lanes() {
        assert_eq!(dup(0xAB, 8, true), 0xABAB_ABAB_ABAB_ABAB_ABAB_ABAB_ABAB_ABABu128);
        assert_eq!(dup(0x1234_5678, 32, false), 0x1234_5678_1234_5678u128);
        let mut v = 0u128;
        set_lane(&mut v, 16, 7, 0xBEEF);
        assert_eq!(lane(v, 16, 7), 0xBEEF);
        assert_eq!(v >> 112, 0xBEEF);
    }

    #[test]
    fn pairwise_ops() {
        // n lanes 0..16, m lanes 16..32 (bytes).
        let n = (0..16u128).fold(0, |acc, i| acc | (i << (i * 8)));
        let m = (0..16u128).fold(0, |acc, i| acc | ((i + 16) << (i * 8)));
        let r = three_same(SimdOp3::Addp, 0, n, m, 8, true);
        for i in 0..16 {
            assert_eq!(lane(r, 8, i), (4 * i + 1) as u64);
        }
        let r = three_same(SimdOp3::Uminp, 0, n, m, 8, true);
        for i in 0..16 {
            assert_eq!(lane(r, 8, i), (2 * i) as u64);
        }
    }

    #[test]
    fn compares_and_bitwise() {
        let n = 0x00FF_0000_0000_0010_0000_0000_0000_FF00u128;
        let r = two_misc(SimdOp2::Cmeq0, n, 8, true);
        for i in 0..16 {
            assert_eq!(lane(r, 8, i), if lane(n, 8, i) == 0 { 0xFF } else { 0 });
        }
        assert_eq!(two_misc(SimdOp2::Cmeq0, n, 8, false) >> 64, 0);
        assert_eq!(three_same(SimdOp3::And, 0, u128::MAX, 0xF0F0, 8, true), 0xF0F0);
        assert_eq!(three_same(SimdOp3::Orr, 0, 1 << 100, 1, 8, false), 1);
        assert_eq!(three_same(SimdOp3::Bsl, 0xFF00, 0x1234, 0xABCD, 8, true), 0x12CD);
        assert_eq!(two_misc(SimdOp2::Cnt, 0xFF07, 8, false), 0x0803);
        assert_eq!(three_same(SimdOp3::Smax, 0, 0x80, 0x7F, 8, false), 0x7F);
        assert_eq!(three_same(SimdOp3::Umax, 0, 0x80, 0x7F, 8, false), 0x80);
    }

    #[test]
    fn permutes() {
        let n = 0x0F0E_0D0C_0B0A_0908_0706_0504_0302_0100u128;
        let m = 0x1F1E_1D1C_1B1A_1918_1716_1514_1312_1110u128;
        let bytes = |v: u128| v.to_le_bytes();
        assert_eq!(bytes(permute(SimdPermuteOp::Zip1, n, m, 8, true))[..4], [0x00, 0x10, 0x01, 0x11]);
        assert_eq!(bytes(permute(SimdPermuteOp::Zip2, n, m, 8, true))[..4], [0x08, 0x18, 0x09, 0x19]);
        assert_eq!(bytes(permute(SimdPermuteOp::Uzp1, n, m, 8, true))[6..10], [0x0C, 0x0E, 0x10, 0x12]);
        assert_eq!(bytes(permute(SimdPermuteOp::Uzp2, n, m, 8, true))[..2], [0x01, 0x03]);
        assert_eq!(bytes(permute(SimdPermuteOp::Trn1, n, m, 8, true))[..4], [0x00, 0x10, 0x02, 0x12]);
        assert_eq!(bytes(permute(SimdPermuteOp::Trn2, n, m, 8, true))[..4], [0x01, 0x11, 0x03, 0x13]);
        assert_eq!(permute(SimdPermuteOp::Zip1, n, m, 8, false) >> 64, 0);
    }

    #[test]
    fn shifts() {
        // SHRN v.8b, v.8h, #4: each halfword 0x0AB0 -> byte 0xAB.
        let n = dup(0x0AB0, 16, true);
        assert_eq!(shrn(0, n, 8, 4, false), 0xABAB_ABAB_ABAB_ABABu128);
        assert_eq!(shrn(0x1111, n, 8, 4, true), 0xABAB_ABAB_ABAB_ABAB_0000_0000_0000_1111u128);
        assert_eq!(shift_imm(dup(0x80, 8, false), 8, 7, false, false, false), dup(1, 8, false));
        assert_eq!(shift_imm(dup(0x80, 8, false), 8, 7, false, true, false), dup(0xFF, 8, false));
        assert_eq!(shift_imm(dup(1, 32, true), 32, 31, true, false, true), dup(0x8000_0000, 32, true));
    }
}
