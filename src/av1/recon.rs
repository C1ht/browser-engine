//! Dequantization and inverse transforms (sections 7.12 and 7.13).

use super::predict::round2;
use super::*;

const SINPI: [i64; 5] = [0, 1321, 2482, 3344, 3803];

fn cos128(angle: i32) -> i64 {
    let a = angle & 255;
    let v = if a <= 64 {
        COS128_LOOKUP[a as usize] as i64
    } else if a <= 128 {
        -(COS128_LOOKUP[(128 - a) as usize] as i64)
    } else if a <= 192 {
        -(COS128_LOOKUP[(a - 128) as usize] as i64)
    } else {
        COS128_LOOKUP[(256 - a) as usize] as i64
    };
    v
}

fn sin128(angle: i32) -> i64 {
    cos128(angle - 64)
}

fn brev(bits: u32, x: usize) -> usize {
    let mut t = 0;
    for i in 0..bits {
        t |= ((x >> i) & 1) << (bits - 1 - i);
    }
    t
}

struct T<'a> {
    t: &'a mut [i64],
    r: u32,
}

impl T<'_> {
    #[inline]
    fn b(&mut self, a: usize, b: usize, angle: i32, flip: bool) {
        let x = self.t[a] * cos128(angle) - self.t[b] * sin128(angle);
        let y = self.t[a] * sin128(angle) + self.t[b] * cos128(angle);
        self.t[a] = round2(x, 12);
        self.t[b] = round2(y, 12);
        if flip {
            self.t.swap(a, b);
        }
    }
    #[inline]
    fn h(&mut self, a: usize, b: usize, flip: bool) {
        let (a, b) = if flip { (b, a) } else { (a, b) };
        let (x, y) = (self.t[a], self.t[b]);
        let lo = -(1i64 << (self.r - 1));
        let hi = (1i64 << (self.r - 1)) - 1;
        self.t[a] = (x + y).clamp(lo, hi);
        self.t[b] = (x - y).clamp(lo, hi);
    }

    fn idct(&mut self, n: u32) {
        let n0 = 1usize << n;
        let copy: Vec<i64> = self.t[..n0].to_vec();
        for i in 0..n0 {
            self.t[i] = copy[brev(n, i)];
        }
        let bi = |x: usize, b: u32| brev(b, x) as i32;
        if n == 6 {
            for i in 0..16 {
                self.b(32 + i, 63 - i, 63 - 4 * bi(i, 4), false);
            }
        }
        if n >= 5 {
            for i in 0..8 {
                self.b(16 + i, 31 - i, 6 + (bi(7 - i, 3) << 3), false);
            }
        }
        if n == 6 {
            for i in 0..16 {
                self.h(32 + i * 2, 33 + i * 2, i & 1 == 1);
            }
        }
        if n >= 4 {
            for i in 0..4 {
                self.b(8 + i, 15 - i, 12 + (bi(3 - i, 2) << 4), false);
            }
        }
        if n >= 5 {
            for i in 0..8 {
                self.h(16 + 2 * i, 17 + 2 * i, i & 1 == 1);
            }
        }
        if n == 6 {
            for i in 0..4 {
                for j in 0..2 {
                    self.b(62 - i * 4 - j, 33 + i * 4 + j, 60 - 16 * bi(i, 2) + 64 * j as i32, true);
                }
            }
        }
        if n >= 3 {
            for i in 0..2 {
                self.b(4 + i, 7 - i, 56 - 32 * i as i32, false);
            }
        }
        if n >= 4 {
            for i in 0..4 {
                self.h(8 + 2 * i, 9 + 2 * i, i & 1 == 1);
            }
        }
        if n >= 5 {
            for i in 0..2 {
                for j in 0..2 {
                    self.b(30 - 4 * i - j, 17 + 4 * i + j, 24 + ((j as i32) << 6) + ((1 - i as i32) << 5), true);
                }
            }
        }
        if n == 6 {
            for i in 0..8 {
                for j in 0..2 {
                    self.h(32 + i * 4 + j, 35 + i * 4 - j, i & 1 == 1);
                }
            }
        }
        for i in 0..2 {
            self.b(2 * i, 2 * i + 1, 32 + 16 * i as i32, i == 0);
        }
        if n >= 3 {
            for i in 0..2 {
                self.h(4 + 2 * i, 5 + 2 * i, i == 1);
            }
        }
        if n >= 4 {
            for i in 0..2 {
                self.b(14 - i, 9 + i, 48 + 64 * i as i32, true);
            }
        }
        if n >= 5 {
            for i in 0..4 {
                for j in 0..2 {
                    self.h(16 + 4 * i + j, 19 + 4 * i - j, i & 1 == 1);
                }
            }
        }
        if n == 6 {
            for i in 0..2 {
                for j in 0..4 {
                    self.b(61 - i * 8 - j, 34 + i * 8 + j, 56 - i as i32 * 32 + (j as i32 >> 1) * 64, true);
                }
            }
        }
        for i in 0..2 {
            self.h(i, 3 - i, false);
        }
        if n >= 3 {
            self.b(6, 5, 32, true);
        }
        if n >= 4 {
            for i in 0..2 {
                for j in 0..2 {
                    self.h(8 + 4 * i + j, 11 + 4 * i - j, i == 1);
                }
            }
        }
        if n >= 5 {
            for i in 0..4 {
                self.b(29 - i, 18 + i, 48 + (i as i32 >> 1) * 64, true);
            }
        }
        if n == 6 {
            for i in 0..4 {
                for j in 0..4 {
                    self.h(32 + 8 * i + j, 39 + 8 * i - j, i & 1 == 1);
                }
            }
        }
        if n >= 3 {
            for i in 0..4 {
                self.h(i, 7 - i, false);
            }
        }
        if n >= 4 {
            for i in 0..2 {
                self.b(13 - i, 10 + i, 32, true);
            }
        }
        if n >= 5 {
            for i in 0..2 {
                for j in 0..4 {
                    self.h(16 + i * 8 + j, 23 + i * 8 - j, i == 1);
                }
            }
        }
        if n == 6 {
            for i in 0..8 {
                self.b(59 - i, 36 + i, if i < 4 { 48 } else { 112 }, true);
            }
        }
        if n >= 4 {
            for i in 0..8 {
                self.h(i, 15 - i, false);
            }
        }
        if n >= 5 {
            for i in 0..4 {
                self.b(27 - i, 20 + i, 32, true);
            }
        }
        if n == 6 {
            for i in 0..8 {
                self.h(32 + i, 47 - i, false);
                self.h(48 + i, 63 - i, true);
            }
        }
        if n >= 5 {
            for i in 0..16 {
                self.h(i, 31 - i, false);
            }
        }
        if n == 6 {
            for i in 0..8 {
                self.b(55 - i, 40 + i, 32, true);
            }
        }
        if n == 6 {
            for i in 0..32 {
                self.h(i, 63 - i, false);
            }
        }
    }

    fn adst_in_perm(&mut self, n: u32) {
        let n0 = 1usize << n;
        let copy: Vec<i64> = self.t[..n0].to_vec();
        for i in 0..n0 {
            let idx = if i & 1 == 1 { i - 1 } else { n0 - i - 1 };
            self.t[i] = copy[idx];
        }
    }

    fn adst_out_perm(&mut self, n: u32) {
        let n0 = 1usize << n;
        let copy: Vec<i64> = self.t[..n0].to_vec();
        for i in 0..n0 {
            let a = (i >> 3) & 1;
            let b = ((i >> 2) & 1) ^ ((i >> 3) & 1);
            let c = ((i >> 1) & 1) ^ ((i >> 2) & 1);
            let d = (i & 1) ^ ((i >> 1) & 1);
            let idx = ((d << 3) | (c << 2) | (b << 1) | a) >> (4 - n);
            self.t[i] = if i & 1 == 1 { -copy[idx] } else { copy[idx] };
        }
    }

    fn adst4(&mut self) {
        let t = &mut self.t;
        let mut s = [0i64; 7];
        s[0] = SINPI[1] * t[0];
        s[1] = SINPI[2] * t[0];
        s[2] = SINPI[3] * t[1];
        s[3] = SINPI[4] * t[2];
        s[4] = SINPI[1] * t[2];
        s[5] = SINPI[2] * t[3];
        s[6] = SINPI[4] * t[3];
        let a7 = t[0] - t[2];
        let b7 = a7 + t[3];
        s[0] += s[3];
        s[1] -= s[4];
        s[3] = s[2];
        s[2] = SINPI[3] * b7;
        s[0] += s[5];
        s[1] -= s[6];
        let x0 = s[0] + s[3];
        let x1 = s[1] + s[3];
        let x2 = s[2];
        let x3 = s[0] + s[1] - s[3];
        t[0] = round2(x0, 12);
        t[1] = round2(x1, 12);
        t[2] = round2(x2, 12);
        t[3] = round2(x3, 12);
    }

    fn adst8(&mut self) {
        self.adst_in_perm(3);
        for i in 0..4 {
            self.b(2 * i, 2 * i + 1, 60 - 16 * i as i32, true);
        }
        for i in 0..4 {
            self.h(i, 4 + i, false);
        }
        for i in 0..2 {
            self.b(4 + 3 * i, 5 + i, 48 - 32 * i as i32, true);
        }
        for i in 0..2 {
            for j in 0..2 {
                self.h(4 * j + i, 2 + 4 * j + i, false);
            }
        }
        for i in 0..2 {
            self.b(2 + 4 * i, 3 + 4 * i, 32, true);
        }
        self.adst_out_perm(3);
    }

    fn adst16(&mut self) {
        self.adst_in_perm(4);
        for i in 0..8 {
            self.b(2 * i, 2 * i + 1, 62 - 8 * i as i32, true);
        }
        for i in 0..8 {
            self.h(i, 8 + i, false);
        }
        for i in 0..2 {
            self.b(8 + 2 * i, 9 + 2 * i, 56 - 32 * i as i32, true);
            self.b(13 + 2 * i, 12 + 2 * i, 8 + 32 * i as i32, true);
        }
        for i in 0..4 {
            for j in 0..2 {
                self.h(8 * j + i, 4 + 8 * j + i, false);
            }
        }
        for i in 0..2 {
            for j in 0..2 {
                self.b(4 + 8 * j + 3 * i, 5 + 8 * j + i, 48 - 32 * i as i32, true);
            }
        }
        for i in 0..2 {
            for j in 0..4 {
                self.h(4 * j + i, 2 + 4 * j + i, false);
            }
        }
        for i in 0..4 {
            self.b(2 + 4 * i, 3 + 4 * i, 32, true);
        }
        self.adst_out_perm(4);
    }

    fn adst(&mut self, n: u32) {
        match n {
            2 => self.adst4(),
            3 => self.adst8(),
            _ => self.adst16(),
        }
    }

    fn identity(&mut self, n: u32) {
        let n0 = 1usize << n;
        for v in self.t[..n0].iter_mut() {
            *v = match n {
                2 => round2(*v * 5793, 12),
                3 => *v * 2,
                4 => round2(*v * 11586, 12),
                _ => *v * 4,
            };
        }
    }

    fn wht(&mut self, shift: u32) {
        let t = &mut self.t;
        let mut a = t[0] >> shift;
        let mut c = t[1] >> shift;
        let mut d = t[2] >> shift;
        let mut b = t[3] >> shift;
        a += c;
        d -= b;
        let e = (a - d) >> 1;
        b = e - b;
        c = e - c;
        a -= b;
        d += c;
        t[0] = a;
        t[1] = b;
        t[2] = c;
        t[3] = d;
    }
}

