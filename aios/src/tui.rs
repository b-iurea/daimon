//! The face of the system on tty1: the conversation with the agent (left) and the machine (right).
//! On a framebuffer it paints itself (fb.rs: 24-bit colour, anti-aliased font, icons); without one it
//! falls back to the text console with plain glyphs.

use crate::agent::{self, Cmd, Ev};
use crate::config;
use crate::judge::Report;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Sparkline};
use ratatui::{Frame, Terminal};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- theme (slate dark + green accent)

const BG: Color = Color::Rgb(15, 23, 42);
const SURFACE: Color = Color::Rgb(30, 41, 59);
const BORDER: Color = Color::Rgb(51, 65, 85);
const TEXT: Color = Color::Rgb(226, 232, 240);
const STRONG: Color = Color::Rgb(248, 250, 252);
const MUTED: Color = Color::Rgb(148, 163, 184);
const FAINT: Color = Color::Rgb(100, 116, 139);
const ACCENT: Color = Color::Rgb(34, 197, 94);
const SKY: Color = Color::Rgb(56, 189, 248);
const VIOLET: Color = Color::Rgb(167, 139, 250);
const AMBER: Color = Color::Rgb(245, 158, 11);
const RED: Color = Color::Rgb(239, 68, 68);

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

// ---------------------------------------------------------------- glyphs

/// Framebuffer = our own font with icons and rounded corners; text console = what the VGA font has.
static FANCY: AtomicBool = AtomicBool::new(false);

fn fancy() -> bool {
    FANCY.load(Ordering::Relaxed)
}

/// Order = tools/mkfont.py ICONS. Icons are two cells wide: U+E100 + 2i and the next code point.
#[derive(Clone, Copy)]
enum Icon {
    Cpu = 0,
    Spark = 3,
    Shield = 4,
    Keyboard = 5,
    Chat = 6,
    Gear = 7,
    Cube = 8,
    Alert = 12,
    User = 15,
    Brain = 17,
}

fn icon(i: Icon) -> String {
    if !fancy() {
        return match i {
            Icon::User => "> ",
            Icon::Spark | Icon::Brain => "* ",
            Icon::Shield => "# ",
            Icon::Alert => "! ",
            _ => "",
        }
        .into();
    }
    let c = 0xE100 + 2 * i as u32;
    [char::from_u32(c).unwrap_or(' '), char::from_u32(c + 1).unwrap_or(' '), ' '].iter().collect()
}

fn g(fancy: &'static str, plain: &'static str) -> &'static str {
    if self::fancy() { fancy } else { plain }
}

fn spinner(tick: usize) -> &'static str {
    const F: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const P: [&str; 4] = ["|", "/", "-", "\\"];
    if fancy() { F[tick % F.len()] } else { P[tick % P.len()] }
}

/// A horizontal meter: filled part in `c` over a faint track, with rounded ends on the framebuffer.
fn bar(width: usize, frac: f64, c: Color) -> Vec<Span<'static>> {
    let full = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    if !fancy() {
        return vec![Span::styled("█".repeat(full), fg(c)), Span::styled("░".repeat(width - full), fg(BORDER))];
    }
    // U+E0C2 / U+E0C0 / U+E0C3: band with rounded left end, band, band with rounded right end
    let cell = |i: usize| match i {
        0 => '\u{E0C2}',
        i if i + 1 == width => '\u{E0C3}',
        _ => '\u{E0C0}',
    };
    let on: String = (0..full).map(cell).collect();
    let off: String = (full..width).map(cell).collect();
    vec![Span::styled(on, fg(c)), Span::styled(off, fg(SURFACE))]
}

fn level_color(frac: f64) -> Color {
    match frac {
        p if p < 0.6 => ACCENT,
        p if p < 0.85 => AMBER,
        _ => RED,
    }
}

