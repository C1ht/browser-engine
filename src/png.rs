//! Minimal PNG encoder (RGB8, zlib "stored" blocks) for screenshots.

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (n, e) in t.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    t
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8], table: &[u32; 256]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let mut crc = 0xFFFF_FFFFu32;
    for &b in &out[start..] {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}

/// Encode 0xAARRGGBB pixels as an RGB PNG.
pub fn encode(pixels: &[u32], w: usize, h: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(h * (w * 3 + 1));
    for row in pixels.chunks(w).take(h) {
        raw.push(0); // filter: none
        for &p in row {
            raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, blk) in blocks.iter().enumerate() {
        z.push((i == blocks.len() - 1) as u8);
        let len = blk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(blk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let table = crc_table();
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr, &table);
    chunk(&mut out, b"IDAT", &z, &table);
    chunk(&mut out, b"IEND", &[], &table);
    out
}
