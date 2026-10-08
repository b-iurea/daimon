mod agent;
mod config;
mod fb;
mod install;
mod judge;
mod keyboard;
mod link;
mod memory;
mod net;
mod splash;
mod tui;

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn main() {
    if std::process::id() == 1 {
        init();
    }
    match std::env::args().nth(1).as_deref() {
        Some("tui") => tui::run(),
        Some("agent") => link::serve(&std::env::args().nth(2).unwrap_or(link::SOCK.into())),
        Some("llm") => config::exec_llm(),
        Some("judge") => judge::exec(),
        _ => {
            eprintln!("usage: daimon tui|agent [socket]|llm|judge   (as PID 1 it boots the system)");
            std::process::exit(2);
        }
    }
}

pub fn log(msg: &str) {
    eprintln!("[daimon] {msg}");
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open("/run/log/daimon.log") {
        let _ = writeln!(f, "{msg}");
    }
}

/// Safe mode (this boot only): ignore /data/modules and /data/daimon/config, run factory defaults.
pub const SAFE_FLAG: &str = "/run/daimon/safe";
pub fn safe_mode() -> bool {
    std::path::Path::new(SAFE_FLAG).exists()
}

/// SIGTERM a module's process group; the supervisor brings it back up.
pub fn restart_module(name: &str) -> Result<String, String> {
    let state = fs::read_to_string("/run/daimon/modules").unwrap_or_default();
    let line = state.lines().find(|l| l.split_whitespace().next() == Some(name)).ok_or(format!("no module '{name}'"))?;
    let pid: i32 = line.split_whitespace().nth(1).and_then(|p| p.parse().ok()).ok_or(format!("{name} is not running"))?;
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    Ok(format!("{name} (pid {pid}) restarting"))
}

/// The GPT partition named `daimon-data` (or `aios-data`, installs before 0.2.1), found via sysfs (no udev here).
fn find_data_partition() -> Option<String> {
    fs::read_dir("/sys/class/block").ok()?.flatten().find_map(|e| {
        let ue = fs::read_to_string(e.path().join("uevent")).ok()?;
        ue.lines().any(|l| l == "PARTNAME=daimon-data" || l == "PARTNAME=aios-data").then(|| format!("/dev/{}", e.file_name().to_string_lossy()))
    })
}

fn mount(src: &str, dst: &str, fstype: &str, data: &str) -> bool {
    let _ = fs::create_dir_all(dst);
    let c = |s: &str| CString::new(s).unwrap_or_default();
    let (src, dst_c, fstype, data) = (c(src), c(dst), c(fstype), c(data));
    let ok = unsafe { libc::mount(src.as_ptr(), dst_c.as_ptr(), fstype.as_ptr(), 0, data.as_ptr().cast()) } == 0;
    if !ok {
        log(&format!("mount {dst}: {}", std::io::Error::last_os_error()));
    }
    ok
}

/// `daimon.key=value` pairs from the kernel command line.
fn cmdline() -> HashMap<String, String> {
    fs::read_to_string("/proc/cmdline")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| k.starts_with("daimon."))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn init() -> ! {
    log("booting");
    mount("proc", "/proc", "proc", "");
    mount("sysfs", "/sys", "sysfs", "");
    mount("devtmpfs", "/dev", "devtmpfs", "");
    mount("devpts", "/dev/pts", "devpts", "");
    mount("tmpfs", "/run", "tmpfs", "mode=755");
    mount("tmpfs", "/tmp", "tmpfs", "");
    let _ = fs::create_dir_all("/run/log");
    let args = cmdline();
    // Every step is best effort: nothing here may stop the supervisor from running.
    // ponytail: disks probed once at boot; a late USB/NVMe controller would be missed
    std::thread::sleep(Duration::from_millis(300));
    match args.get("daimon.data").cloned().or_else(find_data_partition) {
        Some(dev) => {
            if mount(&dev, "/data", "ext4", "") {
                log(&format!("data: {dev} on /data"));
                // installs before 0.2.1 kept settings in /data/aios
                if !std::path::Path::new("/data/daimon").exists() && fs::rename("/data/aios", "/data/daimon").is_ok() {
                    log("data: /data/aios renamed to /data/daimon");
                }
            }
        }
        None => log("data: no daimon-data partition, running without persistence"),
    }
    match keyboard::apply(&config::get("keymap")) {
        Ok(m) => log(&m),
        Err(e) => log(&format!("keyboard: {e}")),
    }
    if let Err(e) = net::set_hostname(&config::get("hostname")) {
        log(&e);
    }
    net::up(&args);
    // Ctrl+Alt+Del no longer reboots: the kernel sends us SIGINT and we restart the console (see supervise)
    unsafe {
        libc::signal(libc::SIGINT, on_ctrl_alt_del as *const () as libc::sighandler_t);
        libc::reboot(libc::RB_DISABLE_CAD);
    }
    supervise();
}

