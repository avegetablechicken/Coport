//! Procedurally rasterized icons, so the app ships without binary assets.
//!
//! The glyph is `‹✦›`: code brackets around the sparkle that marks AI agents,
//! i.e. a proxy built for coding agents.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Stopped,
    Failed,
}

type Rgba = [f32; 4];

const INDIGO_TOP: Rgba = [0.42, 0.40, 0.95, 1.0];
const INDIGO_BOTTOM: Rgba = [0.29, 0.25, 0.80, 1.0];
const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];
const GREEN: Rgba = [0.20, 0.78, 0.42, 1.0];
const GREY: Rgba = [0.62, 0.64, 0.68, 1.0];
const RED: Rgba = [0.94, 0.30, 0.30, 1.0];

/// Monochrome glyph for the macOS menu bar; only alpha is significant.
pub fn template(size: u32, status: Status) -> Vec<u8> {
    let running = status == Status::Running;
    let alpha = if running { 1.0 } else { 0.6 };
    raster(size, |x, y| {
        let g = glyph(x, y, 0.0, 1.0, running);
        [0.0, 0.0, 0.0, g * alpha]
    })
}

/// Colored badge for Windows and Linux trays and the window icon.
pub fn badge(size: u32, status: Option<Status>) -> Vec<u8> {
    raster(size, |x, y| {
        let mut px = [0.0; 4];
        // Rounded-square badge with a vertical gradient.
        let badge = coverage(rounded_rect_sdf(x, y, 0.5, 0.5, 0.46, 0.46, 0.22));
        if badge > 0.0 {
            let c = mix(INDIGO_TOP, INDIGO_BOTTOM, y);
            over(&mut px, c, badge);
        }
        let g = glyph(x, y, 0.08, 0.84, true);
        over(&mut px, WHITE, g * badge);
        if let Some(status) = status {
            let color = match status {
                Status::Running => GREEN,
                Status::Stopped => GREY,
                Status::Failed => RED,
            };
            // Status dot with a transparent ring cut out of the badge.
            let d = circle_sdf(x, y, 0.80, 0.80, 0.19);
            let cut = coverage(d - 0.05);
            px[3] *= 1.0 - cut;
            for c in &mut px[..3] {
                *c *= 1.0 - cut;
            }
            over(&mut px, color, coverage(d));
        }
        px
    })
}

/// Coverage of the `‹✦›` glyph: code brackets around an AI sparkle, mapped
/// into the square `[offset, offset + scale]`. A stopped proxy shows the
/// sparkle as an outline.
fn glyph(x: f32, y: f32, offset: f32, scale: f32, solid: bool) -> f32 {
    let (x, y) = ((x - offset) / scale, (y - offset) / scale);
    let stroke = 0.095;
    let mut d = f32::MAX;
    for (outer, tip) in [(0.27, 0.06), (0.73, 0.94)] {
        d = d.min(segment_sdf(x, y, (outer, 0.25), (tip, 0.5)) - stroke / 2.0);
        d = d.min(segment_sdf(x, y, (tip, 0.5), (outer, 0.75)) - stroke / 2.0);
    }
    let star = sparkle(x, y, 0.5, 0.5, 0.25);
    let star = if solid {
        star
    } else {
        star.max(-sparkle(x, y, 0.5, 0.5, 0.13))
    };
    coverage(d.min(star))
}

/// Four-point star with concave sides: `(|dx|/r)^k + (|dy|/r)^k <= 1`, k < 1.
/// Returns a signed value that is negative inside (not a true distance,
/// which hard-edged supersampled coverage does not need).
fn sparkle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    const K: f32 = 0.62;
    ((x - cx).abs() / r).powf(K) + ((y - cy).abs() / r).powf(K) - 1.0
}

fn raster(size: u32, shade: impl Fn(f32, f32) -> Rgba) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let n = (size * SS) as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f32 + 0.5) / n;
                    let y = ((py * SS + sy) as f32 + 0.5) / n;
                    let c = shade(x, y);
                    // Accumulate premultiplied color.
                    for i in 0..3 {
                        acc[i] += c[i] * c[3];
                    }
                    acc[3] += c[3];
                }
            }
            let a = acc[3] / (SS * SS) as f32;
            for c in &acc[..3] {
                let v = if acc[3] > 0.0 { c / acc[3] } else { 0.0 };
                out.push((v.clamp(0.0, 1.0) * 255.0).round() as u8);
            }
            out.push((a.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }
    out
}

/// Supersampling does the anti-aliasing, so coverage is a hard edge.
fn coverage(d: f32) -> f32 {
    if d <= 0.0 { 1.0 } else { 0.0 }
}

fn circle_sdf(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r
}

fn segment_sdf(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pax, pay) = (x - a.0, y - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt()
}

#[allow(clippy::too_many_arguments)]
fn rounded_rect_sdf(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - hw + r;
    let qy = (y - cy).abs() - hh + r;
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn over(dst: &mut Rgba, src: Rgba, cov: f32) {
    let a = src[3] * cov;
    let out_a = a + dst[3] * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        dst[i] = (src[i] * a + dst[i] * dst[3] * (1.0 - a)) / out_a;
    }
    dst[3] = out_a;
}

/// Minimal PNG encoder (stored deflate blocks) for `--export-icon`.
pub fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(rgba.len() + size as usize);
    for row in rgba.chunks(size as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<_> = raw.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &z), (b"IEND", &Vec::new())] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
