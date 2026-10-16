//! AV1 intra-frame decoder (for AVIF still images), written from the AV1
//! specification: OBU parsing, sequence/frame headers, the symbol decoder,
//! block syntax, intra prediction (directional, smooth, Paeth, CfL, palette,
//! filter intra), coefficient decoding, inverse transforms, and the loop
//! filter, CDEF and loop restoration post-filters.
//!
//! Variable names follow the specification where practical so the code can
//! be checked against it.

use crate::av1_tables::*;
use std::sync::OnceLock;

// =========================================================== bit reading

struct Bits<'a> {
    d: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0 }
    }
    fn bit(&mut self) -> u32 {
        let b = self.d.get(self.pos >> 3).map_or(0, |b| (b >> (7 - (self.pos & 7))) & 1);
        self.pos += 1;
        b as u32
    }
    fn f(&mut self, n: u32) -> u32 {
        let mut x = 0u32;
        for _ in 0..n {
            x = (x << 1) | self.bit();
        }
        x
    }
    fn su(&mut self, n: u32) -> i32 {
        let v = self.f(n) as i32;
        let m = 1 << (n - 1);
        if v & m != 0 { v - 2 * m } else { v }
    }
    fn ns(&mut self, n: u32) -> u32 {
        let w = 32 - n.leading_zeros(); // FloorLog2(n) + 1
        let m = (1 << w) - n;
        let v = self.f(w - 1);
        if v < m {
            return v;
        }
        (v << 1) - m + self.f(1)
    }
    fn uvlc(&mut self) -> u32 {
        let mut lz = 0;
        while self.bit() == 0 {
            lz += 1;
            if lz >= 32 {
                return u32::MAX;
            }
        }
        self.f(lz) + (1 << lz) - 1
    }
    fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

fn leb128(d: &[u8], pos: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    for i in 0..8 {
        let b = *d.get(*pos)?;
        *pos += 1;
        v |= ((b & 0x7f) as usize) << (i * 7);
        if b & 0x80 == 0 {
            break;
        }
    }
    Some(v)
}

// ======================================================= symbol decoder

#[derive(Default)]
struct Sym {
    d: Vec<u8>,
    bitpos: usize,
    end: usize,
    value: u32,
    range: u32,
    max_bits: i64,
    update: bool,
}

