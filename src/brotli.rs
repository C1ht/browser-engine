//! Brotli decompressor (RFC 7932), used for WOFF2 fonts.

use crate::brotli_tables::*;

static DICT: &[u8; 122784] = include_bytes!("../assets/brotli-dictionary.bin");

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u64,
    n: u32,
}

impl Bits<'_> {
    #[inline]
    fn refill(&mut self) {
        while self.n <= 56 {
            let b = *self.d.get(self.pos).unwrap_or(&0);
            self.pos += 1;
            self.buf |= (b as u64) << self.n;
            self.n += 8;
        }
    }
    #[inline]
    fn peek(&mut self, k: u32) -> u32 {
        if self.n < k {
            self.refill();
        }
        (self.buf & ((1u64 << k) - 1)) as u32
    }
    #[inline]
    fn skip(&mut self, k: u32) {
        self.buf >>= k;
        self.n -= k;
    }
    #[inline]
    fn bits(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let v = self.peek(k);
        self.skip(k);
        v
    }
    fn align(&mut self) {
        let r = self.n % 8;
        self.skip(r);
    }
    fn overrun(&self) -> bool {
        self.pos > self.d.len() + 16
    }
}

/// A prefix code: 8-bit fast table plus canonical fallback for long codes.
struct Huff {
    single: Option<u16>,
    fast: Vec<u32>, // (symbol << 8) | length, 0 = slow path
    count: [u16; 16],
    symbols: Vec<u16>,
}

fn reverse(mut c: u32, n: u32) -> u32 {
    let mut r = 0;
    for _ in 0..n {
        r = (r << 1) | (c & 1);
        c >>= 1;
    }
    r
}