fn trunc(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        return s;
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Word wrap by characters (the UI is monospace); long words are split.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        let mut len = 0;
        for word in para.split(' ') {
            let mut w: Vec<char> = word.chars().collect();
            if len > 0 && len + 1 + w.len() > width {
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            if len > 0 {
                line.push(' ');
                len += 1;
            }
            while len + w.len() > width {
                let take = width - len;
                line.extend(w.drain(..take));
                out.push(std::mem::take(&mut line));
                len = 0;
            }
            len += w.len();
            line.extend(w);
        }
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------- state

enum Entry {
    User(String),
    Think(String),
    Text(String),
    Tool(String, String),
    ToolOut(String),
    Info(String),
    Err(String),
    /// the controller handed the decision to the owner
    Ask(String),
    Judging(&'static str, String, Instant),
    Judge(Report),
}

#[derive(Default, Clone)]
struct Llm {
    up: bool,
    judge_up: bool,
    model: String,
    busy_slots: usize,
}

#[derive(Default)]
struct Sys {
    cpu: VecDeque<u64>,
    cpu_prev: (u64, u64),
    mem_used: u64,
    mem_total: u64,
    load: String,
    uptime: u64,
    ips: Vec<String>,
    modules: Vec<(String, String, String)>,
    kernel: String,
    cpus: usize,
}

#[derive(Default)]
struct JudgeStats {
    allowed: u32,
    blocked: u32,
    secs: f64,
    last: Option<(String, f64, bool)>,
}

struct App {
    log: Vec<Entry>,
    input: String,
    busy: bool,
    scroll: usize,
    tok_s: f64,
    ctx_used: u64,
    progress: Option<(u64, u64)>,
    confirm: Option<String>,
    memories: usize,
    conf: HashMap<String, String>,
    sys: Sys,
    llm: Llm,
    judge: JudgeStats,
    tick: usize,
    font: String,
    /// brain warm-up finished: Some(ok)
    warm: Option<bool>,
}

impl App {
    fn push(&mut self, e: Entry) {
        match (self.log.last_mut(), e) {
            (Some(Entry::Think(t)), Entry::Think(s)) | (Some(Entry::Text(t)), Entry::Text(s)) => t.push_str(&s),
            (_, e) => self.log.push(e),
        }
    }

    fn on_event(&mut self, e: Ev) {
        self.progress = None;
        match e {
            Ev::Progress(d, t) => self.progress = Some((d, t)),
            Ev::Think(s) => self.push(Entry::Think(s)),
            Ev::Text(s) => self.push(Entry::Text(s)),
            Ev::Tool(n, a) => self.push(Entry::Tool(n, a)),
            Ev::ToolOut(s) => self.push(Entry::ToolOut(s)),
            Ev::Info(s) => self.push(Entry::Info(s.trim_start_matches("-- ").into())),
            Ev::Confirm(q) => {
                let why = q.split(": ").next().unwrap_or(&q).to_string();
                self.push(Entry::Ask(why));
                self.confirm = Some(q);
            }
            Ev::Ctx(n) => self.ctx_used = n,
            Ev::Ready(ok) => self.warm = Some(ok),
            Ev::Err(s) => {
                self.push(Entry::Err(s));
                self.busy = false;
            }
            Ev::Done(t) => {
                self.tok_s = t;
                self.busy = false;
            }
            Ev::Judging(kind, subject) => self.push(Entry::Judging(kind, subject, Instant::now())),
            Ev::Judge(r) => {
                if r.allowed {
                    self.judge.allowed += 1
                } else {
                    self.judge.blocked += 1
                }
                let n = (self.judge.allowed + self.judge.blocked) as f64;
                self.judge.secs += (r.secs - self.judge.secs) / n;
                self.judge.last = Some((r.kind.into(), r.score, r.allowed));
                // the report replaces its "judging..." placeholder
                match self.log.iter().rposition(|e| matches!(e, Entry::Judging(..))) {
                    Some(i) => self.log[i] = Entry::Judge(r),
                    None => self.log.push(Entry::Judge(r)),
                }
            }
        }
    }
}

// ---------------------------------------------------------------- main loop

pub fn run() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (ev_tx, ev_rx) = channel();
    let prompts = agent::spawn(ev_tx, cancel.clone());
    let llm_rx = llm_poller();
    let mut app = App {
        log: vec![Entry::Info("I am the system. Ask me anything about my state, modules, logs or settings. /help lists console commands.".into())],
        input: String::new(),
        busy: false,
        scroll: 0,
        tok_s: 0.0,
        ctx_used: 0,
        progress: None,
        confirm: None,
        memories: 0,
        conf: config::load(),
        sys: Sys {
            kernel: fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().into(),
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            ..Default::default()
        },
        llm: Llm::default(),
        judge: JudgeStats::default(),
        tick: 0,
        font: String::new(),
        warm: None,
    };
    let _ = ratatui::crossterm::terminal::enable_raw_mode();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::fb::text_mode();
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
        hook(info);
    }));
    // the framebuffer is re-opened when the owner changes ui_font; the conversation lives on
    loop {
        let want = config::get("ui_font");
        let fb = match crate::fb::Fb::open(&want) {
            Ok(fb) => fb,
            Err(why) => {
                FANCY.store(false, Ordering::Relaxed);
                app.font = format!("text console: {why}");
                let mut term = ratatui::init();
                ui_loop(&mut term, &mut app, &ev_rx, &llm_rx, &prompts, &cancel, None);
                ratatui::restore();
                break;
            }
        };
        FANCY.store(true, Ordering::Relaxed);
        let mut fb = fb;
        // ponytail: splash only on the framebuffer; the text console goes straight to the UI
        if !std::path::Path::new(SPLASH_DONE).exists() {
            boot_splash(&mut fb, &mut app, &ev_rx, &llm_rx);
        }
        let (w, h) = fb.cell_px();
        app.font = format!("font {w}x{h}");
        let Ok(mut term) = Terminal::new(fb) else {
            break;
        };
        let _ = term.clear();
        if !ui_loop(&mut term, &mut app, &ev_rx, &llm_rx, &prompts, &cancel, Some(want)) {
            break;
        }
    }
    crate::fb::text_mode();
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
}

/// Present = the splash already ran this boot (a restarted TUI goes straight to the console).
const SPLASH_DONE: &str = "/run/aios/splash-done";

/// The animated boot screen, until the brain has its instructions loaded and the controller answers
/// (or a key is pressed). Events that arrive meanwhile are kept for the console.
fn boot_splash(fb: &mut crate::fb::Fb, app: &mut App, ev_rx: &Receiver<Ev>, llm_rx: &Receiver<Llm>) {
    use crate::splash::{Splash, State, Step};
    let mut sp = Splash::new(fb);
    let mut last_stat = Instant::now() - Duration::from_secs(5);
    loop {
        if last_stat.elapsed() >= Duration::from_secs(1) {
            read_sys(&mut app.sys);
            last_stat = Instant::now();
        }
        while let Ok(l) = llm_rx.try_recv() {
            app.llm = l;
        }
        while let Ok(e) = ev_rx.try_recv() {
            app.on_event(e);
        }
        let running = |m: &str| app.sys.modules.iter().any(|(n, pid, _)| n == m && pid != "-");
        let brain = file_name(&config::get("model"));
        let judge = file_name(&config::get("judge_model"));
        let steps = [
            Step { label: "kernel", detail: format!("linux {} · {} cpus", app.sys.kernel, app.sys.cpus), state: State::Done },
            match app.sys.ips.first() {
                Some(ip) => Step { label: "network", detail: ip.clone(), state: State::Done },
                None => Step { label: "network", detail: "dhcp".into(), state: State::Busy(None) },
            },
            match (app.llm.up, running("llm")) {
                (true, _) => Step { label: "brain", detail: brain, state: State::Done },
                (false, true) => Step { label: "brain", detail: format!("{brain} · loading"), state: State::Busy(None) },
                (false, false) => Step { label: "brain", detail: "not running".into(), state: State::Off },
            },
            match (app.llm.judge_up, running("judge")) {
                (true, _) => Step { label: "controller", detail: judge, state: State::Done },
                (false, true) => Step { label: "controller", detail: format!("{judge} · loading"), state: State::Busy(None) },
                (false, false) => Step { label: "controller", detail: "not running".into(), state: State::Off },
            },
            match (app.warm, app.progress) {
                (Some(true), _) => Step { label: "instructions", detail: "loaded".into(), state: State::Done },
                (Some(false), _) => Step { label: "instructions", detail: "skipped".into(), state: State::Off },
                (None, Some((d, t))) => Step { label: "instructions", detail: "reading".into(), state: State::Busy(Some(d as f32 / t.max(1) as f32)) },
                (None, None) if app.llm.up => Step { label: "instructions", detail: "reading".into(), state: State::Busy(None) },
                (None, None) => Step { label: "instructions", detail: "waiting for the brain".into(), state: State::Wait },
            },
        ];
        let ready = app.warm.is_some() && !matches!(steps[3].state, State::Busy(_));
        // at least one full intro, so a fast machine doesn't just flash it
        if ready && sp.secs() > 2.8 {
            sp.leave();
        }
        if !sp.frame(fb, &steps) {
            break;
        }
        if event::poll(Duration::from_millis(30)).unwrap_or(false) && matches!(event::read(), Ok(Event::Key(_))) {
            sp.leave();
        }
    }
    let _ = fs::write(SPLASH_DONE, "");
}

