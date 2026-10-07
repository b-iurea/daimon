//! Boot splash on the framebuffer: the Daimon mark (a ring drawn on, two orbiting comets, a breathing
//! core), the wordmark with a light sweep, and the boot steps, while the brain and the controller load.
//! Only the three regions that move are recomputed each frame, off-screen, then copied to /dev/fb0.

use crate::fb::Fb;
use std::f32::consts::TAU;
use std::time::Instant;

pub const CODENAME: &str = "Deucalion";
/// built by tools/mklogo.py
const LOGO: &str = "/usr/share/aios/logo.alf";
/// cap height of the master wordmark in LOGO
const LOGO_CAP: f32 = 160.0;

type Rgb = [f32; 3];
const BG: Rgb = [15., 23., 42.];
const ACCENT: Rgb = [34., 197., 94.];
const SKY: Rgb = [56., 189., 248.];
const WHITE: Rgb = [248., 250., 252.];
const TEXT: Rgb = [226., 232., 240.];
const MUTED: Rgb = [148., 163., 184.];
const FAINT: Rgb = [100., 116., 139.];
const TRACK: Rgb = [51., 65., 85.];

pub enum State {
    Wait,
    /// working, with a known fraction when there is one
    Busy(Option<f32>),
    Done,
    /// not running / skipped: doesn't hold the boot
    Off,
}

pub struct Step {
    pub label: &'static str,
    pub detail: String,
    pub state: State,
}

fn mix(a: Rgb, b: Rgb, k: f32) -> Rgb {
    [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k]
}

fn ramp(t: f32, a: f32, b: f32) -> f32 {
    ((t - a) / (b - a)).clamp(0.0, 1.0)
}

fn ease(x: f32) -> f32 {
    1.0 - (1.0 - x.clamp(0.0, 1.0)).powi(3)
}

/// Coverage of a ring of radius `r` and width `w` at distance `d` from its centre (1 px anti-aliasing).
fn ring(d: f32, r: f32, w: f32) -> f32 {
    (w / 2.0 + 0.5 - (d - r).abs()).clamp(0.0, 1.0)
}

/// An off-screen block of the screen, in absolute pixel coordinates.
struct Canvas {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    px: Vec<Rgb>,
}

impl Canvas {
    fn new(fb: &Fb, x0: f32, y0: f32, w: f32, h: f32) -> Canvas {
        let (x0, y0) = (x0.max(0.0) as usize, y0.max(0.0) as usize);
        let (w, h) = ((w as usize).min(fb.px_w.saturating_sub(x0)), (h as usize).min(fb.px_h.saturating_sub(y0)));
        Canvas { x0, y0, w, h, px: vec![BG; w * h] }
    }

    fn at(&mut self, x: usize, y: usize) -> Option<&mut Rgb> {
        (x >= self.x0 && y >= self.y0 && x < self.x0 + self.w && y < self.y0 + self.h).then(|| &mut self.px[(y - self.y0) * self.w + x - self.x0])
    }

    fn over(&mut self, x: usize, y: usize, c: Rgb, a: f32) {
        if let Some(p) = self.at(x, y) {
            *p = mix(*p, c, a.clamp(0.0, 1.0));
        }
    }

    /// Runs `f(x, y)` for every pixel of the canvas (pixel centres, absolute coordinates).
    fn shade(&mut self, mut f: impl FnMut(f32, f32, &mut Rgb)) {
        for (i, p) in self.px.iter_mut().enumerate() {
            f((self.x0 + i % self.w) as f32 + 0.5, (self.y0 + i / self.w) as f32 + 0.5, p);
        }
    }

    /// Text in the UI font with extra letter spacing `track` (px); returns its end x.
    fn text(&mut self, fb: &Fb, x: f32, y: f32, s: &str, c: Rgb, a: f32, track: f32) -> f32 {
        let cw = fb.cell_px().0;
        let mut x = x;
        for chr in s.chars() {
            if let Some(g) = fb.glyph_alpha(chr, false) {
                for (i, v) in g.iter().enumerate().filter(|(_, v)| **v > 0) {
                    self.over(x as usize + i % cw, y as usize + i / cw, c, a * *v as f32 / 255.0);
                }
            }
            x += cw as f32 + track;
        }
        x
    }

