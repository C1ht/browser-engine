//! Intra prediction (section 7.11.2), chroma from luma and palette prediction.

use super::tile::is_directional;
use super::*;

#[inline]
pub(super) fn round2(x: i64, n: u32) -> i64 {
    if n == 0 { x } else { (x + (1 << (n - 1))) >> n }
}

#[inline]
pub(super) fn round2_signed(x: i64, n: u32) -> i64 {
    if x >= 0 { round2(x, n) } else { -round2(-x, n) }
}

/// Offset of index 0 in the edge arrays (which are valid from -2).
const O: usize = 16;

fn edge_strength(w: usize, h: usize, filter_type: bool, delta: i32) -> usize {
    let d = delta.abs();
    let blk = w + h;
    let mut s = 0;
    if !filter_type {
        if blk <= 8 {
            if d >= 56 {
                s = 1;
            }
        } else if blk <= 16 {
            if d >= 40 {
                s = 1;
            }
        } else if blk <= 24 {
            if d >= 8 {
                s = 1;
            }
            if d >= 16 {
                s = 2;
            }
            if d >= 32 {
                s = 3;
            }
        } else if blk <= 32 {
            s = 1;
            if d >= 4 {
                s = 2;
            }
            if d >= 32 {
                s = 3;
            }
        } else {
            s = 3;
        }
    } else if blk <= 8 {
        if d >= 40 {
            s = 1;
        }
        if d >= 64 {
            s = 2;
        }
    } else if blk <= 16 {
        if d >= 20 {
            s = 1;
        }
        if d >= 48 {
            s = 2;
        }
    } else if blk <= 24 {
        if d >= 4 {
            s = 3;
        }
    } else {
        s = 3;
    }
    s
}

fn use_upsample(w: usize, h: usize, filter_type: bool, delta: i32) -> bool {
    let d = delta.abs();
    let blk = w + h;
    if d <= 0 || d >= 40 {
        false
    } else if !filter_type {
        blk <= 16
    } else {
        blk <= 8
    }
}

/// Intra edge filter on buf[-1..sz-1] (stored at O - 1 ..).
fn edge_filter(buf: &mut [i32], sz: usize, strength: usize) {
    if strength == 0 {
        return;
    }
    let edge: Vec<i32> = (0..sz).map(|i| buf[O + i - 1]).collect();
    for i in 1..sz {
        let mut s = 0;
        for j in 0..INTRA_EDGE_TAPS {
            let k = (i as isize - 2 + j as isize).clamp(0, sz as isize - 1) as usize;
            s += INTRA_EDGE_KERNEL[(strength - 1) * INTRA_EDGE_TAPS + j] as i32 * edge[k];
        }
        buf[O + i - 1] = (s + 8) >> 4;
    }
}

fn upsample(buf: &mut [i32], num_px: usize, max: i32) {
    let mut dup = vec![0i32; num_px + 3];
    dup[0] = buf[O - 1];
    for i in -1..num_px as isize {
        dup[(i + 2) as usize] = buf[(O as isize + i) as usize];
    }
    dup[num_px + 2] = buf[O + num_px - 1];
    buf[O - 2] = dup[0];
    for i in 0..num_px {
        let s = -dup[i] + 9 * dup[i + 1] + 9 * dup[i + 2] - dup[i + 3];
        let s = (round2(s as i64, 4) as i32).clamp(0, max);
        buf[O + 2 * i - 1] = s;
        buf[O + 2 * i] = dup[i + 2];
    }
}

fn sm_weights(log2: usize) -> &'static [u8] {
    match log2 {
        2 => &SM_WEIGHTS_TX_4X4,
        3 => &SM_WEIGHTS_TX_8X8,
        4 => &SM_WEIGHTS_TX_16X16,
        5 => &SM_WEIGHTS_TX_32X32,
        _ => &SM_WEIGHTS_TX_64X64,
    }
}

impl Dec {
    fn is_smooth(&self, row: usize, col: usize, plane: usize) -> bool {
        let i = self.mi(row, col);
        let mode = if plane == 0 { self.y_modes[i] } else { self.uv_modes[i] } as usize;
        mode == SMOOTH_PRED || mode == SMOOTH_V_PRED || mode == SMOOTH_H_PRED
    }