impl Huff {
    fn new(lens: &[u8]) -> Option<Huff> {
        let nonzero: Vec<usize> = (0..lens.len()).filter(|&i| lens[i] > 0).collect();
        if nonzero.len() == 1 {
            return Some(Huff { single: Some(nonzero[0] as u16), fast: Vec::new(), count: [0; 16], symbols: Vec::new() });
        }
        if nonzero.is_empty() {
            return None;
        }
        let mut count = [0u16; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut symbols = Vec::with_capacity(nonzero.len());
        for l in 1..16u8 {
            for (s, &x) in lens.iter().enumerate() {
                if x == l {
                    symbols.push(s as u16);
                }
            }
        }
        let mut fast = vec![0u32; 256];
        let mut code = 0u32;
        let mut k = 0;
        for len in 1..16u32 {
            for _ in 0..count[len as usize] {
                if len <= 8 {
                    let r = reverse(code, len) as usize;
                    let mut i = r;
                    while i < 256 {
                        fast[i] = (symbols[k] as u32) << 8 | len;
                        i += 1 << len;
                    }
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Some(Huff { single: None, fast, count, symbols })
    }

    #[inline]
    fn decode(&self, b: &mut Bits) -> Option<u32> {
        if let Some(s) = self.single {
            return Some(s as u32);
        }
        let e = self.fast[b.peek(8) as usize];
        if e != 0 {
            b.skip(e & 0xFF);
            return Some(e >> 8);
        }
        // Canonical decode, one bit at a time.
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.bits(1) as i32;
            let c = self.count[len] as i32;
            if code - first < c {
                return self.symbols.get((index + code - first) as usize).map(|&s| s as u32);
            }
            index += c;
            first = (first + c) << 1;
            code <<= 1;
        }
        None
    }
}

fn read_prefix_code(b: &mut Bits, alphabet: usize) -> Option<Huff> {
    let mut lens = vec![0u8; alphabet];
    let hskip = b.bits(2);
    if hskip == 1 {
        let abits = 32 - ((alphabet - 1) as u32).leading_zeros();
        let nsym = b.bits(2) as usize + 1;
        let mut syms = Vec::with_capacity(nsym);
        for _ in 0..nsym {
            let s = b.bits(abits) as usize;
            if s >= alphabet {
                return None;
            }
            syms.push(s);
        }
        match nsym {
            1 => lens[syms[0]] = 1,
            2 => {
                lens[syms[0]] = 1;
                lens[syms[1]] = 1;
            }
            3 => {
                lens[syms[0]] = 1;
                lens[syms[1]] = 2;
                lens[syms[2]] = 2;
            }
            _ => {
                if b.bits(1) == 0 {
                    syms.iter().for_each(|&s| lens[s] = 2);
                } else {
                    lens[syms[0]] = 1;
                    lens[syms[1]] = 2;
                    lens[syms[2]] = 3;
                    lens[syms[3]] = 3;
                }
            }
        }
        return Huff::new(&lens);
    }
    // Complex prefix code.
    const ORDER: [usize; 18] = [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    const CL_LEN: [u8; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];
    const CL_VAL: [u8; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];
    let mut cl = [0u8; 18];
    let mut space = 32i32;
    let mut num = 0;
    for &o in ORDER.iter().skip(hskip as usize) {
        let p = b.peek(4) as usize;
        b.skip(CL_LEN[p] as u32);
        let v = CL_VAL[p];
        cl[o] = v;
        if v != 0 {
            space -= 32 >> v;
            num += 1;
            if space <= 0 {
                break;
            }
        }
    }
    if num != 1 && space != 0 {
        return None;
    }
    let clh = Huff::new(&cl)?;
    let (mut sym, mut prev, mut repeat, mut repeat_len, mut space) = (0usize, 8u8, 0u32, 0u8, 32768i32);
    while sym < alphabet && space > 0 {
        let c = clh.decode(b)?;
        if c < 16 {
            repeat = 0;
            lens[sym] = c as u8;
            if c != 0 {
                prev = c as u8;
                space -= 32768 >> c;
            }
            sym += 1;
        } else {
            let extra = if c == 16 { 2 } else { 3 };
            let new_len = if c == 16 { prev } else { 0 };
            if repeat_len != new_len {
                repeat = 0;
                repeat_len = new_len;
            }
            let old = repeat;
            if repeat > 0 {
                repeat = (repeat - 2) << extra;
            }
            repeat += b.bits(extra) + 3;
            let delta = (repeat - old) as usize;
            if sym + delta > alphabet {
                return None;
            }
            for _ in 0..delta {
                lens[sym] = repeat_len;
                sym += 1;
            }
            if repeat_len != 0 {
                space -= (delta as i32) << (15 - repeat_len);
            }
        }
        if b.overrun() {
            return None;
        }
    }
    Huff::new(&lens)
}

fn var_u8(b: &mut Bits) -> u32 {
    if b.bits(1) == 0 {
        return 0;
    }
    let n = b.bits(3);
    if n == 0 { 1 } else { (1 << n) + b.bits(n) }
}

const BLOCK_LEN: [(u32, u32); 26] = [
    (1, 2), (5, 2), (9, 2), (13, 2), (17, 3), (25, 3), (33, 3), (41, 3), (49, 4), (65, 4), (81, 4), (97, 4), (113, 5),
    (145, 5), (177, 5), (209, 5), (241, 6), (305, 6), (369, 7), (497, 8), (753, 9), (1265, 10), (2289, 11), (4337, 12),
    (8433, 13), (16625, 24),
];
const INSERT: [(u32, u32); 24] = [
    (0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0), (6, 1), (8, 1), (10, 2), (14, 2), (18, 3), (26, 3), (34, 4), (50, 4),
    (66, 5), (98, 5), (130, 6), (194, 7), (322, 8), (578, 9), (1090, 10), (2114, 12), (6210, 14), (22594, 24),
];
const COPY: [(u32, u32); 24] = [
    (2, 0), (3, 0), (4, 0), (5, 0), (6, 0), (7, 0), (8, 0), (9, 0), (10, 1), (12, 1), (14, 2), (18, 2), (22, 3), (30, 3),
    (38, 4), (54, 4), (70, 5), (102, 5), (134, 6), (198, 7), (326, 8), (582, 9), (1094, 10), (2118, 24),
];
/// (insert code offset, copy code offset) per 64-command cell.
const CELLS: [(u32, u32); 11] = [(0, 0), (0, 8), (0, 0), (0, 8), (8, 0), (8, 8), (0, 16), (16, 0), (8, 16), (16, 8), (16, 16)];

/// Block-type switching state for one category (literal, command, distance).
struct Blocks {
    n: u32,
    types: Option<Huff>,
    lens: Option<Huff>,
    cur: u32,
    prev: u32,
    left: u32,
}

impl Blocks {
    fn read(b: &mut Bits) -> Option<Blocks> {
        let n = var_u8(b) + 1;
        if n < 2 {
            return Some(Blocks { n, types: None, lens: None, cur: 0, prev: 1, left: u32::MAX });
        }
        let types = read_prefix_code(b, n as usize + 2)?;
        let lens = read_prefix_code(b, 26)?;
        let left = block_len(b, &lens)?;
        Some(Blocks { n, types: Some(types), lens: Some(lens), cur: 0, prev: 1, left })
    }
    #[inline]
    fn step(&mut self, b: &mut Bits) -> Option<()> {
        if self.left == 0 {
            let t = self.types.as_ref()?.decode(b)?;
            let next = match t {
                0 => self.prev,
                1 => (self.cur + 1) % self.n,
                t => t - 2,
            };
            self.prev = self.cur;
            self.cur = next;
            self.left = block_len(b, self.lens.as_ref()?)?;
        }
        self.left -= 1;
        Some(())
    }
}

fn block_len(b: &mut Bits, h: &Huff) -> Option<u32> {
    let (base, extra) = *BLOCK_LEN.get(h.decode(b)? as usize)?;
    Some(base + b.bits(extra))
}

fn context_map(b: &mut Bits, size: usize, ntrees: u32) -> Option<Vec<u8>> {
    let mut map = vec![0u8; size];
    if ntrees < 2 {
        return Some(map);
    }
    let rlemax = if b.bits(1) == 1 { b.bits(4) + 1 } else { 0 };
    let h = read_prefix_code(b, (ntrees + rlemax) as usize)?;
    let mut i = 0;
    while i < size {
        let c = h.decode(b)?;
        if c == 0 {
            map[i] = 0;
            i += 1;
        } else if c <= rlemax {
            let run = (1 << c) + b.bits(c);
            for _ in 0..run {
                if i >= size {
                    return None;
                }
                map[i] = 0;
                i += 1;
            }
        } else {
            map[i] = (c - rlemax) as u8;
            i += 1;
        }
        if b.overrun() {
            return None;
        }
    }
    if b.bits(1) == 1 {
        // Inverse move-to-front.
        let mut mtf: Vec<u8> = (0..=255).collect();
        for v in map.iter_mut() {
            let idx = *v as usize;
            let val = mtf[idx];
            *v = val;
            mtf.remove(idx);
            mtf.insert(0, val);
        }
    }
    Some(map)
}

fn ferment(w: &mut [u8], p: usize) -> usize {
    let c = w[p];
    if c < 0xC0 {
        if c.is_ascii_lowercase() {
            w[p] ^= 32;
        }
        1
    } else if c < 0xE0 {
        if p + 1 < w.len() {
            w[p + 1] ^= 32;
        }
        2
    } else {
        if p + 2 < w.len() {
            w[p + 2] ^= 5;
        }
        3
    }
}

fn dictionary_word(len: usize, word_id: usize, out: &mut Vec<u8>) -> Option<()> {
    let nd = NDBITS[len] as usize;
    if nd == 0 {
        return None;
    }
    let mut offset = 0;
    for l in 0..len {
        if NDBITS[l] > 0 {
            offset += l << NDBITS[l];
        }
    }
    let idx = word_id & ((1 << nd) - 1);
    let t = word_id >> nd;
    let (prefix, kind, suffix) = *TRANSFORMS.get(t)?;
    let start = offset + idx * len;
    let mut w: Vec<u8> = DICT.get(start..start + len)?.to_vec();
    match kind {
        3..=11 => {
            let k = (kind - 2) as usize;
            w.drain(..k.min(w.len()));
        }
        12..=20 => {
            let k = (kind - 11) as usize;
            w.truncate(w.len().saturating_sub(k));
        }
        _ => {}
    }
    if kind == 1 && !w.is_empty() {
        ferment(&mut w, 0);
    } else if kind == 2 {
        let mut i = 0;
        while i < w.len() {
            i += ferment(&mut w, i);
        }
    }
    out.extend_from_slice(prefix);
    out.extend_from_slice(&w);
    out.extend_from_slice(suffix);
    Some(())
}

/// Decompress a Brotli stream.
pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut b = Bits { d: data, pos: 0, buf: 0, n: 0 };
    let wbits = if b.bits(1) == 0 {
        16
    } else {
        let n = b.bits(3);
        if n != 0 {
            17 + n
        } else {
            match b.bits(3) {
                0 => 17,
                1 => return None,
                m => 8 + m,
            }
        }
    };
    let max_back = (1usize << wbits) - 16;
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 4);
    let mut dist_rb: [usize; 4] = [16, 15, 11, 4];
    let mut rb_idx = 0usize;
    loop {
        let last = b.bits(1) == 1;
        if last && b.bits(1) == 1 {
            break;
        }
        let mnibbles = match b.bits(2) {
            3 => 0,
            v => v + 4,
        };
        if mnibbles == 0 {
            if b.bits(1) != 0 {
                return None;
            }
            let nbytes = b.bits(2);
            let mut skip = 0usize;
            for i in 0..nbytes {
                skip |= (b.bits(8) as usize) << (8 * i);
            }
            if nbytes > 0 {
                skip += 1;
            }
            b.align();
            for _ in 0..skip {
                b.bits(8);
            }
            if last {
                break;
            }
            continue;
        }
        let mlen = b.bits(mnibbles * 4) as usize + 1;
        if !last && b.bits(1) == 1 {
            b.align();
            for _ in 0..mlen {
                out.push(b.bits(8) as u8);
            }
            if b.overrun() {
                return None;
            }
            continue;
        }
        let mut bl = Blocks::read(&mut b)?;
        let mut bi = Blocks::read(&mut b)?;
        let mut bd = Blocks::read(&mut b)?;
        let npostfix = b.bits(2);
        let ndirect = b.bits(4) << npostfix;
        let modes: Vec<u8> = (0..bl.n).map(|_| b.bits(2) as u8).collect();
        let ntreesl = var_u8(&mut b) + 1;
        let cmap_l = context_map(&mut b, 64 * bl.n as usize, ntreesl)?;
        let ntreesd = var_u8(&mut b) + 1;
        let cmap_d = context_map(&mut b, 4 * bd.n as usize, ntreesd)?;
        let lit: Vec<Huff> = (0..ntreesl).map(|_| read_prefix_code(&mut b, 256)).collect::<Option<_>>()?;
        let cmd: Vec<Huff> = (0..bi.n).map(|_| read_prefix_code(&mut b, 704)).collect::<Option<_>>()?;
        let dist_alpha = 16 + ndirect as usize + (48 << npostfix);
        let dist: Vec<Huff> = (0..ntreesd).map(|_| read_prefix_code(&mut b, dist_alpha)).collect::<Option<_>>()?;
        let end = out.len() + mlen;
        let postfix_mask = (1u32 << npostfix) - 1;
        while out.len() < end {
            if b.overrun() {
                return None;
            }
            bi.step(&mut b)?;
            let c = cmd.get(bi.cur as usize)?.decode(&mut b)?;
            let cell = (c >> 6) as usize;
            let (io, co) = *CELLS.get(cell)?;
            let icode = ((c >> 3) & 7) + io;
            let ccode = (c & 7) + co;
            let (ib, ie) = INSERT[icode as usize];
            let insert = ib + b.bits(ie);
            let (cb, ce) = COPY[ccode as usize];
            let copy = (cb + b.bits(ce)) as usize;
            for _ in 0..insert {
                bl.step(&mut b)?;
                let p1 = out.last().copied().unwrap_or(0) as usize;
                let p2 = if out.len() >= 2 { out[out.len() - 2] as usize } else { 0 };
                let ctx = match modes[bl.cur as usize] {
                    0 => p1 & 0x3f,
                    1 => p1 >> 2,
                    2 => (LUT0[p1] | LUT1[p2]) as usize,
                    _ => ((LUT2[p1] as usize) << 3) | LUT2[p2] as usize,
                };
                let tree = cmap_l[bl.cur as usize * 64 + ctx] as usize;
                out.push(lit.get(tree)?.decode(&mut b)? as u8);
            }
            if out.len() >= end {
                break;
            }
            let (distance, push) = if c < 128 {
                (dist_rb[(rb_idx + 3) & 3], false)
            } else {
                bd.step(&mut b)?;
                let dctx = if copy > 4 { 3 } else { copy - 2 };
                let tree = cmap_d[bd.cur as usize * 4 + dctx] as usize;
                let dc = dist.get(tree)?.decode(&mut b)?;
                if dc < 16 {
                    let last = |k: usize| dist_rb[(rb_idx + 4 - k) & 3] as i64;
                    let d = match dc {
                        0 => last(1),
                        1 => last(2),
                        2 => last(3),
                        3 => last(4),
                        4..=9 => {
                            let off = [-1i64, 1, -2, 2, -3, 3][(dc - 4) as usize];
                            last(1) + off
                        }
                        _ => {
                            let off = [-1i64, 1, -2, 2, -3, 3][(dc - 10) as usize];
                            last(2) + off
                        }
                    };
                    if d <= 0 {
                        return None;
                    }
                    (d as usize, dc != 0)
                } else if dc < 16 + ndirect {
                    ((dc - 15) as usize, true)
                } else {
                    let x = dc - ndirect - 16;
                    let hcode = x >> npostfix;
                    let lcode = x & postfix_mask;
                    let nbits = 1 + (hcode >> 1);
                    let offset = ((2 + (hcode & 1)) << nbits) - 4;
                    let d = ((offset + b.bits(nbits)) << npostfix) + lcode + ndirect + 1;
                    (d as usize, true)
                }
            };
            let max_distance = out.len().min(max_back);
            if distance > max_distance {
                // Static dictionary reference.
                if !(4..=24).contains(&copy) {
                    return None;
                }
                dictionary_word(copy, distance - max_distance - 1, &mut out)?;
            } else {
                if push {
                    dist_rb[rb_idx & 3] = distance;
                    rb_idx += 1;
                }
                let start = out.len() - distance;
                for i in 0..copy {
                    let v = out[start + i];
                    out.push(v);
                }
            }
        }
        if out.len() > 256 << 20 {
            return None;
        }
        if last {
            break;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn decompress_real_streams() {
        for (br, raw) in [(&include_bytes!("../testdata/br_1.br")[..], &include_bytes!("../testdata/br_1.raw")[..]), (&include_bytes!("../testdata/br_2.br")[..], &include_bytes!("../testdata/br_2.raw")[..])] {
            let out = super::decompress(br).expect("decompress");
            assert_eq!(out.len(), raw.len());
            assert!(out == raw);
        }
    }
}
