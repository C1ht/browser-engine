//! Tile, partition and block syntax, including coefficient decoding.

use super::*;

pub(super) const TX_SET_DCTONLY: usize = 0;
const PARTITION_NONE: usize = 0;
const PARTITION_HORZ: usize = 1;
const PARTITION_VERT: usize = 2;
const PARTITION_SPLIT: usize = 3;
const PARTITION_HORZ_A: usize = 4;
const PARTITION_HORZ_B: usize = 5;
const PARTITION_VERT_A: usize = 6;
const PARTITION_VERT_B: usize = 7;
const PARTITION_HORZ_4: usize = 8;
const PARTITION_VERT_4: usize = 9;

fn ceil_log2(x: i32) -> u32 {
    if x < 2 { 0 } else { 32 - ((x - 1) as u32).leading_zeros() }
}

pub(super) fn get_tx_class(t: usize) -> usize {
    match t {
        V_DCT | V_ADST | V_FLIPADST => TX_CLASS_VERT,
        H_DCT | H_ADST | H_FLIPADST => TX_CLASS_HORIZ,
        _ => TX_CLASS_2D,
    }
}

pub(super) fn is_directional(mode: usize) -> bool {
    (V_PRED..=D67_PRED).contains(&mode)
}

impl Dec {
    /// Allocate frame-level arrays once the frame header is known.
    pub(super) fn setup_frame(&mut self) {
        let (mr, mc) = (self.mi_rows, self.mi_cols);
        for p in 0..3 {
            let (sx, sy) = if p == 0 { (0, 0) } else { (self.seq.sub_x, self.seq.sub_y) };
            let w = ((mc * 4) >> sx) + 160;
            let h = ((mr * 4) >> sy) + 160;
            self.stride[p] = w;
            self.cur[p] = if p < self.num_planes { vec![0u16; w * h] } else { Vec::new() };
            self.lf_tx_sizes[p] = vec![0; mr * mc];
            self.above_level[p] = vec![0; mc + 64];
            self.above_dc[p] = vec![0; mc + 64];
            self.left_level[p] = vec![0; mr + 64];
            self.left_dc[p] = vec![0; mr + 64];
            self.block_decoded[p] = vec![false; BD * BD];
        }
        let n = mr * mc;
        self.y_modes = vec![0; n];
        self.uv_modes = vec![0; n];
        self.skips = vec![0; n];
        self.is_inters = vec![0; n];
        self.mi_sizes = vec![0; n];
        self.segment_ids = vec![0; n];
        self.inter_tx_sizes = vec![0; n];
        self.tx_types = vec![0; n];
        self.palette_sizes = [vec![0; n], vec![0; n]];
        self.palette_colors = [vec![0; n * 8], vec![0; n * 8]];
        self.delta_lfs = vec![[0; 4]; n];
        self.mvs = vec![[0; 2]; n];
        self.written = vec![false; n];
        self.cdef_cols = (mc + 15) >> 4;
        self.cdef_idx = vec![-1; self.cdef_cols * ((mr + 15) >> 4)];
        for p in 0..self.num_planes {
            if self.frame_restoration_type[p] != RESTORE_NONE {
                let (sx, sy) = self.sub(p);
                let unit = self.loop_restoration_size[p];
                let count = |size: usize| ((size + (unit >> 1)) / unit).max(1);
                self.lr_unit_rows[p] = count((self.frame_height + sy) >> sy);
                self.lr_unit_cols[p] = count((self.upscaled_width + sx) >> sx);
                let units = self.lr_unit_rows[p] * self.lr_unit_cols[p];
                self.lr_type[p] = vec![0; units];
                self.lr_wiener[p] = vec![[[0; 3]; 2]; units];
                self.lr_sgr_set[p] = vec![0; units];
                self.lr_sgr_xqd[p] = vec![[0; 2]; units];
            }
        }
        self.color_map_y = vec![0; CMAP * CMAP];
        self.color_map_uv = vec![0; CMAP * CMAP];
        self.quant = vec![0; 1024];
    }

    /// Decode a tile group; returns true once the last tile of the frame is done.
    pub(super) fn tile_group(&mut self, data: &[u8], _tile_num: &mut usize) -> Option<bool> {
        let tile_cols = self.mi_col_starts.len() - 1;
        let tile_rows = self.mi_row_starts.len() - 1;
        let num_tiles = tile_cols * tile_rows;
        let mut b = Bits::new(data);
        let (mut tg_start, mut tg_end) = (0, num_tiles - 1);
        if num_tiles > 1 && b.f(1) == 1 {
            let bits = self.tile_cols_log2 + self.tile_rows_log2;
            tg_start = b.f(bits) as usize;
            tg_end = b.f(bits) as usize;
        }
        b.byte_align();
        let mut pos = b.pos / 8;
        for tn in tg_start..=tg_end {
            let (row, col) = (tn / tile_cols, tn % tile_cols);
            let size = if tn == tg_end {
                data.len().checked_sub(pos)?
            } else {
                let mut v = 0usize;
                for i in 0..self.tile_size_bytes {
                    v |= (*data.get(pos + i)? as usize) << (8 * i);
                }
                pos += self.tile_size_bytes;
                v + 1
            };
            let tile = data.get(pos..pos + size)?.to_vec();
            pos += size;
            self.mi_row_start = self.mi_row_starts[row];
            self.mi_row_end = self.mi_row_starts[row + 1];
            self.mi_col_start = self.mi_col_starts[col];
            self.mi_col_end = self.mi_col_starts[col + 1];
            self.current_q_index = self.base_q_idx;
            self.cdf = Cdfs::new(self.base_q_idx as u32);
            self.sd = Sym::new(tile, !self.disable_cdf_update);
            self.decode_tile()?;
        }
        Some(tg_end == num_tiles - 1)
    }

    fn decode_tile(&mut self) -> Option<()> {
        for p in 0..3 {
            self.above_level[p].fill(0);
            self.above_dc[p].fill(0);
        }
        self.delta_lf = [0; 4];
        for p in 0..self.num_planes {
            self.ref_sgr_xqd[p] = [-32, 31];
            self.ref_lr_wiener[p] = [[3, -7, 15]; 2];
        }
        let sb_size = if self.seq.use_128 { BLOCK_128X128 } else { BLOCK_64X64 };
        let sb4 = bw4(sb_size);
        let mut r = self.mi_row_start;
        while r < self.mi_row_end {
            for p in 0..3 {
                self.left_level[p].fill(0);
                self.left_dc[p].fill(0);
            }
            let mut c = self.mi_col_start;
            while c < self.mi_col_end {
                self.read_deltas = self.delta_q_present;
                self.clear_cdef(r, c);
                self.clear_block_decoded_flags(r, c, sb4);
                self.read_lr(r, c, sb_size);
                self.decode_partition(r, c, sb_size)?;
                c += sb4;
            }
            r += sb4;
        }
        Some(())
    }

    fn cdef_slot(&self, r: usize, c: usize) -> Option<usize> {
        let (rr, cc) = (r >> 4, c >> 4);
        (cc < self.cdef_cols && rr < (self.mi_rows + 15) >> 4).then(|| rr * self.cdef_cols + cc)
    }

    fn clear_cdef(&mut self, r: usize, c: usize) {
        let mut spots = vec![(r, c)];
        if self.seq.use_128 {
            spots.extend([(r, c + 16), (r + 16, c), (r + 16, c + 16)]);
        }
        for (r, c) in spots {
            if let Some(i) = self.cdef_slot(r, c) {
                self.cdef_idx[i] = -1;
            }
        }
    }

    fn clear_block_decoded_flags(&mut self, r: usize, c: usize, sb4: usize) {
        for p in 0..self.num_planes {
            let (sx, sy) = self.sub(p);
            let sbw4 = ((self.mi_col_end - c) >> sx) as isize;
            let sbh4 = ((self.mi_row_end - r) >> sy) as isize;
            let bd = &mut self.block_decoded[p];
            for y in -1..=((sb4 >> sy) as isize) {
                for x in -1..=((sb4 >> sx) as isize) {
                    let v = if y < 0 && x < sbw4 {
                        true
                    } else {
                        x < 0 && y < sbh4
                    };
                    bd[Dec::bd_idx(y, x)] = v;
                }
            }
            bd[Dec::bd_idx((sb4 >> sy) as isize, -1)] = false;
        }
    }