/// Returns true when the screen must be re-opened (font changed), false to quit.
fn ui_loop<B: Backend>(
    term: &mut Terminal<B>,
    app: &mut App,
    ev_rx: &Receiver<Ev>,
    llm_rx: &Receiver<Llm>,
    prompts: &Sender<Cmd>,
    cancel: &AtomicBool,
    font: Option<String>,
) -> bool {
    let mut last_stat = Instant::now() - Duration::from_secs(5);
    loop {
        if last_stat.elapsed() >= Duration::from_secs(1) {
            read_sys(&mut app.sys);
            app.conf = config::load();
            app.memories = crate::memory::count();
            last_stat = Instant::now();
            if font.as_ref().is_some_and(|f| *f != app.conf["ui_font"]) {
                return true;
            }
        }
        while let Ok(l) = llm_rx.try_recv() {
            app.llm = l;
        }
        while let Ok(e) = ev_rx.try_recv() {
            app.on_event(e);
        }
        app.tick += 1;
        if term.draw(|f| draw(f, app)).is_err() {
            return false;
        }
        if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(k)) = event::read() else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // a pending controller question takes every key until it is answered
        if app.confirm.is_some() {
            let answer = match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                KeyCode::Char('c') if ctrl => Some(false),
                _ => None,
            };
            if let Some(yes) = answer {
                app.confirm = None;
                app.push(Entry::Info(if yes { "allowed by the owner" } else { "denied by the owner" }.into()));
                let _ = prompts.send(Cmd::Confirm(yes));
            }
            continue;
        }
        match k.code {
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => {
                if app.busy {
                    cancel.store(true, Ordering::Relaxed);
                    app.push(Entry::Info("cancelled".into()));
                } else {
                    app.input.clear();
                }
            }
            KeyCode::Esc if app.busy => cancel.store(true, Ordering::Relaxed),
            // console commands work even with the brain down
            KeyCode::Enter if app.input.starts_with('/') => {
                let line = std::mem::take(&mut app.input);
                app.push(Entry::User(line.clone()));
                app.scroll = 0;
                let out = command(&line, prompts, app.busy);
                app.push(Entry::Info(out));
            }
            KeyCode::Enter if !app.busy && !app.input.trim().is_empty() => {
                let p = std::mem::take(&mut app.input);
                app.push(Entry::User(p.clone()));
                app.busy = true;
                app.scroll = 0;
                let _ = prompts.send(Cmd::Prompt(p));
            }
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::PageUp => app.scroll += 10,
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
            KeyCode::End => app.scroll = 0,
            KeyCode::Char(c) if !ctrl => app.input.push(c),
            _ => {}
        }
    }
}

const HELP: &str = "console commands (they work even with the brain down):
  /config              show settings
  /set <key> <value>   change a setting (e.g. /set ctx 65536, /set keymap it, /set ui_font 24)
  /keymaps             list keyboard layouts
  /reset               factory settings
  /restart <module>    restart a module (e.g. /restart llm)
  /new                 new conversation (clears the context)
  /safe                safe mode on/off: ignore /data/modules and config
  /help                this help";

fn command(line: &str, prompts: &Sender<Cmd>, busy: bool) -> String {
    let mut p = line.trim().splitn(3, ' ');
    let res = match (p.next().unwrap_or(""), p.next(), p.next()) {
        ("/help", ..) => Ok(HELP.into()),
        ("/config", ..) => Ok(config::describe()),
        ("/set", Some(k), Some(v)) => agent::set_config(k, v),
        ("/reset", ..) => config::reset()
            .and_then(|_| crate::keyboard::apply(&config::get("keymap")))
            .and_then(|_| crate::restart_module("llm"))
            .map(|_| "factory settings restored; brain restarting".into()),
        ("/keymaps", ..) => Ok(crate::keyboard::index()),
        ("/restart", Some(m), _) => crate::restart_module(m),
        ("/new", ..) if busy => Err("cancel the running request first (Esc)".into()),
        ("/new", ..) => prompts.send(Cmd::Reset).map(|_| "new conversation".into()).map_err(|e| e.to_string()),
        ("/safe", ..) => {
            let on = !crate::safe_mode();
            let r = if on { fs::write(crate::SAFE_FLAG, "") } else { fs::remove_file(crate::SAFE_FLAG) };
            r.map_err(|e| e.to_string()).map(|_| {
                let _ = crate::restart_module("llm");
                if on { "SAFE MODE: /data modules and config ignored until /safe or reboot" } else { "safe mode off" }.into()
            })
        }
        _ => Err("unknown command, try /help".into()),
    };
    res.unwrap_or_else(|e| format!("error: {e}"))
}