fn row_kind(t: usize) -> u8 {
    match t {
        DCT_DCT | ADST_DCT | FLIPADST_DCT | H_DCT => 0,
        DCT_ADST | ADST_ADST | DCT_FLIPADST | FLIPADST_FLIPADST | ADST_FLIPADST | FLIPADST_ADST | H_ADST | H_FLIPADST => 1,
        _ => 2,
    }
}

fn col_kind(t: usize) -> u8 {
    match t {
        DCT_DCT | DCT_ADST | DCT_FLIPADST | V_DCT => 0,
        ADST_DCT | ADST_ADST | FLIPADST_DCT | FLIPADST_FLIPADST | ADST_FLIPADST | FLIPADST_ADST | V_ADST | V_FLIPADST => 1,
        _ => 2,
    }
}

impl Dec {
    fn dc_q(&self, b: i32) -> i64 {
        DC_QLOOKUP[(((self.bit_depth() - 8) >> 1) * 256) as usize + b.clamp(0, 255) as usize] as i64
    }
    fn ac_q(&self, b: i32) -> i64 {
        AC_QLOOKUP[(((self.bit_depth() - 8) >> 1) * 256) as usize + b.clamp(0, 255) as usize] as i64
    }

    pub(super) fn reconstruct(&mut self, plane: usize, x: usize, y: usize, tx: usize) {
        let dq_denom: i64 = match tx {
            TX_32X32 | TX_16X32 | TX_32X16 | TX_16X64 | TX_64X16 => 2,
            TX_64X64 | TX_32X64 | TX_64X32 => 4,
            _ => 1,
        };
        let log2w = TX_WIDTH_LOG2[tx] as u32;
        let log2h = TX_HEIGHT_LOG2[tx] as u32;
        let w = 1usize << log2w;
        let h = 1usize << log2h;
        let tw = w.min(32);
        let th = h.min(32);
        let t = self.plane_tx_type;
        let flip_ud = matches!(t, FLIPADST_DCT | FLIPADST_ADST | V_FLIPADST | FLIPADST_FLIPADST);
        let flip_lr = matches!(t, DCT_FLIPADST | ADST_FLIPADST | H_FLIPADST | FLIPADST_FLIPADST);
        let bd = self.bit_depth();
        let qindex = self.get_qindex(false, self.segment_id);
        let (dc_delta, ac_delta) = match plane {
            0 => (self.delta_q_y_dc, 0),
            1 => (self.delta_q_u_dc, self.delta_q_u_ac),
            _ => (self.delta_q_v_dc, self.delta_q_v_ac),
        };
        let dcq = self.dc_q(qindex + dc_delta);
        let acq = self.ac_q(qindex + ac_delta);
        let qm_level = if self.using_qmatrix && t < IDTX { self.seg_qm_level[plane][self.segment_id] } else { 15 };
        let lim = 1i64 << (7 + bd);
        // Dequant (tw x th) into a w x h residual working buffer.
        let mut res = vec![0i64; w * h];
        for i in 0..th {
            for j in 0..tw {
                let qv = self.quant[i * tw + j] as i64;
                if qv == 0 {
                    continue;
                }
                let q = if i == 0 && j == 0 { dcq } else { acq };
                let q2 = if qm_level < 15 {
                    let m = QUANTIZER_MATRIX[(qm_level * 2 + (plane > 0) as usize) * QM_TOTAL_SIZE + QM_OFFSET[tx] as usize + i * tw + j] as i64;
                    round2(q * m, 5)
                } else {
                    q
                };
                let dq = qv * q2;
                let dq2 = dq.signum() * ((dq.abs() & 0xFFFFFF) / dq_denom);
                res[i * w + j] = dq2.clamp(-lim, lim - 1);
            }
        }
        let lossless = self.lossless;
        let row_shift = if lossless { 0 } else { TRANSFORM_ROW_SHIFT[tx] as u32 };
        let col_shift = if lossless { 0 } else { 4 };
        let row_clamp = bd + 8;
        let col_clamp = (bd + 6).max(16);
        let mut buf = vec![0i64; 64];
        let rk = row_kind(t);
        let ck = col_kind(t);
        for i in 0..h {
            if i >= 32 {
                // Rows beyond 32 have no coefficients: the transforms map zeros to zeros.
                for j in 0..w {
                    res[i * w + j] = 0;
                }
                continue;
            }
            let row = &mut res[i * w..i * w + w];
            if row.iter().all(|&v| v == 0) && !lossless {
                continue;
            }
            for j in 0..w {
                buf[j] = if j < 32 { row[j] } else { 0 };
            }
            if log2w.abs_diff(log2h) == 1 {
                for v in buf[..w].iter_mut() {
                    *v = round2(*v * 2896, 12);
                }
            }
            let mut tt = T { t: &mut buf, r: row_clamp };
            if lossless {
                tt.wht(2);
            } else {
                match rk {
                    0 => tt.idct(log2w),
                    1 => tt.adst(log2w),
                    _ => tt.identity(log2w),
                }
            }
            for j in 0..w {
                row[j] = round2(buf[j], row_shift);
            }
        }
        let lo = -(1i64 << (col_clamp - 1));
        let hi = (1i64 << (col_clamp - 1)) - 1;
        for v in res.iter_mut() {
            *v = (*v).clamp(lo, hi);
        }
        for j in 0..w {
            for i in 0..h {
                buf[i] = res[i * w + j];
            }
            let mut tt = T { t: &mut buf, r: col_clamp };
            if lossless {
                tt.wht(0);
            } else {
                match ck {
                    0 => tt.idct(log2h),
                    1 => tt.adst(log2h),
                    _ => tt.identity(log2h),
                }
            }
            for i in 0..h {
                res[i * w + j] = round2(buf[i], col_shift);
            }
        }
        let stride = self.stride[plane];
        let maxv = (1i64 << bd) - 1;
        let f = &mut self.cur[plane];
        for i in 0..h {
            let yy = if flip_ud { h - i - 1 } else { i };
            for j in 0..w {
                let xx = if flip_lr { w - j - 1 } else { j };
                let k = (y + yy) * stride + x + xx;
                f[k] = (f[k] as i64 + res[i * w + j]).clamp(0, maxv) as u16;
            }
        }
    }
}
