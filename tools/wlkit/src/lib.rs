//! Shared helpers for the Wayland tools: Catppuccin Mocha colors, a tiny
//! software canvas (BGRA / wl_shm), font rendering and image scaling.
//! Pixel format everywhere is little-endian ARGB8888 (bytes B,G,R,A).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use image::imageops::{self, FilterType};
use image::RgbaImage;

/// Catppuccin Mocha, 0xAARRGGBB.
pub mod theme {
    pub const CRUST: u32 = 0xff11111b;
    pub const MANTLE: u32 = 0xff181825;
    pub const BASE: u32 = 0xff1e1e2e;
    pub const SURFACE0: u32 = 0xff313244;
    pub const SURFACE1: u32 = 0xff45475a;
    pub const OVERLAY0: u32 = 0xff6c7086;
    pub const TEXT: u32 = 0xffcdd6f4;
    pub const BLUE: u32 = 0xff89b4fa;
    pub const MAUVE: u32 = 0xffcba6f7;
    pub const RED: u32 = 0xfff38ba8;
    pub const GREEN: u32 = 0xffa6e3a1;
}

/// UI font size in points; converted to pixels at 96 dpi.
pub const FONT_PT: f32 = 16.0;

pub struct Canvas<'a> {
    pub buf: &'a mut [u8],
    pub w: i32,
    pub h: i32,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut [u8], w: i32, h: i32) -> Self {
        Canvas { buf, w, h }
    }

    pub fn clear(&mut self, c: u32) {
        let px = c.to_le_bytes(); // B G R A
        for chunk in self.buf.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let px = c.to_le_bytes();
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w);
        let y1 = (y + h).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for yy in y0..y1 {
            let row = (yy * self.w) as usize * 4;
            for xx in x0..x1 {
                let i = row + xx as usize * 4;
                if i + 4 <= self.buf.len() {
                    self.buf[i..i + 4].copy_from_slice(&px);
                }
            }
        }
    }

    /// Blend one pixel of color `c` with coverage `cov` over what is there.
    pub fn blend(&mut self, x: i32, y: i32, c: u32, cov: u8) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) as usize) * 4;
        if i + 4 > self.buf.len() {
            return;
        }
        let a = cov as u32;
        let src = [(c & 0xff) as u32, ((c >> 8) & 0xff) as u32, ((c >> 16) & 0xff) as u32];
        for k in 0..3 {
            let d = self.buf[i + k] as u32;
            self.buf[i + k] = ((src[k] * a + d * (255 - a)) / 255) as u8;
        }
    }

    /// Copy a BGRA image (sw*sh*4 bytes) to (dx, dy), clipped.
    pub fn blit(&mut self, src: &[u8], sw: i32, sh: i32, dx: i32, dy: i32) {
        for y in 0..sh {
            let ty = dy + y;
            if ty < 0 || ty >= self.h {
                continue;
            }
            for x in 0..sw {
                let tx = dx + x;
                if tx < 0 || tx >= self.w {
                    continue;
                }
                let s = ((y * sw + x) as usize) * 4;
                let d = ((ty * self.w + tx) as usize) * 4;
                if s + 4 <= src.len() && d + 4 <= self.buf.len() {
                    self.buf[d..d + 4].copy_from_slice(&src[s..s + 4]);
                }
            }
        }
    }
}

struct Glyph {
    m: fontdue::Metrics,
    bmp: Vec<u8>,
}

pub struct Font {
    faces: Vec<fontdue::Font>,
    px: f32,
    pub ascent: i32,
    pub descent: i32,
    cache: RefCell<HashMap<char, Rc<Glyph>>>,
}

