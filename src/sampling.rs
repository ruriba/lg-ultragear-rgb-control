//! Pure color math for the image sync sampler, ported from the original
//! scrap-based implementation (geometry verified against the monitor).

use crate::usb_protocol::RGBColor;

/// Which part of the screen each LED samples.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SamplingMode {
    /// Perimeter ring at 5% inset.
    Border5,
    /// Perimeter ring at 15% inset: reacts to windows closer to the center.
    Border15,
    /// Full screen: each LED averages a strip of its half of the screen, so
    /// content anywhere lights the LEDs while keeping LED-to-position
    /// correspondence.
    Full,
}

/// Adaptive stride shared by the CPU sampler and the GPU averaging path:
/// large blocks (Full mode) are sampled on a coarse grid (~4K samples),
/// small border blocks are read pixel by pixel (exactly the reference
/// behavior).
pub fn block_stride(area: usize) -> usize {
    (((area / 4096) as f32).sqrt() as usize).max(1)
}

/// Applies the color boost and temporal smoothing to the per-block color
/// sums computed by the GPU averaging path. On the first frame the smoothing
/// is skipped, so the LEDs snap to the real colors instead of fading in from
/// black. Blocks with a zero count are skipped entirely.
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    sums: &[[u32; 3]; 48],
    counts: &[u32; 48],
    prev: &mut [[u8; 3]; 48],
    first_frame: &mut bool,
    smoothing: f32,
    boost: f32,
) -> [RGBColor; 48] {
    let smooth = !*first_frame;
    *first_frame = false;
    let inv_smooth = 1.0 - smoothing;
    let mut out = [RGBColor { r: 0, g: 0, b: 0 }; 48];
    for i in 0..48 {
        let n = counts[i];
        if n == 0 {
            continue;
        }
        let [sr, sg, sb] = sums[i];
        let boosted = |v: u32| (v as f32 * boost / n as f32).min(255.0) as u8;
        let (r, g, b) = (boosted(sr), boosted(sg), boosted(sb));

        if smooth {
            let mix = |p: u8, v: u8| (smoothing * p as f32 + inv_smooth * v as f32) as u8;
            prev[i] = [mix(prev[i][0], r), mix(prev[i][1], g), mix(prev[i][2], b)];
        } else {
            prev[i] = [r, g, b];
        }
        out[i] = RGBColor {
            r: prev[i][0],
            g: prev[i][1],
            b: prev[i][2],
        };
    }
    out
}

/// HSL -> RGB; h, s, l in [0.0, 1.0].
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> RGBColor {
    let chan = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
    if s == 0.0 {
        let v = chan(l);
        return RGBColor { r: v, g: v, b: v };
    }
    let hue_to_rgb = |p: f32, q: f32, mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    RGBColor {
        r: chan(hue_to_rgb(p, q, h + 1.0 / 3.0)),
        g: chan(hue_to_rgb(p, q, h)),
        b: chan(hue_to_rgb(p, q, h - 1.0 / 3.0)),
    }
}

/// Sample rectangle per LED, according to the chosen mode.
pub fn build_sample_blocks(w: usize, h: usize, mode: SamplingMode) -> [(u16, u16, u16, u16); 48] {
    match mode {
        SamplingMode::Border5 | SamplingMode::Border15 => {
            let pct = if mode == SamplingMode::Border5 { 5 } else { 15 };
            let mut points = perimeter_points(w, h, pct);
            // The physical strip runs the perimeter opposite to this sampling
            // order (verified: right-edge content used to light the left LEDs).
            points.reverse();
            let bw = (w / 120).max(1);
            let bh = (h / 68).max(1);
            points.map(|(x, y)| {
                let x0 = x.saturating_sub(bw / 2).min(w - 1);
                let y0 = y.saturating_sub(bh / 2).min(h - 1);
                (
                    x0 as u16,
                    y0 as u16,
                    (x0 + bw).min(w) as u16,
                    (y0 + bh).min(h) as u16,
                )
            })
        }
        SamplingMode::Full => {
            let mut blocks = full_screen_blocks(w, h);
            // Same physical-strip reversal as the border modes.
            blocks.reverse();
            blocks
        }
    }
}

