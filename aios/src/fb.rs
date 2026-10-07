//! A ratatui backend that paints straight onto the Linux framebuffer (/dev/fb0): 24-bit colour and
//! anti-aliased glyphs from the fonts built by tools/mkfont.py, instead of the VGA console's 16 colours
//! and 512 glyphs. The VT is switched to graphics mode so the kernel console doesn't draw over us;
//! the keyboard still arrives on the tty as usual.

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;

pub const FONT_DIR: &str = "/usr/share/aios/fonts";

const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOGET_FSCREENINFO: libc::Ioctl = 0x4602;
const KDSETMODE: libc::Ioctl = 0x4B3A;
const KD_TEXT: libc::c_ulong = 0;
const KD_GRAPHICS: libc::c_ulong = 1;

#[repr(C)]
#[derive(Default)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default)]
struct VarInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    // nonstd .. colorspace (16 fields) + reserved[4]: the kernel writes all 160 bytes
    rest: [u32; 20],
}

#[repr(C)]
struct FixInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    kind: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

pub struct Font {
    pub w: usize,
    pub h: usize,
    cps: Vec<u32>,
    data: Vec<u8>,
}

impl Font {
    pub fn load(path: &str) -> Option<Font> {
        let b = fs::read(path).ok()?;
        if b.get(..4)? != b"AFN2" {
            return None;
        }
        let w = u16::from_le_bytes(b.get(4..6)?.try_into().ok()?) as usize;
        let h = u16::from_le_bytes(b.get(6..8)?.try_into().ok()?) as usize;
        let n = u32::from_le_bytes(b.get(8..12)?.try_into().ok()?) as usize;
        let cps = (0..n).map(|i| Some(u32::from_le_bytes(b.get(12 + 4 * i..16 + 4 * i)?.try_into().ok()?))).collect::<Option<Vec<_>>>()?;
        let data = b.get(12 + 4 * n..)?.to_vec();
        (data.len() >= 2 * n * (w * h).div_ceil(2)).then_some(Font { w, h, cps, data })
    }

    /// 4-bit alpha bitmap of `c` (bold or regular), or the replacement glyph
    fn glyph(&self, c: char, bold: bool) -> Option<&[u8]> {
        let i = self.cps.binary_search(&(c as u32)).or_else(|_| self.cps.binary_search(&0xFFFD)).ok()?;
        let size = (self.w * self.h).div_ceil(2);
        let at = (if bold { self.cps.len() } else { 0 } + i) * size;
        self.data.get(at..at + size)
    }

    /// Fonts shipped in the image, smallest cell first: (cell height, path)
    pub fn available() -> Vec<(usize, String)> {
        let mut v: Vec<(usize, String)> = fs::read_dir(FONT_DIR)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let h = n.strip_prefix("font-")?.strip_suffix(".fnt")?.parse().ok()?;
                Some((h, e.path().to_string_lossy().into_owned()))
            })
            .collect();
        v.sort();
        v
    }
}

pub struct Fb {
    mem: *mut u8,
    len: usize,
    stride: usize,
    bytes_pp: usize,
    shifts: (u32, u32, u32),
    pub px_w: usize,
    pub px_h: usize,
    font: Font,
    cols: u16,
    rows: u16,
    cursor: Position,
    tty: Option<fs::File>,
}

// the mapping is only touched from the UI thread
unsafe impl Send for Fb {}

/// The cell height to use: `want` px if that font exists, else the largest font that still gives a
/// usable grid (>= 110 columns and 34 rows) on this screen. The physical world varies: 1024x768 VMs,
/// 4K monitors; `ui_font` in the config overrides the guess.
fn pick_font(px_w: usize, px_h: usize, want: &str) -> Option<Font> {
    let fonts = Font::available();
    if let Some((_, p)) = fonts.iter().find(|(h, _)| h.to_string() == want) {
        return Font::load(p);
    }
    let mut best = fonts.first()?.1.clone();
    for (_, p) in &fonts {
        if let Some(f) = Font::load(p) {
            if px_w / f.w >= 110 && px_h / f.h >= 34 {
                best = p.clone();
            }
        }
    }
    Font::load(&best)
}