    // ----------------------------------------------------------- partition

    fn partition_cdf_off(&self, bsize: usize, r: usize, c: usize) -> (usize, usize) {
        let bsl = MI_WIDTH_LOG2[bsize] as usize;
        let above = self.avail_u && (MI_WIDTH_LOG2[self.mi_sizes[self.mi(r - 1, c)] as usize] as usize) < bsl;
        let left = self.avail_l && (MI_HEIGHT_LOG2[self.mi_sizes[self.mi(r, c - 1)] as usize] as usize) < bsl;
        let ctx = left as usize * 2 + above as usize;
        (bsl, ctx)
    }

    fn partition_cdf(&mut self, bsl: usize, ctx: usize) -> &mut [u16] {
        match bsl {
            1 => &mut self.cdf.partition_w8[ctx * 5..ctx * 5 + 5],
            2 => &mut self.cdf.partition_w16[ctx * 11..ctx * 11 + 11],
            3 => &mut self.cdf.partition_w32[ctx * 11..ctx * 11 + 11],
            4 => &mut self.cdf.partition_w64[ctx * 11..ctx * 11 + 11],
            _ => &mut self.cdf.partition_w128[ctx * 9..ctx * 9 + 9],
        }
    }

    fn decode_partition(&mut self, r: usize, c: usize, bsize: usize) -> Option<()> {
        if r >= self.mi_rows || c >= self.mi_cols {
            return Some(());
        }
        self.avail_u = self.is_inside(r as isize - 1, c as isize);
        self.avail_l = self.is_inside(r as isize, c as isize - 1);
        let num4x4 = bw4(bsize);
        let half = num4x4 >> 1;
        let quarter = half >> 1;
        let has_rows = (r + half) < self.mi_rows;
        let has_cols = (c + half) < self.mi_cols;
        let partition = if bsize < BLOCK_8X8 {
            PARTITION_NONE
        } else if has_rows && has_cols {
            let (bsl, ctx) = self.partition_cdf_off(bsize, r, c);
            let mut tmp = std::mem::take(&mut self.sd);
            let p = tmp.read(self.partition_cdf(bsl, ctx));
            self.sd = tmp;
            p
        } else if has_cols || has_rows {
            let (bsl, ctx) = self.partition_cdf_off(bsize, r, c);
            let pc: Vec<u32> = self.partition_cdf(bsl, ctx).iter().map(|&v| v as u32).collect();
            let d = |i: usize| pc[i] - if i > 0 { pc[i - 1] } else { 0 };
            let mut psum;
            if has_cols {
                psum = d(PARTITION_VERT) + d(PARTITION_SPLIT) + d(PARTITION_HORZ_A) + d(PARTITION_VERT_A) + d(PARTITION_VERT_B);
                if bsize != BLOCK_128X128 {
                    psum += d(PARTITION_VERT_4);
                }
            } else {
                psum = d(PARTITION_HORZ) + d(PARTITION_SPLIT) + d(PARTITION_HORZ_A) + d(PARTITION_HORZ_B) + d(PARTITION_VERT_A);
                if bsize != BLOCK_128X128 {
                    psum += d(PARTITION_HORZ_4);
                }
            }
            let mut cdf = [((1u32 << 15) - psum) as u16, 1 << 15, 0];
            let save = self.sd.update;
            self.sd.update = false;
            let s = self.sd.read(&mut cdf);
            self.sd.update = save;
            if s == 1 {
                PARTITION_SPLIT
            } else if has_cols {
                PARTITION_HORZ
            } else {
                PARTITION_VERT
            }
        } else {
            PARTITION_SPLIT
        };
        let sub = PARTITION_SUBSIZE[partition * BLOCK_SIZES + bsize] as usize;
        let split = PARTITION_SUBSIZE[PARTITION_SPLIT * BLOCK_SIZES + bsize] as usize;
        match partition {
            PARTITION_NONE => self.decode_block(r, c, sub)?,
            PARTITION_HORZ => {
                self.decode_block(r, c, sub)?;
                if has_rows {
                    self.decode_block(r + half, c, sub)?;
                }
            }
            PARTITION_VERT => {
                self.decode_block(r, c, sub)?;
                if has_cols {
                    self.decode_block(r, c + half, sub)?;
                }
            }
            PARTITION_SPLIT => {
                self.decode_partition(r, c, sub)?;
                self.decode_partition(r, c + half, sub)?;
                self.decode_partition(r + half, c, sub)?;
                self.decode_partition(r + half, c + half, sub)?;
            }
            PARTITION_HORZ_A => {
                self.decode_block(r, c, split)?;
                self.decode_block(r, c + half, split)?;
                self.decode_block(r + half, c, sub)?;
            }
            PARTITION_HORZ_B => {
                self.decode_block(r, c, sub)?;
                self.decode_block(r + half, c, split)?;
                self.decode_block(r + half, c + half, split)?;
            }
            PARTITION_VERT_A => {
                self.decode_block(r, c, split)?;
                self.decode_block(r + half, c, split)?;
                self.decode_block(r, c + half, sub)?;
            }
            PARTITION_VERT_B => {
                self.decode_block(r, c, sub)?;
                self.decode_block(r, c + half, split)?;
                self.decode_block(r + half, c + half, split)?;
            }
            PARTITION_HORZ_4 => {
                for i in 0..4 {
                    if i < 3 || r + quarter * 3 < self.mi_rows {
                        self.decode_block(r + quarter * i, c, sub)?;
                    }
                }
            }
            _ => {
                for i in 0..4 {
                    if i < 3 || c + quarter * 3 < self.mi_cols {
                        self.decode_block(r, c + quarter * i, sub)?;
                    }
                }
            }
        }
        Some(())
    }

    // --------------------------------------------------------------- block

    fn decode_block(&mut self, r: usize, c: usize, sub: usize) -> Option<()> {
        self.mi_row = r;
        self.mi_col = c;
        self.mi_size = sub;
        let (bw, bh) = (bw4(sub), bh4(sub));
        let (sx, sy) = (self.seq.sub_x, self.seq.sub_y);
        self.has_chroma = if bh == 1 && sy == 1 && (r & 1) == 0 {
            false
        } else if bw == 1 && sx == 1 && (c & 1) == 0 {
            false
        } else {
            self.num_planes > 1
        };
        let (ri, ci) = (r as isize, c as isize);
        self.avail_u = self.is_inside(ri - 1, ci);
        self.avail_l = self.is_inside(ri, ci - 1);
        self.avail_u_chroma = self.avail_u;
        self.avail_l_chroma = self.avail_l;
        if self.has_chroma {
            if sy == 1 && bh == 1 {
                self.avail_u_chroma = self.is_inside(ri - 2, ci);
            }
            if sx == 1 && bw == 1 {
                self.avail_l_chroma = self.is_inside(ri, ci - 2);
            }
        } else {
            self.avail_u_chroma = false;
            self.avail_l_chroma = false;
        }
        self.intra_frame_mode_info()?;
        self.palette_tokens();
        self.read_block_tx_size();
        if self.skip {
            self.reset_block_context(bw, bh);
        }
        let rows = bh.min(self.mi_rows - r);
        let cols = bw.min(self.mi_cols - c);
        for y in 0..rows {
            for x in 0..cols {
                let i = self.mi(r + y, c + x);
                self.y_modes[i] = self.y_mode as u8;
                if self.has_chroma {
                    self.uv_modes[i] = self.uv_mode as u8;
                }
            }
        }
        if self.is_inter {
            for y in 0..rows {
                for x in 0..cols {
                    let i = self.mi(r + y, c + x);
                    self.is_inters[i] = 1;
                    self.mvs[i] = self.mv;
                }
            }
            self.predict_intrabc();
        }
        self.residual()?;
        for y in 0..rows {
            for x in 0..cols {
                let i = self.mi(r + y, c + x);
                self.is_inters[i] = self.is_inter as u8;
                self.mvs[i] = self.mv;
                self.written[i] = true;
                self.skips[i] = self.skip as u8;
                self.mi_sizes[i] = sub as u8;
                self.segment_ids[i] = self.segment_id as u8;
                self.palette_sizes[0][i] = self.palette_size_y as u8;
                self.palette_sizes[1][i] = self.palette_size_uv as u8;
                self.palette_colors[0][i * 8..i * 8 + 8].copy_from_slice(&self.palette_colors_y);
                self.palette_colors[1][i * 8..i * 8 + 8].copy_from_slice(&self.palette_colors_u);
                self.delta_lfs[i] = self.delta_lf.map(|v| v as i8);
            }
        }
        Some(())
    }