/// Full-screen blocks, in the same perimeter order as `perimeter_points`:
/// each LED averages a strip spanning its half of the screen, so every pixel
/// influences exactly one or two neighboring LEDs and the LED-to-position
/// correspondence is preserved.
fn full_screen_blocks(w: usize, h: usize) -> [(u16, u16, u16, u16); 48] {
    let mut blocks = [(0u16, 0u16, 0u16, 0u16); 48];
    let cx = w / 2;
    let cy = h / 2;
    let blk =
        |x0: usize, y0: usize, x1: usize, y1: usize| (x0 as u16, y0 as u16, x1 as u16, y1 as u16);
    let mut i = 0;
    // 1) Top-right (7): vertical strips of the top half, from center to right.
    for k in 0..7 {
        let x0 = cx + k * (w - cx) / 7;
        let x1 = if k == 6 {
            w
        } else {
            cx + (k + 1) * (w - cx) / 7
        };
        blocks[i] = blk(x0, 0, x1.max(x0 + 1), cy);
        i += 1;
    }
    // 2) Right edge (10): horizontal strips of the right half, top to bottom.
    for k in 0..10 {
        let y0 = k * h / 10;
        let y1 = if k == 9 { h } else { (k + 1) * h / 10 };
        blocks[i] = blk(cx, y0, w, y1.max(y0 + 1));
        i += 1;
    }
    // 3) Bottom (14): vertical strips of the bottom half, right to left.
    for k in 0..14 {
        let x1 = w - k * w / 14;
        let x0 = if k == 13 { 0 } else { w - (k + 1) * w / 14 };
        blocks[i] = blk(x0, cy, x1.max(x0 + 1), h);
        i += 1;
    }
    // 4) Left edge (10): horizontal strips of the left half, bottom to top.
    for k in 0..10 {
        let y1 = h - k * h / 10;
        let y0 = if k == 9 { 0 } else { h - (k + 1) * h / 10 };
        blocks[i] = blk(0, y0, cx, y1.max(y0 + 1));
        i += 1;
    }
    // 5) Top-left (7): vertical strips of the top half, left to center.
    for k in 0..7 {
        let x0 = k * cx / 7;
        let x1 = if k == 6 { cx } else { (k + 1) * cx / 7 };
        blocks[i] = blk(x0, 0, x1.max(x0 + 1), cy);
        i += 1;
    }
    blocks
}

