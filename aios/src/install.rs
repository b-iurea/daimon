//! Installation work: find the disks and the boot medium, write a GPT (ESP + aios-data), copy the system,
//! format, download the models from Hugging Face (resumable, SHA-256 checked) and write the owner's answers.
//! The questions are asked by setup.rs; this module only does what the plan says.
//!
//! Two cases: booted from the ISO with no Daimon disk (full install), or a data partition without models
//! (first boot of a bare image, or the models were deleted): download only.

use crate::{config, memory};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub const MODELS: &str = "/data/models";
/// file name of the EFI system image on the ISO (also its El Torito boot image)
const ESP_IMAGE: &str = "efiboot.img";
const MEDIA: &str = "/run/media";
const MIB: u64 = 1 << 20;
const ESP_MIB: u64 = 64;

/// True when the system can't run as it is: no data partition, or no brain/controller on it.
pub fn needed() -> bool {
    !Path::new(&format!("{MODELS}/current.gguf")).exists() || !Path::new(&format!("{MODELS}/judge.gguf")).exists()
}

/// True when /data is a real partition (else we booted the ISO and must install to a disk).
pub fn have_data_partition() -> bool {
    fs::read_to_string("/proc/mounts").unwrap_or_default().lines().any(|l| l.split_whitespace().nth(1) == Some("/data"))
}

// ---------------------------------------------------------------- hardware

pub fn ram_bytes() -> u64 {
    fs::read_to_string("/proc/meminfo").ok().and_then(|m| m.lines().next()?.split_whitespace().nth(1)?.parse::<u64>().ok()).map_or(0, |kb| kb * 1024)
}

pub struct Disk {
    /// kernel name: sda, vda, nvme0n1
    pub name: String,
    pub model: String,
    pub bytes: u64,
}