/// llama-server is polled off the UI thread so a slow brain never freezes the screen.
fn llm_poller() -> Receiver<Llm> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        loop {
            let get = |p: &str| {
                ureq::get(&format!("{}{p}", agent::llm_url()))
                    .timeout(Duration::from_secs(2))
                    .call()
                    .ok()
                    .and_then(|r| r.into_string().ok())
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            };
            let mut l = Llm::default();
            if let Some(p) = get("/props") {
                l.up = true;
                l.model = file_name(p["model_path"].as_str().unwrap_or("?"));
            }
            l.judge_up = ureq::get(&format!("http://127.0.0.1:{}/health", config::get("judge_port"))).timeout(Duration::from_secs(2)).call().is_ok();
            if let Some(s) = get("/slots") {
                l.busy_slots = s.as_array().into_iter().flatten().filter(|x| x["is_processing"] == true).count();
            }
            if tx.send(l).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    rx
}

/// Basename without .gguf, following a symlink (current.gguf, judge.gguf) to the real model file.
fn file_name(path: &str) -> String {
    let real = fs::read_link(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or(path.into());
    real.rsplit('/').next().unwrap_or("?").trim_end_matches(".gguf").into()
}

fn read_sys(s: &mut Sys) {
    // CPU: busy share of jiffies since last sample
    if let Some(l) = fs::read_to_string("/proc/stat").ok().and_then(|t| t.lines().next().map(String::from)) {
        let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        let total: u64 = v.iter().sum();
        let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
        let (pt, pi) = s.cpu_prev;
        if total > pt {
            let busy = 100 * ((total - pt) - (idle - pi).min(total - pt)) / (total - pt);
            s.cpu.push_back(busy);
            if s.cpu.len() > 120 {
                s.cpu.pop_front();
            }
        }
        s.cpu_prev = (total, idle);
    }
    let mem = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |k: &str| mem.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
    s.mem_total = kb("MemTotal:");
    s.mem_used = s.mem_total.saturating_sub(kb("MemAvailable:"));
    s.load = fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    s.uptime = fs::read_to_string("/proc/uptime").ok().and_then(|u| u.split('.').next()?.parse().ok()).unwrap_or(0);
    s.ips = fs::read_dir("/run/aios")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let ifc = n.strip_prefix("ip.")?.to_string();
            Some(format!("{ifc} {}", fs::read_to_string(e.path()).ok()?.trim()))
        })
        .collect();
    s.ips.sort();
    s.modules = fs::read_to_string("/run/aios/modules")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut p = l.split_whitespace();
            Some((p.next()?.into(), p.next()?.into(), p.next()?.into()))
        })
        .collect();
}

// ---------------------------------------------------------------- drawing

fn border() -> BorderType {
    if fancy() { BorderType::Rounded } else { BorderType::Plain }
}

fn panel(ic: Icon, title: &str) -> Block<'static> {
    Block::bordered().border_type(border()).border_style(fg(BORDER)).title(Line::from(vec![
        Span::raw(" "),
        Span::styled(icon(ic), fg(MUTED)),
        Span::styled(format!("{title} "), fg(TEXT).add_modifier(Modifier::BOLD)),
    ]))
}

/// Inner area of a panel with one column of padding on each side.
fn padded(area: Rect) -> Rect {
    Rect { x: area.x + 2, y: area.y + 1, width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) }
}

/// A rounded "pill": coloured background with half-circle caps on the framebuffer.
fn pill(text: String, bg: Color, ink: Color) -> Vec<Span<'static>> {
    let body = Span::styled(text, Style::new().fg(ink).bg(bg).add_modifier(Modifier::BOLD));
    if !fancy() {
        return vec![body];
    }
    vec![Span::styled("\u{E0B6}", fg(bg)), body, Span::styled("\u{E0B4}", fg(bg))]
}

fn draw(f: &mut Frame, a: &App) {
    f.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), f.area());
    let [head, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(1)]).areas(f.area());
    let side_w = if body.width >= 120 {
        46
    } else if body.width >= 96 {
        38
    } else {
        0
    };
    let [main, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(side_w)]).areas(body);
    let [flow, input] = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).areas(main);
    draw_header(f, head, a);
    draw_flow(f, flow, a);
    draw_input(f, input, a);
    if side_w > 0 {
        draw_side(f, side, a);
    }
    draw_footer(f, foot, a);
}

fn draw_header(f: &mut Frame, area: Rect, a: &App) {
    let up = a.sys.uptime;
    let mut left = vec![Span::raw(" ")];
    left.extend(pill(format!("{}Daimon {} ", icon(Icon::Spark), env!("CARGO_PKG_VERSION")), ACCENT, BG));
    left.push(Span::styled(
        format!("   {}  ·  up {}h{:02}m  ·  linux {}", a.sys.ips.first().map_or("no network", |s| s.as_str()), up / 3600, up / 60 % 60, a.sys.kernel),
        fg(MUTED),
    ));
    let (label, color) = if crate::safe_mode() {
        (" SAFE MODE ".to_string(), RED)
    } else if a.confirm.is_some() {
        (format!(" {}waiting for you ", icon(Icon::Alert)), AMBER)
    } else if a.busy {
        (format!(" {} working ", spinner(a.tick)), AMBER)
    } else if a.llm.busy_slots > 0 {
        (" LAN API request ".to_string(), VIOLET)
    } else if a.llm.up {
        (" ready ".to_string(), ACCENT)
    } else {
        (" brain offline ".to_string(), RED)
    };
    let mut right = pill(label, color, BG);
    right.push(Span::raw(" "));
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(26)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)).style(Style::new().bg(SURFACE)), l);
    f.render_widget(Paragraph::new(Line::from(right).right_aligned()).style(Style::new().bg(SURFACE)), r);
}

