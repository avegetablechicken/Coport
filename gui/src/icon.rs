//! Procedurally rasterized icons, so the app ships without binary assets.
//!
//! The glyph is a lighthouse above the water: a port that shows traffic the
//! way out. Its beams shine while the proxy runs and go dark otherwise. The
//! badge is set at dusk, a clay sky over a teal sea, after Claude and Codex.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Stopped,
    Failed,
}

type Rgba = [f32; 4];

/// Vertical gradient stops of the badge: sky, horizon glow, then the sea.
const DUSK: [(f32, Rgba); 5] = [
    (0.08, [0.722, 0.353, 0.243, 1.0]),
    (0.40, [0.824, 0.494, 0.345, 1.0]),
    (0.57, [0.902, 0.655, 0.486, 1.0]),
    (0.70, [0.243, 0.620, 0.604, 1.0]),
    (0.96, [0.051, 0.373, 0.408, 1.0]),
];
const HALO: Rgba = [1.0, 0.945, 0.839, 1.0];
const SHADOW: Rgba = [0.0, 0.10, 0.35, 1.0];
const WHITE: Rgba = [1.0, 1.0, 1.0, 1.0];
const GREEN: Rgba = [0.20, 0.78, 0.42, 1.0];
const GREY: Rgba = [0.62, 0.64, 0.68, 1.0];
const RED: Rgba = [0.94, 0.30, 0.30, 1.0];

/// The water line: two sine periods across the bottom, thinning to its ends.
const WAVE: Wave = Wave {
    from: 0.12,
    to: 0.88,
    radius: 0.026,
    base: 0.915,
    amplitude: 0.015,
    periods: 2.0,
    phase: 0.0,
};
/// A shorter, fainter swell below the water line.
const SWELL: Wave = Wave {
    from: 0.30,
    to: 0.70,
    radius: 0.016,
    base: 0.985,
    amplitude: 0.010,
    periods: 1.0,
    phase: 1.4,
};

/// Monochrome glyph for the macOS menu bar; only alpha is significant.
#[cfg(target_os = "macos")]
pub fn template(size: u32, status: Status) -> Vec<u8> {
    // Shrink the glyph towards the top to leave room below for both waves,
    // drawn thicker and lower than on the badge so they stay apart at 22pt.
    const SCALE: f32 = 0.88;
    const TOP: f32 = 0.03;
    const WATER: Wave = Wave {
        base: 0.95,
        radius: 0.040,
        ..WAVE
    };
    const SWELL_BELOW: Wave = Wave {
        base: 1.10,
        radius: 0.030,
        ..SWELL
    };
    let lit = status == Status::Running;
    let alpha = if lit { 1.0 } else { 0.6 };
    raster(size, |x, y| {
        let (gx, gy) = ((x - 0.5) / SCALE + 0.5, (y - TOP) / SCALE + 0.13);
        let d = tower_sdf(gx, gy, lit)
            .min(WATER.sdf(gx, gy))
            .min(SWELL_BELOW.sdf(gx, gy));
        let mut a = coverage(d);
        if lit {
            // Beams fade as they travel away from the lantern.
            for side in [-1.0, 1.0] {
                let (d, t) = beam(gx, gy, side);
                a = a.max(coverage(d) * (1.0 - 0.6 * t));
            }
        }
        [0.0, 0.0, 0.0, a * alpha]
    })
}