static CTRL_ALT_DEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_ctrl_alt_del(_: libc::c_int) {
    CTRL_ALT_DEL.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Modules with a `watchdog` file touch this every second or so; see supervise.
pub fn heartbeat(module: &str) {
    let _ = fs::write(format!("/run/daimon/alive.{module}"), "");
}

// ---------------------------------------------------------------- supervisor
//
// A module is a directory `<name>/` holding:
//   cmd       one line: program + args (whitespace separated)
//   tty       optional: run attached to this tty (e.g. "tty1")
//   disabled  optional: present = don't run
//   watchdog  optional: seconds; the module must call heartbeat() at least that often or it is killed
// Built-ins live in /etc/daimon/modules, /data/modules overrides by name.
// The tree is rescanned every tick, so editing it is how the system changes itself.

const MODULE_DIRS: [&str; 2] = ["/etc/daimon/modules", "/data/modules"];
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq)]
struct Spec {
    cmd: Vec<String>,
    tty: Option<String>,
    watchdog: Option<u64>,
}

struct Module {
    spec: Spec,
    pid: Option<i32>,
    started: Instant,
    next_start: Instant,
    backoff: Duration,
    restarts: u32,
    restart_now: bool,
    removed: bool,
}

fn load_specs() -> HashMap<String, Spec> {
    let mut specs = HashMap::new();
    let dirs = if safe_mode() { &MODULE_DIRS[..1] } else { &MODULE_DIRS[..] };
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.join("disabled").exists() {
                specs.remove(&name);
                continue;
            }
            let read = |f: &str| fs::read_to_string(p.join(f)).ok().map(|s| s.trim().to_string());
            let cmd: Vec<String> = read("cmd").unwrap_or_default().split_whitespace().map(String::from).collect();
            if cmd.is_empty() {
                continue;
            }
            let watchdog = read("watchdog").and_then(|w| w.parse().ok());
            specs.insert(name, Spec { cmd, tty: read("tty").filter(|t| !t.is_empty()), watchdog });
        }
    }
    specs
}

fn start(name: &str, m: &mut Module) {
    let _ = fs::create_dir_all("/run/log");
    let mut c = Command::new(&m.spec.cmd[0]);
    c.args(&m.spec.cmd[1..]);
    // ponytail: logs grow unbounded in tmpfs; rotate once modules get chatty
    let logf = OpenOptions::new().create(true).append(true).open(format!("/run/log/{name}.log"));
    if let Some(tty) = &m.spec.tty {
        let t = OpenOptions::new().read(true).write(true).open(format!("/dev/{tty}"));
        if let Ok(t) = t {
            c.stdin(t.try_clone().map(Stdio::from).unwrap_or(Stdio::null()));
            c.stdout(t.try_clone().map(Stdio::from).unwrap_or(Stdio::null()));
            c.stderr(Stdio::from(t));
        }
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 1);
                Ok(())
            });
        }
    } else {
        c.stdin(Stdio::null());
        match logf {
            Ok(f) => {
                c.stdout(f.try_clone().map(Stdio::from).unwrap_or(Stdio::null()));
                c.stderr(Stdio::from(f));
            }
            Err(_) => {
                c.stdout(Stdio::null());
                c.stderr(Stdio::null());
            }
        }
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    m.started = Instant::now();
    match c.spawn() {
        Ok(child) => {
            m.pid = Some(child.id() as i32);
            log(&format!("{name}: started pid {}", child.id()));
        }
        Err(e) => {
            log(&format!("{name}: spawn failed: {e}"));
            schedule_retry(m);
        }
    }
}