fn draw_footer(f: &mut Frame, area: Rect, a: &App) {
    let keys = if a.confirm.is_some() {
        "Y allow  ·  N / Esc deny"
    } else if a.busy {
        "Esc cancel  ·  PgUp/PgDn scroll"
    } else {
        "Enter send  ·  PgUp/PgDn scroll  ·  End latest  ·  /help commands"
    };
    let km = a.conf.get("keymap").map_or("?", String::as_str);
    let right = format!("{}{km}  ·  {}  ", icon(Icon::Keyboard), a.font);
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(right.chars().count() as u16)]).areas(area);
    f.render_widget(Paragraph::new(Span::styled(format!(" {keys}"), fg(FAINT))), l);
    f.render_widget(Paragraph::new(Span::styled(right, fg(FAINT))), r);
}

/// The conversation as pre-wrapped lines, so gutters and cards survive wrapping.
fn flow_lines(a: &App, w: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let body = w.saturating_sub(4);
    let indent = |s: String, st: Style| Line::from(vec![Span::raw("   "), Span::styled(s, st)]);
    let gutter = |c: Color| Span::styled(format!("   {} ", g("│", "|")), fg(c));
    let corner = |c: Color| Span::styled(format!("   {} ", g("╰─", "+-")), fg(c));
    let mut prev_text = false;
    for (i, e) in a.log.iter().enumerate() {
        let next = a.log.get(i + 1);
        let prev = i.checked_sub(1).and_then(|p| a.log.get(p));
        match e {
            Entry::User(t) => {
                out.push(Line::raw(""));
                out.push(Line::from(vec![Span::styled(icon(Icon::User), fg(SKY)), Span::styled("You", fg(SKY).add_modifier(Modifier::BOLD))]));
                out.extend(wrap(t, body.saturating_sub(3)).into_iter().map(|l| indent(l, fg(STRONG))));
            }
            Entry::Text(t) => {
                if !prev_text {
                    out.push(Line::raw(""));
                    out.push(Line::from(vec![Span::styled(icon(Icon::Spark), fg(ACCENT)), Span::styled("Daimon", fg(ACCENT).add_modifier(Modifier::BOLD))]));
                }
                // the model is told to avoid markdown; strip what slips through
                let clean = t.trim().replace("**", "").replace('`', "");
                out.extend(wrap(&clean, body.saturating_sub(3)).into_iter().map(|l| indent(l, fg(TEXT))));
            }
            Entry::Think(t) => {
                let lines = wrap(t.trim(), body.saturating_sub(5));
                let hidden = lines.len().saturating_sub(6);
                if hidden > 0 {
                    out.push(Line::from(vec![gutter(BORDER), Span::styled(format!("thinking · {hidden} earlier lines"), fg(FAINT))]));
                }
                out.extend(lines.into_iter().skip(hidden).map(|l| Line::from(vec![gutter(BORDER), Span::styled(l, fg(FAINT))])));
            }
            Entry::Tool(name, args) => {
                out.push(Line::from(vec![
                    Span::styled(format!("   {} ", g("╭─", "+-")), fg(AMBER)),
                    Span::styled(icon(Icon::Gear), fg(AMBER)),
                    Span::styled(format!("{name} "), fg(AMBER).add_modifier(Modifier::BOLD)),
                    Span::styled(trunc(args, body.saturating_sub(name.len() + 9)), fg(MUTED)),
                ]));
                if !matches!(next, Some(Entry::ToolOut(_) | Entry::Judging(..) | Entry::Judge(_))) {
                    out.push(Line::from(vec![corner(AMBER), Span::styled(format!("{} running", spinner(a.tick)), fg(FAINT))]));
                }
            }
            Entry::ToolOut(t) => {
                let bad = t.starts_with("error") || t.starts_with("denied") || t.starts_with("REFUSED");
                let c = if bad { RED } else { AMBER };
                let all: Vec<String> = t.lines().flat_map(|l| wrap(l, body.saturating_sub(5))).collect();
                for l in all.iter().take(6) {
                    out.push(Line::from(vec![gutter(c), Span::styled(l.clone(), fg(if bad { RED } else { MUTED }))]));
                }
                let (mark, msg) = if bad { (g("✗", "x"), "failed") } else { (g("✓", "+"), "done") };
                let more = if all.len() > 6 { format!(" · {} more lines", all.len() - 6) } else { String::new() };
                out.push(Line::from(vec![corner(c), Span::styled(format!("{mark} "), fg(c)), Span::styled(format!("{msg}{more}"), fg(FAINT))]));
            }
            Entry::Judging(kind, subject, t) => {
                let nested = matches!(prev, Some(Entry::Tool(..)));
                let side = |c: &'static str| vec![lead(nested), Span::styled(format!("{} ", g(c, "|")), fg(VIOLET))];
                out.push(judge_head(kind, &format!("judging {} {:.1} s", spinner(a.tick), t.elapsed().as_secs_f64()), nested));
                out.push(Line::from([side("│"), vec![Span::styled(trunc(subject, body.saturating_sub(7)), fg(MUTED))]].concat()));
                out.push(Line::from([side("╰─"), vec![Span::styled("asking the decision model…", fg(FAINT))]].concat()));
            }
            Entry::Judge(r) => out.extend(judge_card(r, body, matches!(prev, Some(Entry::Tool(..))))),
            Entry::Info(t) => out.extend(
                wrap(t, body.saturating_sub(3))
                    .into_iter()
                    .map(|l| Line::from(vec![Span::styled(format!("   {} ", g("·", "-")), fg(FAINT)), Span::styled(l, fg(MUTED))])),
            ),
            Entry::Ask(why) => {
                for (k, l) in wrap(&format!("Needs you: {why}. Allow it? Y / N"), body.saturating_sub(6)).into_iter().enumerate() {
                    let mark = if k == 0 { icon(Icon::Alert) } else { "   ".into() };
                    out.push(Line::from(vec![Span::raw("   "), Span::styled(mark, fg(AMBER)), Span::styled(l, fg(AMBER).add_modifier(Modifier::BOLD))]));
                }
            }
            Entry::Err(t) => out.extend(
                wrap(t, body.saturating_sub(3))
                    .into_iter()
                    .map(|l| Line::from(vec![Span::styled(format!("   {} ", g("✗", "!")), fg(RED)), Span::styled(l, fg(RED))])),
            ),
        }
        prev_text = matches!(e, Entry::Text(_));
    }
    if let Some((d, t)) = a.progress {
        let mut l = vec![Span::styled(format!("   {} reading context ", spinner(a.tick)), fg(AMBER))];
        l.extend(bar(20, d as f64 / t.max(1) as f64, AMBER));
        l.push(Span::styled(format!(" {d}/{t} tokens"), fg(MUTED)));
        out.push(Line::from(l));
    } else if a.busy && a.confirm.is_none() && !matches!(a.log.last(), Some(Entry::Judging(..) | Entry::Tool(..))) {
        out.push(Line::from(Span::styled(format!("   {} thinking", spinner(a.tick)), fg(FAINT))));
    }
    out
}

