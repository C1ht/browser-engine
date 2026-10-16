//! Intra block copy: reference MV stack (7.10.2, the parts reachable from
//! intra frames), MV reading, block copy prediction (7.11.3 with
//! integer-precision MVs and the bilinear filter), and the variable
//! transform-size tree that "inter" blocks use.

use super::predict::{round2, round2_signed};
use super::*;

impl Dec {
    fn add_ref_mv_candidate(&mut self, r: usize, c: usize, weight: u32) {
        let i = self.mi(r, c);
        if self.is_inters[i] == 0 {
            return;
        }
        // Intra block copy candidates reference INTRA_FRAME in list 0.
        let mut cand = self.mvs[i];
        for v in cand.iter_mut() {
            // lower_mv_precision with force_integer_mv.
            let a = v.abs();
            let ai = (a + 3) >> 3;
            *v = if *v > 0 { ai << 3 } else { -(ai << 3) };
        }
        self.found_match = true;
        for idx in 0..self.num_mv_found {
            if self.ref_stack[idx] == cand {
                self.weight_stack[idx] += weight;
                return;
            }
        }
        if self.num_mv_found < MAX_REF_MV_STACK_SIZE {
            self.ref_stack[self.num_mv_found] = cand;
            self.weight_stack[self.num_mv_found] = weight;
            self.num_mv_found += 1;
        }
    }

    fn scan_row(&mut self, delta_row: isize) {
        let bw = bw4(self.mi_size);
        let end4 = bw.min(self.mi_cols - self.mi_col).min(16);
        let mut delta_row = delta_row;
        let mut delta_col = 0isize;
        let use_step16 = bw >= 16;
        if delta_row.abs() > 1 {
            delta_row += (self.mi_row & 1) as isize;
            delta_col = 1 - (self.mi_col & 1) as isize;
        }
        let mut i = 0;
        while i < end4 {
            let r = self.mi_row as isize + delta_row;
            let c = self.mi_col as isize + delta_col + i as isize;
            if !self.is_inside(r, c) {
                break;
            }
            let (r, c) = (r as usize, c as usize);
            let mut len = bw.min(bw4(self.mi_sizes[self.mi(r, c)] as usize));
            if delta_row.abs() > 1 {
                len = len.max(2);
            }
            if use_step16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(r, c, len as u32 * 2);
            i += len;
        }
    }

    fn scan_col(&mut self, delta_col: isize) {
        let bh = bh4(self.mi_size);
        let end4 = bh.min(self.mi_rows - self.mi_row).min(16);
        let mut delta_row = 0isize;
        let mut delta_col = delta_col;
        let use_step16 = bh >= 16;
        if delta_col.abs() > 1 {
            delta_row = 1 - (self.mi_row & 1) as isize;
            delta_col += (self.mi_col & 1) as isize;
        }
        let mut i = 0;
        while i < end4 {
            let r = self.mi_row as isize + delta_row + i as isize;
            let c = self.mi_col as isize + delta_col;
            if !self.is_inside(r, c) {
                break;
            }
            let (r, c) = (r as usize, c as usize);
            let mut len = bh.min(bh4(self.mi_sizes[self.mi(r, c)] as usize));
            if delta_col.abs() > 1 {
                len = len.max(2);
            }
            if use_step16 {
                len = len.max(4);
            }
            self.add_ref_mv_candidate(r, c, len as u32 * 2);
            i += len;
        }
    }

    fn scan_point(&mut self, delta_row: isize, delta_col: isize) {
        let r = self.mi_row as isize + delta_row;
        let c = self.mi_col as isize + delta_col;
        if self.is_inside(r, c) && (c as usize) < self.mi_cols && self.written[self.mi(r as usize, c as usize)] {
            self.add_ref_mv_candidate(r as usize, c as usize, 4);
        }
    }

    fn sort_stack(&mut self, start: usize, end: usize) {
        let mut end = end;
        while end > start {
            let mut new_end = start;
            for idx in start + 1..end {
                if self.weight_stack[idx - 1] < self.weight_stack[idx] {
                    self.weight_stack.swap(idx - 1, idx);
                    self.ref_stack.swap(idx - 1, idx);
                    new_end = idx;
                }
            }
            end = new_end;
        }
    }