/// Colored badge for Windows and Linux trays and the window icon.
pub fn badge(size: u32, status: Option<Status>) -> Vec<u8> {
    const OFFSET: f32 = 0.10;
    const SCALE: f32 = 0.80;
    let lit = status.is_none_or(|s| s == Status::Running);
    raster(size, |x, y| {
        let mut px = [0.0; 4];
        // Rounded-square badge with a dusk gradient.
        let badge = coverage(rounded_rect_sdf(x, y, 0.5, 0.5, 0.46, 0.46, 0.22));
        if badge > 0.0 {
            over(&mut px, gradient(&DUSK, y), badge);
        }
        let (gx, gy) = ((x - OFFSET) / SCALE, (y - OFFSET) / SCALE);
        if lit {
            // Warm halo around the lantern.
            let r = ((gx - 0.5).powi(2) + ((gy - 0.30) * 1.3).powi(2)).sqrt();
            over(
                &mut px,
                HALO,
                (1.0 - smoothstep(0.06, 0.30, r)) * 0.55 * badge,
            );
        }
        // Soft shadow: the tower's distance field, offset downwards and faded.
        let shadow = (0.5 - tower_sdf(gx, gy - 0.025, lit) / 0.05).clamp(0.0, 1.0);
        over(&mut px, SHADOW, shadow * 0.22 * badge);
        if lit {
            // Beams fade as they travel away from the lantern.
            for side in [-1.0, 1.0] {
                let (d, t) = beam(gx, gy, side);
                over(&mut px, WHITE, coverage(d) * (1.0 - 0.6 * t) * badge);
            }
        }
        over(&mut px, WHITE, coverage(SWELL.sdf(gx, gy)) * 0.7 * badge);
        over(&mut px, WHITE, coverage(WAVE.sdf(gx, gy)) * badge);
        over(&mut px, WHITE, coverage(tower_sdf(gx, gy, lit)) * badge);
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

/// Signed distance (negative inside) to the lighthouse without its beams, in
/// the unit square. An unlit lighthouse has a hollow lantern.
fn tower_sdf(x: f32, y: f32, lit: bool) -> f32 {
    // Tower tapering outwards from y 0.40 to 0.84, with one band cut out.
    let half = 0.085 + (y - 0.40) / 0.44 * 0.075;
    let tower = ((x - 0.5).abs() - half).max((y - 0.62).abs() - 0.22);
    let tower = tower.max(-((y - 0.58).abs() - 0.035));
    let gallery = rounded_rect_sdf(x, y, 0.5, 0.385, 0.14, 0.025, 0.02);
    let lantern = rounded_rect_sdf(x, y, 0.5, 0.30, 0.075, 0.065, 0.02);
    let roof = triangle_sdf(x, y, [(0.39, 0.24), (0.61, 0.24), (0.5, 0.13)]) - 0.012;
    let d = tower.min(gallery).min(lantern).min(roof);
    if lit {
        d
    } else {
        d.max(-rounded_rect_sdf(x, y, 0.5, 0.30, 0.035, 0.03, 0.01))
    }
}

/// Distance to the beam on `side` (-1 left, 1 right): a cone from the lantern
/// with an arc at its far end. Also returns how far along the beam the point
/// lies, from 0 at the lantern to 1 at the arc.
fn beam(x: f32, y: f32, side: f32) -> (f32, f32) {
    const NEAR: f32 = 0.12;
    const FAR: f32 = 0.47;
    const HALF_ANGLE: f32 = 13.0 * std::f32::consts::PI / 180.0;
    let (dx, dy) = ((x - 0.5) * side, y - 0.30);
    let r = (dx * dx + dy * dy).sqrt();
    let edge = (dy.atan2(dx).abs() - HALF_ANGLE) * r;
    let d = edge.max(r - FAR).max(NEAR - r) - 0.006;
    (d, ((r - NEAR) / (FAR - NEAR)).clamp(0.0, 1.0))
}

struct Wave {
    from: f32,
    to: f32,
    radius: f32,
    base: f32,
    amplitude: f32,
    periods: f32,
    phase: f32,
}

impl Wave {
    /// Distance to a sine stroke whose radius tapers towards both ends.
    fn sdf(&self, x: f32, y: f32) -> f32 {
        const STEPS: usize = 32;
        if (y - self.base).abs() > self.amplitude + self.radius + 0.05 {
            return f32::MAX;
        }
        let point = |t: f32| {
            let phase = t * self.periods * 2.0 * std::f32::consts::PI + self.phase;
            let x = self.from + (self.to - self.from) * t;
            (x, self.base + self.amplitude * phase.sin())
        };
        let radius = |t: f32| self.radius * (std::f32::consts::PI * t).sin().powf(0.6).max(0.25);
        (0..STEPS)
            .map(|i| {
                let (t0, t1) = (i as f32 / STEPS as f32, (i + 1) as f32 / STEPS as f32);
                let (a, b) = (point(t0), point(t1));
                let (pax, pay) = (x - a.0, y - a.1);
                let (bax, bay) = (b.0 - a.0, b.1 - a.1);
                let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
                let d = ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt();
                // Interpolate the radius along the segment so the edge stays smooth.
                d - radius(t0 + (t1 - t0) * h)
            })
            .fold(f32::MAX, f32::min)
    }
}

/// Smooth interpolation between gradient stops sorted by position.
fn gradient(stops: &[(f32, Rgba)], t: f32) -> Rgba {
    let mut color = stops[0].1;
    for pair in stops.windows(2) {
        let ((t0, a), (t1, b)) = (pair[0], pair[1]);
        if t > t0 {
            color = mix(a, b, smoothstep(t0, t1, t));
        }
    }
    color
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
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