fn schedule_retry(m: &mut Module) {
    // A module that stayed up a while gets a fresh backoff; a crash loop doubles it.
    m.backoff = if m.started.elapsed() > MAX_BACKOFF { Duration::from_secs(1) } else { (m.backoff * 2).clamp(Duration::from_secs(1), MAX_BACKOFF) };
    m.next_start = Instant::now() + m.backoff;
}

fn supervise() -> ! {
    let mut mods: HashMap<String, Module> = HashMap::new();
    loop {
        let specs = load_specs();
        for (name, m) in mods.iter_mut() {
            match specs.get(name) {
                None => m.removed = true,
                Some(s) if *s != m.spec => {
                    log(&format!("{name}: spec changed, restarting"));
                    m.spec = s.clone();
                    m.restart_now = true;
                }
                _ => continue,
            }
            if let Some(pid) = m.pid {
                unsafe { libc::kill(-pid, libc::SIGTERM) };
            }
        }
        for (name, spec) in specs {
            mods.entry(name).or_insert_with(|| Module {
                spec,
                pid: None,
                started: Instant::now(),
                next_start: Instant::now(),
                backoff: Duration::ZERO,
                restarts: 0,
                restart_now: false,
                removed: false,
            });
        }

        // PID 1 reaps every orphan; only our modules get restarted.
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if let Some((name, m)) = mods.iter_mut().find(|(_, m)| m.pid == Some(pid)) {
                log(&format!("{name}: exited (status {status})"));
                m.pid = None;
                m.restarts += 1;
                if m.restart_now {
                    m.restart_now = false;
                    m.next_start = Instant::now();
                } else {
                    schedule_retry(m);
                }
            }
        }
        mods.retain(|_, m| !(m.removed && m.pid.is_none()));

        // A frozen console can't fix itself: no heartbeat in time, or the owner pressed Ctrl+Alt+Del -> kill it,
        // the restart below brings it back.
        let cad = CTRL_ALT_DEL.swap(false, std::sync::atomic::Ordering::Relaxed);
        for (name, m) in mods.iter_mut() {
            let (Some(pid), Some(limit)) = (m.pid, m.spec.watchdog) else {
                continue;
            };
            let beat = fs::metadata(format!("/run/daimon/alive.{name}")).and_then(|md| md.modified()).ok().and_then(|t| t.elapsed().ok());
            let quiet = beat.map_or(m.started.elapsed(), |b| b.min(m.started.elapsed()));
            let why = if cad && m.spec.tty.is_some() {
                "Ctrl+Alt+Del".to_string()
            } else if quiet > Duration::from_secs(limit) {
                format!("watchdog: no heartbeat for {}s", quiet.as_secs())
            } else {
                continue;
            };
            log(&format!("{name}: {why}, killing pid {pid}"));
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            m.restart_now = true;
        }

        let now = Instant::now();
        for (name, m) in mods.iter_mut() {
            if m.pid.is_none() && !m.removed && now >= m.next_start {
                start(name, m);
            }
        }
        // State for the TUI and the agent: "name pid|- restarts" per line.
        let mut names: Vec<_> = mods.keys().collect();
        names.sort();
        let state: String = names
            .iter()
            .map(|n| {
                let m = &mods[*n];
                format!("{n} {} {}\n", m.pid.map_or("-".into(), |p| p.to_string()), m.restarts)
            })
            .collect();
        let _ = fs::create_dir_all("/run/daimon");
        let _ = fs::write("/run/daimon/modules", state);

        // ponytail: 500ms polling; switch to signalfd(SIGCHLD)+inotify if latency matters
        std::thread::sleep(Duration::from_millis(500));
    }
}
