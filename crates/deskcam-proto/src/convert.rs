//! CPU conversions from the shared NV12 frame to the layouts DirectShow consumers negotiate.
//!
//! All buffers are tightly packed (stride == width); callers bound-check sizes with the
//! `*_size` helpers before converting.

pub fn nv12_size(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

pub fn i420_size(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

pub fn yuy2_size(width: usize, height: usize) -> usize {
    width * height * 2
}

/// NV12 → I420: copies luma and de-interleaves the chroma plane into U then V.
pub fn nv12_to_i420(src: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let luma = width * height;
    let quarter = luma / 4;
    dst[..luma].copy_from_slice(&src[..luma]);
    let (u, v) = dst[luma..luma + 2 * quarter].split_at_mut(quarter);
    for (i, pair) in src[luma..luma + 2 * quarter].chunks_exact(2).enumerate() {
        u[i] = pair[0];
        v[i] = pair[1];
    }
}

/// NV12 → YUY2 (packed Y0 U Y1 V); each chroma row is shared by two luma rows.
pub fn nv12_to_yuy2(src: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let luma = width * height;
    for r in 0..height {
        let y = &src[r * width..][..width];
        let uv = &src[luma + (r / 2) * width..][..width];
        let out = &mut dst[r * width * 2..][..width * 2];
        for (x, px) in out.chunks_exact_mut(4).enumerate() {
            px[0] = y[2 * x];
            px[1] = uv[2 * x];
            px[2] = y[2 * x + 1];
            px[3] = uv[2 * x + 1];
        }
    }
}

/// Nearest-neighbour NV12 resize, used when the running app's resolution no longer matches the
/// format a consumer negotiated. Dimensions must be even.
pub fn scale_nv12_nearest(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    let map = |d: usize, dn: usize, sn: usize| (d * sn) / dn;
    let cols: Vec<usize> = (0..dw).map(|x| map(x, dw, sw)).collect();
    for y in 0..dh {
        let s = &src[map(y, dh, sh) * sw..][..sw];
        let d = &mut dst[y * dw..][..dw];
        for (x, &c) in cols.iter().enumerate() {
            d[x] = s[c];
        }
    }
    let (s_uv, d_uv) = (&src[sw * sh..], &mut dst[dw * dh..]);
    let (scw, sch, dcw, dch) = (sw / 2, sh / 2, dw / 2, dh / 2);
    for y in 0..dch {
        let s = &s_uv[map(y, dch, sch) * sw..][..sw];
        let d = &mut d_uv[y * dw..][..dw];
        for x in 0..dcw {
            let c = map(x, dcw, scw);
            d[2 * x] = s[2 * c];
            d[2 * x + 1] = s[2 * c + 1];
        }
    }
}

/// BT.601 limited-range black in NV12 or I420 (both are 4:2:0 with a full-size luma plane).
pub fn black_420(width: usize, height: usize, dst: &mut [u8]) {
    let luma = width * height;
    dst[..luma].fill(16);
    dst[luma..luma + luma / 2].fill(128);
}

pub fn black_yuy2(width: usize, height: usize, dst: &mut [u8]) {
    for px in dst[..yuy2_size(width, height)].chunks_exact_mut(2) {
        px.copy_from_slice(&[16, 128]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x2 NV12 frame: luma 0..8, chroma row (U,V) pairs (100,200) and (101,201).
    fn frame() -> Vec<u8> {
        let mut f: Vec<u8> = (0..8).collect();
        f.extend_from_slice(&[100, 200, 101, 201]);
        f
    }

    #[test]
    fn i420_splits_chroma() {
        let mut out = vec![0u8; i420_size(4, 2)];
        nv12_to_i420(&frame(), 4, 2, &mut out);
        assert_eq!(out, [0, 1, 2, 3, 4, 5, 6, 7, 100, 101, 200, 201]);
    }

    #[test]
    fn yuy2_interleaves_shared_chroma() {
        let mut out = vec![0u8; yuy2_size(4, 2)];
        nv12_to_yuy2(&frame(), 4, 2, &mut out);
        assert_eq!(&out[..8], &[0, 100, 1, 200, 2, 101, 3, 201]);
        assert_eq!(&out[8..], &[4, 100, 5, 200, 6, 101, 7, 201]);
    }

    #[test]
    fn scale_identity_and_downscale() {
        let src = frame();
        let mut same = vec![0u8; src.len()];
        scale_nv12_nearest(&src, 4, 2, &mut same, 4, 2);
        assert_eq!(same, src);

        let mut half = vec![0u8; nv12_size(2, 2)];
        scale_nv12_nearest(&src, 4, 2, &mut half, 2, 2);
        assert_eq!(half, [0, 2, 4, 6, 100, 200]);
    }

    #[test]
    fn scale_upscale_repeats_pixels() {
        let src = frame();
        let mut big = vec![0u8; nv12_size(8, 4)];
        scale_nv12_nearest(&src, 4, 2, &mut big, 8, 4);
        assert_eq!(&big[..8], &[0, 0, 1, 1, 2, 2, 3, 3]);
        assert_eq!(&big[24..32], &[4, 4, 5, 5, 6, 6, 7, 7]);
        assert_eq!(&big[32..40], &[100, 200, 100, 200, 101, 201, 101, 201]);
    }

    #[test]
    fn black_frames() {
        let mut nv12 = vec![0u8; nv12_size(4, 2)];
        black_420(4, 2, &mut nv12);
        assert_eq!(nv12, [16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128]);
        let mut yuy2 = vec![0u8; yuy2_size(2, 2)];
        black_yuy2(2, 2, &mut yuy2);
        assert_eq!(yuy2, [16, 128, 16, 128, 16, 128, 16, 128]);
    }
}