    /// To the screen; `fade` 1 = as drawn, 0 = plain background.
    fn flush(&self, fb: &mut Fb, fade: f32) {
        let px: Vec<(u8, u8, u8)> = self
            .px
            .iter()
            .map(|p| {
                let c = mix(BG, *p, fade);
                (c[0].clamp(0.0, 255.0) as u8, c[1].clamp(0.0, 255.0) as u8, c[2].clamp(0.0, 255.0) as u8)
            })
            .collect();
        fb.blit(self.x0, self.y0, self.w, &px);
    }
}

pub struct Splash {
    t0: Instant,
    leaving: Option<Instant>,
    /// 1.0 on a 1080-line screen
    s: f32,
    /// the wordmark, scaled to this screen: (w, h, 8-bit alpha)
    wm: (usize, usize, Vec<u8>),
}

impl Splash {
    pub fn new(fb: &mut Fb) -> Splash {
        let s = (fb.px_h as f32 / 1080.0).min(fb.px_w as f32 / 1440.0).clamp(0.5, 2.5);
        let wm = load_logo(50.0 * s).unwrap_or((0, 0, vec![]));
        let all = Canvas::new(fb, 0.0, 0.0, fb.px_w as f32, fb.px_h as f32);
        all.flush(fb, 1.0);
        Splash { t0: Instant::now(), leaving: None, s, wm }
    }

    pub fn secs(&self) -> f32 {
        self.t0.elapsed().as_secs_f32()
    }

    /// Start the fade into the console (idempotent).
    pub fn leave(&mut self) {
        self.leaving.get_or_insert_with(Instant::now);
    }

