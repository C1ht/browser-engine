//! Frame header parsing (uncompressed_header and its sub-structures).

use super::*;

impl Dec {
    pub(super) fn uncompressed_header(&mut self, b: &mut Bits) -> Option<()> {
        let s = self.seq.clone();
        let id_len = s.additional_frame_id_length_minus_1 + s.delta_frame_id_length_minus_2 + 3;
        let frame_type;
        let error_resilient_mode;
        if s.reduced {
            frame_type = 0;
            self.frame_is_intra = true;
            self.show_frame = true;
            self.showable_frame = false;
            error_resilient_mode = true;
        } else {
            if b.f(1) == 1 {
                return None; // show_existing_frame
            }
            frame_type = b.f(2);
            self.frame_is_intra = frame_type == 0 || frame_type == 2;
            self.show_frame = b.f(1) == 1;
            if self.show_frame && s.decoder_model_info_present && !s.equal_picture_interval {
                b.f(s.frame_presentation_time_length);
            }
            self.showable_frame = if self.show_frame { frame_type != 0 } else { b.f(1) == 1 };
            error_resilient_mode = if frame_type == 3 || (frame_type == 0 && self.show_frame) { true } else { b.f(1) == 1 };
        }
        if !self.frame_is_intra {
            return None; // only intra frames are supported
        }
        self.disable_cdf_update = b.f(1) == 1;
        self.allow_screen_content_tools =
            if s.force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS as u32 { b.f(1) == 1 } else { s.force_screen_content_tools == 1 };
        if self.allow_screen_content_tools && s.force_integer_mv == SELECT_INTEGER_MV as u32 {
            b.f(1); // force_integer_mv
        }
        if s.frame_id_numbers_present {
            b.f(id_len);
        }
        let frame_size_override = if frame_type == 3 {
            true
        } else if s.reduced {
            false
        } else {
            b.f(1) == 1
        };
        b.f(s.order_hint_bits);
        // primary_ref_frame is PRIMARY_REF_NONE for intra frames.
        if s.decoder_model_info_present && b.f(1) == 1 {
            for (op, &idc) in s.op_idc.iter().enumerate() {
                if s.decoder_model_present_for_op[op] {
                    let in_t = idc & 1;
                    let in_s = (idc >> 8) & 1;
                    if idc == 0 || (in_t == 1 && in_s == 1) {
                        b.f(s.buffer_removal_time_length);
                    }
                }
            }
        }
        let refresh = if frame_type == 3 || (frame_type == 0 && self.show_frame) { 0xFF } else { b.f(8) };
        if refresh != 0xFF && error_resilient_mode && s.enable_order_hint {
            for _ in 0..NUM_REF_FRAMES {
                b.f(s.order_hint_bits);
            }
        }
        // frame_size()
        if frame_size_override {
            self.frame_width = b.f(s.frame_width_bits) as usize + 1;
            self.frame_height = b.f(s.frame_height_bits) as usize + 1;
        } else {
            self.frame_width = s.max_w as usize;
            self.frame_height = s.max_h as usize;
        }
        self.use_superres = s.enable_superres && b.f(1) == 1;
        self.superres_denom = if self.use_superres { b.f(SUPERRES_DENOM_BITS as u32) as usize + SUPERRES_DENOM_MIN } else { SUPERRES_NUM };
        self.upscaled_width = self.frame_width;
        self.frame_width = (self.upscaled_width * SUPERRES_NUM + self.superres_denom / 2) / self.superres_denom;
        self.mi_cols = 2 * ((self.frame_width + 7) >> 3);
        self.mi_rows = 2 * ((self.frame_height + 7) >> 3);
        // render_size()
        if b.f(1) == 1 {
            b.f(16);
            b.f(16);
        }
        if self.allow_screen_content_tools && self.upscaled_width == self.frame_width {
            self.allow_intrabc = b.f(1) == 1;
        }
        if !(s.reduced || self.disable_cdf_update) {
            b.f(1); // disable_frame_end_update_cdf
        }
        self.tile_info(b)?;
        // quantization_params()
        self.base_q_idx = b.f(8) as i32;
        let dq = |b: &mut Bits| if b.f(1) == 1 { b.su(7) } else { 0 };
        self.delta_q_y_dc = dq(b);
        if self.num_planes > 1 {
            let diff_uv = s.separate_uv_delta_q && b.f(1) == 1;
            self.delta_q_u_dc = dq(b);
            self.delta_q_u_ac = dq(b);
            if diff_uv {
                self.delta_q_v_dc = dq(b);
                self.delta_q_v_ac = dq(b);
            } else {
                self.delta_q_v_dc = self.delta_q_u_dc;
                self.delta_q_v_ac = self.delta_q_u_ac;
            }
        }
        self.using_qmatrix = b.f(1) == 1;
        if self.using_qmatrix {
            self.qm[0] = b.f(4) as usize;
            self.qm[1] = b.f(4) as usize;
            self.qm[2] = if !s.separate_uv_delta_q { self.qm[1] } else { b.f(4) as usize };
        }
        // segmentation_params(): with PRIMARY_REF_NONE the map and data are always updated.
        self.segmentation_enabled = b.f(1) == 1;
        if self.segmentation_enabled {
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let en = b.f(1) == 1;
                    self.feature_enabled[i][j] = en;
                    let mut v = 0;
                    if en {
                        let bits = SEGMENTATION_FEATURE_BITS[j] as u32;
                        let limit = SEGMENTATION_FEATURE_MAX[j] as i32;
                        if SEGMENTATION_FEATURE_SIGNED[j] == 1 {
                            v = b.su(1 + bits).clamp(-limit, limit);
                        } else {
                            v = (b.f(bits) as i32).clamp(0, limit);
                        }
                    }
                    self.feature_data[i][j] = v;
                }
            }
        }
        for i in 0..MAX_SEGMENTS {
            for j in 0..SEG_LVL_MAX {
                if self.feature_enabled[i][j] {
                    self.last_active_seg_id = i;
                    if j >= SEG_LVL_REF_FRAME {
                        self.seg_id_pre_skip = true;
                    }
                }
            }
        }
        // delta_q_params(), delta_lf_params()
        if self.base_q_idx > 0 {
            self.delta_q_present = b.f(1) == 1;
        }
        if self.delta_q_present {
            self.delta_q_res = b.f(2);
            if !self.allow_intrabc {
                self.delta_lf_present = b.f(1) == 1;
            }
            if self.delta_lf_present {
                self.delta_lf_res = b.f(2);
                self.delta_lf_multi = b.f(1) == 1;
            }
        }
        self.coded_lossless = true;
        for seg in 0..MAX_SEGMENTS {
            let q = self.get_qindex(true, seg);
            let l = q == 0 && self.delta_q_y_dc == 0 && self.delta_q_u_ac == 0 && self.delta_q_u_dc == 0 && self.delta_q_v_ac == 0 && self.delta_q_v_dc == 0;
            self.lossless_array[seg] = l;
            if !l {
                self.coded_lossless = false;
            }
            if self.using_qmatrix {
                for p in 0..3 {
                    self.seg_qm_level[p][seg] = if l { 15 } else { self.qm[p] };
                }
            }
        }
        self.all_lossless = self.coded_lossless && self.frame_width == self.upscaled_width;
        // loop_filter_params()
        self.loop_filter_ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];
        self.loop_filter_mode_deltas = [0, 0];
        if !(self.coded_lossless || self.allow_intrabc) {
            self.loop_filter_level[0] = b.f(6) as i32;
            self.loop_filter_level[1] = b.f(6) as i32;
            if self.num_planes > 1 && (self.loop_filter_level[0] != 0 || self.loop_filter_level[1] != 0) {
                self.loop_filter_level[2] = b.f(6) as i32;
                self.loop_filter_level[3] = b.f(6) as i32;
            }
            self.loop_filter_sharpness = b.f(3) as i32;
            self.loop_filter_delta_enabled = b.f(1) == 1;
            if self.loop_filter_delta_enabled && b.f(1) == 1 {
                for i in 0..8 {
                    if b.f(1) == 1 {
                        self.loop_filter_ref_deltas[i] = b.su(7);
                    }
                }
                for i in 0..2 {
                    if b.f(1) == 1 {
                        self.loop_filter_mode_deltas[i] = b.su(7);
                    }
                }
            }
        }
        // cdef_params()
        self.cdef_damping = 3;
        if !(self.coded_lossless || self.allow_intrabc || !s.enable_cdef) {
            self.cdef_damping = b.f(2) as i32 + 3;
            self.cdef_bits = b.f(2);
            for i in 0..(1usize << self.cdef_bits) {
                self.cdef_y_pri[i] = b.f(4) as i32;
                self.cdef_y_sec[i] = b.f(2) as i32;
                if self.cdef_y_sec[i] == 3 {
                    self.cdef_y_sec[i] += 1;
                }
                if self.num_planes > 1 {
                    self.cdef_uv_pri[i] = b.f(4) as i32;
                    self.cdef_uv_sec[i] = b.f(2) as i32;
                    if self.cdef_uv_sec[i] == 3 {
                        self.cdef_uv_sec[i] += 1;
                    }
                }
            }
        }
        // lr_params()
        if !(self.all_lossless || self.allow_intrabc || !s.enable_restoration) {
            let mut uses_chroma = false;
            for i in 0..self.num_planes {
                let t = REMAP_LR_TYPE[b.f(2) as usize] as usize;
                self.frame_restoration_type[i] = t;
                if t != RESTORE_NONE {
                    self.uses_lr = true;
                    if i > 0 {
                        uses_chroma = true;
                    }
                }
            }
            if self.uses_lr {
                let mut shift = b.f(1) as usize;
                if s.use_128 {
                    shift += 1;
                } else if shift == 1 {
                    shift += b.f(1) as usize;
                }
                self.loop_restoration_size[0] = RESTORATION_TILESIZE_MAX >> (2 - shift);
                let uv_shift = if s.sub_x == 1 && s.sub_y == 1 && uses_chroma { b.f(1) as usize } else { 0 };
                self.loop_restoration_size[1] = self.loop_restoration_size[0] >> uv_shift;
                self.loop_restoration_size[2] = self.loop_restoration_size[0] >> uv_shift;
            }
        }
        // read_tx_mode()
        if self.coded_lossless {
            self.tx_mode_only4x4 = true;
        } else {
            self.tx_mode_select = b.f(1) == 1;
        }
        // frame_reference_mode, skip_mode_params and allow_warped_motion
        // are not coded for intra frames.
        self.reduced_tx_set = b.f(1) == 1;
        // global_motion_params() returns immediately for intra frames.
        // film_grain_params()
        if s.film_grain_present && (self.show_frame || self.showable_frame) && b.f(1) == 1 {
            let g = &mut self.grain;
            g.apply = true;
            g.seed = b.f(16);
            // update_grain is implied for intra frames.
            let n = b.f(4);
            g.y_points = (0..n).map(|_| (b.f(8) as i32, b.f(8) as i32)).collect();
            g.chroma_scaling_from_luma = if s.mono { false } else { b.f(1) == 1 };
            if !(s.mono || g.chroma_scaling_from_luma || (s.sub_x == 1 && s.sub_y == 1 && n == 0)) {
                let ncb = b.f(4);
                g.cb_points = (0..ncb).map(|_| (b.f(8) as i32, b.f(8) as i32)).collect();
                let ncr = b.f(4);
                g.cr_points = (0..ncr).map(|_| (b.f(8) as i32, b.f(8) as i32)).collect();
            }
            g.scaling_shift = b.f(2) as i32 + 8;
            g.ar_coeff_lag = b.f(2) as i32;
            let num_pos_luma = 2 * g.ar_coeff_lag * (g.ar_coeff_lag + 1);
            let num_pos_chroma = if n > 0 {
                g.ar_y = (0..num_pos_luma).map(|_| b.f(8) as i32 - 128).collect();
                num_pos_luma + 1
            } else {
                num_pos_luma
            };
            if g.chroma_scaling_from_luma || !g.cb_points.is_empty() {
                g.ar_cb = (0..num_pos_chroma).map(|_| b.f(8) as i32 - 128).collect();
            }
            if g.chroma_scaling_from_luma || !g.cr_points.is_empty() {
                g.ar_cr = (0..num_pos_chroma).map(|_| b.f(8) as i32 - 128).collect();
            }
            g.ar_shift = b.f(2) as i32 + 6;
            g.grain_scale_shift = b.f(2) as i32;
            if !g.cb_points.is_empty() {
                g.cb_mult = b.f(8) as i32;
                g.cb_luma_mult = b.f(8) as i32;
                g.cb_offset = b.f(9) as i32;
            }
            if !g.cr_points.is_empty() {
                g.cr_mult = b.f(8) as i32;
                g.cr_luma_mult = b.f(8) as i32;
                g.cr_offset = b.f(9) as i32;
            }
            g.overlap = b.f(1) == 1;
            g.clip_restricted = b.f(1) == 1;
        }
        Some(())
    }

    fn tile_info(&mut self, b: &mut Bits) -> Option<()> {
        let use_128 = self.seq.use_128;
        let sb_cols = if use_128 { (self.mi_cols + 31) >> 5 } else { (self.mi_cols + 15) >> 4 };
        let sb_rows = if use_128 { (self.mi_rows + 31) >> 5 } else { (self.mi_rows + 15) >> 4 };
        let sb_shift = if use_128 { 5 } else { 4 };
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let mut max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let tile_log2 = |blk: usize, target: usize| -> usize {
            let mut k = 0;
            while (blk << k) < target {
                k += 1;
            }
            k
        };
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        self.mi_col_starts.clear();
        self.mi_row_starts.clear();
        if b.f(1) == 1 {
            let mut cols_log2 = min_log2_tile_cols;
            while cols_log2 < max_log2_tile_cols {
                if b.f(1) == 1 {
                    cols_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
            let mut start = 0;
            while start < sb_cols {
                self.mi_col_starts.push(start << sb_shift);
                start += tile_width_sb;
            }
            self.mi_col_starts.push(self.mi_cols);
            let mut rows_log2 = min_log2_tiles.saturating_sub(cols_log2);
            while rows_log2 < max_log2_tile_rows {
                if b.f(1) == 1 {
                    rows_log2 += 1;
                } else {
                    break;
                }
            }
            let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
            let mut start = 0;
            while start < sb_rows {
                self.mi_row_starts.push(start << sb_shift);
                start += tile_height_sb;
            }
            self.mi_row_starts.push(self.mi_rows);
            self.tile_cols_log2 = cols_log2 as u32;
            self.tile_rows_log2 = rows_log2 as u32;
        } else {
            let mut widest = 0;
            let mut start = 0;
            while start < sb_cols {
                self.mi_col_starts.push(start << sb_shift);
                let max_w = (sb_cols - start).min(max_tile_width_sb);
                let size = b.ns(max_w as u32) as usize + 1;
                widest = widest.max(size);
                start += size;
            }
            self.mi_col_starts.push(self.mi_cols);
            let tile_cols = self.mi_col_starts.len() - 1;
            self.tile_cols_log2 = tile_log2(1, tile_cols) as u32;
            max_tile_area_sb = if min_log2_tiles > 0 { (sb_rows * sb_cols) >> (min_log2_tiles + 1) } else { sb_rows * sb_cols };
            let max_tile_height_sb = (max_tile_area_sb / widest.max(1)).max(1);
            let mut start = 0;
            while start < sb_rows {
                self.mi_row_starts.push(start << sb_shift);
                let max_h = (sb_rows - start).min(max_tile_height_sb);
                start += b.ns(max_h as u32) as usize + 1;
            }
            self.mi_row_starts.push(self.mi_rows);
            let tile_rows = self.mi_row_starts.len() - 1;
            self.tile_rows_log2 = tile_log2(1, tile_rows) as u32;
        }
        if self.tile_cols_log2 > 0 || self.tile_rows_log2 > 0 {
            b.f(self.tile_rows_log2 + self.tile_cols_log2); // context_update_tile_id
            self.tile_size_bytes = b.f(2) as usize + 1;
        }
        Some(())
    }

    pub(super) fn seg_feature_active_idx(&self, idx: usize, feature: usize) -> bool {
        self.segmentation_enabled && self.feature_enabled[idx][feature]
    }

    /// get_qindex( ignoreDeltaQ, segmentId ).
    pub(super) fn get_qindex(&self, ignore_delta_q: bool, seg: usize) -> i32 {
        if self.seg_feature_active_idx(seg, SEG_LVL_ALT_Q) {
            let data = self.feature_data[seg][SEG_LVL_ALT_Q];
            let mut q = self.base_q_idx + data;
            if !ignore_delta_q && self.delta_q_present {
                q = self.current_q_index + data;
            }
            return q.clamp(0, 255);
        }
        if !ignore_delta_q && self.delta_q_present {
            return self.current_q_index;
        }
        self.base_q_idx
    }
}