impl Sym {
    fn new(d: Vec<u8>, update: bool) -> Self {
        let sz = d.len();
        let mut s = Sym { d, bitpos: 0, end: sz * 8, value: 0, range: 1 << 15, max_bits: 8 * sz as i64 - 15, update };
        let num_bits = (sz * 8).min(15) as u32;
        let buf = s.f(num_bits);
        let padded = buf << (15 - num_bits);
        s.value = ((1 << 15) - 1) ^ padded;
        s
    }
    #[inline]
    fn f(&mut self, n: u32) -> u32 {
        let mut x = 0;
        for _ in 0..n {
            let b = if self.bitpos < self.end { (self.d[self.bitpos >> 3] >> (7 - (self.bitpos & 7))) & 1 } else { 0 };
            self.bitpos += 1;
            x = (x << 1) | b as u32;
        }
        x
    }
    /// read_symbol: `cdf` has N + 1 entries (the last is the adaptation counter).
    #[inline]
    fn read(&mut self, cdf: &mut [u16]) -> usize {
        let n = cdf.len() - 1;
        let mut cur = self.range;
        let mut prev;
        let mut symbol = 0usize;
        loop {
            prev = cur;
            let f = (1u32 << 15) - cdf[symbol] as u32;
            cur = ((self.range >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT);
            cur += (EC_MIN_PROB * (n - symbol - 1)) as u32;
            if self.value >= cur {
                break;
            }
            symbol += 1;
        }
        self.range = prev - cur;
        self.value -= cur;
        self.renorm();
        if self.update {
            let cnt = cdf[n];
            let rate = 3 + (cnt > 15) as u32 + (cnt > 31) as u32 + (31 - (n as u32).leading_zeros()).min(2);
            let mut tmp = 0u32;
            for i in 0..n - 1 {
                if i == symbol {
                    tmp = 1 << 15;
                }
                let c = cdf[i] as u32;
                if tmp < c {
                    cdf[i] = (c - ((c - tmp) >> rate)) as u16;
                } else {
                    cdf[i] = (c + ((tmp - c) >> rate)) as u16;
                }
            }
            if cnt < 32 {
                cdf[n] += 1;
            }
        }
        symbol
    }
    #[inline]
    fn renorm(&mut self) {
        let bits = 15 - (31 - self.range.leading_zeros());
        self.range <<= bits;
        let num_bits = (bits as i64).min(self.max_bits.max(0)) as u32;
        let new_data = self.f(num_bits);
        let padded = new_data << (bits - num_bits);
        self.value = padded ^ (((self.value + 1) << bits) - 1);
        self.max_bits -= bits as i64;
    }
    fn bool(&mut self) -> u32 {
        let mut cdf = [1u16 << 14, 1 << 15, 0];
        let save = self.update;
        self.update = false;
        let s = self.read(&mut cdf);
        self.update = save;
        s as u32
    }
    fn lit(&mut self, n: u32) -> u32 {
        let mut x = 0;
        for _ in 0..n {
            x = 2 * x + self.bool();
        }
        x
    }
    fn ns(&mut self, n: u32) -> u32 {
        let w = 32 - n.leading_zeros();
        let m = (1 << w) - n;
        let v = self.lit(w - 1);
        if v < m {
            return v;
        }
        (v << 1) - m + self.lit(1)
    }
}

// ================================================================= CDFs

macro_rules! cdfs {
    ($($name:ident = $default:ident),* ; coeff: $($cname:ident = $cdefault:ident),* ; per_comp: $($pname:ident = $pdefault:ident),*) => {
        #[derive(Clone, Default)]
        struct Cdfs { $($name: Vec<u16>,)* $($cname: Vec<u16>,)* $($pname: Vec<u16>,)* delta_lf_multi: Vec<u16> }
        impl Cdfs {
            fn new(base_q_idx: u32) -> Cdfs {
                let idx = if base_q_idx <= 20 { 0 } else if base_q_idx <= 60 { 1 } else if base_q_idx <= 120 { 2 } else { 3 };
                Cdfs {
                    $($name: $default.to_vec(),)*
                    $($cname: { let l = $cdefault.len() / 4; $cdefault[idx * l..(idx + 1) * l].to_vec() },)*
                    $($pname: $pdefault.repeat(2),)*
                    delta_lf_multi: [DEFAULT_DELTA_LF_CDF; FRAME_LF_COUNT].concat(),
                }
            }
        }
    };
}

cdfs!(
    intra_frame_y_mode = DEFAULT_INTRA_FRAME_Y_MODE_CDF,
    uv_mode_cfl_not_allowed = DEFAULT_UV_MODE_CFL_NOT_ALLOWED_CDF,
    uv_mode_cfl_allowed = DEFAULT_UV_MODE_CFL_ALLOWED_CDF,
    angle_delta = DEFAULT_ANGLE_DELTA_CDF,
    intrabc = DEFAULT_INTRABC_CDF,
    partition_w8 = DEFAULT_PARTITION_W8_CDF,
    partition_w16 = DEFAULT_PARTITION_W16_CDF,
    partition_w32 = DEFAULT_PARTITION_W32_CDF,
    partition_w64 = DEFAULT_PARTITION_W64_CDF,
    partition_w128 = DEFAULT_PARTITION_W128_CDF,
    segment_id = DEFAULT_SEGMENT_ID_CDF,
    tx_8x8 = DEFAULT_TX_8X8_CDF,
    tx_16x16 = DEFAULT_TX_16X16_CDF,
    tx_32x32 = DEFAULT_TX_32X32_CDF,
    tx_64x64 = DEFAULT_TX_64X64_CDF,
    filter_intra_mode = DEFAULT_FILTER_INTRA_MODE_CDF,
    filter_intra = DEFAULT_FILTER_INTRA_CDF,
    skip = DEFAULT_SKIP_CDF,
    palette_y_mode = DEFAULT_PALETTE_Y_MODE_CDF,
    palette_uv_mode = DEFAULT_PALETTE_UV_MODE_CDF,
    palette_y_size = DEFAULT_PALETTE_Y_SIZE_CDF,
    palette_uv_size = DEFAULT_PALETTE_UV_SIZE_CDF,
    pal2y = DEFAULT_PALETTE_SIZE_2_Y_COLOR_CDF,
    pal3y = DEFAULT_PALETTE_SIZE_3_Y_COLOR_CDF,
    pal4y = DEFAULT_PALETTE_SIZE_4_Y_COLOR_CDF,
    pal5y = DEFAULT_PALETTE_SIZE_5_Y_COLOR_CDF,
    pal6y = DEFAULT_PALETTE_SIZE_6_Y_COLOR_CDF,
    pal7y = DEFAULT_PALETTE_SIZE_7_Y_COLOR_CDF,
    pal8y = DEFAULT_PALETTE_SIZE_8_Y_COLOR_CDF,
    pal2uv = DEFAULT_PALETTE_SIZE_2_UV_COLOR_CDF,
    pal3uv = DEFAULT_PALETTE_SIZE_3_UV_COLOR_CDF,
    pal4uv = DEFAULT_PALETTE_SIZE_4_UV_COLOR_CDF,
    pal5uv = DEFAULT_PALETTE_SIZE_5_UV_COLOR_CDF,
    pal6uv = DEFAULT_PALETTE_SIZE_6_UV_COLOR_CDF,
    pal7uv = DEFAULT_PALETTE_SIZE_7_UV_COLOR_CDF,
    pal8uv = DEFAULT_PALETTE_SIZE_8_UV_COLOR_CDF,
    delta_q = DEFAULT_DELTA_Q_CDF,
    delta_lf = DEFAULT_DELTA_LF_CDF,
    intra_tx_type_set1 = DEFAULT_INTRA_TX_TYPE_SET1_CDF,
    intra_tx_type_set2 = DEFAULT_INTRA_TX_TYPE_SET2_CDF,
    cfl_sign = DEFAULT_CFL_SIGN_CDF,
    cfl_alpha = DEFAULT_CFL_ALPHA_CDF,
    use_wiener = DEFAULT_USE_WIENER_CDF,
    use_sgrproj = DEFAULT_USE_SGRPROJ_CDF,
    restoration_type = DEFAULT_RESTORATION_TYPE_CDF,
    inter_tx_type_set1 = DEFAULT_INTER_TX_TYPE_SET1_CDF,
    inter_tx_type_set2 = DEFAULT_INTER_TX_TYPE_SET2_CDF,
    inter_tx_type_set3 = DEFAULT_INTER_TX_TYPE_SET3_CDF,
    txfm_split = DEFAULT_TXFM_SPLIT_CDF,
    mv_joint = DEFAULT_MV_JOINT_CDF,
    mv_class = DEFAULT_MV_CLASS_CDF
    ; coeff:
    txb_skip = DEFAULT_TXB_SKIP_CDF,
    eob_pt_16 = DEFAULT_EOB_PT_16_CDF,
    eob_pt_32 = DEFAULT_EOB_PT_32_CDF,
    eob_pt_64 = DEFAULT_EOB_PT_64_CDF,
    eob_pt_128 = DEFAULT_EOB_PT_128_CDF,
    eob_pt_256 = DEFAULT_EOB_PT_256_CDF,
    eob_pt_512 = DEFAULT_EOB_PT_512_CDF,
    eob_pt_1024 = DEFAULT_EOB_PT_1024_CDF,
    eob_extra = DEFAULT_EOB_EXTRA_CDF,
    dc_sign = DEFAULT_DC_SIGN_CDF,
    coeff_base_eob = DEFAULT_COEFF_BASE_EOB_CDF,
    coeff_base = DEFAULT_COEFF_BASE_CDF,
    coeff_br = DEFAULT_COEFF_BR_CDF
    ; per_comp:
    mv_sign = DEFAULT_MV_SIGN_CDF,
    mv_class0_bit = DEFAULT_MV_CLASS0_BIT_CDF,
    mv_bit = DEFAULT_MV_BIT_CDF
);

/// Read a symbol using `len` entries of a CDF table starting at `off`.
macro_rules! sym {
    ($s:expr, $table:ident, $off:expr, $len:expr) => {{
        let o = $off;
        $s.sd.read(&mut $s.cdf.$table[o..o + $len])
    }};
}

// ================================================================ tables

#[inline]
fn bw4(bs: usize) -> usize {
    NUM_4X4_BLOCKS_WIDE[bs] as usize
}
#[inline]
fn bh4(bs: usize) -> usize {
    NUM_4X4_BLOCKS_HIGH[bs] as usize
}
#[inline]
fn block_width(bs: usize) -> usize {
    bw4(bs) * 4
}
#[inline]
fn block_height(bs: usize) -> usize {
    bh4(bs) * 4
}
#[inline]
fn tx_w(t: usize) -> usize {
    TX_WIDTH[t] as usize
}
#[inline]
fn tx_h(t: usize) -> usize {
    TX_HEIGHT[t] as usize
}

/// Scan orders as u16, indexed [kind][txSz] (kind 0 default, 1 mrow, 2 mcol).
fn scans() -> &'static [Vec<Vec<u16>>; 3] {
    static S: OnceLock<[Vec<Vec<u16>>; 3]> = OnceLock::new();
    S.get_or_init(|| {
        let v8 = |a: &[u8]| a.iter().map(|&x| x as u16).collect::<Vec<u16>>();
        let mut def = vec![Vec::new(); TX_SIZES_ALL];
        def[TX_4X4] = v8(&DEFAULT_SCAN_4X4);
        def[TX_4X8] = v8(&DEFAULT_SCAN_4X8);
        def[TX_8X4] = v8(&DEFAULT_SCAN_8X4);
        def[TX_8X8] = v8(&DEFAULT_SCAN_8X8);
        def[TX_8X16] = v8(&DEFAULT_SCAN_8X16);
        def[TX_16X8] = v8(&DEFAULT_SCAN_16X8);
        def[TX_16X16] = v8(&DEFAULT_SCAN_16X16);
        def[TX_16X32] = DEFAULT_SCAN_16X32.to_vec();
        def[TX_32X16] = DEFAULT_SCAN_32X16.to_vec();
        def[TX_4X16] = v8(&DEFAULT_SCAN_4X16);
        def[TX_16X4] = v8(&DEFAULT_SCAN_16X4);
        def[TX_8X32] = v8(&DEFAULT_SCAN_8X32);
        def[TX_32X8] = v8(&DEFAULT_SCAN_32X8);
        def[TX_32X32] = DEFAULT_SCAN_32X32.to_vec();
        let mut mrow = vec![Vec::new(); TX_SIZES_ALL];
        let mut mcol = vec![Vec::new(); TX_SIZES_ALL];
        for (t, r, c) in [
            (TX_4X4, &MROW_SCAN_4X4[..], &MCOL_SCAN_4X4[..]),
            (TX_4X8, &MROW_SCAN_4X8[..], &MCOL_SCAN_4X8[..]),
            (TX_8X4, &MROW_SCAN_8X4[..], &MCOL_SCAN_8X4[..]),
            (TX_8X8, &MROW_SCAN_8X8[..], &MCOL_SCAN_8X8[..]),
            (TX_8X16, &MROW_SCAN_8X16[..], &MCOL_SCAN_8X16[..]),
            (TX_16X8, &MROW_SCAN_16X8[..], &MCOL_SCAN_16X8[..]),
            (TX_16X16, &MROW_SCAN_16X16[..], &MCOL_SCAN_16X16[..]),
            (TX_4X16, &MROW_SCAN_4X16[..], &MCOL_SCAN_4X16[..]),
            (TX_16X4, &MROW_SCAN_16X4[..], &MCOL_SCAN_16X4[..]),
        ] {
            mrow[t] = v8(r);
            mcol[t] = v8(c);
        }
        [def, mrow, mcol]
    })
}