impl Fb {
    /// Err(why) when there is no usable framebuffer: the caller falls back to the text console.
    pub fn open(font_pref: &str) -> Result<Fb, String> {
        let f = fs::OpenOptions::new().read(true).write(true).open("/dev/fb0").map_err(|e| format!("/dev/fb0: {e}"))?;
        let fd = f.as_raw_fd();
        let mut var = VarInfo::default();
        let mut fix: FixInfo = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) } < 0 || unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) } < 0 {
            return Err("framebuffer info ioctl failed".into());
        }
        // ponytail: 32 bpp only (what simpledrm/efifb/virtio give); 16 bpp panels fall back to the text console
        if var.bits_per_pixel != 32 {
            return Err(format!("{} bpp framebuffer", var.bits_per_pixel));
        }
        let (px_w, px_h) = (var.xres as usize, var.yres as usize);
        let font = pick_font(px_w, px_h, font_pref).ok_or(format!("no font in {FONT_DIR}"))?;
        let len = fix.smem_len as usize;
        let mem = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        if mem == libc::MAP_FAILED {
            return Err(format!("mmap: {}", io::Error::last_os_error()));
        }
        let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty0").ok();
        if let Some(t) = &tty {
            unsafe { libc::ioctl(t.as_raw_fd(), KDSETMODE, KD_GRAPHICS) };
        }
        Ok(Fb {
            mem: mem.cast(),
            len,
            stride: fix.line_length as usize,
            bytes_pp: 4,
            shifts: (var.red.offset, var.green.offset, var.blue.offset),
            px_w,
            px_h,
            cols: (px_w / font.w) as u16,
            rows: (px_h / font.h) as u16,
            font,
            cursor: Position::default(),
            tty,
        })
    }

    pub fn cell_px(&self) -> (usize, usize) {
        (self.font.w, self.font.h)
    }

    fn pixel(&self, (r, g, b): (u8, u8, u8)) -> u32 {
        (r as u32) << self.shifts.0 | (g as u32) << self.shifts.1 | (b as u32) << self.shifts.2
    }

    fn fill(&mut self, x0: usize, y0: usize, w: usize, h: usize, rgb: (u8, u8, u8)) {
        let p = self.pixel(rgb);
        for y in y0..(y0 + h).min(self.px_h) {
            for x in x0..(x0 + w).min(self.px_w) {
                let at = y * self.stride + x * self.bytes_pp;
                if at + 4 <= self.len {
                    unsafe { (self.mem.add(at) as *mut u32).write_volatile(p) };
                }
            }
        }
    }

    fn put(&mut self, col: u16, row: u16, cell: &Cell) {
        let (w, h) = (self.font.w, self.font.h);
        let (x0, y0) = (col as usize * w, row as usize * h);
        let m = cell.modifier;
        let (mut fg, mut bg) = (rgb(cell.fg, true), rgb(cell.bg, false));
        if m.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if m.contains(Modifier::DIM) {
            fg = mix(bg, fg, 150);
        }
        let ch = cell.symbol().chars().next().unwrap_or(' ');
        let glyph = if ch == ' ' { None } else { self.font.glyph(ch, m.contains(Modifier::BOLD)).map(<[u8]>::to_vec) };
        let Some(glyph) = glyph else {
            return self.fill(x0, y0, w, h, bg);
        };
        let underline = m.contains(Modifier::UNDERLINED);
        for y in 0..h {
            if y0 + y >= self.px_h {
                break;
            }
            for x in 0..w {
                if x0 + x >= self.px_w {
                    break;
                }
                let i = y * w + x;
                let nib = if i % 2 == 0 { glyph[i / 2] >> 4 } else { glyph[i / 2] & 15 };
                let a = if underline && y == h - 2 { 255 } else { nib as u32 * 17 };
                let at = (y0 + y) * self.stride + (x0 + x) * self.bytes_pp;
                if at + 4 <= self.len {
                    let p = self.pixel(mix(bg, fg, a));
                    unsafe { (self.mem.add(at) as *mut u32).write_volatile(p) };
                }
            }
        }
    }

    /// 8-bit alpha of `c` in the UI font, cell-sized, row major: for code that composes its own pixels (splash).
    pub fn glyph_alpha(&self, c: char, bold: bool) -> Option<Vec<u8>> {
        let g = self.font.glyph(c, bold)?;
        Some((0..self.font.w * self.font.h).map(|i| if i % 2 == 0 { g[i / 2] >> 4 } else { g[i / 2] & 15 } * 17).collect())
    }

    /// Copies a w-wide block of pixels to (x0, y0), clipped to the screen.
    pub fn blit(&mut self, x0: usize, y0: usize, w: usize, px: &[(u8, u8, u8)]) {
        for (dy, row) in px.chunks(w.max(1)).enumerate() {
            for (dx, &c) in row.iter().enumerate() {
                let (x, y) = (x0 + dx, y0 + dy);
                let at = y * self.stride + x * self.bytes_pp;
                if x < self.px_w && y < self.px_h && at + 4 <= self.len {
                    let p = self.pixel(c);
                    unsafe { (self.mem.add(at) as *mut u32).write_volatile(p) };
                }
            }
        }
    }

    /// Back to the text console (on exit, and from the panic hook).
    pub fn restore(&self) {
        if let Some(t) = &self.tty {
            unsafe { libc::ioctl(t.as_raw_fd(), KDSETMODE, KD_TEXT) };
        }
    }
}