    fn reset_block_context(&mut self, bw: usize, bh: usize) {
        for p in 0..(1 + 2 * self.has_chroma as usize) {
            let (sx, sy) = self.sub(p);
            for i in (self.mi_col >> sx)..((self.mi_col + bw) >> sx) {
                self.above_level[p][i] = 0;
                self.above_dc[p][i] = 0;
            }
            for i in (self.mi_row >> sy)..((self.mi_row + bh) >> sy) {
                self.left_level[p][i] = 0;
                self.left_dc[p][i] = 0;
            }
        }
    }

    fn intra_frame_mode_info(&mut self) -> Option<()> {
        self.skip = false;
        if self.seg_id_pre_skip {
            self.intra_segment_id();
        }
        self.read_skip();
        if !self.seg_id_pre_skip {
            self.intra_segment_id();
        }
        self.read_cdef();
        self.read_delta_qindex();
        self.read_delta_lf();
        self.read_deltas = false;
        self.use_intrabc = self.allow_intrabc && sym!(self, intrabc, 0, 3) == 1;
        self.is_inter = self.use_intrabc;
        if self.use_intrabc {
            self.y_mode = DC_PRED;
            self.uv_mode = DC_PRED;
            self.angle_delta_y = 0;
            self.angle_delta_uv = 0;
            self.cfl_alpha_u = 0;
            self.cfl_alpha_v = 0;
            self.palette_size_y = 0;
            self.palette_size_uv = 0;
            self.use_filter_intra = false;
            self.find_mv_stack();
            self.assign_mv();
            return Some(());
        }
        let above = INTRA_MODE_CONTEXT[if self.avail_u { self.y_modes[self.mi(self.mi_row - 1, self.mi_col)] as usize } else { DC_PRED }] as usize;
        let left = INTRA_MODE_CONTEXT[if self.avail_l { self.y_modes[self.mi(self.mi_row, self.mi_col - 1)] as usize } else { DC_PRED }] as usize;
        self.y_mode = sym!(self, intra_frame_y_mode, (above * 5 + left) * 14, 14);
        self.angle_delta_y = 0;
        if self.mi_size >= BLOCK_8X8 && is_directional(self.y_mode) {
            self.angle_delta_y = sym!(self, angle_delta, (self.y_mode - V_PRED) * 8, 8) as i32 - MAX_ANGLE_DELTA as i32;
        }
        self.uv_mode = DC_PRED;
        self.angle_delta_uv = 0;
        self.cfl_alpha_u = 0;
        self.cfl_alpha_v = 0;
        if self.has_chroma {
            let bw = block_width(self.mi_size);
            let bh = block_height(self.mi_size);
            let cfl_allowed = if self.lossless && self.plane_residual_size(self.mi_size, 1) == BLOCK_4X4 {
                true
            } else {
                !self.lossless && bw.max(bh) <= 32
            };
            self.uv_mode = if cfl_allowed {
                sym!(self, uv_mode_cfl_allowed, self.y_mode * 15, 15)
            } else {
                sym!(self, uv_mode_cfl_not_allowed, self.y_mode * 14, 14)
            };
            if self.uv_mode == UV_CFL_PRED {
                self.read_cfl_alphas();
            }
            if self.mi_size >= BLOCK_8X8 && is_directional(self.uv_mode) {
                self.angle_delta_uv = sym!(self, angle_delta, (self.uv_mode - V_PRED) * 8, 8) as i32 - MAX_ANGLE_DELTA as i32;
            }
        }
        self.palette_size_y = 0;
        self.palette_size_uv = 0;
        self.palette_colors_y = [0; 8];
        self.palette_colors_u = [0; 8];
        if self.mi_size >= BLOCK_8X8 && block_width(self.mi_size) <= 64 && block_height(self.mi_size) <= 64 && self.allow_screen_content_tools {
            self.palette_mode_info();
        }
        // filter_intra_mode_info()
        self.use_filter_intra = false;
        if self.seq.enable_filter_intra && self.y_mode == DC_PRED && self.palette_size_y == 0 && block_width(self.mi_size).max(block_height(self.mi_size)) <= 32 {
            self.use_filter_intra = sym!(self, filter_intra, self.mi_size * 3, 3) == 1;
            if self.use_filter_intra {
                self.filter_intra_mode = sym!(self, filter_intra_mode, 0, 6);
            }
        }
        Some(())
    }

    pub(super) fn plane_residual_size(&self, subsize: usize, plane: usize) -> usize {
        let (sx, sy) = self.sub(plane);
        SUBSAMPLED_SIZE[(subsize * 2 + sx) * 2 + sy] as usize
    }

    fn intra_segment_id(&mut self) {
        if self.segmentation_enabled {
            self.read_segment_id();
        } else {
            self.segment_id = 0;
        }
        self.lossless = self.lossless_array[self.segment_id];
    }

    fn read_segment_id(&mut self) {
        let (r, c) = (self.mi_row, self.mi_col);
        let seg = |d: &Dec, r: usize, c: usize| d.segment_ids[d.mi(r, c)] as i32;
        let prev_ul = if self.avail_u && self.avail_l { seg(self, r - 1, c - 1) } else { -1 };
        let prev_u = if self.avail_u { seg(self, r - 1, c) } else { -1 };
        let prev_l = if self.avail_l { seg(self, r, c - 1) } else { -1 };
        let pred = if prev_u == -1 {
            if prev_l == -1 { 0 } else { prev_l }
        } else if prev_l == -1 {
            prev_u
        } else if prev_ul == prev_u {
            prev_u
        } else {
            prev_l
        };
        if self.skip {
            self.segment_id = pred as usize;
            return;
        }
        let ctx = if prev_ul < 0 {
            0
        } else if prev_ul == prev_u && prev_ul == prev_l {
            2
        } else if prev_ul == prev_u || prev_ul == prev_l || prev_u == prev_l {
            1
        } else {
            0
        };
        let diff = sym!(self, segment_id, ctx * 9, 9) as i32;
        let max = self.last_active_seg_id as i32 + 1;
        let r = pred;
        let v = if r == 0 {
            diff
        } else if r >= max - 1 {
            max - diff - 1
        } else if 2 * r < max {
            if diff <= 2 * r {
                if diff & 1 == 1 { r + ((diff + 1) >> 1) } else { r - (diff >> 1) }
            } else {
                diff
            }
        } else if diff <= 2 * (max - r - 1) {
            if diff & 1 == 1 { r + ((diff + 1) >> 1) } else { r - (diff >> 1) }
        } else {
            max - (diff + 1)
        };
        self.segment_id = v.clamp(0, 7) as usize;
    }

    fn read_skip(&mut self) {
        if self.seg_id_pre_skip && self.seg_feature_active_idx(self.segment_id, SEG_LVL_SKIP) {
            self.skip = true;
            return;
        }
        let mut ctx = 0;
        if self.avail_u {
            ctx += self.skips[self.mi(self.mi_row - 1, self.mi_col)] as usize;
        }
        if self.avail_l {
            ctx += self.skips[self.mi(self.mi_row, self.mi_col - 1)] as usize;
        }
        self.skip = sym!(self, skip, ctx * 3, 3) == 1;
    }

    fn read_cdef(&mut self) {
        if self.skip || self.coded_lossless || !self.seq.enable_cdef || self.allow_intrabc {
            return;
        }
        let r = self.mi_row & !15;
        let c = self.mi_col & !15;
        let Some(slot) = self.cdef_slot(r, c) else { return };
        if self.cdef_idx[slot] == -1 {
            let v = self.sd.lit(self.cdef_bits) as i8;
            let (w4, h4) = (bw4(self.mi_size), bh4(self.mi_size));
            let mut i = r;
            while i < r + h4 {
                let mut j = c;
                while j < c + w4 {
                    if let Some(s) = self.cdef_slot(i, j) {
                        self.cdef_idx[s] = v;
                    }
                    j += 16;
                }
                i += 16;
            }
        }
    }