impl Font {
    /// Loads FiraCode Nerd Font / Fira Code (bold if asked) plus a few
    /// fallbacks for missing glyphs. Returns None if no font is installed.
    pub fn load(pt: f32, bold: bool) -> Option<Font> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let weight = if bold { fontdb::Weight::BOLD } else { fontdb::Weight::NORMAL };
        let mut faces = Vec::new();
        for fam in [
            "FiraCode Nerd Font",
            "Fira Code",
            "DejaVu Sans Mono",
            "Noto Sans Mono",
            "DejaVu Sans",
            "Noto Sans",
        ] {
            let q = fontdb::Query {
                families: &[fontdb::Family::Name(fam)],
                weight,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = db.query(&q) {
                let face = db
                    .with_face_data(id, |data, index| {
                        fontdue::Font::from_bytes(
                            data,
                            fontdue::FontSettings {
                                collection_index: index,
                                ..Default::default()
                            },
                        )
                        .ok()
                    })
                    .flatten();
                if let Some(f) = face {
                    faces.push(f);
                }
            }
        }
        if faces.is_empty() {
            return None;
        }
        let px = pt * 96.0 / 72.0;
        let (ascent, descent) = match faces[0].horizontal_line_metrics(px) {
            Some(m) => (m.ascent.ceil() as i32, (-m.descent).ceil() as i32),
            None => (px.ceil() as i32, (px / 4.0).ceil() as i32),
        };
        Some(Font {
            faces,
            px,
            ascent,
            descent,
            cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn height(&self) -> i32 {
        self.ascent + self.descent
    }

    fn glyph(&self, ch: char) -> Rc<Glyph> {
        if let Some(g) = self.cache.borrow().get(&ch) {
            return g.clone();
        }
        let face = self
            .faces
            .iter()
            .find(|f| f.lookup_glyph_index(ch) != 0)
            .unwrap_or(&self.faces[0]);
        let (m, bmp) = face.rasterize(ch, self.px);
        let g = Rc::new(Glyph { m, bmp });
        self.cache.borrow_mut().insert(ch, g.clone());
        g
    }

    pub fn width(&self, s: &str) -> i32 {
        let mut w = 0f32;
        for ch in s.chars() {
            if ch.is_control() {
                continue;
            }
            w += self.glyph(ch).m.advance_width;
        }
        w.ceil() as i32
    }

    /// Longest prefix of `s` that is at most `max_w` pixels wide.
    pub fn fit<'s>(&self, s: &'s str, max_w: i32) -> &'s str {
        let mut w = 0f32;
        for (i, ch) in s.char_indices() {
            if !ch.is_control() {
                w += self.glyph(ch).m.advance_width;
            }
            if w.ceil() as i32 > max_w {
                return &s[..i];
            }
        }
        s
    }

    /// Draw `s` with its top at `y`, clipped to `max_w`. Returns the width drawn.
    pub fn draw(&self, cv: &mut Canvas, x: i32, y: i32, s: &str, color: u32, max_w: i32) -> i32 {
        let base = y + self.ascent;
        let mut pen = 0f32;
        for ch in s.chars() {
            if ch.is_control() {
                continue;
            }
            let g = self.glyph(ch);
            if (pen + g.m.advance_width).ceil() as i32 > max_w {
                break;
            }
            let gx = x + pen as i32 + g.m.xmin;
            let gy = base - g.m.height as i32 - g.m.ymin;
            for row in 0..g.m.height {
                for col in 0..g.m.width {
                    let cov = g.bmp[row * g.m.width + col];
                    if cov != 0 {
                        cv.blend(gx + col as i32, gy + row as i32, color, cov);
                    }
                }
            }
            pen += g.m.advance_width;
        }
        pen.ceil() as i32
    }
}

pub fn open_rgba(path: &str) -> Option<RgbaImage> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > (256 << 20) {
        return None;
    }
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
        .map(|i| i.to_rgba8())
}

pub fn solid(w: u32, h: u32, c: u32) -> Vec<u8> {
    let px = c.to_le_bytes();
    let mut v = vec![0u8; (w as usize) * (h as usize) * 4];
    for chunk in v.chunks_exact_mut(4) {
        chunk.copy_from_slice(&px);
    }
    v
}

/// Scale that makes an iw x ih image fit (or cover) a w x h area.
pub fn fit_scale(iw: u32, ih: u32, w: u32, h: u32, cover: bool) -> f64 {
    let sx = w as f64 / iw.max(1) as f64;
    let sy = h as f64 / ih.max(1) as f64;
    if cover {
        sx.max(sy)
    } else {
        sx.min(sy)
    }
}