    /// Draws one frame; false once the fade-out is over.
    pub fn frame(&mut self, fb: &mut Fb, steps: &[Step]) -> bool {
        let t = self.secs();
        let fade = self.leaving.map_or(1.0, |l| 1.0 - ease(l.elapsed().as_secs_f32() / 0.6));
        if fade <= 0.0 {
            return false;
        }
        let s = self.s;
        let (cw, ch) = fb.cell_px();
        let (cw, ch) = (cw as f32, ch as f32);
        let (cx, cy) = (fb.px_w as f32 / 2.0, fb.px_h as f32 * 0.31);
        let r = 80.0 * s;

        // --- the mark
        let half = 1.75 * r;
        let mut c = Canvas::new(fb, cx - half, cy - half, 2.0 * half, 2.0 * half);
        let appear = ease(ramp(t, 0.0, 1.1));
        let orbit = ease(ramp(t, 0.6, 1.5));
        let breath = 0.5 + 0.5 * (t * TAU / 3.2).sin();
        let sweep = ease(ramp(t, 0.1, 1.3)) * TAU;
        let core = 0.16 * r * (0.94 + 0.06 * breath) * ease(ramp(t, 0.3, 0.9));
        let (inner, outer) = ((t * TAU / 2.2).rem_euclid(TAU), (-t * TAU / 3.4).rem_euclid(TAU));
        c.shade(|x, y, p| {
            let (dx, dy) = (x - cx, y - cy);
            let d = (dx * dx + dy * dy).sqrt();
            // 0 at the top, clockwise
            let a = dx.atan2(-dy).rem_euclid(TAU);
            let halo = (0.10 + 0.06 * breath) * (-(d / (0.9 * r)).powi(2)).exp() + 0.35 * breath * (-(d / (0.42 * r)).powi(2)).exp();
            *p = mix(*p, ACCENT, halo * appear);
            let drawn = ((sweep - a) * r / 1.5).clamp(0.0, 1.0);
            *p = mix(*p, mix(ACCENT, SKY, 0.5 - 0.5 * (a - t * 0.5).cos()), ring(d, r, 2.0 * s) * drawn * 0.9);
            *p = mix(*p, TRACK, ring(d, 0.58 * r, 1.0 * s) * 0.5 * appear);
            // comets: inner one clockwise (green), outer one counter-clockwise (blue)
            let k = 1.0 - (inner - a).rem_euclid(TAU) / 2.4;
            if k > 0.0 {
                *p = mix(*p, mix(ACCENT, WHITE, 0.4 * k.powi(4)), ring(d, 0.58 * r, 3.0 * s) * k * k * orbit);
            }
            let k = 1.0 - (a - outer).rem_euclid(TAU) / 1.4;
            if k > 0.0 {
                *p = mix(*p, SKY, ring(d, 0.80 * r, 1.6 * s) * k * k * 0.75 * orbit);
            }
            *p = mix(*p, mix(ACCENT, WHITE, 0.3), (core - d + 0.5).clamp(0.0, 1.0));
        });
        c.flush(fb, fade);

        // --- wordmark, version and codename
        let (ww, wh) = (self.wm.0 as f32, self.wm.1 as f32);
        let wm_y = cy + half + 22.0 * s;
        let ver = format!("v{}  ·  {}", env!("CARGO_PKG_VERSION"), CODENAME.to_uppercase());
        let track = cw * 0.3;
        let ver_w = ver.chars().count() as f32 * (cw + track) - track;
        let block_w = ww.max(ver_w) + 4.0;
        let mut c = Canvas::new(fb, cx - block_w / 2.0, wm_y - 14.0 * s, block_w, wh + 14.0 * s + 18.0 * s + ch + 2.0);
        let shown = ease(ramp(t, 0.7, 1.6));
        let slide = (1.0 - shown) * 12.0 * s;
        let x0 = cx - ww / 2.0;
        // a soft band of light crosses the name every 6 s
        let band = x0 - 80.0 * s + (ww + 160.0 * s) * ease(ramp((t - 1.6).rem_euclid(6.0), 0.0, 1.4));
        let shine = if t > 1.6 { 1.0 } else { 0.0 };
        for y in 0..self.wm.1 {
            for x in 0..self.wm.0 {
                let v = self.wm.2[y * self.wm.0 + x];
                if v > 0 {
                    let lit = shine * (-((x0 + x as f32 - band) / (34.0 * s)).powi(2)).exp();
                    let col = mix(mix(BG, WHITE, 0.86), mix(WHITE, ACCENT, 0.15), lit);
                    c.over((x0 + x as f32) as usize, (wm_y + y as f32 + slide) as usize, col, shown * v as f32 / 255.0);
                }
            }
        }
        c.text(fb, cx - ver_w / 2.0, wm_y + wh + 18.0 * s, &ver, FAINT, ease(ramp(t, 1.0, 1.8)), track);
        let ver_bottom = wm_y + wh + 18.0 * s + ch;
        c.flush(fb, fade);

        // --- boot steps and overall progress
        let col_w = 44.0 * cw;
        let row_h = (ch * 1.55).round();
        let left = cx - col_w / 2.0;
        let top = ver_bottom + 40.0 * s;
        let mut c = Canvas::new(fb, left - 4.0, top, col_w + 8.0, row_h * steps.len() as f32 + ch * 3.0);
        let (mut done, mut total) = (0.0, 0.0);
        for (i, st) in steps.iter().enumerate() {
            let a = ease(ramp(t, 1.2 + i as f32 * 0.1, 1.6 + i as f32 * 0.1));
            let y = top + i as f32 * row_h;
            let (ix, iy, ir) = (left + ch * 0.4, y + ch / 2.0, ch * 0.27);
            for py in (iy - ir - 2.0) as usize..=(iy + ir + 2.0) as usize {
                for px in (ix - ir - 2.0) as usize..=(ix + ir + 2.0) as usize {
                    let (dx, dy) = (px as f32 + 0.5 - ix, py as f32 + 0.5 - iy);
                    let d = (dx * dx + dy * dy).sqrt();
                    let ang = dx.atan2(-dy).rem_euclid(TAU);
                    let (col, cov) = match st.state {
                        State::Done => (ACCENT, (ir - d + 0.5).clamp(0.0, 1.0)),
                        State::Busy(_) => {
                            let arc = (1.0 - (t * TAU / 1.1 - ang).rem_euclid(TAU) / 1.9).max(0.0);
                            let track = ring(d, ir - 0.75 * s, 1.5 * s);
                            c.over(px, py, TRACK, track * a);
                            (ACCENT, track * arc)
                        }
                        State::Wait => (FAINT, ring(d, ir - 0.75 * s, 1.2 * s)),
                        State::Off => (FAINT, (ir * 0.35 - d + 0.5).clamp(0.0, 1.0)),
                    };
                    c.over(px, py, col, cov * a);
                }
            }
            let strong = matches!(st.state, State::Done | State::Busy(_));
            c.text(fb, left + ch * 1.3, y, st.label, if strong { TEXT } else { MUTED }, a, 0.0);
            let (detail, dc) = match st.state {
                State::Busy(Some(f)) => (format!("{}  {:>3.0}%", st.detail, f * 100.0), ACCENT),
                _ => (st.detail.clone(), FAINT),
            };
            let dw = detail.chars().count() as f32 * cw;
            c.text(fb, left + col_w - dw, y, &detail, dc, a, 0.0);
            let (d, n) = match st.state {
                State::Done => (1.0, 1.0),
                State::Busy(f) => (f.unwrap_or(0.0) * 0.9, 1.0),
                State::Wait => (0.0, 1.0),
                State::Off => (0.0, 0.0),
            };
            done += d;
            total += n;
        }
        // overall bar: gradient fill and a glint running along the filled part
        let by = top + steps.len() as f32 * row_h + ch * 0.6;
        let bh = (3.0 * s).max(2.0);
        let frac = if total > 0.0 { done / total } else { 1.0 };
        let a = ease(ramp(t, 1.6, 2.2));
        let glint = (t * 0.7).rem_euclid(1.0);
        c.shade(|x, y, p| {
            if y < by || y > by + bh {
                return;
            }
            let u = (x - left) / col_w;
            if !(0.0..=1.0).contains(&u) {
                return;
            }
            *p = mix(*p, TRACK, 0.8 * a);
            if u <= frac {
                let g = (-((u - glint * frac) / 0.04).powi(2)).exp();
                *p = mix(*p, mix(mix(ACCENT, SKY, u), WHITE, 0.6 * g), a);
            }
        });
        if t > 4.0 {
            let hint = "press any key to skip";
            let hw = hint.chars().count() as f32 * cw;
            c.text(fb, cx - hw / 2.0, by + bh + ch * 0.9, hint, FAINT, 0.7 * ramp(t, 4.0, 5.0), 0.0);
        }
        c.flush(fb, fade);
        true
    }
}

/// The wordmark scaled (area average) to a cap height of `cap` px.
fn load_logo(cap: f32) -> Option<(usize, usize, Vec<u8>)> {
    let b = std::fs::read(LOGO).ok()?;
    if b.get(..4)? != b"ALF1" {
        return None;
    }
    let sw = u16::from_le_bytes(b.get(4..6)?.try_into().ok()?) as usize;
    let sh = u16::from_le_bytes(b.get(6..8)?.try_into().ok()?) as usize;
    let src = b.get(8..8 + sw * sh)?;
    let k = (cap / LOGO_CAP).min(1.0);
    let (w, h) = (((sw as f32 * k) as usize).max(1), ((sh as f32 * k) as usize).max(1));
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1));
            let sum: u32 = (y0..y1).flat_map(|sy| src[sy * sw + x0..sy * sw + x1].iter()).map(|&v| v as u32).sum();
            out[y * w + x] = (sum / ((y1 - y0) * (x1 - x0)) as u32) as u8;
        }
    }
    Some((w, h, out))
}