// ============================================================ structures

/// A decoded frame: planes of `BitDepth`-bit samples.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub bit_depth: u32,
    pub sub_x: usize,
    pub sub_y: usize,
    pub mono: bool,
    /// Y, U, V planes (U/V empty for monochrome), tightly packed.
    pub planes: [Vec<u16>; 3],
    pub color_primaries: u32,
    pub transfer: u32,
    pub matrix: u32,
    pub full_range: bool,
}

#[derive(Default, Clone)]
struct Seq {
    profile: u32,
    reduced: bool,
    frame_width_bits: u32,
    frame_height_bits: u32,
    max_w: u32,
    max_h: u32,
    frame_id_numbers_present: bool,
    delta_frame_id_length_minus_2: u32,
    additional_frame_id_length_minus_1: u32,
    use_128: bool,
    enable_filter_intra: bool,
    enable_intra_edge_filter: bool,
    enable_order_hint: bool,
    order_hint_bits: u32,
    force_screen_content_tools: u32,
    force_integer_mv: u32,
    enable_superres: bool,
    enable_cdef: bool,
    enable_restoration: bool,
    bit_depth: u32,
    mono: bool,
    color_primaries: u32,
    transfer: u32,
    matrix: u32,
    full_range: bool,
    sub_x: usize,
    sub_y: usize,
    separate_uv_delta_q: bool,
    film_grain_present: bool,
    decoder_model_info_present: bool,
    equal_picture_interval: bool,
    frame_presentation_time_length: u32,
    buffer_removal_time_length: u32,
    op_idc: Vec<u32>,
    decoder_model_present_for_op: Vec<bool>,
}