/// Left margin of a controller card: plain indent, or the gutter of the tool card it judges.
fn lead(nested: bool) -> Span<'static> {
    if nested { Span::styled(format!("   {} ", g("│", "|")), fg(AMBER)) } else { Span::raw("   ") }
}

fn judge_head(kind: &str, right: &str, nested: bool) -> Line<'static> {
    Line::from(vec![
        lead(nested),
        Span::styled(format!("{} ", g("╭─", "+-")), fg(VIOLET)),
        Span::styled(icon(Icon::Shield), fg(VIOLET)),
        Span::styled("Controller", fg(VIOLET).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {kind} · {right}"), fg(MUTED)),
    ])
}

/// The controller's questions, probabilities, thresholds and the final acceptance score.
fn judge_card(r: &Report, body: usize, nested: bool) -> Vec<Line<'static>> {
    let gut = || vec![lead(nested), Span::styled(format!("{} ", g("│", "|")), fg(VIOLET))];
    let room = body.saturating_sub(if nested { 7 } else { 5 });
    let mut out = vec![judge_head(r.kind, &format!("{:.1} s", r.secs), nested)];
    out.push(Line::from([gut(), vec![Span::styled(trunc(&r.subject, room), fg(MUTED))]].concat()));
    let show_rule = room >= 72;
    let qw = room.saturating_sub(2 + 13 + 5 + if show_rule { 24 } else { 0 }).max(12);
    for row in &r.rows {
        let (mark, c) = if row.ok { (g("✓", "+"), ACCENT) } else { (g("✗", "x"), RED) };
        let mut l = gut();
        l.push(Span::styled(format!("{mark} "), fg(c)));
        l.push(Span::styled(format!("{:<qw$} ", trunc(row.question, qw)), fg(TEXT)));
        l.extend(bar(12, row.p, c));
        l.push(Span::styled(format!("{:>4.0}%", row.p * 100.0), fg(STRONG).add_modifier(Modifier::BOLD)));
        if show_rule {
            l.push(Span::styled(format!("  {}", trunc(row.rule, 22)), fg(FAINT)));
        }
        out.push(Line::from(l));
    }
    let c = if r.allowed {
        ACCENT
    } else if r.rows.is_empty() {
        RED
    } else {
        AMBER
    };
    let mut last = vec![lead(nested), Span::styled(format!("{} ", g("╰─", "+-")), fg(VIOLET))];
    let mut used = 0;
    if !r.rows.is_empty() {
        last.push(Span::styled("acceptance ", fg(MUTED)));
        last.extend(bar(10, r.score, c));
        last.push(Span::styled(format!(" {:>3.0}%  ", r.score * 100.0), fg(c).add_modifier(Modifier::BOLD)));
        used = 11 + 10 + 7;
    }
    let verdict = format!("{} {}", g("→", "->"), r.verdict);
    last.push(Span::styled(trunc(&verdict, room.saturating_sub(used)), fg(c).add_modifier(Modifier::BOLD)));
    out.push(Line::from(last));
    out
}

fn draw_flow(f: &mut Frame, area: Rect, a: &App) {
    let ctx = a.conf.get("ctx").and_then(|c| c.parse::<u64>().ok()).unwrap_or(1).max(1);
    let stats = format!(" ctx {:.1}k/{}k · {:.1} tok/s ", a.ctx_used as f64 / 1000.0, ctx / 1000, a.tok_s);
    let blk = panel(Icon::Chat, "Conversation").title(Line::from(Span::styled(stats, fg(FAINT))).right_aligned());
    f.render_widget(blk, area);
    let inner = padded(area);
    let lines = flow_lines(a, inner.width as usize);
    let h = inner.height as usize;
    let max = lines.len().saturating_sub(h);
    let top = max.saturating_sub(a.scroll.min(max));
    let visible: Vec<Line> = lines.into_iter().skip(top).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if top < max {
        let more = format!(" {} {} newer lines · End ", g("↓", "v"), max - top);
        let w = more.chars().count() as u16;
        let r = Rect { x: area.right().saturating_sub(w + 2), y: area.bottom().saturating_sub(1), width: w, height: 1 };
        f.render_widget(Paragraph::new(Span::styled(more, fg(SKY))), r);
    }
}