    fn read_delta_qindex(&mut self) {
        let sb_size = if self.seq.use_128 { BLOCK_128X128 } else { BLOCK_64X64 };
        if self.mi_size == sb_size && self.skip {
            return;
        }
        if self.read_deltas {
            let mut abs = sym!(self, delta_q, 0, 5) as i32;
            if abs == DELTA_Q_SMALL as i32 {
                let rem = self.sd.lit(3) + 1;
                abs = self.sd.lit(rem) as i32 + (1 << rem) + 1;
            }
            if abs != 0 {
                let sign = self.sd.lit(1);
                let reduced = if sign == 1 { -abs } else { abs };
                self.current_q_index = (self.current_q_index + (reduced << self.delta_q_res)).clamp(1, 255);
            }
        }
    }

    fn read_delta_lf(&mut self) {
        let sb_size = if self.seq.use_128 { BLOCK_128X128 } else { BLOCK_64X64 };
        if self.mi_size == sb_size && self.skip {
            return;
        }
        if self.read_deltas && self.delta_lf_present {
            let count = if self.delta_lf_multi { if self.num_planes > 1 { FRAME_LF_COUNT } else { FRAME_LF_COUNT - 2 } } else { 1 };
            for i in 0..count {
                let a = if self.delta_lf_multi { sym!(self, delta_lf_multi, i * 5, 5) } else { sym!(self, delta_lf, 0, 5) } as i32;
                let abs = if a == DELTA_LF_SMALL as i32 {
                    let n = self.sd.lit(3) + 1;
                    self.sd.lit(n) as i32 + (1 << n) + 1
                } else {
                    a
                };
                if abs != 0 {
                    let sign = self.sd.lit(1);
                    let reduced = if sign == 1 { -abs } else { abs };
                    let m = MAX_LOOP_FILTER as i32;
                    self.delta_lf[i] = (self.delta_lf[i] + (reduced << self.delta_lf_res)).clamp(-m, m);
                }
            }
        }
    }

    fn read_cfl_alphas(&mut self) {
        let signs = sym!(self, cfl_sign, 0, 9);
        let sign_u = (signs + 1) / 3;
        let sign_v = (signs + 1) % 3;
        self.cfl_alpha_u = 0;
        self.cfl_alpha_v = 0;
        if sign_u != CFL_SIGN_ZERO {
            let ctx = (sign_u - 1) * 3 + sign_v;
            let a = 1 + sym!(self, cfl_alpha, ctx * 17, 17) as i32;
            self.cfl_alpha_u = if sign_u == CFL_SIGN_NEG { -a } else { a };
        }
        if sign_v != CFL_SIGN_ZERO {
            let ctx = (sign_v - 1) * 3 + sign_u;
            let a = 1 + sym!(self, cfl_alpha, ctx * 17, 17) as i32;
            self.cfl_alpha_v = if sign_v == CFL_SIGN_NEG { -a } else { a };
        }
    }

    // ------------------------------------------------------------- palette