fn parse_seq(b: &mut Bits) -> Option<Seq> {
    let mut s = Seq { profile: b.f(3), ..Default::default() };
    b.f(1); // still_picture
    s.reduced = b.f(1) == 1;
    if s.reduced {
        b.f(5);
        s.op_idc = vec![0];
        s.decoder_model_present_for_op = vec![false];
    } else {
        let timing = b.f(1) == 1;
        if timing {
            b.f(32);
            b.f(32);
            s.equal_picture_interval = b.f(1) == 1;
            if s.equal_picture_interval {
                b.uvlc();
            }
            s.decoder_model_info_present = b.f(1) == 1;
            if s.decoder_model_info_present {
                let buffer_delay_length = b.f(5) + 1;
                b.f(32);
                s.buffer_removal_time_length = b.f(5) + 1;
                s.frame_presentation_time_length = b.f(5) + 1;
                s.frame_presentation_time_length |= buffer_delay_length << 16;
            }
        }
        let initial_display_delay_present = b.f(1) == 1;
        let cnt = b.f(5) + 1;
        for _ in 0..cnt {
            s.op_idc.push(b.f(12));
            let lvl = b.f(5);
            if lvl > 7 {
                b.f(1);
            }
            let mut present = false;
            if s.decoder_model_info_present {
                present = b.f(1) == 1;
                if present {
                    let n = s.frame_presentation_time_length >> 16;
                    b.f(n);
                    b.f(n);
                    b.f(1);
                }
            }
            s.decoder_model_present_for_op.push(present);
            if initial_display_delay_present && b.f(1) == 1 {
                b.f(4);
            }
        }
        s.frame_presentation_time_length &= 0xFFFF;
    }
    s.frame_width_bits = b.f(4) + 1;
    s.frame_height_bits = b.f(4) + 1;
    s.max_w = b.f(s.frame_width_bits) + 1;
    s.max_h = b.f(s.frame_height_bits) + 1;
    if !s.reduced {
        s.frame_id_numbers_present = b.f(1) == 1;
    }
    if s.frame_id_numbers_present {
        s.delta_frame_id_length_minus_2 = b.f(4);
        s.additional_frame_id_length_minus_1 = b.f(3);
    }
    s.use_128 = b.f(1) == 1;
    s.enable_filter_intra = b.f(1) == 1;
    s.enable_intra_edge_filter = b.f(1) == 1;
    if s.reduced {
        s.force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS as u32;
        s.force_integer_mv = SELECT_INTEGER_MV as u32;
    } else {
        b.f(4); // interintra, masked compound, warped motion, dual filter
        s.enable_order_hint = b.f(1) == 1;
        if s.enable_order_hint {
            b.f(2); // jnt_comp, ref_frame_mvs
        }
        s.force_screen_content_tools = if b.f(1) == 1 { SELECT_SCREEN_CONTENT_TOOLS as u32 } else { b.f(1) };
        s.force_integer_mv = if s.force_screen_content_tools > 0 {
            if b.f(1) == 1 { SELECT_INTEGER_MV as u32 } else { b.f(1) }
        } else {
            SELECT_INTEGER_MV as u32
        };
        if s.enable_order_hint {
            s.order_hint_bits = b.f(3) + 1;
        }
    }
    s.enable_superres = b.f(1) == 1;
    s.enable_cdef = b.f(1) == 1;
    s.enable_restoration = b.f(1) == 1;
    // color_config
    let high_bitdepth = b.f(1) == 1;
    s.bit_depth = if s.profile == 2 && high_bitdepth {
        if b.f(1) == 1 { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    s.mono = if s.profile == 1 { false } else { b.f(1) == 1 };
    let desc = b.f(1) == 1;
    if desc {
        s.color_primaries = b.f(8);
        s.transfer = b.f(8);
        s.matrix = b.f(8);
    } else {
        s.color_primaries = 2;
        s.transfer = 2;
        s.matrix = 2;
    }
    if s.mono {
        s.full_range = b.f(1) == 1;
        s.sub_x = 1;
        s.sub_y = 1;
    } else if s.color_primaries == 1 && s.transfer == 13 && s.matrix == 0 {
        s.full_range = true;
    } else {
        s.full_range = b.f(1) == 1;
        if s.profile == 0 {
            s.sub_x = 1;
            s.sub_y = 1;
        } else if s.profile == 1 {
        } else if s.bit_depth == 12 {
            s.sub_x = b.f(1) as usize;
            if s.sub_x == 1 {
                s.sub_y = b.f(1) as usize;
            }
        } else {
            s.sub_x = 1;
        }
        if s.sub_x == 1 && s.sub_y == 1 {
            b.f(2); // chroma_sample_position
        }
    }
    if !s.mono {
        s.separate_uv_delta_q = b.f(1) == 1;
    }
    s.film_grain_present = b.f(1) == 1;
    Some(s)
}

/// Film grain parameters (synthesis is applied to the output).
#[derive(Default, Clone)]
struct Grain {
    apply: bool,
    seed: u32,
    y_points: Vec<(i32, i32)>,
    chroma_scaling_from_luma: bool,
    cb_points: Vec<(i32, i32)>,
    cr_points: Vec<(i32, i32)>,
    scaling_shift: i32,
    ar_coeff_lag: i32,
    ar_y: Vec<i32>,
    ar_cb: Vec<i32>,
    ar_cr: Vec<i32>,
    ar_shift: i32,
    grain_scale_shift: i32,
    cb_mult: i32,
    cb_luma_mult: i32,
    cb_offset: i32,
    cr_mult: i32,
    cr_luma_mult: i32,
    cr_offset: i32,
    overlap: bool,
    clip_restricted: bool,
}

// ============================================================= decoder

/// All decoder state, named after the specification's variables.
#[derive(Default)]
struct Dec {
    seq: Seq,
    num_planes: usize,
    // ---- frame header
    frame_is_intra: bool,
    show_frame: bool,
    showable_frame: bool,
    disable_cdf_update: bool,
    allow_screen_content_tools: bool,
    frame_width: usize,
    frame_height: usize,
    upscaled_width: usize,
    use_superres: bool,
    superres_denom: usize,
    mi_cols: usize,
    mi_rows: usize,
    allow_intrabc: bool,
    tile_cols_log2: u32,
    tile_rows_log2: u32,
    mi_col_starts: Vec<usize>,
    mi_row_starts: Vec<usize>,
    tile_size_bytes: usize,
    base_q_idx: i32,
    delta_q_y_dc: i32,
    delta_q_u_dc: i32,
    delta_q_u_ac: i32,
    delta_q_v_dc: i32,
    delta_q_v_ac: i32,
    using_qmatrix: bool,
    qm: [usize; 3],
    segmentation_enabled: bool,
    feature_enabled: [[bool; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_data: [[i32; SEG_LVL_MAX]; MAX_SEGMENTS],
    seg_id_pre_skip: bool,
    last_active_seg_id: usize,
    delta_q_present: bool,
    delta_q_res: u32,
    delta_lf_present: bool,
    delta_lf_res: u32,
    delta_lf_multi: bool,
    lossless_array: [bool; MAX_SEGMENTS],
    coded_lossless: bool,
    all_lossless: bool,
    seg_qm_level: [[usize; MAX_SEGMENTS]; 3],
    loop_filter_level: [i32; 4],
    loop_filter_sharpness: i32,
    loop_filter_delta_enabled: bool,
    loop_filter_ref_deltas: [i32; 8],
    loop_filter_mode_deltas: [i32; 2],
    cdef_damping: i32,
    cdef_bits: u32,
    cdef_y_pri: [i32; 8],
    cdef_y_sec: [i32; 8],
    cdef_uv_pri: [i32; 8],
    cdef_uv_sec: [i32; 8],
    frame_restoration_type: [usize; 3],
    loop_restoration_size: [usize; 3],
    uses_lr: bool,
    tx_mode_select: bool,
    tx_mode_only4x4: bool,
    reduced_tx_set: bool,
    grain: Grain,

    // ---- frame buffers (CurrFrame) with padding to the right and below
    cur: [Vec<u16>; 3],
    stride: [usize; 3],
    // ---- per mode-info (4x4) arrays, MiRows * MiCols
    y_modes: Vec<u8>,
    uv_modes: Vec<u8>,
    skips: Vec<u8>,
    is_inters: Vec<u8>,
    mi_sizes: Vec<u8>,
    segment_ids: Vec<u8>,
    inter_tx_sizes: Vec<u8>,
    tx_types: Vec<u8>,
    palette_sizes: [Vec<u8>; 2],
    palette_colors: [Vec<u16>; 2],
    delta_lfs: Vec<[i8; 4]>,
    /// LoopfilterTxSizes per plane, indexed by (row >> subY, col >> subX) on the MI grid.
    lf_tx_sizes: [Vec<u8>; 3],
    cdef_idx: Vec<i8>,
    cdef_cols: usize,
    lr_unit_rows: [usize; 3],
    lr_unit_cols: [usize; 3],
    lr_type: [Vec<u8>; 3],
    lr_wiener: [Vec<[[i32; 3]; 2]>; 3],
    lr_sgr_set: [Vec<u8>; 3],
    lr_sgr_xqd: [Vec<[i32; 2]>; 3],

    // ---- tile state
    sd: Sym,
    cdf: Cdfs,
    mi_row_start: usize,
    mi_row_end: usize,
    mi_col_start: usize,
    mi_col_end: usize,
    current_q_index: i32,
    delta_lf: [i32; 4],
    ref_sgr_xqd: [[i32; 2]; 3],
    ref_lr_wiener: [[[i32; 3]; 2]; 3],
    above_level: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_level: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    /// BlockDecoded[plane][y][x] for y, x in -1..=32, stored with offset 1.
    block_decoded: [Vec<bool>; 3],
    read_deltas: bool,

    // ---- block state
    mi_row: usize,
    mi_col: usize,
    mi_size: usize,
    has_chroma: bool,
    avail_u: bool,
    avail_l: bool,
    avail_u_chroma: bool,
    avail_l_chroma: bool,
    skip: bool,
    segment_id: usize,
    lossless: bool,
    use_intrabc: bool,
    y_mode: usize,
    uv_mode: usize,
    angle_delta_y: i32,
    angle_delta_uv: i32,
    cfl_alpha_u: i32,
    cfl_alpha_v: i32,
    use_filter_intra: bool,
    filter_intra_mode: usize,
    palette_size_y: usize,
    palette_size_uv: usize,
    palette_colors_y: [u16; 8],
    palette_colors_u: [u16; 8],
    palette_colors_v: [u16; 8],
    color_map_y: Vec<u8>,
    color_map_uv: Vec<u8>,
    tx_size: usize,
    max_luma_w: usize,
    max_luma_h: usize,
    plane_tx_type: usize,
    quant: Vec<i32>,
    // ---- intra block copy
    is_inter: bool,
    mv: [i32; 2],
    mvs: Vec<[i32; 2]>,
    /// Mode info has been written for this frame (RefFrames "has been written").
    written: Vec<bool>,
    ref_stack: [[i32; 2]; MAX_REF_MV_STACK_SIZE + 1],
    weight_stack: [u32; MAX_REF_MV_STACK_SIZE + 1],
    num_mv_found: usize,
    found_match: bool,
}

/// Stride of the palette colour maps.
const CMAP: usize = 72;
/// Stride of BlockDecoded (indices -1..=32 plus slack).
const BD: usize = 36;

impl Dec {
    #[inline]
    fn mi(&self, row: usize, col: usize) -> usize {
        row * self.mi_cols + col
    }
    #[inline]
    fn bd_idx(y: isize, x: isize) -> usize {
        ((y + 1) as usize) * BD + (x + 1) as usize
    }
    fn sub(&self, plane: usize) -> (usize, usize) {
        if plane == 0 { (0, 0) } else { (self.seq.sub_x, self.seq.sub_y) }
    }
    fn is_inside(&self, r: isize, c: isize) -> bool {
        c >= self.mi_col_start as isize && c < self.mi_col_end as isize && r >= self.mi_row_start as isize && r < self.mi_row_end as isize
    }
    fn bit_depth(&self) -> u32 {
        self.seq.bit_depth
    }
    #[inline]
    fn clip1(&self, v: i32) -> u16 {
        v.clamp(0, (1 << self.seq.bit_depth) - 1) as u16
    }
}

/// Decode the first shown frame of an AV1 bitstream (a sequence of OBUs).
pub fn decode(data: &[u8]) -> Option<Frame> {
    let mut dec = Dec::default();
    let mut have_seq = false;
    let mut pos = 0;
    let mut tile_num = 0usize;
    while pos < data.len() {
        let h = data[pos];
        pos += 1;
        let obu_type = (h >> 3) & 15;
        let ext = (h >> 2) & 1 == 1;
        let has_size = (h >> 1) & 1 == 1;
        if ext {
            pos += 1;
        }
        let size = if has_size { leb128(data, &mut pos)? } else { data.len() - pos };
        let payload = data.get(pos..pos.checked_add(size)?)?;
        pos += size;
        match obu_type {
            1 => {
                dec.seq = parse_seq(&mut Bits::new(payload))?;
                dec.num_planes = if dec.seq.mono { 1 } else { 3 };
                have_seq = true;
            }
            3 | 6 if have_seq => {
                let mut b = Bits::new(payload);
                dec.uncompressed_header(&mut b)?;
                dec.setup_frame();
                tile_num = 0;
                if obu_type == 6 {
                    b.byte_align();
                    let rest = &payload[b.pos / 8..];
                    if dec.tile_group(rest, &mut tile_num)? {
                        return dec.finish();
                    }
                }
            }
            4 if have_seq => {
                if dec.tile_group(payload, &mut tile_num)? {
                    return dec.finish();
                }
            }
            _ => {}
        }
    }
    None
}

mod filter;
mod grain;
mod header;
mod intrabc;
mod predict;
mod recon;
mod tile;