/// Disks we could install to: real block devices, not the boot medium, not CD drives, at least 8 GB.
pub fn disks(exclude: Option<&str>) -> Vec<Disk> {
    let mut v: Vec<Disk> = fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if ["loop", "ram", "sr", "zram", "dm-", "md"].iter().any(|p| name.starts_with(p)) || Some(name.as_str()) == exclude {
                return None;
            }
            let rd = |f: &str| fs::read_to_string(e.path().join(f)).unwrap_or_default().trim().to_string();
            let bytes = rd("size").parse::<u64>().unwrap_or(0) * 512;
            // virtio disks have no model, only a PCI vendor id (0x1af4)
            let model = [rd("device/model"), rd("device/vendor")].into_iter().find(|s| !s.is_empty() && !s.starts_with("0x")).unwrap_or_else(|| "virtual disk".into());
            (bytes >= 8 << 30).then_some(Disk { name, model, bytes })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The device holding the ISO (CD drive or USB stick), mounted read-only at /run/media: (disk name, image path).
pub fn boot_medium() -> Option<(String, String)> {
    let img = format!("{MEDIA}/{ESP_IMAGE}");
    let _ = fs::create_dir_all(MEDIA);
    if Path::new(&img).exists() {
        return disk_of_mount(MEDIA).map(|d| (d, img));
    }
    let mut devs: Vec<String> = fs::read_dir("/sys/class/block").into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    devs.sort();
    for d in devs {
        if !mount_ro(&format!("/dev/{d}"), MEDIA, "iso9660") {
            continue;
        }
        if Path::new(&img).exists() {
            // a partition of a hybrid USB stick -> the stick itself
            let disk = fs::read_link(format!("/sys/class/block/{d}"))
                .ok()
                .filter(|_| Path::new(&format!("/sys/class/block/{d}/partition")).exists())
                .and_then(|p| p.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or(d);
            return Some((disk, img));
        }
        unsafe { libc::umount2(c(MEDIA).as_ptr(), 0) };
    }
    None
}

fn disk_of_mount(dir: &str) -> Option<String> {
    let dev = fs::read_to_string("/proc/mounts").ok()?.lines().find(|l| l.split_whitespace().nth(1) == Some(dir))?.split_whitespace().next()?.to_string();
    Some(dev.trim_start_matches("/dev/").to_string())
}

fn c(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_default()
}

fn mount_ro(src: &str, dst: &str, fs: &str) -> bool {
    unsafe { libc::mount(c(src).as_ptr(), c(dst).as_ptr(), c(fs).as_ptr(), libc::MS_RDONLY, std::ptr::null()) == 0 }
}

// ---------------------------------------------------------------- models

pub struct Model {
    pub label: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub note: &'static str,
}

/// Agent brains offered by the installer, smallest first. Sizes are the real file sizes on Hugging Face
/// (2026-10-08); what fits is decided by RAM only (see ram_needed), not by benchmarks.
pub const BRAINS: [Model; 6] = [
    Model { label: "MiniCPM5 1B", repo: "openbmb/MiniCPM5-1B-GGUF", file: "MiniCPM5-1B-Q4_K_M.gguf", bytes: 688_065_920, note: "smallest" },
    Model { label: "MiniCPM5 2B", repo: "openbmb/MiniCPM5-2B-GGUF", file: "MiniCPM5-2B-Q4_K_M.gguf", bytes: 1_561_318_368, note: "recommended" },
    Model { label: "Qwen3.5 4B", repo: "unsloth/Qwen3.5-4B-GGUF", file: "Qwen3.5-4B-Q4_K_M.gguf", bytes: 2_740_937_888, note: "" },
    Model { label: "Qwen3.5 9B", repo: "unsloth/Qwen3.5-9B-GGUF", file: "Qwen3.5-9B-Q4_K_M.gguf", bytes: 5_680_522_464, note: "" },
    Model { label: "Qwen3.5 35B-A3B", repo: "unsloth/Qwen3.5-35B-A3B-GGUF", file: "Qwen3.5-35B-A3B-Q4_K_M.gguf", bytes: 22_016_023_168, note: "MoE: fast for its size" },
    Model { label: "Qwen3.5 27B", repo: "unsloth/Qwen3.5-27B-GGUF", file: "Qwen3.5-27B-Q4_K_M.gguf", bytes: 16_740_812_704, note: "slow on CPU" },
];
pub const DEFAULT_BRAIN: usize = 1;

pub const JUDGES: [Model; 2] = [
    Model { label: "Kev 4B", repo: "ggml-org/Kev-4B-GGUF", file: "Kev-4B-Q4_K_M.gguf", bytes: 3_033_489_824, note: "recommended: 0 errors in our bench" },
    Model { label: "Kev 0.8B", repo: "ggml-org/Kev-0.8B-GGUF", file: "Kev-0.8B-Q8_0.gguf", bytes: 812_406_304, note: "for small machines" },
];

/// RAM the pair needs: weights, KV cache and runtime (~25% on the brain at the default context, ~12% on the
/// controller's small one), plus 1.5 GB for the system.
// ponytail: a rule of thumb from file sizes; measure per model if machines run out of memory
pub fn ram_needed(brain: u64, judge: u64) -> u64 {
    brain + brain / 4 + judge + judge / 8 + (3 << 29)
}

/// What to download: (repo, file).
#[derive(Clone)]
pub struct Pick {
    pub repo: String,
    pub file: String,
}

impl Pick {
    pub fn of(m: &Model) -> Pick {
        Pick { repo: m.repo.into(), file: m.file.into() }
    }

    /// "owner/repo/file.gguf" or a huggingface.co URL to the file.
    pub fn parse(s: &str) -> Option<Pick> {
        let s = s.trim().trim_start_matches("https://").trim_start_matches("huggingface.co/").trim_start_matches("hf.co/");
        let s = s.replace("/blob/main/", "/").replace("/resolve/main/", "/");
        let mut p = s.splitn(3, '/');
        let (owner, repo, file) = (p.next()?, p.next()?, p.next()?);
        (file.ends_with(".gguf") && !owner.is_empty() && !repo.is_empty()).then(|| Pick { repo: format!("{owner}/{repo}"), file: file.into() })
    }
}

// ---------------------------------------------------------------- the plan

pub struct Plan {
    /// target disk for a full install; None = /data exists, download only
    pub disk: Option<String>,
    pub esp_image: Option<String>,
    pub keymap: String,
    pub owner: String,
    pub hostname: String,
    pub brain: Pick,
    pub judge: Pick,
    pub extra: String,
}

pub enum Progress {
    Step(String),
    /// file, bytes done, bytes total, bytes/s
    Bytes(String, u64, u64, f64),
    Done,
    Failed(String),
}

pub fn run(plan: &Plan, tx: &Sender<Progress>) -> Result<(), String> {
    let step = |s: &str| {
        crate::log(&format!("install: {s}"));
        let _ = tx.send(Progress::Step(s.into()));
    };
    if let Some(disk) = &plan.disk {
        step(&format!("partitioning /dev/{disk}"));
        let (esp, data) = partition(disk)?;
        step("copying the system to the EFI partition");
        copy(plan.esp_image.as_deref().ok_or("installation medium not found")?, &esp)?;
        step("formatting the data partition");
        let out = std::process::Command::new("/usr/bin/mke2fs").args(["-q", "-F", "-t", "ext4", "-L", "aios-data", &data]).output().map_err(|e| format!("mke2fs: {e}"))?;
        if !out.status.success() {
            return Err(format!("mke2fs: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        if unsafe { libc::mount(c(&data).as_ptr(), c("/data").as_ptr(), c("ext4").as_ptr(), 0, std::ptr::null()) } != 0 {
            return Err(format!("mount {data}: {}", std::io::Error::last_os_error()));
        }
    }
    fs::create_dir_all(MODELS).map_err(|e| e.to_string())?;
    fs::create_dir_all("/data/aios").map_err(|e| e.to_string())?;
    // controller first: it is the one every other step depends on
    for (pick, link) in [(&plan.judge, "judge.gguf"), (&plan.brain, "current.gguf")] {
        step(&format!("downloading {}", pick.file));
        download(pick, tx)?;
        let l = format!("{MODELS}/{link}");
        let _ = fs::remove_file(&l);
        std::os::unix::fs::symlink(&pick.file, &l).map_err(|e| e.to_string())?;
    }
    step("writing settings and memory");
    for (k, v) in [("keymap", &plan.keymap), ("hostname", &plan.hostname)] {
        let old = config::get(k);
        config::set(k, v)?;
        memory::record_setting(k, &old, v, "owner");
    }
    if !plan.owner.trim().is_empty() {
        memory::note("owner", "Owner's name", &format!("The owner wants the agent to call them {}.", plan.owner.trim()));
    }
    if !plan.extra.trim().is_empty() {
        fs::write(crate::agent::EXTRA_PROMPT, format!("{}\n", plan.extra.trim())).map_err(|e| e.to_string())?;
    }
    let what = format!("installed Daimon {} (brain {}, controller {})", env!("CARGO_PKG_VERSION"), plan.brain.file, plan.judge.file);
    memory::record_change("owner", &what);
    unsafe { libc::sync() };
    let _ = tx.send(Progress::Done);
    Ok(())
}

// ---------------------------------------------------------------- disk

const BLKRRPART: libc::Ioctl = 0x125F;
const BLKSSZGET: libc::Ioctl = 0x1268;
const BLKGETSIZE64: libc::Ioctl = 0x8008_1272u32 as libc::Ioctl;

/// Writes a fresh GPT (1 MiB gap, 64 MiB ESP, the rest aios-data) and returns the two partition devices.
fn partition(disk: &str) -> Result<(String, String), String> {
    let dev = format!("/dev/{disk}");
    let mut f = OpenOptions::new().read(true).write(true).open(&dev).map_err(|e| format!("{dev}: {e}"))?;
    let (mut ss, mut bytes) = (0 as libc::c_int, 0u64);
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKSSZGET, &mut ss) } < 0 || unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64, &mut bytes) } < 0 {
        return Err(format!("{dev}: cannot read its size"));
    }
    write_gpt(&mut f, ss as u64, bytes / ss as u64, &random_guid)?;
    f.sync_all().map_err(|e| e.to_string())?;
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKRRPART) } < 0 {
        return Err(format!("{dev}: the kernel did not reread the partition table ({})", std::io::Error::last_os_error()));
    }
    drop(f);
    // nvme0n1 -> nvme0n1p1, sda -> sda1
    let p = if disk.ends_with(|c: char| c.is_ascii_digit()) { "p" } else { "" };
    let (esp, data) = (format!("/dev/{disk}{p}1"), format!("/dev/{disk}{p}2"));
    for _ in 0..50 {
        if Path::new(&data).exists() && Path::new(&esp).exists() {
            return Ok((esp, data));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{data} did not appear"))
}

fn copy(src: &str, dst: &str) -> Result<(), String> {
    let mut i = File::open(src).map_err(|e| format!("{src}: {e}"))?;
    let mut o = OpenOptions::new().write(true).open(dst).map_err(|e| format!("{dst}: {e}"))?;
    std::io::copy(&mut i, &mut o).map_err(|e| format!("copy to {dst}: {e}"))?;
    o.sync_all().map_err(|e| e.to_string())
}

fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    let _ = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut g));
    g[7] = (g[7] & 0x0F) | 0x40; // version 4 (field 3 is little endian on disk)
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

