//! Procedurally rasterized icons, so the app ships without binary assets.
//!
//! The glyph is a lighthouse above the water: a port that shows traffic the
//! way out. Its beams shine while the proxy runs and go dark otherwise.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Stopped,
    Failed,
}

type Rgba = [f32; 4];

const NIGHT_TOP: Rgba = [0.08, 0.15, 0.40, 1.0];
const NIGHT_BOTTOM: Rgba = [0.10, 0.42, 0.92, 1.0];
const SHADOW: Rgba = [0.0, 0.10, 0.35, 1.0];
const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];
const GREEN: Rgba = [0.20, 0.78, 0.42, 1.0];
const GREY: Rgba = [0.62, 0.64, 0.68, 1.0];
const RED: Rgba = [0.94, 0.30, 0.30, 1.0];

/// Monochrome glyph for the macOS menu bar; only alpha is significant.
#[cfg(target_os = "macos")]
pub fn template(size: u32, status: Status) -> Vec<u8> {
    let lit = status == Status::Running;
    let alpha = if lit { 1.0 } else { 0.6 };
    raster(size, |x, y| {
        let g = coverage(glyph_sdf(x, y, lit));
        [0.0, 0.0, 0.0, g * alpha]
    })
}

/// Colored badge for Windows and Linux trays and the window icon.
pub fn badge(size: u32, status: Option<Status>) -> Vec<u8> {
    const OFFSET: f32 = 0.10;
    const SCALE: f32 = 0.80;
    let lit = status.is_none_or(|s| s == Status::Running);
    raster(size, |x, y| {
        let mut px = [0.0; 4];
        // Rounded-square badge with a night-sky gradient.
        let badge = coverage(rounded_rect_sdf(x, y, 0.5, 0.5, 0.46, 0.46, 0.22));
        if badge > 0.0 {
            let c = mix(NIGHT_TOP, NIGHT_BOTTOM, ((y - 0.04) / 0.92).clamp(0.0, 1.0));
            over(&mut px, c, badge);
        }
        let (gx, gy) = ((x - OFFSET) / SCALE, (y - OFFSET) / SCALE);
        // Soft shadow: the glyph's distance field, offset downwards and faded.
        let shadow = (0.5 - glyph_sdf(gx, gy - 0.025, lit) / 0.05).clamp(0.0, 1.0);
        over(&mut px, SHADOW, shadow * 0.28 * badge);
        over(&mut px, WHITE, coverage(glyph_sdf(gx, gy, lit)) * badge);
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

/// Signed distance (negative inside) to the lighthouse glyph in the unit
/// square. An unlit lighthouse has no beams and a hollow lantern.
fn glyph_sdf(x: f32, y: f32, lit: bool) -> f32 {
    // Tower tapering outwards from y 0.40 to 0.84, with one band cut out.
    let half = 0.085 + (y - 0.40) / 0.44 * 0.075;
    let tower = ((x - 0.5).abs() - half).max((y - 0.62).abs() - 0.22);
    let tower = tower.max(-((y - 0.58).abs() - 0.035));
    let gallery = rounded_rect_sdf(x, y, 0.5, 0.385, 0.14, 0.025, 0.02);
    let lantern = rounded_rect_sdf(x, y, 0.5, 0.30, 0.075, 0.065, 0.02);
    let roof = triangle_sdf(x, y, [(0.39, 0.24), (0.61, 0.24), (0.5, 0.13)]) - 0.012;
    let mut d = tower.min(gallery).min(lantern).min(roof);
    if lit {
        for s in [-1.0, 1.0] {
            let beam = [
                (0.5 + s * 0.13, 0.29),
                (0.5 + s * 0.45, 0.17),
                (0.5 + s * 0.45, 0.39),
            ];
            d = d.min(triangle_sdf(x, y, beam) - 0.012);
        }
    } else {
        d = d.max(-rounded_rect_sdf(x, y, 0.5, 0.30, 0.035, 0.03, 0.01));
    }
    d.min(wave_sdf(x, y) - 0.03)
}

/// Distance to the water line: two sine periods across the bottom.
fn wave_sdf(x: f32, y: f32) -> f32 {
    const STEPS: usize = 32;
    let point = |i: usize| {
        let t = i as f32 / STEPS as f32;
        let phase = t * 4.0 * std::f32::consts::PI;
        (0.12 + 0.76 * t, 0.92 + 0.015 * phase.sin())
    };
    (1..=STEPS)
        .map(|i| segment_sdf(x, y, point(i - 1), point(i)))
        .fold(f32::MAX, f32::min)
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

/// Signed distance to a triangle, negative inside.
fn triangle_sdf(x: f32, y: f32, p: [(f32, f32); 3]) -> f32 {
    let mut d = f32::MAX;
    let mut sides = [0.0; 3];
    for i in 0..3 {
        let (a, b) = (p[i], p[(i + 1) % 3]);
        d = d.min(segment_sdf(x, y, a, b));
        sides[i] = (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
    }
    let inside = sides.iter().all(|&s| s >= 0.0) || sides.iter().all(|&s| s <= 0.0);
    if inside { -d } else { d }
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
