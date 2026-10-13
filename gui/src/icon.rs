//! Procedurally rasterized icons, so the app ships without binary assets.
//!
//! The glyph is a lighthouse above the water: a port that shows traffic the
//! way out. Its beams shine while the proxy runs and go dark otherwise; the
//! lens turns half a circle when a model call reaches its upstream. The badge
//! is set at dusk, a clay sky over a teal sea, after Claude and Codex.

use coport::routing::Service;
use std::f32::consts::PI;

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

/// A moment of the half turn the lens makes when a model call reaches its
/// upstream: the angle of the beams, and how strongly they take the app's color.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Turn {
    angle: f32,
    tint: f32,
}

impl Turn {
    /// The beams across the tower, as the still icon shows them.
    pub const REST: Self = Self {
        angle: 0.0,
        tint: 0.0,
    };

    /// The turn at `progress` from 0 to 1, easing out of and back into rest.
    pub fn at(progress: f32) -> Self {
        let p = progress.clamp(0.0, 1.0);
        Self {
            angle: PI * smoothstep(0.0, 1.0, p),
            tint: smoothstep(0.0, 0.15, p) * (1.0 - smoothstep(0.85, 1.0, p)),
        }
    }
}

/// Monochrome glyph for the macOS menu bar; only alpha is significant.
#[cfg(target_os = "macos")]
pub fn template(size: u32, status: Status) -> Vec<u8> {
    const INK: Rgba = [0.0, 0.0, 0.0, 1.0];
    glyph(size, status, Turn::REST, INK, INK)
}

/// A frame of the turn for the macOS menu bar. A colored image cannot be a
/// template, so it is drawn in the bar's own ink for its appearance.
#[cfg(target_os = "macos")]
pub fn beacon(size: u32, turn: Turn, service: Service, dark: bool) -> Vec<u8> {
    let (ink, accent) = if dark {
        let accent = match service {
            Service::Claude => [0.929, 0.553, 0.424, 1.0],
            Service::Codex => [0.384, 0.792, 0.765, 1.0],
        };
        ([1.0, 1.0, 1.0, 1.0], accent)
    } else {
        let accent = match service {
            Service::Claude => [0.851, 0.467, 0.341, 1.0],
            Service::Codex => [0.200, 0.580, 0.565, 1.0],
        };
        ([0.0, 0.0, 0.0, 0.85], accent)
    };
    glyph(size, Status::Running, turn, ink, accent)
}

/// The menu bar lighthouse in `ink`, its light taking `accent` as the turn
/// asks: the badge's clay sky for Claude, its teal sea for Codex.
#[cfg(target_os = "macos")]
fn glyph(size: u32, status: Status, turn: Turn, ink: Rgba, accent: Rgba) -> Vec<u8> {
    const SCALE: f32 = 0.88;
    // Center between the roof tip (including its stroke) and the tower base.
    const CENTER_Y: f32 = (0.13 - 0.012 + 0.84) * 0.5;
    let lens = Lens::new(turn.angle);
    // Light becomes opaque as it takes the accent.
    let tinted = |w: f32| {
        let mut color = mix(ink, accent, w);
        color[3] = ink[3] + (1.0 - ink[3]) * w;
        color
    };
    raster(size, |x, y| {
        let (gx, gy) = ((x - 0.5) / SCALE + 0.5, (y - 0.5) / SCALE + CENTER_Y);
        let mut px = [0.0; 4];
        if status != Status::Running {
            over(&mut px, ink, coverage(tower_sdf(gx, gy, false)) * 0.6);
            return px;
        }
        // A glow around the lantern while a beam points at the viewer.
        let glow = turn.tint * lens.facing().powi(3) * 0.6;
        if glow > 0.0 {
            let r = ((gx - LANTERN.0).powi(2) + ((gy - LANTERN.1) * 1.15).powi(2)).sqrt();
            over(&mut px, accent, glow * (1.0 - smoothstep(0.04, 0.22, r)));
        }
        let beams = lens.hits(gx, gy);
        let beam = |px: &mut Rgba, hit: &Hit| {
            over(px, tinted(turn.tint * (0.6 + 0.4 * hit.glow)), hit.alpha);
        };
        // Beams behind the lantern pass under the tower, the rest over it.
        for hit in beams.iter().flatten().filter(|h| !h.front) {
            beam(&mut px, hit);
        }
        let tower = coverage(tower_sdf(gx, gy, true));
        if tower > 0.0 {
            let lantern = coverage(lantern_sdf(gx, gy));
            let w = turn.tint * (0.35 + 0.65 * lens.facing().powi(2)) * lantern;
            over(&mut px, tinted(w), tower);
        }
        for hit in beams.iter().flatten().filter(|h| h.front) {
            beam(&mut px, hit);
        }
        px
    })
}

