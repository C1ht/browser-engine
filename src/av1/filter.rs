//! Post-filters: deblocking loop filter (7.14), CDEF (7.15) and loop
//! restoration (7.17), then output.

use super::predict::round2;
use super::*;

impl Dec {
    /// Run the post-filters and produce the output frame.
    pub(super) fn finish(&mut self) -> Option<Frame> {
        if self.use_superres {
            return None; // superres upscaling is not supported
        }
        if self.loop_filter_level[0] != 0 || self.loop_filter_level[1] != 0 {
            self.loop_filter();
        }
        let cdef = self.cdef();
        let out = if self.uses_lr { self.loop_restoration(&cdef) } else { cdef };
        let (w, h) = (self.upscaled_width, self.frame_height);
        let mut planes: [Vec<u16>; 3] = Default::default();
        for p in 0..self.num_planes {
            let (sx, sy) = self.sub(p);
            let (pw, ph) = ((w + sx) >> sx, (h + sy) >> sy);
            let stride = self.stride[p];
            let mut v = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                v.extend_from_slice(&out[p][y * stride..y * stride + pw]);
            }
            planes[p] = v;
        }
        if self.grain.apply {
            self.film_grain(&mut planes, w, h);
        }
        Some(Frame {
            width: w,
            height: h,
            bit_depth: self.seq.bit_depth,
            sub_x: self.seq.sub_x,
            sub_y: self.seq.sub_y,
            mono: self.seq.mono,
            planes,
            color_primaries: self.seq.color_primaries,
            transfer: self.seq.transfer,
            matrix: self.seq.matrix,
            full_range: self.seq.full_range,
        })
    }

    // ---------------------------------------------------------- deblocking

    fn loop_filter(&mut self) {
        for plane in 0..self.num_planes {
            if plane == 0 || self.loop_filter_level[1 + plane] != 0 {
                let (sx, sy) = self.sub(plane);
                for pass in 0..2 {
                    let row_step = 1 << sy;
                    let col_step = 1 << sx;
                    let mut row = 0;
                    while row < self.mi_rows {
                        let mut col = 0;
                        while col < self.mi_cols {
                            self.edge(plane, pass, row, col);
                            col += col_step;
                        }
                        row += row_step;
                    }
                }
            }
        }
    }

    fn filter_strength(&self, row: usize, col: usize, plane: usize, pass: usize) -> (i32, i32, i32, i32) {
        let i = self.mi(row, col);
        let segment = self.segment_ids[i] as usize;
        let delta_lf = if self.delta_lf_multi { self.delta_lfs[i][if plane == 0 { pass } else { plane + 1 }] } else { self.delta_lfs[i][0] } as i32;
        let li = if plane == 0 { pass } else { plane + 1 };
        let m = MAX_LOOP_FILTER as i32;
        let base = (delta_lf + self.loop_filter_level[li]).clamp(0, m);
        let mut lvl = base;
        let feature = SEG_LVL_ALT_LF_Y_V + li;
        if self.seg_feature_active_idx(segment, feature) {
            lvl = (self.feature_data[segment][feature] + lvl).clamp(0, m);
        }
        if self.loop_filter_delta_enabled {
            let n_shift = lvl >> 5;
            lvl += self.loop_filter_ref_deltas[0] << n_shift;
            lvl = lvl.clamp(0, m);
        }
        let sharp = self.loop_filter_sharpness;
        let shift = if sharp > 4 {
            2
        } else if sharp > 0 {
            1
        } else {
            0
        };
        let limit = if sharp > 0 { (lvl >> shift).clamp(1, 9 - sharp) } else { (lvl >> shift).max(1) };
        let blimit = 2 * (lvl + 2) + limit;
        let thresh = lvl >> 4;
        (lvl, limit, blimit, thresh)
    }

    fn edge(&mut self, plane: usize, pass: usize, row: usize, col: usize) {
        let (sx, sy) = self.sub(plane);
        let (dx, dy) = if pass == 0 { (1usize, 0usize) } else { (0, 1) };
        let x = col * 4;
        let y = row * 4;
        let row = row | sy;
        let col = col | sx;
        if x >= self.frame_width || y >= self.frame_height || (pass == 0 && x == 0) || (pass == 1 && y == 0) {
            return;
        }
        let xp = x >> sx;
        let yp = y >> sy;
        let prev_row = row - (dy << sy);
        let prev_col = col - (dx << sx);
        let mi_size = self.mi_sizes[self.mi(row, col)] as usize;
        let tx = self.lf_tx_sizes[plane][(row >> sy) * self.mi_cols + (col >> sx)] as usize;
        let plane_size = self.plane_residual_size(mi_size, plane);
        let prev_tx = self.lf_tx_sizes[plane][(prev_row >> sy) * self.mi_cols + (prev_col >> sx)] as usize;
        let is_tx_edge = if pass == 0 { xp % tx_w(tx) == 0 } else { yp % tx_h(tx) == 0 };
        let _ = plane_size;
        // Intra frames: every transform edge is filtered.
        if !is_tx_edge {
            return;
        }
        let base = if pass == 0 { tx_w(prev_tx).min(tx_w(tx)) } else { tx_h(prev_tx).min(tx_h(tx)) };
        let filter_size = if plane == 0 { base.min(16) } else { base.min(8) };
        let (mut lvl, mut limit, mut blimit, mut thresh) = self.filter_strength(row, col, plane, pass);
        if lvl == 0 {
            (lvl, limit, blimit, thresh) = self.filter_strength(prev_row, prev_col, plane, pass);
        }
        if lvl == 0 {
            return;
        }
        for i in 0..4 {
            self.sample_filter(xp + dy * i, yp + dx * i, plane, limit, blimit, thresh, dx, dy, filter_size);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_filter(&mut self, x: usize, y: usize, plane: usize, limit: i32, blimit: i32, thresh: i32, dx: usize, dy: usize, filter_size: usize) {
        let stride = self.stride[plane];
        let step = (dx + dy * stride) as isize;
        let bd = self.bit_depth();
        let f = &mut self.cur[plane];
        let c = (y * stride + x) as isize;
        let g = |k: isize| f[(c + k * step) as usize] as i32;
        let (q0, q1, q2, q3) = (g(0), g(1), g(2), g(3));
        let (p0, p1, p2, p3) = (g(-1), g(-2), g(-3), g(-4));
        let sh = bd - 8;
        let hev = (p1 - p0).abs() > (thresh << sh) || (q1 - q0).abs() > (thresh << sh);
        let filter_len = if filter_size == 4 {
            4
        } else if plane != 0 {
            6
        } else if filter_size == 8 {
            8
        } else {
            16
        };
        let limit_bd = limit << sh;
        let blimit_bd = blimit << sh;
        let mut mask = (p1 - p0).abs() > limit_bd || (q1 - q0).abs() > limit_bd || (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > blimit_bd;
        if filter_len >= 6 {
            mask |= (p2 - p1).abs() > limit_bd || (q2 - q1).abs() > limit_bd;
        }
        if filter_len >= 8 {
            mask |= (p3 - p2).abs() > limit_bd || (q3 - q2).abs() > limit_bd;
        }
        if mask {
            return;
        }
        let thr = 1 << sh;
        let mut flat = false;
        if filter_size >= 8 {
            let mut m = (p1 - p0).abs() > thr || (q1 - q0).abs() > thr || (p2 - p0).abs() > thr || (q2 - q0).abs() > thr;
            if filter_len >= 8 {
                m |= (p3 - p0).abs() > thr || (q3 - q0).abs() > thr;
            }
            flat = !m;
        }
        let mut flat2 = false;
        if filter_size >= 16 {
            let (q4, q5, q6) = (g(4), g(5), g(6));
            let (p4, p5, p6) = (g(-5), g(-6), g(-7));
            let m = (p6 - p0).abs() > thr || (q6 - q0).abs() > thr || (p5 - p0).abs() > thr || (q5 - q0).abs() > thr || (p4 - p0).abs() > thr || (q4 - q0).abs() > thr;
            flat2 = !m;
        }
        if filter_size == 4 || !flat {
            // Narrow filter.
            let half = 0x80 << sh;
            let lo = -(1 << (bd - 1));
            let hi = (1 << (bd - 1)) - 1;
            let c4 = |v: i32| v.clamp(lo, hi);
            let (ps1, ps0, qs0, qs1) = (p1 - half, p0 - half, q0 - half, q1 - half);
            let mut filter = if hev { c4(ps1 - qs1) } else { 0 };
            filter = c4(filter + 3 * (qs0 - ps0));
            let filter1 = c4(filter + 4) >> 3;
            let filter2 = c4(filter + 3) >> 3;
            let mut set = |k: isize, v: i32| f[(c + k * step) as usize] = v as u16;
            set(0, c4(qs0 - filter1) + half);
            set(-1, c4(ps0 + filter2) + half);
            if !hev {
                let fl = round2(filter1 as i64, 1) as i32;
                set(1, c4(qs1 - fl) + half);
                set(-2, c4(ps1 + fl) + half);
            }
        } else {
            let log2 = if filter_size == 8 || !flat2 { 3 } else { 4 };
            let n: isize = if log2 == 4 {
                6
            } else if plane == 0 {
                3
            } else {
                2
            };
            let n2: isize = if log2 == 3 && plane == 0 { 0 } else { 1 };
            let mut out = [0i32; 12];
            for i in -n..n {
                let mut t = 0i64;
                for j in -n..=n {
                    let p = (i + j).clamp(-(n + 1), n);
                    let tap = if j.abs() <= n2 { 2 } else { 1 };
                    t += g(p) as i64 * tap;
                }
                out[(i + n) as usize] = round2(t, log2) as i32;
            }
            for i in -n..n {
                f[(c + i * step) as usize] = out[(i + n) as usize] as u16;
            }
        }
    }

    // ---------------------------------------------------------------- CDEF

    fn cdef(&self) -> [Vec<u16>; 3] {
        let mut out = self.cur.clone();
        let mut r = 0;
        while r < self.mi_rows {
            let mut c = 0;
            while c < self.mi_cols {
                let idx = self.cdef_idx[(r >> 4) * self.cdef_cols + (c >> 4)];
                if idx >= 0 {
                    self.cdef_block(r, c, idx as usize, &mut out);
                }
                c += 2;
            }
            r += 2;
        }
        out
    }

    fn cdef_block(&self, r: usize, c: usize, idx: usize, out: &mut [Vec<u16>; 3]) {
        let s = |r: usize, c: usize| self.skips[self.mi(r, c)] == 1;
        let skip = s(r, c) && s(r + 1, c) && s(r, c + 1) && s(r + 1, c + 1);
        if skip {
            return;
        }
        let coeff_shift = self.bit_depth() as i32 - 8;
        let (y_dir, var) = self.cdef_direction(r, c);
        let pri = self.cdef_y_pri[idx] << coeff_shift;
        let sec = self.cdef_y_sec[idx] << coeff_shift;
        let dir = if pri == 0 { 0 } else { y_dir };
        let var_str = if (var >> 6) != 0 { (31 - ((var >> 6) as u32).leading_zeros()).min(12) as i32 } else { 0 };
        let pri = if var != 0 { (pri * (4 + var_str) + 8) >> 4 } else { 0 };
        let damping = self.cdef_damping + coeff_shift;
        self.cdef_filter(0, r, c, pri, sec, damping, dir, out);
        if self.num_planes == 1 {
            return;
        }
        let pri = self.cdef_uv_pri[idx] << coeff_shift;
        let sec = self.cdef_uv_sec[idx] << coeff_shift;
        let dir = if pri == 0 { 0 } else { CDEF_UV_DIR[(self.seq.sub_x * 2 + self.seq.sub_y) * 8 + y_dir] as usize };
        let damping = self.cdef_damping + coeff_shift - 1;
        self.cdef_filter(1, r, c, pri, sec, damping, dir, out);
        self.cdef_filter(2, r, c, pri, sec, damping, dir, out);
    }

    fn cdef_direction(&self, r: usize, c: usize) -> (usize, i32) {
        let mut cost = [0i64; 8];
        let mut partial = [[0i64; 15]; 8];
        let x0 = c * 4;
        let y0 = r * 4;
        let stride = self.stride[0];
        let sh = self.bit_depth() - 8;
        for i in 0..8 {
            for j in 0..8 {
                let x = (self.cur[0][(y0 + i) * stride + x0 + j] >> sh) as i64 - 128;
                partial[0][i + j] += x;
                partial[1][i + j / 2] += x;
                partial[2][i] += x;
                partial[3][3 + i - j / 2] += x;
                partial[4][7 + i - j] += x;
                partial[5][3 - i / 2 + j] += x;
                partial[6][j] += x;
                partial[7][i / 2 + j] += x;
            }
        }
        let dt = |i: usize| DIV_TABLE[i] as i64;
        for i in 0..8 {
            cost[2] += partial[2][i] * partial[2][i];
            cost[6] += partial[6][i] * partial[6][i];
        }
        cost[2] *= dt(8);
        cost[6] *= dt(8);
        for i in 0..7 {
            cost[0] += (partial[0][i] * partial[0][i] + partial[0][14 - i] * partial[0][14 - i]) * dt(i + 1);
            cost[4] += (partial[4][i] * partial[4][i] + partial[4][14 - i] * partial[4][14 - i]) * dt(i + 1);
        }
        cost[0] += partial[0][7] * partial[0][7] * dt(8);
        cost[4] += partial[4][7] * partial[4][7] * dt(8);
        for i in (1..8).step_by(2) {
            for j in 0..5 {
                cost[i] += partial[i][3 + j] * partial[i][3 + j];
            }
            cost[i] *= dt(8);
            for j in 0..3 {
                cost[i] += (partial[i][j] * partial[i][j] + partial[i][10 - j] * partial[i][10 - j]) * dt(2 * j + 2);
            }
        }
        let mut best = 0i64;
        let mut y_dir = 0;
        for (i, &cst) in cost.iter().enumerate() {
            if cst > best {
                best = cst;
                y_dir = i;
            }
        }
        let var = ((best - cost[(y_dir + 4) & 7]) >> 10) as i32;
        (y_dir, var)
    }

    #[allow(clippy::too_many_arguments)]
    fn cdef_filter(&self, plane: usize, r: usize, c: usize, pri: i32, sec: i32, damping: i32, dir: usize, out: &mut [Vec<u16>; 3]) {
        let coeff_shift = self.bit_depth() as i32 - 8;
        let (sx, sy) = self.sub(plane);
        let x0 = (c * 4) >> sx;
        let y0 = (r * 4) >> sy;
        let w = 8 >> sx;
        let h = 8 >> sy;
        let stride = self.stride[plane];
        let cur = &self.cur[plane];
        let constrain = |diff: i32, threshold: i32, damping: i32| -> i32 {
            if threshold == 0 {
                return 0;
            }
            let adj = (damping - (31 - (threshold as u32).leading_zeros()) as i32).max(0);
            let mag = (threshold - (diff.abs() >> adj)).clamp(0, diff.abs());
            if diff < 0 { -mag } else { mag }
        };
        let get = |i: isize, j: isize, d: usize, k: usize, sign: isize| -> Option<i32> {
            let yy = y0 as isize + i + sign * CDEF_DIRECTIONS[(d * 2 + k) * 2] as isize;
            let xx = x0 as isize + j + sign * CDEF_DIRECTIONS[(d * 2 + k) * 2 + 1] as isize;
            let cr = (yy << sy) >> 2;
            let cc = (xx << sx) >> 2;
            if cc >= 0 && cc < self.mi_cols as isize && cr >= 0 && cr < self.mi_rows as isize {
                Some(cur[yy as usize * stride + xx as usize] as i32)
            } else {
                None
            }
        };
        let pt = ((pri >> coeff_shift) & 1) as usize;
        for i in 0..h {
            for j in 0..w {
                let x = cur[(y0 + i) * stride + x0 + j] as i32;
                let mut sum = 0i32;
                let mut max = x;
                let mut min = x;
                for k in 0..2 {
                    for sign in [-1isize, 1] {
                        if let Some(p) = get(i as isize, j as isize, dir, k, sign) {
                            sum += CDEF_PRI_TAPS[pt * 2 + k] as i32 * constrain(p - x, pri, damping);
                            max = max.max(p);
                            min = min.min(p);
                        }
                        for off in [-2isize, 2] {
                            let d = ((dir as isize + off) & 7) as usize;
                            if let Some(s) = get(i as isize, j as isize, d, k, sign) {
                                sum += CDEF_SEC_TAPS[pt * 2 + k] as i32 * constrain(s - x, sec, damping);
                                max = max.max(s);
                                min = min.min(s);
                            }
                        }
                    }
                }
                let v = x + ((8 + sum - (sum < 0) as i32) >> 4);
                out[plane][(y0 + i) * stride + x0 + j] = v.clamp(min, max) as u16;
            }
        }
    }

    // ---------------------------------------------------- loop restoration

    fn loop_restoration(&self, cdef: &[Vec<u16>; 3]) -> [Vec<u16>; 3] {
        let mut out = cdef.clone();
        for plane in 0..self.num_planes {
            if self.frame_restoration_type[plane] == RESTORE_NONE {
                continue;
            }
            let mut y = 0;
            while y < self.frame_height {
                let mut x = 0;
                while x < self.upscaled_width {
                    self.lr_block(plane, y >> 2, x >> 2, cdef, &mut out);
                    x += 4;
                }
                y += 4;
            }
        }
        out
    }

    fn lr_block(&self, plane: usize, row: usize, col: usize, cdef: &[Vec<u16>; 3], out: &mut [Vec<u16>; 3]) {
        let luma_y = row * 4;
        let stripe = (luma_y + 8) / 64;
        let (sx, sy) = self.sub(plane);
        let stripe_start = ((stripe * 64) as isize - 8) >> sy;
        let stripe_end = stripe_start + (64 >> sy) - 1;
        let unit = self.loop_restoration_size[plane];
        let unit_rows = self.lr_unit_rows[plane];
        let unit_cols = self.lr_unit_cols[plane];
        let ur = (unit_rows - 1).min(((row * 4 + 8) >> sy) / unit);
        let uc = (unit_cols - 1).min(((col * 4) >> sx) / unit);
        let plane_end_x = ((self.upscaled_width + sx) >> sx) as isize - 1;
        let plane_end_y = ((self.frame_height + sy) >> sy) as isize - 1;
        let x = (col * 4) >> sx;
        let y = (row * 4) >> sy;
        let w = ((4 >> sx) as isize).min(plane_end_x - x as isize + 1);
        let h = ((4 >> sy) as isize).min(plane_end_y - y as isize + 1);
        if w <= 0 || h <= 0 {
            return;
        }
        let (w, h) = (w as usize, h as usize);
        let u = ur * unit_cols + uc;
        let stride = self.stride[plane];
        let cur = &self.cur[plane];
        let cdf = &cdef[plane];
        let src = |xx: isize, yy: isize| -> i32 {
            let xx = xx.clamp(0, plane_end_x);
            let mut yy = yy.clamp(0, plane_end_y);
            if yy < stripe_start {
                yy = yy.max(stripe_start - 2);
                cur[yy as usize * stride + xx as usize] as i32
            } else if yy > stripe_end {
                yy = yy.min(stripe_end + 2);
                cur[yy as usize * stride + xx as usize] as i32
            } else {
                cdf[yy as usize * stride + xx as usize] as i32
            }
        };
        let bd = self.bit_depth();
        let maxv = (1i64 << bd) - 1;
        match self.lr_type[plane][u] as usize {
            RESTORE_WIENER => {
                let round0: u32 = if bd == 12 { 5 } else { 3 };
                let round1: u32 = if bd == 12 { 9 } else { 11 };
                let coef = |c: &[i32; 3]| {
                    let mut f = [0i64; 7];
                    f[3] = 128;
                    for i in 0..3 {
                        f[i] = c[i] as i64;
                        f[6 - i] = c[i] as i64;
                        f[3] -= 2 * c[i] as i64;
                    }
                    f
                };
                let vf = coef(&self.lr_wiener[plane][u][0]);
                let hf = coef(&self.lr_wiener[plane][u][1]);
                let fb = FILTER_BITS as u32;
                let offset = 1i64 << (bd + fb - round0 - 1);
                let limit = (1i64 << (bd + 1 + fb - round0)) - 1;
                let mut inter = vec![0i64; (h + 6) * w];
                for r in 0..h + 6 {
                    for c in 0..w {
                        let mut s = 0i64;
                        for t in 0..7 {
                            s += hf[t] * src(x as isize + c as isize + t as isize - 3, y as isize + r as isize - 3) as i64;
                        }
                        let v = round2(s, round0);
                        inter[r * w + c] = v.clamp(-offset, limit - offset);
                    }
                }
                for r in 0..h {
                    for c in 0..w {
                        let mut s = 0i64;
                        for t in 0..7 {
                            s += vf[t] * inter[(r + t) * w + c];
                        }
                        let v = round2(s, round1);
                        out[plane][(y + r) * stride + x + c] = v.clamp(0, maxv) as u16;
                    }
                }
            }
            RESTORE_SGRPROJ => {
                let set = self.lr_sgr_set[plane][u] as usize;
                let flt = |pass: usize| -> Option<Vec<i64>> {
                    let r = SGR_PARAMS[set * 4 + pass * 2] as i64;
                    if r == 0 {
                        return None;
                    }
                    let eps = SGR_PARAMS[set * 4 + pass * 2 + 1] as i64;
                    let n = (2 * r + 1) * (2 * r + 1);
                    let n2e = n * n * eps;
                    let s = ((1i64 << SGRPROJ_MTABLE_BITS) + n2e / 2) / n2e;
                    let (aw, ah) = (w + 2, h + 2);
                    let mut a_arr = vec![0i64; aw * ah];
                    let mut b_arr = vec![0i64; aw * ah];
                    for i in -1..h as isize + 1 {
                        for j in -1..w as isize + 1 {
                            let mut a = 0i64;
                            let mut b = 0i64;
                            for dy in -r..=r {
                                for dx in -r..=r {
                                    let c = src(x as isize + j + dx as isize, y as isize + i + dy as isize) as i64;
                                    a += c * c;
                                    b += c;
                                }
                            }
                            let a = round2(a, 2 * (bd - 8));
                            let d = round2(b, bd - 8);
                            let p = (a * n - d * d).max(0);
                            let z = round2(p * s, SGRPROJ_MTABLE_BITS as u32);
                            let a2 = if z >= 255 {
                                256
                            } else if z == 0 {
                                1
                            } else {
                                ((z << SGRPROJ_SGR_BITS) + z / 2) / (z + 1)
                            };
                            let one_over_n = ((1i64 << SGRPROJ_RECIP_BITS) + n / 2) / n;
                            let b2 = ((1i64 << SGRPROJ_SGR_BITS) - a2) * b * one_over_n;
                            let k = ((i + 1) as usize) * aw + (j + 1) as usize;
                            a_arr[k] = a2;
                            b_arr[k] = round2(b2, SGRPROJ_RECIP_BITS as u32);
                        }
                    }
                    let mut f = vec![0i64; w * h];
                    for i in 0..h {
                        let shift = if pass == 0 && i & 1 == 1 { 4 } else { 5 };
                        for j in 0..w {
                            let mut a = 0i64;
                            let mut b = 0i64;
                            for dy in -1isize..=1 {
                                for dx in -1isize..=1 {
                                    let weight = if pass == 0 {
                                        if (i as isize + dy) & 1 != 0 {
                                            if dx == 0 { 6 } else { 5 }
                                        } else {
                                            0
                                        }
                                    } else if dx == 0 || dy == 0 {
                                        4
                                    } else {
                                        3
                                    };
                                    let k = ((i as isize + dy + 1) as usize) * aw + (j as isize + dx + 1) as usize;
                                    a += weight * a_arr[k];
                                    b += weight * b_arr[k];
                                }
                            }
                            let v = a * cdf[(y + i) * stride + x + j] as i64 + b;
                            f[i * w + j] = round2(v, SGRPROJ_SGR_BITS as u32 + shift - SGRPROJ_RST_BITS as u32);
                        }
                    }
                    Some(f)
                };
                let flt0 = flt(0);
                let flt1 = flt(1);
                let w0 = self.lr_sgr_xqd[plane][u][0] as i64;
                let w1 = self.lr_sgr_xqd[plane][u][1] as i64;
                let w2 = (1i64 << SGRPROJ_PRJ_BITS) - w0 - w1;
                for i in 0..h {
                    for j in 0..w {
                        let uu = (cdf[(y + i) * stride + x + j] as i64) << SGRPROJ_RST_BITS;
                        let mut v = w1 * uu;
                        v += w0 * flt0.as_ref().map_or(uu, |f| f[i * w + j]);
                        v += w2 * flt1.as_ref().map_or(uu, |f| f[i * w + j]);
                        let s = round2(v, (SGRPROJ_RST_BITS + SGRPROJ_PRJ_BITS) as u32);
                        out[plane][(y + i) * stride + x + j] = s.clamp(0, maxv) as u16;
                    }
                }
            }
            _ => {}
        }
    }
}