/// 48 points along the screen perimeter: top-right (7), right edge (10),
/// bottom (14), left edge (10), top-left (7).
/// The inset percentage selects how deep into the screen the ring sits.
fn perimeter_points(w: usize, h: usize, pct: usize) -> [(usize, usize); 48] {
    let mut points = [(0, 0); 48];
    let offset_x = (w * pct / 100).max(1).min(w / 4);
    let offset_y = (h * pct / 100).max(1).min(h / 4);
    let cx = w / 2;

    let mut i = 0;
    for k in 0..7 {
        points[i] = (cx + k * (w - offset_x - cx) / 7, offset_y);
        i += 1;
    }
    for k in 0..10 {
        points[i] = (w - offset_x, offset_y + k * (h - 2 * offset_y) / 10);
        i += 1;
    }
    for k in 0..14 {
        points[i] = (w - offset_x - k * (w - 2 * offset_x) / 14, h - offset_y);
        i += 1;
    }
    for k in 0..10 {
        points[i] = (offset_x, h - offset_y - k * (h - 2 * offset_y) / 10);
        i += 1;
    }
    for k in 0..7 {
        points[i] = (offset_x + k * (cx - offset_x) / 7, offset_y);
        i += 1;
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_primaries_and_gray() {
        let red = hsl_to_rgb(0.0, 1.0, 0.5);
        assert_eq!((red.r, red.g, red.b), (255, 0, 0));
        let gray = hsl_to_rgb(0.7, 0.0, 0.5);
        assert_eq!((gray.r, gray.g, gray.b), (127, 127, 127));
    }

    #[test]
    fn every_mode_yields_48_valid_blocks() {
        for mode in [
            SamplingMode::Border5,
            SamplingMode::Border15,
            SamplingMode::Full,
        ] {
            let blocks = build_sample_blocks(1920, 1080, mode);
            assert_eq!(blocks.len(), 48);
            for (x0, y0, x1, y1) in blocks {
                assert!(
                    x1 > x0 && y1 > y0,
                    "degenerate block {mode:?} {x0},{y0},{x1},{y1}"
                );
                assert!(x1 <= 1920 && y1 <= 1080, "out of bounds {mode:?}");
            }
        }
    }

    #[test]
    fn full_screen_blocks_tile_the_screen() {
        // (w, h, cx, cy) = (1920, 1080, 960, 540). The five groups tile the
        // screen: sums of widths/heights per group must add up exactly.
        let (w, h) = (1920usize, 1080usize);
        let (cx, cy) = (w / 2, h / 2);
        let blocks = full_screen_blocks(w, h);
        let (mut tr, mut right, mut bottom, mut left, mut tl) = (0, 0, 0, 0, 0);
        for (i, &(x0, y0, x1, y1)) in blocks.iter().enumerate() {
            match i {
                0..7 => {
                    assert_eq!((y0, y1), (0, cy as u16));
                    tr += (x1 - x0) as usize;
                }
                7..17 => {
                    assert_eq!((x0, x1), (cx as u16, w as u16));
                    right += (y1 - y0) as usize;
                }
                17..31 => {
                    assert_eq!((y0, y1), (cy as u16, h as u16));
                    bottom += (x1 - x0) as usize;
                }
                31..41 => {
                    assert_eq!((x0, x1), (0, cx as u16));
                    left += (y1 - y0) as usize;
                }
                41..48 => {
                    assert_eq!((y0, y1), (0, cy as u16));
                    tl += (x1 - x0) as usize;
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(tr, w - cx);
        assert_eq!(tl, cx);
        assert_eq!(bottom, w);
        assert_eq!(right, h);
        assert_eq!(left, h);
    }

    #[test]
    fn finalize_average_boost_and_smoothing() {
        let mut prev = [[0u8; 3]; 48];
        let mut first = true;
        let counts = [4u32; 48];

        // First frame, no smoothing, 1.0x boost: exact average of 4 samples
        // of (30, 20, 10).
        let sums = [[(30 * 4) as u32, (20 * 4) as u32, (10 * 4) as u32]; 48];
        let out = finalize(&sums, &counts, &mut prev, &mut first, 0.0, 1.0);
        assert!(out.iter().all(|c| (c.r, c.g, c.b) == (30, 20, 10)));

        // Second frame of pure red (100, 0, 0) with smoothing 0.5:
        // mix = 0.5*prev + 0.5*new per channel.
        let red = [[(100 * 4) as u32, 0, 0]; 48];
        let out = finalize(&red, &counts, &mut prev, &mut first, 0.5, 1.0);
        assert!(out.iter().all(|c| (c.r, c.g, c.b) == (65, 10, 5)));

        // Boost 2.0x: average 100 -> 200 (no clamp needed here).
        let out = finalize(&red, &counts, &mut prev, &mut first, 0.0, 2.0);
        assert!(out.iter().all(|c| (c.r, c.g, c.b) == (200, 0, 0)));
    }

    #[test]
    fn finalize_skips_zero_count_blocks() {
        let mut counts = [4u32; 48];
        counts[7] = 0;
        let mut prev = [[9u8; 3]; 48];
        let mut first = true;
        let sums = [[120u32, 0, 0]; 48];
        let out = finalize(&sums, &counts, &mut prev, &mut first, 0.0, 1.0);
        assert_eq!((out[7].r, out[7].g, out[7].b), (0, 0, 0)); // skipped
        assert_eq!(prev[7], [9, 9, 9]); // envelope state untouched
        assert_eq!((out[0].r, out[0].g, out[0].b), (30, 0, 0)); // 120/4
    }
}