    /// find_mv_stack( 0 ) for an intra block copy block.
    pub(super) fn find_mv_stack(&mut self) {
        let bw = bw4(self.mi_size);
        let bh = bh4(self.mi_size);
        self.num_mv_found = 0;
        self.found_match = false;
        self.scan_row(-1);
        self.found_match = false;
        self.scan_col(-1);
        self.found_match = false;
        if bw.max(bh) <= 16 {
            self.scan_point(-1, bw as isize);
        }
        let num_nearest = self.num_mv_found;
        for idx in 0..num_nearest {
            self.weight_stack[idx] += REF_CAT_LEVEL as u32;
        }
        self.scan_point(-1, -1);
        self.scan_row(-3);
        self.scan_col(-3);
        if bh > 1 {
            self.scan_row(-5);
        }
        if bw > 1 {
            self.scan_col(-5);
        }
        self.sort_stack(0, num_nearest);
        self.sort_stack(num_nearest, self.num_mv_found);
        // Extra search adds nothing for intra references; pad with the (zero) global MV.
        for idx in self.num_mv_found..2 {
            self.ref_stack[idx] = [0, 0];
        }
        // Clamping.
        let bwp = (bw * 4) as i32;
        let bhp = (bh * 4) as i32;
        for idx in 0..self.num_mv_found {
            let mv = self.ref_stack[idx];
            self.ref_stack[idx] = [self.clamp_mv_row(mv[0], MV_BORDER as i32 + bhp * 8), self.clamp_mv_col(mv[1], MV_BORDER as i32 + bwp * 8)];
        }
    }

    fn clamp_mv_row(&self, v: i32, border: i32) -> i32 {
        let bh = bh4(self.mi_size) as i32;
        let top = -((self.mi_row as i32 * 4) * 8);
        let bottom = ((self.mi_rows as i32 - bh - self.mi_row as i32) * 4) * 8;
        v.clamp(top - border, bottom + border)
    }

    fn clamp_mv_col(&self, v: i32, border: i32) -> i32 {
        let bw = bw4(self.mi_size) as i32;
        let left = -((self.mi_col as i32 * 4) * 8);
        let right = ((self.mi_cols as i32 - bw - self.mi_col as i32) * 4) * 8;
        v.clamp(left - border, right + border)
    }

    fn read_mv_component(&mut self, comp: usize) -> i32 {
        let sign = sym!(self, mv_sign, comp * 3, 3);
        let class = sym!(self, mv_class, comp * 12, 12);
        let mag = if class == MV_CLASS_0 {
            let c0 = sym!(self, mv_class0_bit, comp * 3, 3) as i32;
            // force_integer_mv: fr = 3, hp = 1.
            ((c0 << 3) | (3 << 1) | 1) + 1
        } else {
            let mut d = 0i32;
            for i in 0..class {
                d |= (sym!(self, mv_bit, comp * 30 + i * 3, 3) as i32) << i;
            }
            ((CLASS0_SIZE as i32) << (class + 2)) + ((d << 3) | (3 << 1) | 1) + 1
        };
        if sign == 1 { -mag } else { mag }
    }

    /// assign_mv( 0 ) for intra block copy.
    pub(super) fn assign_mv(&mut self) {
        let mut pred = self.ref_stack[0];
        if pred == [0, 0] {
            pred = self.ref_stack[1];
        }
        if pred == [0, 0] {
            let sb = if self.seq.use_128 { BLOCK_128X128 } else { BLOCK_64X64 };
            let sb4 = bh4(sb) as i32;
            if (self.mi_row as i32) - sb4 < self.mi_row_start as i32 {
                pred = [0, -(sb4 * 4 + INTRABC_DELAY_PIXELS as i32) * 8];
            } else {
                pred = [-(sb4 * 4 * 8), 0];
            }
        }
        let joint = sym!(self, mv_joint, 0, 5);
        let mut diff = [0i32; 2];
        if joint == 2 || joint == 3 {
            diff[0] = self.read_mv_component(0);
        }
        if joint == 1 || joint == 3 {
            diff[1] = self.read_mv_component(1);
        }
        self.mv = [pred[0] + diff[0], pred[1] + diff[1]];
    }