/// GUID text -> on-disk bytes (first three fields little endian).
fn guid(s: &str) -> [u8; 16] {
    let h: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&s.replace('-', "")[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&[h[3], h[2], h[1], h[0]]);
    g[4..6].copy_from_slice(&[h[5], h[4]]);
    g[6..8].copy_from_slice(&[h[7], h[6]]);
    g[8..].copy_from_slice(&h[8..]);
    g
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// Protective MBR, primary and backup GPT with two partitions: EFI system and Linux "aios-data".
fn write_gpt(f: &mut (impl Write + Seek), ss: u64, sectors: u64, new_guid: &dyn Fn() -> [u8; 16]) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    let ent_sectors = 128 * 128 / ss;
    let last = sectors - 1;
    let (first_usable, last_usable) = (2 + ent_sectors, last - 1 - ent_sectors);
    let mib = MIB / ss;
    let esp = (mib, mib * (1 + ESP_MIB) - 1);
    let data = (mib * (1 + ESP_MIB), last_usable / mib * mib - 1);
    if data.1 <= data.0 + mib {
        return Err("disk too small".into());
    }
    let mut ents = vec![0u8; 128 * 128];
    for (i, (ty, (a, b), name)) in
        [("C12A7328-F81F-11D2-BA4B-00A0C93EC93B", esp, "EFI"), ("0FC63DAF-8483-4772-8E79-3D69D8477DE4", data, "aios-data")].into_iter().enumerate()
    {
        let e = &mut ents[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(&guid(ty));
        e[16..32].copy_from_slice(&new_guid());
        e[32..40].copy_from_slice(&a.to_le_bytes());
        e[40..48].copy_from_slice(&b.to_le_bytes());
        for (j, u) in name.encode_utf16().enumerate() {
            e[56 + 2 * j..58 + 2 * j].copy_from_slice(&u.to_le_bytes());
        }
    }
    let ents_crc = crc32(&ents);
    let disk_guid = new_guid();
    let header = |me: u64, other: u64, ents_at: u64| {
        let mut h = vec![0u8; ss as usize];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&me.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&ents_at.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&ents_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    let mut mbr = vec![0u8; ss as usize];
    mbr[446..462].copy_from_slice(&[0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0, 0, 0, 0, 0]);
    mbr[458..462].copy_from_slice(&((sectors - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
    let mut put = |lba: u64, b: &[u8]| f.seek(SeekFrom::Start(lba * ss)).and_then(|_| f.write_all(b)).map_err(err);
    put(0, &mbr)?;
    put(1, &header(1, last, 2))?;
    put(2, &ents)?;
    put(last - ent_sectors, &ents)?;
    put(last, &header(last, 1, last - ent_sectors))?;
    // old filesystem signatures at the start of each partition would confuse nothing we run, but blkid-style
    // probes on other systems might: clear the first MiB of each
    put(esp.0, &vec![0u8; MIB as usize])?;
    put(data.0, &vec![0u8; MIB as usize])
}

// ---------------------------------------------------------------- downloads

/// Resumable download into /data/models/<file>, checked against the SHA-256 Hugging Face publishes.
fn download(p: &Pick, tx: &Sender<Progress>) -> Result<(), String> {
    let dest = format!("{MODELS}/{}", p.file);
    let (size, sha) = remote_meta(p)?;
    if fs::metadata(&dest).is_ok_and(|m| m.len() == size) {
        return Ok(()); // already there (a retry after a later step failed)
    }
    let part = format!("{dest}.part");
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(20)).timeout_read(Duration::from_secs(60)).build();
    let url = format!("https://huggingface.co/{}/resolve/main/{}", p.repo, p.file);
    let mut tries = 0;
    loop {
        match fetch(&agent, &url, &part, size, &p.file, tx) {
            Ok(()) => break,
            Err(e) if tries < 20 => {
                tries += 1;
                crate::log(&format!("install: {} interrupted ({e}), resuming", p.file));
                let _ = tx.send(Progress::Step(format!("connection lost, resuming {} ({tries}/20)", p.file)));
                std::thread::sleep(Duration::from_secs(3));
            }
            Err(e) => return Err(format!("{}: {e}", p.file)),
        }
    }
    let _ = tx.send(Progress::Step(format!("verifying {}", p.file)));
    let got = sha256_file(&part)?;
    if !sha.is_empty() && got != sha {
        let _ = fs::remove_file(&part);
        return Err(format!("{}: checksum mismatch, the download was corrupted (deleted, retry)", p.file));
    }
    fs::rename(&part, &dest).map_err(|e| e.to_string())
}

/// (size, sha256 hex) of the file from the Hugging Face API.
fn remote_meta(p: &Pick) -> Result<(u64, String), String> {
    let url = format!("https://huggingface.co/api/models/{}/tree/main", p.repo);
    let body = ureq::get(&url).timeout(Duration::from_secs(30)).call().map_err(|e| format!("{}: {e}", p.repo))?.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let f = v.as_array().into_iter().flatten().find(|f| f["path"] == p.file.as_str()).ok_or(format!("{} not found in {}", p.file, p.repo))?;
    let size = f["lfs"]["size"].as_u64().or(f["size"].as_u64()).unwrap_or(0);
    Ok((size, f["lfs"]["oid"].as_str().unwrap_or("").to_string()))
}

fn fetch(agent: &ureq::Agent, url: &str, part: &str, size: u64, name: &str, tx: &Sender<Progress>) -> Result<(), String> {
    let mut have = fs::metadata(part).map_or(0, |m| m.len());
    if have >= size && size > 0 {
        return Ok(());
    }
    let r = agent.get(url).set("Range", &format!("bytes={have}-")).call().map_err(|e| e.to_string())?;
    if r.status() != 206 {
        have = 0; // the server ignored the range: start over
    }
    let mut out = OpenOptions::new().create(true).write(true).open(part).map_err(|e| e.to_string())?;
    out.set_len(have).and_then(|_| out.seek(SeekFrom::Start(have))).map_err(|e| e.to_string())?;
    let mut body = r.into_reader();
    let mut buf = vec![0u8; MIB as usize];
    let (start, start_bytes) = (Instant::now(), have);
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        have += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            let speed = (have - start_bytes) as f64 / start.elapsed().as_secs_f64().max(0.001);
            let _ = tx.send(Progress::Bytes(name.into(), have, size, speed));
            last = Instant::now();
        }
    }
    out.sync_all().map_err(|e| e.to_string())?;
    if size > 0 && have < size {
        return Err(format!("connection closed at {have} of {size} bytes"));
    }
    Ok(())
}

fn sha256_file(path: &str) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 4 * MIB as usize];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_is_what_sfdisk_reads() {
        let path = std::env::temp_dir().join(format!("aios-gpt-{}", std::process::id()));
        let sectors = (200 * MIB) / 512;
        let mut f = OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&path).unwrap();
        f.set_len(sectors * 512).unwrap();
        let n = std::cell::Cell::new(0u8);
        write_gpt(&mut f, 512, sectors, &|| {
            n.set(n.get() + 1);
            [n.get(); 16]
        })
        .unwrap();
        drop(f);
        let Ok(out) = std::process::Command::new("sfdisk").arg("-d").arg(&path).output() else {
            return; // no sfdisk on this host
        };
        let d = String::from_utf8_lossy(&out.stdout);
        let _ = fs::remove_file(&path);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(d.contains("label: gpt"), "{d}");
        assert!(d.contains("start=        2048, size=      131072, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B"), "{d}");
        assert!(d.contains("start=      133120,") && d.contains("type=0FC63DAF-8483-4772-8E79-3D69D8477DE4") && d.contains("name=\"aios-data\""), "{d}");
    }

    #[test]
    fn helpers() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")[..4], [0x28, 0x73, 0x2A, 0xC1]);
        let p = Pick::parse("https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/blob/main/Qwen3.5-4B-Q4_K_M.gguf").unwrap();
        assert_eq!((p.repo.as_str(), p.file.as_str()), ("unsloth/Qwen3.5-4B-GGUF", "Qwen3.5-4B-Q4_K_M.gguf"));
        assert!(Pick::parse("openbmb/MiniCPM5-2B-GGUF/MiniCPM5-2B-Q4_K_M.gguf").is_some());
        assert!(Pick::parse("openbmb/MiniCPM5-2B-GGUF").is_none());
        // the default pair fits an 8 GB machine
        assert!(ram_needed(BRAINS[DEFAULT_BRAIN].bytes, JUDGES[0].bytes) < 8 << 30);
    }
}