/// Colored badge for Windows and Linux trays and the window icon. Its beams
/// follow `turn` but stay white, since the badge's sky and sea are the colors.
pub fn badge(size: u32, status: Option<Status>, turn: Turn) -> Vec<u8> {
    const OFFSET: f32 = 0.10;
    const SCALE: f32 = 0.80;
    let lit = status.is_none_or(|s| s == Status::Running);
    let lens = Lens::new(turn.angle);
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
        let beams = if lit { lens.hits(gx, gy) } else { [None, None] };
        // Beams behind the lantern pass under the tower, the rest over it.
        for hit in beams.iter().flatten().filter(|h| !h.front) {
            over(&mut px, WHITE, hit.alpha * badge);
        }
        over(&mut px, WHITE, coverage(SWELL.sdf(gx, gy)) * 0.7 * badge);
        over(&mut px, WHITE, coverage(WAVE.sdf(gx, gy)) * badge);
        over(&mut px, WHITE, coverage(tower_sdf(gx, gy, lit)) * badge);
        for hit in beams.iter().flatten().filter(|h| h.front) {
            over(&mut px, WHITE, hit.alpha * badge);
        }
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
    let lantern = lantern_sdf(x, y);
    let roof = triangle_sdf(x, y, [(0.39, 0.24), (0.61, 0.24), (0.5, 0.13)]) - 0.012;
    let d = tower.min(gallery).min(lantern).min(roof);
    if lit {
        d
    } else {
        d.max(-rounded_rect_sdf(x, y, 0.5, 0.30, 0.035, 0.03, 0.01))
    }
}

fn lantern_sdf(x: f32, y: f32) -> f32 {
    rounded_rect_sdf(x, y, LANTERN.0, LANTERN.1, 0.075, 0.065, 0.02)
}

/// The lantern's center in the unit square.
const LANTERN: (f32, f32) = (0.5, 0.30);

type Vec3 = [f32; 3];

/// The lantern's two opposite beams: cones of light turned by an angle about
/// the tower and seen from slightly above with perspective. Side on, they are
/// the still icon's beams; a beam turning towards the viewer drops and grows
/// until it flares at the lantern, while one turning away rises, shrinks and
/// passes behind the tower.
struct Lens {
    axes: [Vec3; 2],
}

/// How one beam covers a point of the image.
struct Hit {
    alpha: f32,
    /// The flare of a beam seen end on, from 0 to 1.
    glow: f32,
    /// Whether the beam passes in front of the tower here.
    front: bool,
}

/// Where a ray runs through a beam.
struct Span {
    /// The nearest distance to the lantern along it.
    near: f32,
    /// Depth towards the viewer where it enters.
    depth: f32,
    /// The length of the ray inside the beam.
    length: f32,
}

impl Lens {
    /// Downward view angle, and the viewer's distance in lantern-to-tip units.
    const TILT: f32 = 14.0 * PI / 180.0;
    const EYE: f32 = 1.9;
    /// A beam starts outside the lantern and fades towards its tip.
    const NEAR: f32 = 0.12;
    const FAR: f32 = 0.47;
    const HALF_ANGLE: f32 = 13.0 * PI / 180.0;

    /// At angle 0 the beams point left and right; positive angles turn the
    /// right one towards the viewer.
    fn new(angle: f32) -> Self {
        let (s, c) = angle.sin_cos();
        Self {
            axes: [[c, 0.0, s], [-c, 0.0, -s]],
        }
    }

    /// How directly a beam points at the viewer, from 0 (side on) to 1.
    fn facing(&self) -> f32 {
        self.axes.iter().map(|a| a[2]).fold(0.0, f32::max)
    }

    fn hits(&self, x: f32, y: f32) -> [Option<Hit>; 2] {
        let (eye, ray) = Self::view(x, y);
        self.axes.map(|axis| {
            let span = Self::span(eye, ray, axis)?;
            let t = ((span.near - Self::NEAR) / (Self::FAR - Self::NEAR)).clamp(0.0, 1.0);
            // Seen end on, the light along the whole beam adds up into a flare.
            let glow = smoothstep(0.10, 0.30, span.length) * smoothstep(0.2, 0.8, axis[2]);
            let fade = 1.0 - 0.6 * t;
            Some(Hit {
                alpha: fade + (1.0 - fade) * glow,
                glow,
                front: span.depth > 0.0,
            })
        })
    }