fn draw_input(f: &mut Frame, area: Rect, a: &App) {
    if a.confirm.is_some() {
        let blk = Block::bordered().border_type(border()).border_style(fg(AMBER)).title(Line::from(vec![
            Span::raw(" "),
            Span::styled(icon(Icon::Shield), fg(AMBER)),
            Span::styled("The controller needs you ", fg(AMBER).add_modifier(Modifier::BOLD)),
        ]));
        let mut l = vec![Span::styled(" Allow this action?   ", fg(STRONG).add_modifier(Modifier::BOLD))];
        l.extend(pill(" Y  allow ".into(), ACCENT, BG));
        l.push(Span::raw("   "));
        l.extend(pill(" N  deny ".into(), RED, BG));
        f.render_widget(Paragraph::new(Line::from(l)).block(blk), area);
        return;
    }
    let (title, color) = if a.busy { (format!("{} working · Esc to cancel ", spinner(a.tick)), AMBER) } else { ("Message ".into(), SKY) };
    let blk = Block::bordered()
        .border_type(border())
        .border_style(fg(if a.busy { BORDER } else { SKY }))
        .title(Line::from(vec![Span::raw(" "), Span::styled(title, fg(color).add_modifier(Modifier::BOLD))]));
    let cursor = if a.busy || a.tick / 5 % 2 == 1 { " " } else { g("▏", "_") };
    let width = area.width.saturating_sub(7) as usize;
    let n = a.input.chars().count();
    let shown: String = a.input.chars().skip(n.saturating_sub(width)).collect();
    let prompt = Span::styled(format!(" {} ", g("❯", ">")), fg(SKY).add_modifier(Modifier::BOLD));
    let line = if a.input.is_empty() && !a.busy {
        Line::from(vec![prompt, Span::styled(cursor, fg(SKY)), Span::styled("Ask the system anything, or /help", fg(FAINT))])
    } else {
        Line::from(vec![prompt, Span::styled(shown, fg(STRONG)), Span::styled(cursor, fg(SKY))])
    };
    f.render_widget(Paragraph::new(line).block(blk), area);
}

fn kv(k: &str, v: String, c: Color) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<7}"), fg(FAINT)), Span::styled(v, fg(c))])
}

fn meter(label: &str, frac: f64, text: String, width: usize) -> Line<'static> {
    let mut l = vec![Span::styled(format!("{label:<5}"), fg(MUTED))];
    l.extend(bar(width.saturating_sub(5 + text.chars().count() + 1), frac, level_color(frac)));
    l.push(Span::styled(format!(" {text}"), fg(STRONG)));
    Line::from(l)
}

