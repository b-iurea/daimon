//! Console keyboard layouts. The tables are built at image build time from XKB (tools/mkkeymaps.py)
//! and loaded here with the kernel's keymap ioctls: no loadkeys, no kbd package.

use std::fs;
use std::os::fd::AsRawFd;

pub const DIR: &str = "/usr/share/daimon/keymaps";

const KDSKBENT: libc::Ioctl = 0x4B47;
const KDSKBMODE: libc::Ioctl = 0x4B45;
const KDSKBDIACRUC: libc::Ioctl = 0x4BFB;
const K_UNICODE: libc::c_ulong = 0x03;
const K_HOLE: u16 = 0x0200;
const K_NOSUCHMAP: u16 = 0x027F;

#[repr(C)]
struct KbEntry {
    table: u8,
    index: u8,
    value: u16,
}

#[repr(C)]
struct Diacrs {
    count: u32,
    entries: [[u32; 3]; 256],
}

/// `name<TAB>description` lines of every layout in the image.
pub fn index() -> String {
    fs::read_to_string(format!("{DIR}/index.txt")).unwrap_or_default()
}

fn names() -> String {
    index().lines().filter_map(|l| l.split('\t').next()).collect::<Vec<_>>().join(", ")
}

struct Map {
    keys: Vec<(u8, u8, u16)>,
    diacrs: Vec<[u32; 3]>,
}

fn parse(b: &[u8]) -> Option<Map> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?));
    if b.get(..4)? != b"AKM1" {
        return None;
    }
    let n = u16_at(4)? as usize;
    let keys = (0..n).map(|i| Some((*b.get(6 + 4 * i)?, *b.get(7 + 4 * i)?, u16_at(8 + 4 * i)?))).collect::<Option<Vec<_>>>()?;
    let at = 6 + 4 * n;
    let d = u16_at(at)? as usize;
    let diacrs = (0..d.min(256)).map(|i| Some([u32_at(at + 2 + 12 * i)?, u32_at(at + 6 + 12 * i)?, u32_at(at + 10 + 12 * i)?])).collect::<Option<Vec<_>>>()?;
    Some(Map { keys, diacrs })
}

/// Loads a layout into the kernel (global for every console). Unknown name -> error listing the valid ones.
pub fn apply(name: &str) -> Result<String, String> {
    let bytes = fs::read(format!("{DIR}/{name}.akm")).map_err(|_| format!("unknown keymap '{name}'. Available: {}", names()))?;
    let map = parse(&bytes).ok_or(format!("{name}: corrupt keymap file"))?;
    let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty0").map_err(|e| format!("/dev/tty0: {e}"))?;
    let fd = tty.as_raw_fd();
    let set = |table: u8, index: u8, value: u16| unsafe { libc::ioctl(fd, KDSKBENT, &KbEntry { table, index, value }) };
    // Unicode keysyms are only accepted on a console in Unicode keyboard mode (the default with UTF-8)
    unsafe { libc::ioctl(fd, KDSKBMODE, K_UNICODE) };
    let mut table = [[K_HOLE; 256]; 256];
    let mut used = [false; 256];
    for &(t, k, v) in &map.keys {
        table[t as usize][k as usize] = v;
        used[t as usize] = true;
    }
    let mut errors = 0;
    for t in 0..256 {
        if !used[t] {
            // free tables a previous layout allocated (table 0 always stays)
            if t > 0 {
                set(t as u8, 0, K_NOSUCHMAP);
            }
            continue;
        }
        // index 0 only validates in the kernel, so start at 1
        for k in 1..256 {
            if set(t as u8, k as u8, table[t][k]) < 0 {
                errors += 1;
            }
        }
    }
    let mut d = Diacrs { count: map.diacrs.len() as u32, entries: [[0; 3]; 256] };
    d.entries[..map.diacrs.len()].copy_from_slice(&map.diacrs);
    unsafe { libc::ioctl(fd, KDSKBDIACRUC, &d) };
    let desc = index().lines().find_map(|l| l.strip_prefix(&format!("{name}\t")).map(String::from)).unwrap_or_default();
    match errors {
        0 => Ok(format!("keyboard layout {name} ({desc}) active")),
        n => Ok(format!("keyboard layout {name} ({desc}) active, {n} keys rejected by the kernel")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_akm() {
        let mut b = b"AKM1".to_vec();
        b.extend(2u16.to_le_bytes());
        b.extend([0, 26, 0xe8, 0x0b, 1, 26, 0xc8, 0x0b]);
        b.extend(1u16.to_le_bytes());
        for v in [b'`' as u32, b'a' as u32, 'à' as u32] {
            b.extend(v.to_le_bytes());
        }
        let m = parse(&b).unwrap();
        assert_eq!(m.keys, vec![(0, 26, 0x0be8), (1, 26, 0x0bc8)]);
        assert_eq!(m.diacrs, vec![[96, 97, 0xe0]]);
        assert!(parse(&b[..b.len() - 1]).is_none());
        assert!(parse(b"XKM1").is_none());
    }
}
