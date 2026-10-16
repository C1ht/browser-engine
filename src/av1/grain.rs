//! Film grain synthesis (section 7.18.3), applied to the output planes.

use super::predict::round2;
use super::*;

fn rnd(reg: &mut u32, bits: u32) -> usize {
    let r = *reg;
    let bit = (r ^ (r >> 1) ^ (r >> 3) ^ (r >> 12)) & 1;
    let r = (r >> 1) | (bit << 15);
    *reg = r;
    ((r >> (16 - bits)) & ((1 << bits) - 1)) as usize
}

impl Dec {
    pub(super) fn film_grain(&self, planes: &mut [Vec<u16>; 3], w: usize, h: usize) {
        let g = &self.grain;
        let bd = self.bit_depth() as i64;
        let (sx, sy) = (self.seq.sub_x, self.seq.sub_y);
        let mono = self.seq.mono;
        let mut reg: u32 = g.seed;
        let center = 128i64 << (bd - 8);
        let gmin = -center;
        let gmax = (256i64 << (bd - 8)) - 1 - center;
        let shift = (12 - bd + g.grain_scale_shift as i64) as u32;
        let lag = g.ar_coeff_lag as isize;
        let ar_shift = g.ar_shift as u32;

        // Luma grain: white noise, then the auto-regressive filter.
        let mut luma = vec![[0i64; 82]; 73];
        for row in luma.iter_mut() {
            for v in row.iter_mut() {
                let gs = if !g.y_points.is_empty() { GAUSSIAN_SEQUENCE[rnd(&mut reg, 11)] as i64 } else { 0 };
                *v = round2(gs, shift);
            }
        }
        if !g.y_points.is_empty() {
            for y in 3..73 {
                for x in 3..82 - 3 {
                    let mut s = 0i64;
                    let mut pos = 0;
                    'taps: for dr in -lag..=0 {
                        for dc in -lag..=lag {
                            if dr == 0 && dc == 0 {
                                break 'taps;
                            }
                            s += luma[(y as isize + dr) as usize][(x as isize + dc) as usize] * g.ar_y[pos] as i64;
                            pos += 1;
                        }
                    }
                    luma[y][x] = (luma[y][x] + round2(s, ar_shift)).clamp(gmin, gmax);
                }
            }
        }