    fn palette_cache(&self, plane: usize) -> Vec<u16> {
        let (r, c) = (self.mi_row, self.mi_col);
        let above_n = if (r * 4) % 64 != 0 && self.avail_u { self.palette_sizes[plane][self.mi(r - 1, c)] as usize } else { 0 };
        let left_n = if self.avail_l { self.palette_sizes[plane][self.mi(r, c - 1)] as usize } else { 0 };
        let above: &[u16] = if above_n > 0 { &self.palette_colors[plane][self.mi(r - 1, c) * 8..][..above_n] } else { &[] };
        let left: &[u16] = if left_n > 0 { &self.palette_colors[plane][self.mi(r, c - 1) * 8..][..left_n] } else { &[] };
        let mut all: Vec<u16> = above.iter().chain(left).copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    fn read_palette_colors(&mut self, n: usize, plane: usize) -> [u16; 8] {
        let bd = self.bit_depth();
        let cache = self.palette_cache(plane);
        let mut colors = [0u16; 8];
        let mut idx = 0;
        for &c in &cache {
            if idx >= n {
                break;
            }
            if self.sd.lit(1) == 1 {
                colors[idx] = c;
                idx += 1;
            }
        }
        if idx < n {
            colors[idx] = self.sd.lit(bd) as u16;
            idx += 1;
        }
        let mut palette_bits = 0;
        if idx < n {
            let min_bits = bd - 3;
            palette_bits = min_bits + self.sd.lit(2);
        }
        while idx < n {
            let mut delta = self.sd.lit(palette_bits) as i32;
            if plane == 0 {
                delta += 1;
            }
            colors[idx] = self.clip1(colors[idx - 1] as i32 + delta);
            let range = (1 << bd) - colors[idx] as i32 - if plane == 0 { 1 } else { 0 };
            palette_bits = palette_bits.min(ceil_log2(range));
            idx += 1;
        }
        colors[..n].sort_unstable();
        colors
    }

    fn palette_mode_info(&mut self) {
        let bsize_ctx = MI_WIDTH_LOG2[self.mi_size] as usize + MI_HEIGHT_LOG2[self.mi_size] as usize - 2;
        if self.y_mode == DC_PRED {
            let mut ctx = 0;
            if self.avail_u && self.palette_sizes[0][self.mi(self.mi_row - 1, self.mi_col)] > 0 {
                ctx += 1;
            }
            if self.avail_l && self.palette_sizes[0][self.mi(self.mi_row, self.mi_col - 1)] > 0 {
                ctx += 1;
            }
            if sym!(self, palette_y_mode, (bsize_ctx * 3 + ctx) * 3, 3) == 1 {
                self.palette_size_y = sym!(self, palette_y_size, bsize_ctx * 8, 8) + 2;
                self.palette_colors_y = self.read_palette_colors(self.palette_size_y, 0);
            }
        }
        if self.has_chroma && self.uv_mode == DC_PRED {
            let ctx = (self.palette_size_y > 0) as usize;
            if sym!(self, palette_uv_mode, ctx * 3, 3) == 1 {
                self.palette_size_uv = sym!(self, palette_uv_size, bsize_ctx * 8, 8) + 2;
                let n = self.palette_size_uv;
                self.palette_colors_u = self.read_palette_colors(n, 1);
                let bd = self.bit_depth();
                if self.sd.lit(1) == 1 {
                    let min_bits = bd - 4;
                    let max_val = 1i32 << bd;
                    let bits = min_bits + self.sd.lit(2);
                    self.palette_colors_v[0] = self.sd.lit(bd) as u16;
                    for idx in 1..n {
                        let mut delta = self.sd.lit(bits) as i32;
                        if delta != 0 && self.sd.lit(1) == 1 {
                            delta = -delta;
                        }
                        let mut val = self.palette_colors_v[idx - 1] as i32 + delta;
                        if val < 0 {
                            val += max_val;
                        }
                        if val >= max_val {
                            val -= max_val;
                        }
                        self.palette_colors_v[idx] = self.clip1(val);
                    }
                } else {
                    for idx in 0..n {
                        self.palette_colors_v[idx] = self.sd.lit(bd) as u16;
                    }
                }
            }
        }
    }

    fn palette_color_context(map: &[u8], r: usize, c: usize, n: usize) -> (usize, [u8; 8]) {
        let mut scores = [0i32; 8];
        let mut order = [0u8, 1, 2, 3, 4, 5, 6, 7];
        if c > 0 {
            scores[map[r * CMAP + c - 1] as usize] += 2;
        }
        if r > 0 && c > 0 {
            scores[map[(r - 1) * CMAP + c - 1] as usize] += 1;
        }
        if r > 0 {
            scores[map[(r - 1) * CMAP + c] as usize] += 2;
        }
        for i in 0..PALETTE_NUM_NEIGHBORS {
            let mut max_score = scores[i];
            let mut max_idx = i;
            for j in i + 1..n {
                if scores[j] > max_score {
                    max_score = scores[j];
                    max_idx = j;
                }
            }
            if max_idx != i {
                let ms = scores[max_idx];
                let mo = order[max_idx];
                for k in (i + 1..=max_idx).rev() {
                    scores[k] = scores[k - 1];
                    order[k] = order[k - 1];
                }
                scores[i] = ms;
                order[i] = mo;
            }
        }
        let mut hash = 0;
        for i in 0..PALETTE_NUM_NEIGHBORS {
            hash += scores[i] * PALETTE_COLOR_HASH_MULTIPLIERS[i] as i32;
        }
        (PALETTE_COLOR_CONTEXT[hash as usize] as usize, order)
    }

    fn read_color_map(&mut self, plane: usize, n: usize, bw: usize, bh: usize, onw: usize, onh: usize) {
        let mut map = std::mem::take(if plane == 0 { &mut self.color_map_y } else { &mut self.color_map_uv });
        map[0] = self.sd.ns(n as u32) as u8;
        for i in 1..onh + onw - 1 {
            let jmax = i.min(onw - 1);
            let jmin = i.saturating_sub(onh - 1);
            let mut j = jmax as isize;
            while j >= jmin as isize {
                let ju = j as usize;
                let (ctx, order) = Dec::palette_color_context(&map, i - ju, ju, n);
                let off = ctx * (n + 1);
                let s = match (plane, n) {
                    (0, 2) => sym!(self, pal2y, off, 3),
                    (0, 3) => sym!(self, pal3y, off, 4),
                    (0, 4) => sym!(self, pal4y, off, 5),
                    (0, 5) => sym!(self, pal5y, off, 6),
                    (0, 6) => sym!(self, pal6y, off, 7),
                    (0, 7) => sym!(self, pal7y, off, 8),
                    (0, _) => sym!(self, pal8y, off, 9),
                    (_, 2) => sym!(self, pal2uv, off, 3),
                    (_, 3) => sym!(self, pal3uv, off, 4),
                    (_, 4) => sym!(self, pal4uv, off, 5),
                    (_, 5) => sym!(self, pal5uv, off, 6),
                    (_, 6) => sym!(self, pal6uv, off, 7),
                    (_, 7) => sym!(self, pal7uv, off, 8),
                    _ => sym!(self, pal8uv, off, 9),
                };
                map[(i - ju) * CMAP + ju] = order[s];
                j -= 1;
            }
        }
        for i in 0..onh {
            for j in onw..bw {
                map[i * CMAP + j] = map[i * CMAP + onw - 1];
            }
        }
        for i in onh..bh {
            for j in 0..bw {
                map[i * CMAP + j] = map[(onh - 1) * CMAP + j];
            }
        }
        if plane == 0 {
            self.color_map_y = map;
        } else {
            self.color_map_uv = map;
        }
    }

    fn palette_tokens(&mut self) {
        let mut bh = block_height(self.mi_size);
        let mut bw = block_width(self.mi_size);
        let mut onh = bh.min((self.mi_rows - self.mi_row) * 4);
        let mut onw = bw.min((self.mi_cols - self.mi_col) * 4);
        if self.palette_size_y > 0 {
            self.read_color_map(0, self.palette_size_y, bw, bh, onw, onh);
        }
        if self.palette_size_uv > 0 {
            let (sx, sy) = (self.seq.sub_x, self.seq.sub_y);
            bh >>= sy;
            bw >>= sx;
            onh >>= sy;
            onw >>= sx;
            if bw < 4 {
                bw += 2;
                onw += 2;
            }
            if bh < 4 {
                bh += 2;
                onh += 2;
            }
            self.read_color_map(1, self.palette_size_uv, bw, bh, onw, onh);
        }
    }

    // ------------------------------------------------------------- tx size

    fn above_tx_width(&self, row: usize, col: usize) -> usize {
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

    fn left_tx_height(&self, row: usize, col: usize) -> usize {
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

    fn read_block_tx_size(&mut self) {
        if self.tx_mode_select && self.mi_size > BLOCK_4X4 && self.is_inter && !self.skip && !self.lossless {
            let max_tx = MAX_TX_SIZE_RECT[self.mi_size] as usize;
            let (tw4, th4) = (tx_w(max_tx) / 4, tx_h(max_tx) / 4);
            let mut row = self.mi_row;
            while row < self.mi_row + bh4(self.mi_size) {
                let mut col = self.mi_col;
                while col < self.mi_col + bw4(self.mi_size) {
                    self.read_var_tx_size(row, col, max_tx, 0);
                    col += tw4;
                }
                row += th4;
            }
            return;
        }
        let allow_select = !self.skip || !self.is_inter;
        if self.lossless {
            self.tx_size = TX_4X4;
        } else {
            let max_rect = MAX_TX_SIZE_RECT[self.mi_size] as usize;
            let max_depth = MAX_TX_DEPTH_TABLE[self.mi_size] as usize;
            self.tx_size = max_rect;
            if self.mi_size > BLOCK_4X4 && allow_select && self.tx_mode_select {
                let (r, c) = (self.mi_row, self.mi_col);
                let above_w = if self.avail_u && self.is_inters[self.mi(r - 1, c)] == 1 {
                    block_width(self.mi_sizes[self.mi(r - 1, c)] as usize)
                } else if self.avail_u {
                    self.above_tx_width(r, c)
                } else {
                    0
                };
                let left_h = if self.avail_l && self.is_inters[self.mi(r, c - 1)] == 1 {
                    block_height(self.mi_sizes[self.mi(r, c - 1)] as usize)
                } else if self.avail_l {
                    self.left_tx_height(r, c)
                } else {
                    0
                };
                let ctx = (above_w >= tx_w(max_rect)) as usize + (left_h >= tx_h(max_rect)) as usize;
                let depth = match max_depth {
                    4 => sym!(self, tx_64x64, ctx * 4, 4),
                    3 => sym!(self, tx_32x32, ctx * 4, 4),
                    2 => sym!(self, tx_16x16, ctx * 4, 4),
                    _ => sym!(self, tx_8x8, ctx * 3, 3),
                };
                for _ in 0..depth {
                    self.tx_size = SPLIT_TX_SIZE[self.tx_size] as usize;
                }
            }
        }
        let rows = bh4(self.mi_size).min(self.mi_rows - self.mi_row);
        let cols = bw4(self.mi_size).min(self.mi_cols - self.mi_col);
        for y in 0..rows {
            for x in 0..cols {
                let i = self.mi(self.mi_row + y, self.mi_col + x);
                self.inter_tx_sizes[i] = self.tx_size as u8;
            }
        }
    }

    pub(super) fn get_tx_size(&self, plane: usize, tx: usize) -> usize {
        if plane == 0 {
            return tx;
        }
        let uv = MAX_TX_SIZE_RECT[self.plane_residual_size(self.mi_size, plane)] as usize;
        if tx_w(uv) == 64 || tx_h(uv) == 64 {
            if tx_w(uv) == 16 {
                return TX_16X32;
            }
            if tx_h(uv) == 16 {
                return TX_32X16;
            }
            return TX_32X32;
        }
        uv
    }

    // ------------------------------------------------------------ residual

    fn residual(&mut self) -> Option<()> {
        let sb_mask = if self.seq.use_128 { 31 } else { 15 };
        let _ = sb_mask;
        let width_chunks = (block_width(self.mi_size) >> 6).max(1);
        let height_chunks = (block_height(self.mi_size) >> 6).max(1);
        let size_chunk = if width_chunks > 1 || height_chunks > 1 { BLOCK_64X64 } else { self.mi_size };
        for cy in 0..height_chunks {
            for cx in 0..width_chunks {
                let row_chunk = self.mi_row + (cy << 4);
                let col_chunk = self.mi_col + (cx << 4);
                let _ = (row_chunk, col_chunk);
                for plane in 0..(1 + 2 * self.has_chroma as usize) {
                    if self.is_inter && !self.lossless && plane == 0 {
                        let plane_sz = self.plane_residual_size(size_chunk, 0);
                        self.transform_tree(col_chunk * 4, row_chunk * 4, bw4(plane_sz) * 4, bh4(plane_sz) * 4)?;
                        continue;
                    }
                    let tx = if self.lossless { TX_4X4 } else { self.get_tx_size(plane, self.tx_size) };
                    let step_x = tx_w(tx) >> 2;
                    let step_y = tx_h(tx) >> 2;
                    let plane_sz = self.plane_residual_size(size_chunk, plane);
                    let n4w = bw4(plane_sz);
                    let n4h = bh4(plane_sz);
                    let (sx, sy) = self.sub(plane);
                    let base_x = (self.mi_col >> sx) * 4;
                    let base_y = (self.mi_row >> sy) * 4;
                    let mut y = 0;
                    while y < n4h {
                        let mut x = 0;
                        while x < n4w {
                            self.transform_block(plane, base_x, base_y, tx, x + ((cx << 4) >> sx), y + ((cy << 4) >> sy))?;
                            x += step_x;
                        }
                        y += step_y;
                    }
                }
            }
        }
        Some(())
    }

    pub(super) fn transform_block(&mut self, plane: usize, base_x: usize, base_y: usize, tx: usize, x: usize, y: usize) -> Option<()> {
        let start_x = base_x + 4 * x;
        let start_y = base_y + 4 * y;
        let (sx, sy) = self.sub(plane);
        let row = (start_y << sy) >> 2;
        let col = (start_x << sx) >> 2;
        let sb_mask = if self.seq.use_128 { 31 } else { 15 };
        let sub_row = (row & sb_mask) as isize;
        let sub_col = (col & sb_mask) as isize;
        let step_x = tx_w(tx) >> 2;
        let step_y = tx_h(tx) >> 2;
        let max_x = (self.mi_cols * 4) >> sx;
        let max_y = (self.mi_rows * 4) >> sy;
        if start_x >= max_x || start_y >= max_y {
            return Some(());
        }
        let has_pal = (plane == 0 && self.palette_size_y > 0) || (plane != 0 && self.palette_size_uv > 0);
        if self.is_inter {
            // Predicted already (intra block copy).
        } else if has_pal {
            self.predict_palette(plane, start_x, start_y, x, y, tx);
        } else {
            let is_cfl = plane > 0 && self.uv_mode == UV_CFL_PRED;
            let mode = if plane == 0 {
                self.y_mode
            } else if is_cfl {
                DC_PRED
            } else {
                self.uv_mode
            };
            let log2w = TX_WIDTH_LOG2[tx] as usize;
            let log2h = TX_HEIGHT_LOG2[tx] as usize;
            let bd = &self.block_decoded[plane];
            let (br, bc) = (sub_row >> sy, sub_col >> sx);
            let above_right = bd[Dec::bd_idx(br - 1, bc + step_x as isize)];
            let below_left = bd[Dec::bd_idx(br + step_y as isize, bc - 1)];
            let have_left = (if plane == 0 { self.avail_l } else { self.avail_l_chroma }) || x > 0;
            let have_above = (if plane == 0 { self.avail_u } else { self.avail_u_chroma }) || y > 0;
            self.predict_intra(plane, start_x, start_y, have_left, have_above, above_right, below_left, mode, log2w, log2h);
            if is_cfl {
                self.predict_cfl(plane, start_x, start_y, tx);
            }
        }
        if plane == 0 {
            self.max_luma_w = start_x + step_x * 4;
            self.max_luma_h = start_y + step_y * 4;
        }
        if !self.skip {
            let eob = self.coeffs(plane, start_x, start_y, tx)?;
            if eob > 0 {
                self.reconstruct(plane, start_x, start_y, tx);
            }
        }
        for i in 0..step_y {
            for j in 0..step_x {
                let (lr, lc) = ((row >> sy) + i, (col >> sx) + j);
                if lr < self.mi_rows && lc < self.mi_cols {
                    let k = lr * self.mi_cols + lc;
                    self.lf_tx_sizes[plane][k] = tx as u8;
                }
                let bi = Dec::bd_idx((sub_row >> sy) + i as isize, (sub_col >> sx) + j as isize);
                if bi < BD * BD {
                    self.block_decoded[plane][bi] = true;
                }
            }
        }
        Some(())
    }

    fn get_tx_set(&self, tx: usize) -> usize {
        let sqr = TX_SIZE_SQR[tx] as usize;
        let sqr_up = TX_SIZE_SQR_UP[tx] as usize;
        if sqr_up > TX_32X32 {
            return TX_SET_DCTONLY;
        }
        if self.is_inter {
            if self.reduced_tx_set || sqr_up == TX_32X32 {
                return TX_SET_INTER_3;
            }
            if sqr == TX_16X16 {
                return TX_SET_INTER_2;
            }
            return TX_SET_INTER_1;
        }
        if sqr_up == TX_32X32 {
            return TX_SET_DCTONLY;
        }
        if self.reduced_tx_set || sqr == TX_16X16 {
            return TX_SET_INTRA_2;
        }
        TX_SET_INTRA_1
    }

    fn compute_tx_type(&self, plane: usize, tx: usize, bx: usize, by: usize) -> usize {
        if self.lossless || TX_SIZE_SQR_UP[tx] as usize > TX_32X32 {
            return DCT_DCT;
        }
        let set = self.get_tx_set(tx);
        if plane == 0 {
            return self.tx_types[self.mi(by.min(self.mi_rows - 1), bx.min(self.mi_cols - 1))] as usize;
        }
        if self.is_inter {
            let x4 = self.mi_col.max(bx << self.seq.sub_x).min(self.mi_cols - 1);
            let y4 = self.mi_row.max(by << self.seq.sub_y).min(self.mi_rows - 1);
            let t = self.tx_types[self.mi(y4, x4)] as usize;
            if TX_TYPE_IN_SET_INTER[set * TX_TYPES + t] == 0 {
                return DCT_DCT;
            }
            return t;
        }
        let t = MODE_TO_TXFM[self.uv_mode] as usize;
        if TX_TYPE_IN_SET_INTRA[set * TX_TYPES + t] == 0 {
            return DCT_DCT;
        }
        t
    }

    fn transform_type(&mut self, x4: usize, y4: usize, tx: usize) {
        let set = self.get_tx_set(tx);
        let q = if self.segmentation_enabled { self.get_qindex(true, self.segment_id) } else { self.base_q_idx };
        let mut t = DCT_DCT;
        if set > 0 && q > 0 && self.is_inter {
            let sqr = TX_SIZE_SQR[tx] as usize;
            t = match set {
                TX_SET_INTER_1 => TX_TYPE_INTER_INV_SET1[sym!(self, inter_tx_type_set1, sqr * 17, 17)] as usize,
                TX_SET_INTER_2 => TX_TYPE_INTER_INV_SET2[sym!(self, inter_tx_type_set2, 0, 13)] as usize,
                _ => TX_TYPE_INTER_INV_SET3[sym!(self, inter_tx_type_set3, sqr * 3, 3)] as usize,
            };
        } else if set > 0 && q > 0 {
            let dir = if self.use_filter_intra { FILTER_INTRA_MODE_TO_INTRA_DIR[self.filter_intra_mode] as usize } else { self.y_mode };
            let sqr = TX_SIZE_SQR[tx] as usize;
            t = if set == TX_SET_INTRA_1 {
                TX_TYPE_INTRA_INV_SET1[sym!(self, intra_tx_type_set1, (sqr * 13 + dir) * 8, 8)] as usize
            } else {
                TX_TYPE_INTRA_INV_SET2[sym!(self, intra_tx_type_set2, (sqr * 13 + dir) * 6, 6)] as usize
            };
        }
        self.set_tx_types(x4, y4, tx, t);
    }

    fn set_tx_types(&mut self, x4: usize, y4: usize, tx: usize, t: usize) {
        for j in 0..(tx_h(tx) >> 2) {
            for i in 0..(tx_w(tx) >> 2) {
                if y4 + j < self.mi_rows && x4 + i < self.mi_cols {
                    let k = self.mi(y4 + j, x4 + i);
                    self.tx_types[k] = t as u8;
                }
            }
        }
    }

    fn get_scan(&self, tx: usize) -> &'static [u16] {
        let s = scans();
        if tx == TX_16X64 {
            return &s[0][TX_16X32];
        }
        if tx == TX_64X16 {
            return &s[0][TX_32X16];
        }
        if TX_SIZE_SQR_UP[tx] as usize == TX_64X64 {
            return &s[0][TX_32X32];
        }
        let t = self.plane_tx_type;
        if t == IDTX {
            return &s[0][tx];
        }
        let prefer_row = matches!(t, V_DCT | V_ADST | V_FLIPADST);
        let prefer_col = matches!(t, H_DCT | H_ADST | H_FLIPADST);
        if prefer_row && !s[1][tx].is_empty() {
            &s[1][tx]
        } else if prefer_col && !s[2][tx].is_empty() {
            &s[2][tx]
        } else {
            &s[0][tx]
        }
    }

    fn coeff_base_ctx(&self, tx: usize, pos: usize, c: usize, is_eob: bool, tx_class: usize) -> usize {
        let adj = ADJUSTED_TX_SIZE[tx] as usize;
        let bwl = TX_WIDTH_LOG2[adj] as usize;
        let width = 1usize << bwl;
        let height = tx_h(adj);
        if is_eob {
            if c == 0 {
                return SIG_COEF_CONTEXTS - 4;
            }
            if c <= (height << bwl) / 8 {
                return SIG_COEF_CONTEXTS - 3;
            }
            if c <= (height << bwl) / 4 {
                return SIG_COEF_CONTEXTS - 2;
            }
            return SIG_COEF_CONTEXTS - 1;
        }
        let row = pos >> bwl;
        let col = pos - (row << bwl);
        let mut mag = 0;
        for idx in 0..5 {
            let o = (tx_class * 5 + idx) * 2;
            let rr = row + SIG_REF_DIFF_OFFSET[o] as usize;
            let cc = col + SIG_REF_DIFF_OFFSET[o + 1] as usize;
            if rr < height && cc < width {
                mag += self.quant[(rr << bwl) + cc].abs().min(3);
            }
        }
        let ctx = (((mag + 1) >> 1) as usize).min(4);
        if tx_class == TX_CLASS_2D {
            if row == 0 && col == 0 {
                return 0;
            }
            return ctx + COEFF_BASE_CTX_OFFSET[(tx * 5 + row.min(4)) * 5 + col.min(4)] as usize;
        }
        let idx = if tx_class == TX_CLASS_VERT { row } else { col };
        ctx + COEFF_BASE_POS_CTX_OFFSET[idx.min(2)] as usize
    }

    fn coeff_br_ctx(&self, tx: usize, pos: usize, tx_class: usize) -> usize {
        let adj = ADJUSTED_TX_SIZE[tx] as usize;
        let bwl = TX_WIDTH_LOG2[adj] as usize;
        let txw = tx_w(adj);
        let txh = tx_h(adj);
        let row = pos >> bwl;
        let col = pos - (row << bwl);
        let mut mag = 0;
        for idx in 0..3 {
            let o = (tx_class * 3 + idx) * 2;
            let rr = row + MAG_REF_OFFSET_WITH_TX_CLASS[o] as usize;
            let cc = col + MAG_REF_OFFSET_WITH_TX_CLASS[o + 1] as usize;
            if rr < txh && cc < (1 << bwl) {
                mag += self.quant[rr * txw + cc].min((COEFF_BASE_RANGE + NUM_BASE_LEVELS + 1) as i32);
            }
        }
        let mag = (((mag + 1) >> 1) as usize).min(6);
        if pos == 0 {
            mag
        } else if tx_class == 0 {
            if row < 2 && col < 2 { mag + 7 } else { mag + 14 }
        } else if tx_class == 1 {
            if col == 0 { mag + 7 } else { mag + 14 }
        } else if row == 0 {
            mag + 7
        } else {
            mag + 14
        }
    }

    fn coeffs(&mut self, plane: usize, start_x: usize, start_y: usize, tx: usize) -> Option<usize> {
        let x4 = start_x >> 2;
        let y4 = start_y >> 2;
        let w4 = tx_w(tx) >> 2;
        let h4 = tx_h(tx) >> 2;
        let tx_sz_ctx = (TX_SIZE_SQR[tx] as usize + TX_SIZE_SQR_UP[tx] as usize + 1) >> 1;
        let ptype = (plane > 0) as usize;
        let seg_eob = if tx == TX_16X64 || tx == TX_64X16 { 512 } else { (tx_w(tx) * tx_h(tx)).min(1024) };
        self.quant[..seg_eob].fill(0);
        let mut eob = 0;
        let mut cul_level = 0u32;
        let mut dc_category = 0u8;
        let (sx, sy) = self.sub(plane);
        let max_x4 = self.mi_cols >> sx;
        let max_y4 = self.mi_rows >> sy;
        // all_zero context
        let ctx = {
            let bsize = self.plane_residual_size(self.mi_size, plane);
            let (bw, bh) = (block_width(bsize), block_height(bsize));
            let (w, h) = (tx_w(tx), tx_h(tx));
            if plane == 0 {
                let mut top = 0u32;
                let mut left = 0u32;
                for k in 0..w4 {
                    if x4 + k < max_x4 {
                        top = top.max(self.above_level[0][x4 + k] as u32);
                    }
                }
                for k in 0..h4 {
                    if y4 + k < max_y4 {
                        left = left.max(self.left_level[0][y4 + k] as u32);
                    }
                }
                let (top, left) = (top.min(255), left.min(255));
                if bw == w && bh == h {
                    0
                } else if top == 0 && left == 0 {
                    1
                } else if top == 0 || left == 0 {
                    2 + (top.max(left) > 3) as usize
                } else if top.max(left) <= 3 {
                    4
                } else if top.min(left) <= 3 {
                    5
                } else {
                    6
                }
            } else {
                let mut above = 0u8;
                let mut left = 0u8;
                for i in 0..w4 {
                    if x4 + i < max_x4 {
                        above |= self.above_level[plane][x4 + i] | self.above_dc[plane][x4 + i];
                    }
                }
                for i in 0..h4 {
                    if y4 + i < max_y4 {
                        left |= self.left_level[plane][y4 + i] | self.left_dc[plane][y4 + i];
                    }
                }
                let mut ctx = (above != 0) as usize + (left != 0) as usize + 7;
                if bw * bh > w * h {
                    ctx += 3;
                }
                ctx
            }
        };
        let all_zero = sym!(self, txb_skip, (tx_sz_ctx * 13 + ctx) * 3, 3) == 1;
        if all_zero {
            if plane == 0 {
                self.set_tx_types(x4, y4, tx, DCT_DCT);
            }
        } else {
            if plane == 0 {
                self.transform_type(x4, y4, tx);
            }
            self.plane_tx_type = self.compute_tx_type(plane, tx, x4, y4);
            let tx_class = get_tx_class(self.plane_tx_type);
            let scan = self.get_scan(tx);
            let multisize = (TX_WIDTH_LOG2[tx] as usize).min(5) + (TX_HEIGHT_LOG2[tx] as usize).min(5) - 4;
            let ectx = (tx_class != TX_CLASS_2D) as usize;
            let eob_pt = 1 + match multisize {
                0 => sym!(self, eob_pt_16, (ptype * 2 + ectx) * 6, 6),
                1 => sym!(self, eob_pt_32, (ptype * 2 + ectx) * 7, 7),
                2 => sym!(self, eob_pt_64, (ptype * 2 + ectx) * 8, 8),
                3 => sym!(self, eob_pt_128, (ptype * 2 + ectx) * 9, 9),
                4 => sym!(self, eob_pt_256, (ptype * 2 + ectx) * 10, 10),
                5 => sym!(self, eob_pt_512, ptype * 11, 11),
                _ => sym!(self, eob_pt_1024, ptype * 12, 12),
            };
            eob = if eob_pt < 2 { eob_pt } else { (1 << (eob_pt - 2)) + 1 };
            let eob_shift = eob_pt as isize - 3;
            if eob_shift >= 0 {
                if sym!(self, eob_extra, ((tx_sz_ctx * 2 + ptype) * 9 + (eob_pt - 3)) * 3, 3) == 1 {
                    eob += 1 << eob_shift;
                }
                let m = (eob_pt as isize - 2).max(0) as usize;
                for i in 1..m {
                    let sh = m - 1 - i;
                    if self.sd.lit(1) == 1 {
                        eob += 1 << sh;
                    }
                }
            }
            if eob > seg_eob {
                return None;
            }
            for c in (0..eob).rev() {
                let pos = scan[c] as usize;
                let mut level;
                if c == eob - 1 {
                    let ctx = self.coeff_base_ctx(tx, pos, c, true, tx_class) - SIG_COEF_CONTEXTS + SIG_COEF_CONTEXTS_EOB;
                    level = sym!(self, coeff_base_eob, ((tx_sz_ctx * 2 + ptype) * 4 + ctx) * 4, 4) + 1;
                } else {
                    let ctx = self.coeff_base_ctx(tx, pos, c, false, tx_class);
                    level = sym!(self, coeff_base, ((tx_sz_ctx * 2 + ptype) * 42 + ctx) * 5, 5);
                }
                if level > NUM_BASE_LEVELS {
                    let ctx = self.coeff_br_ctx(tx, pos, tx_class);
                    for _ in 0..COEFF_BASE_RANGE / (BR_CDF_SIZE - 1) {
                        let br = sym!(self, coeff_br, ((tx_sz_ctx.min(TX_32X32) * 2 + ptype) * 21 + ctx) * 5, 5);
                        level += br;
                        if br < BR_CDF_SIZE - 1 {
                            break;
                        }
                    }
                }
                self.quant[pos] = level as i32;
            }
            for c in 0..eob {
                let pos = scan[c] as usize;
                let mut sign = 0;
                if self.quant[pos] != 0 {
                    if c == 0 {
                        let mut dc_sign = 0i32;
                        for k in 0..w4 {
                            if x4 + k < max_x4 {
                                match self.above_dc[plane][x4 + k] {
                                    1 => dc_sign -= 1,
                                    2 => dc_sign += 1,
                                    _ => {}
                                }
                            }
                        }
                        for k in 0..h4 {
                            if y4 + k < max_y4 {
                                match self.left_dc[plane][y4 + k] {
                                    1 => dc_sign -= 1,
                                    2 => dc_sign += 1,
                                    _ => {}
                                }
                            }
                        }
                        let ctx = if dc_sign < 0 {
                            1
                        } else if dc_sign > 0 {
                            2
                        } else {
                            0
                        };
                        sign = sym!(self, dc_sign, (ptype * 3 + ctx) * 3, 3);
                    } else {
                        sign = self.sd.lit(1) as usize;
                    }
                }
                if self.quant[pos] > (NUM_BASE_LEVELS + COEFF_BASE_RANGE) as i32 {
                    let mut length = 0;
                    loop {
                        length += 1;
                        if self.sd.lit(1) == 1 {
                            break;
                        }
                        if length > 32 {
                            return None;
                        }
                    }
                    let mut x = 1u32;
                    for _ in 0..length - 1 {
                        x = (x << 1) | self.sd.lit(1);
                    }
                    self.quant[pos] = (x + (COEFF_BASE_RANGE + NUM_BASE_LEVELS) as u32) as i32;
                }
                if pos == 0 && self.quant[pos] > 0 {
                    dc_category = if sign == 1 { 1 } else { 2 };
                }
                self.quant[pos] &= 0xFFFFF;
                cul_level += self.quant[pos] as u32;
                if sign == 1 {
                    self.quant[pos] = -self.quant[pos];
                }
            }
            cul_level = cul_level.min(63);
        }
        for i in 0..w4 {
            if x4 + i < self.above_level[plane].len() {
                self.above_level[plane][x4 + i] = cul_level as u8;
                self.above_dc[plane][x4 + i] = dc_category;
            }
        }
        for i in 0..h4 {
            if y4 + i < self.left_level[plane].len() {
                self.left_level[plane][y4 + i] = cul_level as u8;
                self.left_dc[plane][y4 + i] = dc_category;
            }
        }
        Some(eob)
    }

    // ---------------------------------------------------- loop restoration

    fn read_lr(&mut self, r: usize, c: usize, bsize: usize) {
        if self.allow_intrabc {
            return;
        }
        let w = bw4(bsize);
        let h = bh4(bsize);
        for plane in 0..self.num_planes {
            if self.frame_restoration_type[plane] == RESTORE_NONE {
                continue;
            }
            let (sx, sy) = self.sub(plane);
            let unit = self.loop_restoration_size[plane];
            let unit_rows = self.lr_unit_rows[plane];
            let unit_cols = self.lr_unit_cols[plane];
            let row_start = (r * (4 >> sy) + unit - 1) / unit;
            let row_end = unit_rows.min(((r + h) * (4 >> sy) + unit - 1) / unit);
            let (num, den) = if self.use_superres { ((4 >> sx) * self.superres_denom, unit * SUPERRES_NUM) } else { (4 >> sx, unit) };
            let col_start = (c * num + den - 1) / den;
            let col_end = unit_cols.min(((c + w) * num + den - 1) / den);
            for ur in row_start..row_end {
                for uc in col_start..col_end {
                    self.read_lr_unit(plane, ur, uc);
                }
            }
        }
    }

    fn subexp_bool(&mut self, num_syms: i32, k: i32) -> i32 {
        let mut i = 0;
        let mut mk = 0;
        loop {
            let b2 = if i > 0 { k + i - 1 } else { k };
            let a = 1 << b2;
            if num_syms <= mk + 3 * a {
                return self.sd.ns((num_syms - mk) as u32) as i32 + mk;
            }
            if self.sd.lit(1) == 1 {
                i += 1;
                mk += a;
            } else {
                return self.sd.lit(b2 as u32) as i32 + mk;
            }
        }
    }

    fn signed_subexp_with_ref_bool(&mut self, low: i32, high: i32, k: i32, r: i32) -> i32 {
        let mx = high - low;
        let r = r - low;
        let v = self.subexp_bool(mx, k);
        let inv = |r: i32, v: i32| {
            if v > 2 * r {
                v
            } else if v & 1 == 1 {
                r - ((v + 1) >> 1)
            } else {
                r + (v >> 1)
            }
        };
        let x = if (r << 1) <= mx { inv(r, v) } else { mx - 1 - inv(mx - 1 - r, v) };
        x + low
    }

    fn read_lr_unit(&mut self, plane: usize, ur: usize, uc: usize) {
        let ft = self.frame_restoration_type[plane];
        let rt = if ft == RESTORE_WIENER {
            if sym!(self, use_wiener, 0, 3) == 1 { RESTORE_WIENER } else { RESTORE_NONE }
        } else if ft == RESTORE_SGRPROJ {
            if sym!(self, use_sgrproj, 0, 3) == 1 { RESTORE_SGRPROJ } else { RESTORE_NONE }
        } else {
            sym!(self, restoration_type, 0, 4)
        };
        let u = ur * self.lr_unit_cols[plane] + uc;
        self.lr_type[plane][u] = rt as u8;
        if rt == RESTORE_WIENER {
            for pass in 0..2 {
                let first = if plane > 0 {
                    self.lr_wiener[plane][u][pass][0] = 0;
                    1
                } else {
                    0
                };
                for j in first..3 {
                    let min = WIENER_TAPS_MIN[j] as i32;
                    let max = WIENER_TAPS_MAX[j] as i32;
                    let k = WIENER_TAPS_K[j] as i32;
                    let v = self.signed_subexp_with_ref_bool(min, max + 1, k, self.ref_lr_wiener[plane][pass][j]);
                    self.lr_wiener[plane][u][pass][j] = v;
                    self.ref_lr_wiener[plane][pass][j] = v;
                }
            }
        } else if rt == RESTORE_SGRPROJ {
            let set = self.sd.lit(SGRPROJ_PARAMS_BITS as u32) as usize;
            self.lr_sgr_set[plane][u] = set as u8;
            for i in 0..2 {
                let radius = SGR_PARAMS[set * 4 + i * 2];
                let min = SGRPROJ_XQD_MIN[i] as i32;
                let max = SGRPROJ_XQD_MAX[i] as i32;
                let v = if radius != 0 {
                    self.signed_subexp_with_ref_bool(min, max + 1, SGRPROJ_PRJ_SUBEXP_K as i32, self.ref_sgr_xqd[plane][i])
                } else if i == 1 {
                    ((1 << SGRPROJ_PRJ_BITS) - self.ref_sgr_xqd[plane][0]).clamp(min, max)
                } else {
                    0
                };
                self.lr_sgr_xqd[plane][u][i] = v;
                self.ref_sgr_xqd[plane][i] = v;
            }
        }
    }
}