/// Text console back on, without needing the Fb (panic hook, exit).
pub fn text_mode() {
    if let Ok(t) = fs::OpenOptions::new().read(true).write(true).open("/dev/tty0") {
        unsafe { libc::ioctl(t.as_raw_fd(), KDSETMODE, KD_TEXT) };
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        self.restore();
        unsafe { libc::munmap(self.mem.cast(), self.len) };
    }
}

fn mix(bg: (u8, u8, u8), fg: (u8, u8, u8), a: u32) -> (u8, u8, u8) {
    let m = |b: u8, f: u8| ((b as u32 * (255 - a) + f as u32 * a) / 255) as u8;
    (m(bg.0, fg.0), m(bg.1, fg.1), m(bg.2, fg.2))
}

/// Named colours map to the UI palette's neighbours; the UI itself uses Rgb everywhere.
fn rgb(c: Color, fg: bool) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => {
            if fg {
                (226, 232, 240)
            } else {
                (15, 23, 42)
            }
        }
        Color::Black => (15, 23, 42),
        Color::Red | Color::LightRed => (239, 68, 68),
        Color::Green | Color::LightGreen => (34, 197, 94),
        Color::Yellow | Color::LightYellow => (245, 158, 11),
        Color::Blue | Color::LightBlue => (59, 130, 246),
        Color::Magenta | Color::LightMagenta => (167, 139, 250),
        Color::Cyan | Color::LightCyan => (56, 189, 248),
        Color::Gray => (148, 163, 184),
        Color::DarkGray => (100, 116, 139),
        Color::White => (248, 250, 252),
        Color::Indexed(_) => {
            if fg {
                (226, 232, 240)
            } else {
                (15, 23, 42)
            }
        }
    }
}

impl Backend for Fb {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, c) in content {
            if x < self.cols && y < self.rows {
                self.put(x, y, c);
            }
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.cursor = p.into();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        let (w, h) = (self.px_w, self.px_h);
        self.fill(0, 0, w, h, rgb(Color::Reset, false));
        Ok(())
    }

    fn clear_region(&mut self, _: ClearType) -> io::Result<()> {
        self.clear()
    }

    fn size(&self) -> io::Result<Size> {
        Ok(Size::new(self.cols, self.rows))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize { columns_rows: Size::new(self.cols, self.rows), pixels: Size::new(self.px_w as u16, self.px_h as u16) })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_struct_sizes() {
        assert_eq!(std::mem::size_of::<VarInfo>(), 160);
        assert_eq!(std::mem::size_of::<FixInfo>(), 80);
    }

    #[test]
    fn font_lookup_and_fallback() {
        // 2x2 cells, glyphs for 'A' and U+FFFD, regular then bold, 4-bit packed
        let mut b = b"AFN2".to_vec();
        b.extend(2u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(('A' as u32).to_le_bytes());
        b.extend(0xFFFDu32.to_le_bytes());
        b.extend([0xF0, 0x0F, 0x11, 0x22, 0xAA, 0xBB, 0xCC, 0xDD]);
        let p = std::env::temp_dir().join("aios-test.fnt");
        fs::write(&p, b).unwrap();
        let f = Font::load(p.to_str().unwrap()).unwrap();
        assert_eq!((f.w, f.h), (2, 2));
        assert_eq!(f.glyph('A', false).unwrap(), &[0xF0, 0x0F]);
        assert_eq!(f.glyph('A', true).unwrap(), &[0xAA, 0xBB]);
        assert_eq!(f.glyph('Z', false).unwrap(), &[0x11, 0x22]); // replacement glyph
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 255), (255, 255, 255));
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0), (0, 0, 0));
    }
}