        // Chroma grain, correlated with luma through the last coefficient.
        let cw = if sx == 1 { 44 } else { 82 };
        let ch = if sy == 1 { 38 } else { 73 };
        let mut cb = vec![vec![0i64; cw]; ch];
        let mut cr = vec![vec![0i64; cw]; ch];
        if !mono {
            let cfl = g.chroma_scaling_from_luma;
            reg = g.seed ^ 0xb524;
            for row in cb.iter_mut() {
                for v in row.iter_mut() {
                    let gs = if !g.cb_points.is_empty() || cfl { GAUSSIAN_SEQUENCE[rnd(&mut reg, 11)] as i64 } else { 0 };
                    *v = round2(gs, shift);
                }
            }
            reg = g.seed ^ 0x49d8;
            for row in cr.iter_mut() {
                for v in row.iter_mut() {
                    let gs = if !g.cr_points.is_empty() || cfl { GAUSSIAN_SEQUENCE[rnd(&mut reg, 11)] as i64 } else { 0 };
                    *v = round2(gs, shift);
                }
            }
            let coef = |a: &Vec<i32>, pos: usize| a.get(pos).copied().unwrap_or(0) as i64;
            for y in 3..ch {
                for x in 3..cw - 3 {
                    let (mut s0, mut s1) = (0i64, 0i64);
                    let mut pos = 0;
                    'ctaps: for dr in -lag..=0 {
                        for dc in -lag..=lag {
                            let c0 = coef(&g.ar_cb, pos);
                            let c1 = coef(&g.ar_cr, pos);
                            if dr == 0 && dc == 0 {
                                if !g.y_points.is_empty() {
                                    let lx = ((x - 3) << sx) + 3;
                                    let ly = ((y - 3) << sy) + 3;
                                    let mut l = 0i64;
                                    for i in 0..=sy {
                                        for j in 0..=sx {
                                            l += luma[ly + i][lx + j];
                                        }
                                    }
                                    let l = round2(l, (sx + sy) as u32);
                                    s0 += l * c0;
                                    s1 += l * c1;
                                }
                                break 'ctaps;
                            }
                            let (yy, xx) = ((y as isize + dr) as usize, (x as isize + dc) as usize);
                            s0 += cb[yy][xx] * c0;
                            s1 += cr[yy][xx] * c1;
                            pos += 1;
                        }
                    }
                    cb[y][x] = (cb[y][x] + round2(s0, ar_shift)).clamp(gmin, gmax);
                    cr[y][x] = (cr[y][x] + round2(s1, ar_shift)).clamp(gmin, gmax);
                }
            }
        }

        // Scaling lookup tables (piecewise linear).
        let points = |p: usize| -> &Vec<(i32, i32)> {
            if p == 0 || g.chroma_scaling_from_luma {
                &g.y_points
            } else if p == 1 {
                &g.cb_points
            } else {
                &g.cr_points
            }
        };
        let mut lut = [[0i64; 256]; 3];
        for p in 0..self.num_planes {
            let pts = points(p);
            if pts.is_empty() {
                continue;
            }
            for x in 0..(pts[0].0 as usize).min(256) {
                lut[p][x] = pts[0].1 as i64;
            }
            for i in 0..pts.len() - 1 {
                let dy = (pts[i + 1].1 - pts[i].1) as i64;
                let dx = (pts[i + 1].0 - pts[i].0) as i64;
                if dx <= 0 {
                    continue;
                }
                let delta = dy * ((65536 + (dx >> 1)) / dx);
                for x in 0..dx {
                    let k = pts[i].0 as i64 + x;
                    if (0..256).contains(&k) {
                        lut[p][k as usize] = pts[i].1 as i64 + ((x * delta + 32768) >> 16);
                    }
                }
            }
            for x in (pts[pts.len() - 1].0 as usize).min(256)..256 {
                lut[p][x] = pts[pts.len() - 1].1 as i64;
            }
        }
        let scale_lut = |p: usize, index: i64| -> i64 {
            let sh = (bd - 8) as u32;
            let x = (index >> sh) as usize;
            let rem = index - ((x as i64) << sh);
            if bd == 8 || x >= 255 {
                lut[p][x.min(255)]
            } else {
                let start = lut[p][x];
                let end = lut[p][x + 1];
                start + round2((end - start) * rem, sh)
            }
        };

        // Noise stripes, 32 luma rows each, blended horizontally.
        let num_stripes = ((h + 1) / 2).div_ceil(16).max(1);
        let sw = w + 80;
        let mut noise: Vec<[Vec<i64>; 3]> = (0..num_stripes).map(|_| [vec![0i64; 34 * sw], vec![0i64; 34 * sw], vec![0i64; 34 * sw]]).collect();
        let overlap = g.overlap;
        let mut luma_num = 0usize;
        let mut y = 0;
        while y < (h + 1) / 2 {
            reg = g.seed;
            reg ^= (((luma_num * 37 + 178) & 255) << 8) as u32;
            reg ^= ((luma_num * 173 + 105) & 255) as u32;
            let mut x = 0;
            while x < (w + 1) / 2 {
                let r = rnd(&mut reg, 8);
                let off_x = r >> 4;
                let off_y = r & 15;
                for p in 0..self.num_planes {
                    let (psx, psy) = if p > 0 { (sx, sy) } else { (0, 0) };
                    let pox = if psx == 1 { 6 + off_x } else { 9 + off_x * 2 };
                    let poy = if psy == 1 { 6 + off_y } else { 9 + off_y * 2 };
                    let st = &mut noise[luma_num][p];
                    for i in 0..(34 >> psy) {
                        for j in 0..(34 >> psx) {
                            let mut gv = match p {
                                0 => luma[poy + i][pox + j],
                                1 => cb[poy + i][pox + j],
                                _ => cr[poy + i][pox + j],
                            };
                            let col = if psx == 0 { x * 2 + j } else { x + j };
                            if col >= sw {
                                continue;
                            }
                            if psx == 0 {
                                if j < 2 && overlap && x > 0 {
                                    let old = st[i * sw + col];
                                    gv = if j == 0 { old * 27 + gv * 17 } else { old * 17 + gv * 27 };
                                    gv = round2(gv, 5).clamp(gmin, gmax);
                                }
                            } else if j == 0 && overlap && x > 0 {
                                let old = st[i * sw + col];
                                gv = round2(old * 23 + gv * 22, 5).clamp(gmin, gmax);
                            }
                            st[i * sw + col] = gv;
                        }
                    }
                }
                x += 16;
            }
            luma_num += 1;
            y += 16;
        }
        // Noise image sample with vertical blending between stripes.
        let noise_at = |p: usize, yy: usize, xx: usize| -> i64 {
            let psy = if p > 0 { sy } else { 0 };
            let ln = yy >> (5 - psy);
            let i = yy - (ln << (5 - psy));
            let mut gv = noise[ln][p][i * sw + xx];
            if psy == 0 {
                if i < 2 && ln > 0 && overlap {
                    let old = noise[ln - 1][p][(i + 32) * sw + xx];
                    gv = if i == 0 { old * 27 + gv * 17 } else { old * 17 + gv * 27 };
                    gv = round2(gv, 5).clamp(gmin, gmax);
                }
            } else if i < 1 && ln > 0 && overlap {
                let old = noise[ln - 1][p][(i + 16) * sw + xx];
                gv = round2(old * 23 + gv * 22, 5).clamp(gmin, gmax);
            }
            gv
        };
        let (min_v, max_luma, max_chroma) = if g.clip_restricted {
            let mx = 235i64 << (bd - 8);
            (16i64 << (bd - 8), mx, if self.seq.matrix == 0 { mx } else { 240i64 << (bd - 8) })
        } else {
            let mx = (256i64 << (bd - 8)) - 1;
            (0, mx, mx)
        };
        let ss = g.scaling_shift as u32;
        let maxv = (1i64 << bd) - 1;
        if !mono {
            let cwp = (w + sx) >> sx;
            let chp = (h + sy) >> sy;
            for yy in 0..chp {
                for xx in 0..cwp {
                    let lx = xx << sx;
                    let ly = yy << sy;
                    let lnx = (lx + 1).min(w - 1);
                    let avg = if sx == 1 { round2(planes[0][ly * w + lx] as i64 + planes[0][ly * w + lnx] as i64, 1) } else { planes[0][ly * w + lx] as i64 };
                    for (p, pts, mult, lmult, off) in [(1usize, &g.cb_points, g.cb_mult, g.cb_luma_mult, g.cb_offset), (2, &g.cr_points, g.cr_mult, g.cr_luma_mult, g.cr_offset)] {
                        if pts.is_empty() && !g.chroma_scaling_from_luma {
                            continue;
                        }
                        let orig = planes[p][yy * cwp + xx] as i64;
                        let merged = if g.chroma_scaling_from_luma {
                            avg
                        } else {
                            let combined = avg * (lmult as i64 - 128) + orig * (mult as i64 - 128);
                            ((combined >> 6) + ((off as i64 - 256) << (bd - 8))).clamp(0, maxv)
                        };
                        let n = round2(scale_lut(p, merged) * noise_at(p, yy, xx), ss);
                        planes[p][yy * cwp + xx] = (orig + n).clamp(min_v, max_chroma) as u16;
                    }
                }
            }
        }
        if !g.y_points.is_empty() {
            for yy in 0..h {
                for xx in 0..w {
                    let orig = planes[0][yy * w + xx] as i64;
                    let n = round2(scale_lut(0, orig) * noise_at(0, yy, xx), ss);
                    planes[0][yy * w + xx] = (orig + n).clamp(min_v, max_luma) as u16;
                }
            }
        }
    }
}
