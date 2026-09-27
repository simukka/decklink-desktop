//! Multithreaded fixed-point bilinear BGRA scaler.
//!
//! The decklink crate's CPU scalers are single-threaded float code (~55 ms
//! for 2560x1440 -> 1920x1080); splitting rows across cores with 8-bit
//! fixed-point weights brings that down to a few milliseconds.

/// Source sample index and 8-bit weight of the next sample for each
/// destination coordinate (pixel-centre aligned).
fn taps(src_len: usize, dst_len: usize) -> Vec<(usize, u32)> {
    let ratio = src_len as f64 / dst_len as f64;
    (0..dst_len)
        .map(|d| {
            let s = ((d as f64 + 0.5) * ratio - 0.5).clamp(0.0, (src_len - 1) as f64);
            let i = (s as usize).min(src_len.saturating_sub(2));
            (i, ((s - i as f64) * 256.0).round().min(256.0) as u32)
        })
        .collect()
}

/// Scale `src` (BGRA, `src_w`x`src_h`, `src_stride` bytes/row) into a
/// `dst_w`x`dst_h` region of `dst` starting at (`x_off`, `y_off`).
#[allow(clippy::too_many_arguments)]
pub fn bilinear_bgra(
    src: &[u8], src_stride: usize, src_w: usize, src_h: usize,
    dst: &mut [u8], dst_stride: usize, x_off: usize, y_off: usize, dst_w: usize, dst_h: usize,
) {
    let xs = taps(src_w, dst_w);
    let ys = taps(src_h, dst_h);
    let rows = &mut dst[y_off * dst_stride..];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let rows_per_chunk = dst_h.div_ceil(threads);

    std::thread::scope(|scope| {
        for (chunk_idx, chunk) in rows.chunks_mut(rows_per_chunk * dst_stride).enumerate() {
            let (xs, ys) = (&xs, &ys);
            scope.spawn(move || {
                for (i, row) in chunk.chunks_mut(dst_stride).enumerate() {
                    let dy = chunk_idx * rows_per_chunk + i;
                    if dy >= dst_h {
                        break;
                    }
                    let (sy, fy) = ys[dy];
                    let r0 = &src[sy * src_stride..];
                    let r1 = &src[(sy + 1).min(src_h - 1) * src_stride..];
                    let out = &mut row[x_off * 4..(x_off + dst_w) * 4];
                    for (px, &(sx, fx)) in out.chunks_exact_mut(4).zip(xs) {
                        let (a, b) = (sx * 4, (sx + 1) * 4);
                        for c in 0..3 {
                            let top = r0[a + c] as u32 * (256 - fx) + r0[b + c] as u32 * fx;
                            let bot = r1[a + c] as u32 * (256 - fx) + r1[b + c] as u32 * fx;
                            px[c] = ((top * (256 - fy) + bot * fy + (1 << 15)) >> 16) as u8;
                        }
                        px[3] = 255;
                    }
                }
            });
        }
    });
}