    /// compute_prediction() for an intra block copy block: every plane is
    /// predicted as one block from the already decoded part of the frame.
    pub(super) fn predict_intrabc(&mut self) {
        for plane in 0..(1 + 2 * self.has_chroma as usize) {
            let plane_sz = self.plane_residual_size(self.mi_size, plane);
            let (sx, sy) = self.sub(plane);
            let x = (self.mi_col >> sx) * 4;
            let y = (self.mi_row >> sy) * 4;
            self.predict_inter(plane, x, y, bw4(plane_sz) * 4, bh4(plane_sz) * 4);
        }
    }

    fn predict_inter(&mut self, plane: usize, x: usize, y: usize, w: usize, h: usize) {
        let (sx, sy) = self.sub(plane);
        let mv = self.mv;
        let orig_x = ((x as i64) << SUBPEL_BITS) + ((2 * mv[1] as i64) >> sx) + 8;
        let orig_y = ((y as i64) << SUBPEL_BITS) + ((2 * mv[0] as i64) >> sy) + 8;
        let base_x = orig_x * (1 << REF_SCALE_SHIFT) - (8 << REF_SCALE_SHIFT);
        let base_y = orig_y * (1 << REF_SCALE_SHIFT) - (8 << REF_SCALE_SHIFT);
        let off = (1 << (SCALE_SUBPEL_BITS - SUBPEL_BITS)) / 2;
        let sh = (REF_SCALE_SHIFT + SUBPEL_BITS - SCALE_SUBPEL_BITS) as u32;
        let start_x = round2_signed(base_x, sh) + off;
        let start_y = round2_signed(base_y, sh) + off;
        let last_x = (((self.mi_cols * 4) + sx) >> sx) as i64 - 1;
        let last_y = (((self.mi_rows * 4) + sy) >> sy) as i64 - 1;
        let bd = self.bit_depth();
        let round0 = if bd == 12 { 5 } else { 3 };
        let round1 = if bd == 12 { 9 } else { 11 };
        let filt = |pos: i64, t: usize| SUBPEL_FILTERS[(BILINEAR * 16 + ((pos >> 6) & 15) as usize) * 8 + t] as i64;
        let ih = h + 8;
        let stride = self.stride[plane];
        let mut inter = vec![0i64; ih * w];
        {
            let f = &self.cur[plane];
            for r in 0..ih {
                let ry = ((start_y >> 10) + r as i64 - 3).clamp(0, last_y) as usize;
                for c in 0..w {
                    let p = start_x + 1024 * c as i64;
                    let mut s = 0i64;
                    for t in 0..8 {
                        let rx = ((p >> 10) + t as i64 - 3).clamp(0, last_x) as usize;
                        s += filt(p, t) * f[ry * stride + rx] as i64;
                    }
                    inter[r * w + c] = round2(s, round0);
                }
            }
        }
        let maxv = (1i64 << bd) - 1;
        let mut pred = vec![0u16; w * h];
        for r in 0..h {
            let p = (start_y & 1023) + 1024 * r as i64;
            for c in 0..w {
                let mut s = 0i64;
                for t in 0..8 {
                    s += filt(p, t) * inter[((p >> 10) as usize + t) * w + c];
                }
                pred[r * w + c] = round2(s, round1).clamp(0, maxv) as u16;
            }
        }
        let f = &mut self.cur[plane];
        for r in 0..h {
            f[(y + r) * stride + x..(y + r) * stride + x + w].copy_from_slice(&pred[r * w..r * w + w]);
        }
    }