    /// The eye and the unit ray through a point of the image, with the
    /// lantern at the origin and the image plane through it at scale 1.
    fn view(x: f32, y: f32) -> (Vec3, Vec3) {
        let (s, c) = Self::TILT.sin_cos();
        let eye = [0.0, Self::EYE * s, Self::EYE * c];
        let (u, v) = (x - LANTERN.0, LANTERN.1 - y);
        let ray = [u, v * c - eye[1], -v * s - eye[2]];
        let n = dot(ray, ray).sqrt();
        (eye, ray.map(|r| r / n))
    }

    /// Where `eye + s·ray` lies within `HALF_ANGLE` of `axis`, between
    /// `NEAR` and `FAR` from the lantern. The spherical ends make a beam seen
    /// side on a sector, like the still icon's.
    fn span(eye: Vec3, ray: Vec3, axis: Vec3) -> Option<Span> {
        let point = |s: f32| -> Vec3 { std::array::from_fn(|i| eye[i] + s * ray[i]) };
        let cos = Self::HALF_ANGLE.cos();
        let inside = |s: f32| {
            let p = point(s);
            let r = dot(p, p).sqrt();
            r >= Self::NEAR && dot(p, axis) >= r * cos
        };
        // |point(s)|² = s² + 2·er·s + ee.
        let (er, ee) = (dot(eye, ray), dot(eye, eye));
        let [lo, hi] = roots(1.0, 2.0 * er, ee - Self::FAR * Self::FAR)?;
        // Split the far sphere's chord where the ray crosses the near sphere,
        // the cone or the plane through the lantern, then keep the inside pieces.
        let (ea, ra) = (dot(eye, axis), dot(ray, axis));
        let cos2 = cos * cos;
        let mut cuts = [lo; 8];
        let mut n = 1;
        let crossings = [
            roots(1.0, 2.0 * er, ee - Self::NEAR * Self::NEAR),
            roots(
                ra * ra - cos2,
                2.0 * (ea * ra - cos2 * er),
                ea * ea - cos2 * ee,
            ),
            (ra != 0.0).then(|| [-ea / ra; 2]),
        ];
        for s in crossings.into_iter().flatten().flatten() {
            if s > lo && s < hi {
                cuts[n] = s;
                n += 1;
            }
        }
        cuts[n] = hi;
        let cuts = &mut cuts[..=n];
        cuts.sort_by(f32::total_cmp);
        let mut span: Option<Span> = None;
        for piece in cuts.windows(2) {
            let (a, b) = (piece[0], piece[1]);
            if b <= a || !inside((a + b) * 0.5) {
                continue;
            }
            // The ray is nearest the lantern at s = -er.
            let p = point((-er).clamp(a, b));
            let span = span.get_or_insert(Span {
                near: f32::MAX,
                depth: point(a)[2],
                length: 0.0,
            });
            span.near = span.near.min(dot(p, p).sqrt());
            span.length += b - a;
        }
        span
    }
}

fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Real roots of a·s² + b·s + c in ascending order; a linear equation's root twice.
fn roots(a: f32, b: f32, c: f32) -> Option<[f32; 2]> {
    if a.abs() < 1e-6 {
        return (b.abs() > 1e-6).then(|| [-c / b; 2]);
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let q = disc.sqrt();
    let (r0, r1) = ((-b - q) / (2.0 * a), (-b + q) / (2.0 * a));
    Some([r0.min(r1), r0.max(r1)])
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Area and mean height of a beam's image over the unit square, sampled
    /// on a grid; `None` when it covers nothing.
    fn extent(lens: &Lens, beam: usize) -> Option<(f32, f32)> {
        const N: usize = 200;
        let (mut area, mut y_sum) = (0.0, 0.0);
        for j in 0..N {
            for i in 0..N {
                let (x, y) = ((i as f32 + 0.5) / N as f32, (j as f32 + 0.5) / N as f32);
                if let Some(hit) = &lens.hits(x, y)[beam] {
                    area += hit.alpha;
                    y_sum += hit.alpha * y;
                }
            }
        }
        (area > 0.0).then(|| (area / (N * N) as f32, y_sum / area))
    }

    #[test]
    fn beams_at_rest_lie_level_across_the_tower() {
        let lens = Lens::new(0.0);
        let (right, left) = (extent(&lens, 0).unwrap(), extent(&lens, 1).unwrap());
        assert!((right.0 - left.0).abs() < 1e-3);
        assert!((right.1 - LANTERN.1).abs() < 0.01 && (left.1 - LANTERN.1).abs() < 0.01);
        // Side on, each beam is a sector from just outside the lantern to its tip.
        for (x, covered) in [
            (0.5 + 0.30, true),
            (0.5 - 0.30, true),
            (0.5 + 0.08, false),
            (0.5 + 0.55, false),
        ] {
            assert_eq!(
                lens.hits(x, LANTERN.1).iter().any(Option::is_some),
                covered,
                "x = {x}"
            );
        }
    }

    #[test]
    fn a_beam_turning_towards_the_viewer_drops_and_grows() {
        // A quarter of the way, the right beam swings towards the viewer and
        // the left one away.
        let lens = Lens::new(PI / 4.0);
        let (near, far) = (extent(&lens, 0).unwrap(), extent(&lens, 1).unwrap());
        assert!(near.0 > far.0 * 1.2, "near {near:?}, far {far:?}");
        assert!(
            near.1 > LANTERN.1 + 0.02 && far.1 < LANTERN.1 - 0.02,
            "near {near:?}, far {far:?}"
        );
        // End on, it flares around the lantern and nothing reaches the sides.
        let lens = Lens::new(PI / 2.0);
        let [flare, _] = lens.hits(LANTERN.0, LANTERN.1 + 0.05);
        let flare = flare.unwrap();
        assert!(flare.front && flare.glow > 0.5);
        assert!(lens.hits(0.5 + 0.40, LANTERN.1).iter().all(Option::is_none));
        // The beam turning away passes behind the tower.
        assert!(
            lens.hits(LANTERN.0, LANTERN.1 - 0.05)[1]
                .as_ref()
                .is_none_or(|h| !h.front)
        );
    }

    #[test]
    fn a_turn_eases_from_rest_back_to_rest() {
        assert_eq!(Turn::at(0.0), Turn::REST);
        let end = Turn::at(1.0);
        assert_eq!(end.tint, 0.0);
        // Half a circle later the beams have traded places, which looks the same.
        let same = |a: &[u8], b: &[u8]| a.iter().zip(b).all(|(a, b)| a.abs_diff(*b) <= 1);
        assert!(same(
            &badge(64, Some(Status::Running), end),
            &badge(64, Some(Status::Running), Turn::REST)
        ));
        assert!(!same(
            &badge(64, Some(Status::Running), Turn::at(0.3)),
            &badge(64, Some(Status::Running), Turn::REST)
        ));
        // A stopped lighthouse has no beams to turn.
        assert_eq!(
            badge(64, Some(Status::Stopped), Turn::at(0.3)),
            badge(64, Some(Status::Stopped), Turn::REST)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn menu_bar_frames_start_from_the_template_and_take_the_app_color() {
        // At rest a colored frame matches the template drawn in the light bar's
        // ink, so swapping between them does not jump.
        let template = template(44, Status::Running);
        for service in [Service::Claude, Service::Codex] {
            let rest = beacon(44, Turn::REST, service, false);
            for (t, c) in template.chunks(4).zip(rest.chunks(4)) {
                assert!((t[3] as f32 * 0.85 - c[3] as f32).abs() <= 1.0);
            }
        }
        // Halfway the lantern flares in the app's color.
        // The lantern sits at row 15 of the 44 px glyph.
        let (x, y) = (22 * 4, 15 * 44 * 4);
        let lantern = |service, dark| {
            let px = beacon(44, Turn::at(0.5), service, dark);
            [px[y + x], px[y + x + 1], px[y + x + 2]]
        };
        for dark in [false, true] {
            let (claude, codex) = (
                lantern(Service::Claude, dark),
                lantern(Service::Codex, dark),
            );
            assert!(claude[0] > claude[2] + 40, "{claude:?}");
            assert!(codex[2] > codex[0] + 40, "{codex:?}");
        }
        assert!(alpha(&template) > alpha(&super::template(44, Status::Stopped)));
    }

    fn alpha(rgba: &[u8]) -> u64 {
        rgba.chunks(4).map(|px| px[3] as u64).sum()
    }
}