fn draw_side(f: &mut Frame, area: Rect, a: &App) {
    let [sys, brain, ctrl, mods] = Layout::vertical([Constraint::Length(9), Constraint::Length(7), Constraint::Length(6), Constraint::Min(3)]).areas(area);
    let cf = |k: &str| a.conf.get(k).cloned().unwrap_or_default();

    // system
    f.render_widget(panel(Icon::Cpu, "System"), sys);
    let inner = padded(sys);
    let w = inner.width as usize;
    let [c, spark, m, rest] = Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let cpu = a.sys.cpu.back().copied().unwrap_or(0);
    f.render_widget(Paragraph::new(meter("CPU", cpu as f64 / 100.0, format!("{cpu:>3}%"), w)), c);
    let data: Vec<u64> = a.sys.cpu.iter().rev().take(spark.width as usize).rev().copied().collect();
    let levels = if fancy() { ratatui::symbols::bar::NINE_LEVELS } else { ratatui::symbols::bar::THREE_LEVELS };
    f.render_widget(Sparkline::default().data(&data).max(100).bar_set(levels).style(fg(SKY)), spark);
    let mem = if a.sys.mem_total > 0 { a.sys.mem_used as f64 / a.sys.mem_total as f64 } else { 0.0 };
    let gb = |kb: u64| kb as f64 / 1048576.0;
    f.render_widget(Paragraph::new(meter("RAM", mem, format!("{:.1}/{:.1} GB", gb(a.sys.mem_used), gb(a.sys.mem_total)), w)), m);
    let mut lines = vec![kv("load", format!("{} · {} cpu", a.sys.load, a.sys.cpus), TEXT)];
    lines.extend(a.sys.ips.iter().map(|ip| kv("net", ip.clone(), TEXT)));
    if a.sys.ips.is_empty() {
        lines.push(kv("net", "not configured".into(), AMBER));
    }
    f.render_widget(Paragraph::new(lines), rest);

    // brain
    f.render_widget(panel(Icon::Brain, "Brain"), brain);
    let inner = padded(brain);
    let w = inner.width as usize;
    let ctx = cf("ctx").parse::<u64>().unwrap_or(1).max(1);
    let think = if cf("thinking") == "on" { format!("on · max {} tok", cf("thinking_budget")) } else { "off".into() };
    let lines = vec![
        kv("model", if a.llm.up { trunc(&a.llm.model, w.saturating_sub(7)) } else { "offline".into() }, if a.llm.up { STRONG } else { RED }),
        kv("api", format!(":{} · OpenAI", cf("port")), TEXT),
        kv("think", think, TEXT),
        kv("memory", format!("{} notes", a.memories), TEXT),
        meter("CTX", a.ctx_used as f64 / ctx as f64, format!("{:.1}k/{}k", a.ctx_used as f64 / 1000.0, ctx / 1000), w),
    ];
    f.render_widget(Paragraph::new(lines), inner);

    // controller
    f.render_widget(panel(Icon::Shield, "Controller"), ctrl);
    let inner = padded(ctrl);
    let w = inner.width as usize;
    let up = a.sys.modules.iter().any(|(n, pid, _)| n == "judge" && pid != "-");
    let on = cf("controller") == "on";
    let (state, sc) = if !up {
        ("down: actions need you", RED)
    } else if on {
        ("checking actions", ACCENT)
    } else {
        ("memory rule only", AMBER)
    };
    let mut lines = vec![kv("model", trunc(&file_name(&cf("judge_model")), w.saturating_sub(7)), STRONG), kv("state", format!("{} {state}", g("●", "*")), sc)];
    lines.push(match &a.judge.last {
        Some((kind, score, ok)) => {
            kv("last", format!("{kind} · {:.0}% · {}", score * 100.0, if *ok { "allowed" } else { "stopped" }), if *ok { ACCENT } else { AMBER })
        }
        None => kv("last", "no decisions yet".into(), FAINT),
    });
    lines.push(kv("stats", format!("{} {}   {} {}   avg {:.1} s", g("✓", "+"), a.judge.allowed, g("✗", "x"), a.judge.blocked, a.judge.secs), TEXT));
    f.render_widget(Paragraph::new(lines), inner);

    // modules
    f.render_widget(panel(Icon::Cube, "Modules"), mods);
    let lines: Vec<Line> = a
        .sys
        .modules
        .iter()
        .map(|(n, pid, r)| {
            let (dot, c) = if pid == "-" { (g("○", "-"), RED) } else { (g("●", "*"), ACCENT) };
            let mut l =
                vec![Span::styled(format!("{dot} "), fg(c)), Span::styled(format!("{n:<10}"), fg(TEXT)), Span::styled(format!("pid {pid:<7}"), fg(FAINT))];
            if r != "0" {
                l.push(Span::styled(format!("{} {r}", g("↻", "restarts")), fg(AMBER)));
            }
            Line::from(l)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), padded(mods));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::Row;
    use ratatui::backend::TestBackend;

    fn demo() -> App {
        let report = |kind, subject: &str, rows: Vec<Row>, score, allowed, verdict: &str| {
            Entry::Judge(Report { kind, subject: subject.into(), rows, score, allowed, verdict: verdict.into(), secs: 7.8 })
        };
        let log = vec![
            Entry::User("imposta la tastiera italiana".into()),
            Entry::Think("The owner wants the Italian layout. config_set keymap it.".into()),
            Entry::Tool("config_set".into(), r#"{"key":"keymap","value":"it"}"#.into()),
            report(
                "action check",
                r#"config_set {"key":"keymap","value":"it"}"#,
                vec![
                    Row { question: "Does it do what the owner asked?", p: 0.93, rule: "needs >= 60%", ok: true },
                    Row { question: "Could it delete data, break the system or cut the agent off?", p: 0.08, rule: "needs < 40%", ok: true },
                ],
                0.86,
                true,
                "allowed",
            ),
            Entry::ToolOut("keyboard layout it (Italian) active".into()),
            Entry::Tool(
                "memory_save".into(),
                r#"{"category":"owner","title":"Keyboard","content":"The owner wants the agent to use the Italian keyboard layout."}"#.into(),
            ),
            report(
                "memory gate",
                "Keyboard. The owner wants the agent to use the Italian keyboard layout.",
                vec![Row { question: "About this system, the agent or the owner's preferences?", p: 0.94, rule: "pass >= 70%, refuse < 40%", ok: true }],
                0.94,
                true,
                "stored: topic 'owner_preferences'",
            ),
            Entry::ToolOut("saved /data/memory/owner/wiki/keyboard.md".into()),
            Entry::Text("Fatto: la tastiera ora è italiana (it). L'ho anche annotato tra le tue preferenze.".into()),
            Entry::User("cancella il modulo prova".into()),
            Entry::Tool("write_file".into(), r#"{"path":"/data/modules/tui/disabled","content":""}"#.into()),
            report(
                "action check",
                r#"write_file {"path":"/data/modules/tui/disabled","content":""}"#,
                vec![
                    Row { question: "Does it do what the owner asked?", p: 0.46, rule: "needs >= 60%", ok: false },
                    Row { question: "Could it delete data, break the system or cut the agent off?", p: 0.39, rule: "needs < 40%", ok: true },
                ],
                0.28,
                false,
                "asks the owner: the controller thinks this is not what you asked",
            ),
            Entry::Ask("the controller thinks this is not what you asked".into()),
        ];
        let mut conf = config::load();
        conf.insert("keymap".into(), "it".into());
        App {
            log,
            input: String::new(),
            busy: true,
            scroll: 0,
            tok_s: 14.2,
            ctx_used: 6400,
            progress: None,
            confirm: Some("x".into()),
            memories: 3,
            conf,
            sys: Sys {
                cpu: (0..120).map(|i| (40.0 + 35.0 * (i as f64 / 6.0).sin()) as u64).collect(),
                mem_used: 5_100_000,
                mem_total: 8_000_000,
                load: "1.52 0.42 0.14".into(),
                uptime: 3720,
                ips: vec!["eth0 10.0.2.15/24".into()],
                modules: vec![("judge".into(), "79".into(), "0".into()), ("llm".into(), "81".into(), "0".into()), ("tui".into(), "82".into(), "1".into())],
                kernel: "6.18.55".into(),
                cpus: 4,
                ..Default::default()
            },
            llm: Llm { up: true, model: "MiniCPM5-2B-Q4_K_M".into(), busy_slots: 0, judge_up: true },
            judge: JudgeStats { allowed: 2, blocked: 1, secs: 7.6, last: Some(("action check".into(), 0.28, false)) },
            tick: 3,
            font: "font 10x19".into(),
            warm: Some(true),
        }
    }

    /// Draws a whole screen; AIOS_DUMP=<file> writes the cells (for tools/preview of the real pixels).
    #[test]
    fn renders_controller_cards() {
        FANCY.store(true, Ordering::Relaxed);
        let mut t = Terminal::new(TestBackend::new(128, 42)).unwrap();
        let app = demo();
        t.draw(|f| draw(f, &app)).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        for want in ["Controller", "acceptance", "86%", "93%", "28%", "Needs you", "allowed", "stored: topic", "Allow this action?"] {
            assert!(text.contains(want), "missing {want}");
        }
        if let Ok(path) = std::env::var("AIOS_DUMP") {
            let rgb = |c: Color, d: (u8, u8, u8)| match c {
                Color::Rgb(r, g, b) => format!("{r},{g},{b}"),
                _ => format!("{},{},{}", d.0, d.1, d.2),
            };
            let rows: Vec<String> = buf
                .content()
                .iter()
                .map(|c| format!("{}\t{}\t{}\t{}", c.symbol(), rgb(c.fg, (226, 232, 240)), rgb(c.bg, (15, 23, 42)), c.modifier.contains(Modifier::BOLD) as u8))
                .collect();
            fs::write(path, format!("{} {}\n{}", buf.area.width, buf.area.height, rows.join("\n"))).unwrap();
        }
    }

    #[test]
    fn wrap_keeps_words_and_splits_long_ones() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("abcdefghijkl", 8), vec!["abcdefgh", "ijkl"]);
        assert_eq!(wrap("a\nb", 8), vec!["a", "b"]);
        assert_eq!(wrap("", 8), vec![""]);
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("abc", 4), "abc");
    }
}