    /// read_var_tx_size for "inter" (intra block copy) blocks.
    pub(super) fn read_var_tx_size(&mut self, row: usize, col: usize, tx: usize, depth: usize) {
        if row >= self.mi_rows || col >= self.mi_cols {
            return;
        }
        let split = if tx == TX_4X4 || depth == MAX_VARTX_DEPTH {
            false
        } else {
            let above = (self.var_above_tx_width(row, col) < tx_w(tx)) as usize;
            let left = (self.var_left_tx_height(row, col) < tx_h(tx)) as usize;
            let size = 64.min(block_width(self.mi_size).max(block_height(self.mi_size)));
            let max_tx = (0..TX_SIZES_ALL).find(|&t| tx_w(t) == size && tx_h(t) == size).unwrap_or(TX_64X64);
            let sqr_up = TX_SIZE_SQR_UP[tx] as usize;
            let ctx = (sqr_up != max_tx) as usize * 3 + (TX_SIZES - 1 - max_tx) * 6 + above + left;
            sym!(self, txfm_split, ctx * 3, 3) == 1
        };
        let w4 = tx_w(tx) / 4;
        let h4 = tx_h(tx) / 4;
        if split {
            let sub = SPLIT_TX_SIZE[tx] as usize;
            let sw = tx_w(sub) / 4;
            let shh = tx_h(sub) / 4;
            let mut i = 0;
            while i < h4 {
                let mut j = 0;
                while j < w4 {
                    self.read_var_tx_size(row + i, col + j, sub, depth + 1);
                    j += sw;
                }
                i += shh;
            }
        } else {
            for i in 0..h4 {
                for j in 0..w4 {
                    if row + i < self.mi_rows && col + j < self.mi_cols {
                        let k = self.mi(row + i, col + j);
                        self.inter_tx_sizes[k] = tx as u8;
                    }
                }
            }
            self.tx_size = tx;
        }
    }

    fn var_above_tx_width(&self, row: usize, col: usize) -> usize {
        if row == self.mi_row {
            if !self.avail_u {
                return 64;
            }
            let i = self.mi(row - 1, col);
            if self.skips[i] == 1 && self.is_inters[i] == 1 {
                return block_width(self.mi_sizes[i] as usize);
            }
        }
        tx_w(self.inter_tx_sizes[self.mi(row - 1, col)] as usize)
    }

    fn var_left_tx_height(&self, row: usize, col: usize) -> usize {
        if col == self.mi_col {
            if !self.avail_l {
                return 64;
            }
            let i = self.mi(row, col - 1);
            if self.skips[i] == 1 && self.is_inters[i] == 1 {
                return block_height(self.mi_sizes[i] as usize);
            }
        }
        tx_h(self.inter_tx_sizes[self.mi(row, col - 1)] as usize)
    }

    /// transform_tree for the luma residual of an "inter" block.
    pub(super) fn transform_tree(&mut self, start_x: usize, start_y: usize, w: usize, h: usize) -> Option<()> {
        if start_x >= self.mi_cols * 4 || start_y >= self.mi_rows * 4 {
            return Some(());
        }
        let luma = self.inter_tx_sizes[self.mi(start_y >> 2, start_x >> 2)] as usize;
        if w <= tx_w(luma) && h <= tx_h(luma) {
            let tx = (0..TX_SIZES_ALL).find(|&t| tx_w(t) == w && tx_h(t) == h)?;
            return self.transform_block(0, start_x, start_y, tx, 0, 0);
        }
        if w > h {
            self.transform_tree(start_x, start_y, w / 2, h)?;
            self.transform_tree(start_x + w / 2, start_y, w / 2, h)
        } else if w < h {
            self.transform_tree(start_x, start_y, w, h / 2)?;
            self.transform_tree(start_x, start_y + h / 2, w, h / 2)
        } else {
            self.transform_tree(start_x, start_y, w / 2, h / 2)?;
            self.transform_tree(start_x + w / 2, start_y, w / 2, h / 2)?;
            self.transform_tree(start_x, start_y + h / 2, w / 2, h / 2)?;
            self.transform_tree(start_x + w / 2, start_y + h / 2, w / 2, h / 2)
        }
    }
}
