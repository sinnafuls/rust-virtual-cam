//! NV12 (BT.601 limited range) to BGRX conversion for consumers that negotiate RGB32.

#[inline]
fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Converts one output row. `y` holds `width` luma bytes, `uv` the interleaved chroma row
/// shared by this luma row pair (`width` bytes), `out` receives `width * 4` bytes (B, G, R, 255).
pub fn nv12_row_to_bgrx(y: &[u8], uv: &[u8], out: &mut [u8]) {
    let width = out.len() / 4;
    for (x, px) in out.chunks_exact_mut(4).enumerate().take(width) {
        let c = 298 * (y[x] as i32 - 16);
        let d = uv[x & !1] as i32 - 128;
        let e = uv[(x & !1) + 1] as i32 - 128;
        px[0] = clamp((c + 516 * d + 128) >> 8);
        px[1] = clamp((c - 100 * d - 208 * e + 128) >> 8);
        px[2] = clamp((c + 409 * e + 128) >> 8);
        px[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(y: u8, u: u8, v: u8) -> [u8; 4] {
        let mut out = [0u8; 8];
        nv12_row_to_bgrx(&[y, y], &[u, v], &mut out);
        out[..4].try_into().unwrap()
    }

    #[test]
    fn black_and_white() {
        assert_eq!(convert(16, 128, 128), [0, 0, 0, 255]);
        assert_eq!(convert(235, 128, 128), [255, 255, 255, 255]);
    }

    #[test]
    fn red() {
        let [b, g, r, _] = convert(81, 90, 240);
        assert!(r >= 250 && g <= 5 && b <= 5, "got {r} {g} {b}");
    }
}