/// Render `img` scaled by `scale` (destination px per source px), centered in
/// a w x h BGRA buffer filled with `bg`, shifted by (panx, pany). Only the
/// visible part is resampled, so heavy zoom stays cheap.
pub fn render(img: &RgbaImage, w: u32, h: u32, scale: f64, panx: i32, pany: i32, bg: u32) -> Vec<u8> {
    let mut out = solid(w, h, bg);
    if w == 0 || h == 0 || scale <= 0.0 {
        return out;
    }
    let (iw, ih) = (img.width() as f64, img.height() as f64);
    let (dw, dh) = (iw * scale, ih * scale);
    let x0 = (w as f64 - dw) / 2.0 + panx as f64;
    let y0 = (h as f64 - dh) / 2.0 + pany as f64;
    let vx0 = x0.max(0.0).floor();
    let vy0 = y0.max(0.0).floor();
    let vx1 = (x0 + dw).min(w as f64).ceil();
    let vy1 = (y0 + dh).min(h as f64).ceil();
    if vx1 <= vx0 || vy1 <= vy0 {
        return out;
    }
    let sx0 = ((vx0 - x0) / scale).floor().max(0.0);
    let sy0 = ((vy0 - y0) / scale).floor().max(0.0);
    let sx1 = ((vx1 - x0) / scale).ceil().min(iw);
    let sy1 = ((vy1 - y0) / scale).ceil().min(ih);
    let (sw, sh) = ((sx1 - sx0) as u32, (sy1 - sy0) as u32);
    let (tw, th) = ((vx1 - vx0) as u32, (vy1 - vy0) as u32);
    if sw == 0 || sh == 0 || tw == 0 || th == 0 {
        return out;
    }
    let crop = imageops::crop_imm(img, sx0 as u32, sy0 as u32, sw, sh).to_image();
    let filter = if scale > 4.0 { FilterType::Nearest } else { FilterType::Triangle };
    let scaled = imageops::resize(&crop, tw, th, filter);
    for y in 0..th {
        for x in 0..tw {
            let dx = vx0 as u32 + x;
            let dy = vy0 as u32 + y;
            if dx >= w || dy >= h {
                continue;
            }
            let p = scaled.get_pixel(x, y).0;
            let a = p[3] as u32;
            let i = ((dy * w + dx) as usize) * 4;
            out[i] = ((p[2] as u32 * a + out[i] as u32 * (255 - a)) / 255) as u8;
            out[i + 1] = ((p[1] as u32 * a + out[i + 1] as u32 * (255 - a)) / 255) as u8;
            out[i + 2] = ((p[0] as u32 * a + out[i + 2] as u32 * (255 - a)) / 255) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

/// Load `path` scaled to fit (or cover) w x h on a `bg` background. BGRA, opaque.
pub fn load_fit(path: &str, w: u32, h: u32, cover: bool, bg: u32) -> Option<Vec<u8>> {
    let img = open_rgba(path)?;
    let s = fit_scale(img.width(), img.height(), w, h, cover);
    Some(render(&img, w, h, s, 0, 0, bg))
}

/// Nearest-neighbour scale of raw RGB(A) pixel data into a size x size BGRA tile
/// (used for notification `image-data` hints).
pub fn raw_to_tile(src: &[u8], w: i32, h: i32, stride: i32, ch: i32, size: u32, bg: u32) -> Vec<u8> {
    let mut out = solid(size, size, bg);
    if w <= 0 || h <= 0 || ch < 3 {
        return out;
    }
    let s = w.max(h);
    let dw = ((w as i64 * size as i64) / s as i64).max(1) as i32;
    let dh = ((h as i64 * size as i64) / s as i64).max(1) as i32;
    let ox = (size as i32 - dw) / 2;
    let oy = (size as i32 - dh) / 2;
    for y in 0..dh {
        for x in 0..dw {
            let sx = (x as i64 * w as i64 / dw as i64) as i32;
            let sy = (y as i64 * h as i64 / dh as i64) as i32;
            let si = (sy * stride + sx * ch) as usize;
            if si + 3 > src.len() {
                continue;
            }
            let a = if ch == 4 && si + 3 < src.len() { src[si + 3] as u32 } else { 255 };
            let di = (((oy + y) as u32 * size + (ox + x) as u32) as usize) * 4;
            out[di] = ((src[si + 2] as u32 * a + out[di] as u32 * (255 - a)) / 255) as u8;
            out[di + 1] = ((src[si + 1] as u32 * a + out[di + 1] as u32 * (255 - a)) / 255) as u8;
            out[di + 2] = ((src[si] as u32 * a + out[di + 2] as u32 * (255 - a)) / 255) as u8;
            out[di + 3] = 255;
        }
    }
    out
}