    fn filter_type(&self, plane: usize) -> bool {
        let (sx, sy) = (self.seq.sub_x, self.seq.sub_y);
        let mut above = false;
        let mut left = false;
        if if plane == 0 { self.avail_u } else { self.avail_u_chroma } {
            let mut r = self.mi_row - 1;
            let mut c = self.mi_col;
            if plane > 0 {
                if sx == 1 && self.mi_col & 1 == 0 {
                    c += 1;
                }
                if sy == 1 && self.mi_row & 1 == 1 {
                    r -= 1;
                }
            }
            above = self.is_smooth(r, c.min(self.mi_cols - 1), plane);
        }
        if if plane == 0 { self.avail_l } else { self.avail_l_chroma } {
            let mut r = self.mi_row;
            let mut c = self.mi_col - 1;
            if plane > 0 {
                if sx == 1 && self.mi_col & 1 == 1 {
                    c -= 1;
                }
                if sy == 1 && self.mi_row & 1 == 0 {
                    r += 1;
                }
            }
            left = self.is_smooth(r.min(self.mi_rows - 1), c, plane);
        }
        above || left
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn predict_intra(&mut self, plane: usize, x: usize, y: usize, have_left: bool, have_above: bool, have_above_right: bool, have_below_left: bool, mode: usize, log2w: usize, log2h: usize) {
        let w = 1usize << log2w;
        let h = 1usize << log2h;
        let (sx, sy) = self.sub(plane);
        let max_x = ((self.mi_cols * 4) >> sx) as isize - 1;
        let max_y = ((self.mi_rows * 4) >> sy) as isize - 1;
        let stride = self.stride[plane];
        let bd = self.bit_depth();
        let maxv = (1i32 << bd) - 1;
        let n = w + h;
        let mut above = vec![0i32; 2 * n + 2 * O];
        let mut left = vec![0i32; 2 * n + 2 * O];
        {
            let f = &self.cur[plane];
            let px = |xx: usize, yy: usize| f[yy * stride + xx] as i32;
            for i in 0..n {
                above[O + i] = if !have_above && have_left {
                    px(x - 1, y)
                } else if !have_above {
                    (1 << (bd - 1)) - 1
                } else {
                    let lim = max_x.min((x + if have_above_right { 2 * w } else { w }) as isize - 1);
                    px(((x + i) as isize).min(lim) as usize, y - 1)
                };
                left[O + i] = if !have_left && have_above {
                    px(x, y - 1)
                } else if !have_left {
                    (1 << (bd - 1)) + 1
                } else {
                    let lim = max_y.min((y + if have_below_left { 2 * h } else { h }) as isize - 1);
                    px(x - 1, ((y + i) as isize).min(lim) as usize)
                };
            }
            let corner = if have_above && have_left {
                px(x - 1, y - 1)
            } else if have_above {
                px(x, y - 1)
            } else if have_left {
                px(x - 1, y)
            } else {
                1 << (bd - 1)
            };
            above[O - 1] = corner;
            left[O - 1] = corner;
        }
        let mut pred = vec![0i32; w * h];
        if plane == 0 && self.use_filter_intra {
            // Recursive (filter intra) prediction.
            let w4 = w >> 2;
            let h2 = h >> 1;
            let mode = self.filter_intra_mode;
            for i2 in 0..h2 {
                for j4 in 0..w4 {
                    let mut p = [0i32; 7];
                    for (i, pv) in p.iter_mut().enumerate() {
                        *pv = if i < 5 {
                            if i2 == 0 {
                                above[O + (j4 << 2) + i - 1]
                            } else if j4 == 0 && i == 0 {
                                left[O + (i2 << 1) - 1]
                            } else {
                                pred[((i2 << 1) - 1) * w + (j4 << 2) + i - 1]
                            }
                        } else if j4 == 0 {
                            left[O + (i2 << 1) + i - 5]
                        } else {
                            pred[((i2 << 1) + i - 5) * w + (j4 << 2) - 1]
                        };
                    }
                    for i1 in 0..2 {
                        for j1 in 0..4 {
                            let mut pr = 0i64;
                            for (i, &pv) in p.iter().enumerate() {
                                pr += INTRA_FILTER_TAPS[(mode * 8 + (i1 << 2) + j1) * 7 + i] as i64 * pv as i64;
                            }
                            pred[((i2 << 1) + i1) * w + (j4 << 2) + j1] = (round2_signed(pr, INTRA_FILTER_SCALE_BITS as u32) as i32).clamp(0, maxv);
                        }
                    }
                }
            }
        } else if is_directional(mode) {
            let angle_delta = if plane == 0 { self.angle_delta_y } else { self.angle_delta_uv };
            let p_angle = MODE_TO_ANGLE[mode] as i32 + angle_delta * ANGLE_STEP as i32;
            let mut up_above = 0u32;
            let mut up_left = 0u32;
            if self.seq.enable_intra_edge_filter {
                let filter_type = self.filter_type(plane);
                if p_angle != 90 && p_angle != 180 {
                    if p_angle > 90 && p_angle < 180 && (w + h) >= 24 {
                        let s = left[O] * 5 + above[O - 1] * 6 + above[O] * 5;
                        let c = round2(s as i64, 4) as i32;
                        left[O - 1] = c;
                        above[O - 1] = c;
                    }
                    if have_above {
                        let strength = edge_strength(w, h, filter_type, p_angle - 90);
                        let num_px = (w as isize).min(max_x - x as isize + 1) as usize + if p_angle < 90 { h } else { 0 } + 1;
                        edge_filter(&mut above, num_px, strength);
                    }
                    if have_left {
                        let strength = edge_strength(w, h, filter_type, p_angle - 180);
                        let num_px = (h as isize).min(max_y - y as isize + 1) as usize + if p_angle > 180 { w } else { 0 } + 1;
                        edge_filter(&mut left, num_px, strength);
                    }
                }
                if use_upsample(w, h, filter_type, p_angle - 90) {
                    up_above = 1;
                    upsample(&mut above, w + if p_angle < 90 { h } else { 0 }, maxv);
                }
                if use_upsample(w, h, filter_type, p_angle - 180) {
                    up_left = 1;
                    upsample(&mut left, h + if p_angle > 180 { w } else { 0 }, maxv);
                }
            }
            let dx = if p_angle < 90 {
                DR_INTRA_DERIVATIVE[p_angle as usize] as i32
            } else if p_angle > 90 && p_angle < 180 {
                DR_INTRA_DERIVATIVE[(180 - p_angle) as usize] as i32
            } else {
                0
            };
            let dy = if p_angle > 90 && p_angle < 180 {
                DR_INTRA_DERIVATIVE[(p_angle - 90) as usize] as i32
            } else if p_angle > 180 {
                DR_INTRA_DERIVATIVE[(270 - p_angle) as usize] as i32
            } else {
                0
            };
            let a = |i: i32| above[(O as i32 + i) as usize];
            let l = |i: i32| left[(O as i32 + i) as usize];
            for i in 0..h as i32 {
                for j in 0..w as i32 {
                    let v = if p_angle < 90 {
                        let idx = (i + 1) * dx;
                        let base = (idx >> (6 - up_above)) + (j << up_above);
                        let shift = ((idx << up_above) >> 1) & 0x1F;
                        let max_base_x = (w as i32 + h as i32 - 1) << up_above;
                        if base < max_base_x {
                            round2((a(base) * (32 - shift) + a(base + 1) * shift) as i64, 5) as i32
                        } else {
                            a(max_base_x)
                        }
                    } else if p_angle > 90 && p_angle < 180 {
                        let idx = (j << 6) - (i + 1) * dx;
                        let base = idx >> (6 - up_above);
                        if base >= -(1 << up_above) {
                            let shift = ((idx << up_above) >> 1) & 0x1F;
                            round2((a(base) * (32 - shift) + a(base + 1) * shift) as i64, 5) as i32
                        } else {
                            let idx = (i << 6) - (j + 1) * dy;
                            let base = idx >> (6 - up_left);
                            let shift = ((idx << up_left) >> 1) & 0x1F;
                            round2((l(base) * (32 - shift) + l(base + 1) * shift) as i64, 5) as i32
                        }
                    } else if p_angle > 180 {
                        let idx = (j + 1) * dy;
                        let base = (idx >> (6 - up_left)) + (i << up_left);
                        let shift = ((idx << up_left) >> 1) & 0x1F;
                        round2((l(base) * (32 - shift) + l(base + 1) * shift) as i64, 5) as i32
                    } else if p_angle == 90 {
                        a(j)
                    } else {
                        l(i)
                    };
                    pred[i as usize * w + j as usize] = v;
                }
            }
        } else if mode == SMOOTH_PRED || mode == SMOOTH_V_PRED || mode == SMOOTH_H_PRED {
            let wx = sm_weights(log2w);
            let wy = sm_weights(log2h);
            for i in 0..h {
                for j in 0..w {
                    let v = match mode {
                        SMOOTH_PRED => {
                            let s = wy[i] as i64 * above[O + j] as i64
                                + (256 - wy[i] as i64) * left[O + h - 1] as i64
                                + wx[j] as i64 * left[O + i] as i64
                                + (256 - wx[j] as i64) * above[O + w - 1] as i64;
                            round2(s, 9)
                        }
                        SMOOTH_V_PRED => round2(wy[i] as i64 * above[O + j] as i64 + (256 - wy[i] as i64) * left[O + h - 1] as i64, 8),
                        _ => round2(wx[j] as i64 * left[O + i] as i64 + (256 - wx[j] as i64) * above[O + w - 1] as i64, 8),
                    };
                    pred[i * w + j] = v as i32;
                }
            }
        } else if mode == DC_PRED {
            let v = if have_left && have_above {
                let mut sum: i64 = (0..h).map(|k| left[O + k] as i64).sum::<i64>() + (0..w).map(|k| above[O + k] as i64).sum::<i64>();
                sum += ((w + h) >> 1) as i64;
                (sum / (w + h) as i64) as i32
            } else if have_left {
                let sum: i64 = (0..h).map(|k| left[O + k] as i64).sum();
                (((sum + (h >> 1) as i64) >> log2h) as i32).clamp(0, maxv)
            } else if have_above {
                let sum: i64 = (0..w).map(|k| above[O + k] as i64).sum();
                (((sum + (w >> 1) as i64) >> log2w) as i32).clamp(0, maxv)
            } else {
                1 << (bd - 1)
            };
            pred.fill(v);
        } else {
            // PAETH_PRED
            let tl = above[O - 1];
            for i in 0..h {
                for j in 0..w {
                    let base = above[O + j] + left[O + i] - tl;
                    let p_left = (base - left[O + i]).abs();
                    let p_top = (base - above[O + j]).abs();
                    let p_tl = (base - tl).abs();
                    pred[i * w + j] = if p_left <= p_top && p_left <= p_tl {
                        left[O + i]
                    } else if p_top <= p_tl {
                        above[O + j]
                    } else {
                        tl
                    };
                }
            }
        }
        let f = &mut self.cur[plane];
        for i in 0..h {
            let row = (y + i) * stride + x;
            for j in 0..w {
                f[row + j] = pred[i * w + j] as u16;
            }
        }
    }

    pub(super) fn predict_cfl(&mut self, plane: usize, start_x: usize, start_y: usize, tx: usize) {
        let w = tx_w(tx);
        let h = tx_h(tx);
        let (sx, sy) = (self.seq.sub_x, self.seq.sub_y);
        let alpha = if plane == 1 { self.cfl_alpha_u } else { self.cfl_alpha_v } as i64;
        let ys = self.stride[0];
        let mut l = vec![0i64; w * h];
        let mut avg = 0i64;
        for i in 0..h {
            let luma_y = ((start_y + i) << sy).min(self.max_luma_h - (1 << sy));
            for j in 0..w {
                let luma_x = ((start_x + j) << sx).min(self.max_luma_w - (1 << sx));
                let mut t = 0i64;
                for dy in 0..=sy {
                    for dx in 0..=sx {
                        t += self.cur[0][(luma_y + dy) * ys + luma_x + dx] as i64;
                    }
                }
                let v = t << (3 - sx - sy);
                l[i * w + j] = v;
                avg += v;
            }
        }
        let avg = round2(avg, (TX_WIDTH_LOG2[tx] + TX_HEIGHT_LOG2[tx]) as u32);
        let stride = self.stride[plane];
        let maxv = (1i64 << self.bit_depth()) - 1;
        let f = &mut self.cur[plane];
        for i in 0..h {
            for j in 0..w {
                let k = (start_y + i) * stride + start_x + j;
                let dc = f[k] as i64;
                let scaled = round2_signed(alpha * (l[i * w + j] - avg), 6);
                f[k] = (dc + scaled).clamp(0, maxv) as u16;
            }
        }
    }

    pub(super) fn predict_palette(&mut self, plane: usize, start_x: usize, start_y: usize, x: usize, y: usize, tx: usize) {
        let w = tx_w(tx);
        let h = tx_h(tx);
        let palette = match plane {
            0 => self.palette_colors_y,
            1 => self.palette_colors_u,
            _ => self.palette_colors_v,
        };
        let stride = self.stride[plane];
        let map = if plane == 0 { &self.color_map_y } else { &self.color_map_uv };
        let f = &mut self.cur[plane];
        for i in 0..h {
            for j in 0..w {
                let m = map[(y * 4 + i) * CMAP + x * 4 + j] as usize;
                f[(start_y + i) * stride + start_x + j] = palette[m];
            }
        }
    }
}
