//! The installation wizard (a child of tui.rs: same theme and helpers). Asks, then hands the plan to
//! install.rs in a thread and shows its progress. Full install when booted from the ISO, models only
//! when a data partition exists but has no models.

use super::*;
use crate::install::{self, BRAINS, DEFAULT_BRAIN, Disk, JUDGES, Pick, Plan, Progress};

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Welcome,
    Keyboard,
    Disk,
    Owner,
    Host,
    Brain,
    Custom,
    Judge,
    Extra,
    Summary,
    Work,
    Done,
}

struct Setup {
    step: Step,
    /// booted from the ISO: pick a disk and install everything
    full: bool,
    medium: Option<(String, String)>,
    disks: Vec<Disk>,
    keymaps: Vec<(String, String)>,
    /// cursor in the current list
    sel: usize,
    disk: usize,
    keymap: String,
    filter: String,
    owner: String,
    host: String,
    brain: usize,
    custom: String,
    judge: usize,
    extra: String,
    confirm: String,
    ram: u64,
    done: Vec<String>,
    bytes: Option<(String, u64, u64, f64)>,
    error: Option<String>,
    rx: Option<Receiver<Progress>>,
    tick: usize,
}

const GB: f64 = (1u64 << 30) as f64;

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / GB)
}

impl Setup {
    fn new() -> Setup {
        let full = !install::have_data_partition();
        let medium = if full { install::boot_medium() } else { None };
        let disks = install::disks(medium.as_ref().map(|m| m.0.as_str()));
        let ram = install::ram_bytes();
        let smallest = JUDGES[1].bytes;
        // the recommended brain if it fits next to the small controller, else the biggest that does
        let brain = if install::ram_needed(BRAINS[DEFAULT_BRAIN].bytes, smallest) <= ram {
            DEFAULT_BRAIN
        } else {
            (0..BRAINS.len()).rev().find(|&i| install::ram_needed(BRAINS[i].bytes, smallest) <= ram).unwrap_or(0)
        };
        let keymaps = crate::keyboard::index().lines().filter_map(|l| l.split_once('\t')).map(|(a, b)| (a.to_string(), b.to_string())).collect();
        Setup {
            step: Step::Welcome,
            full,
            medium,
            disks,
            keymaps,
            sel: 0,
            disk: 0,
            keymap: config::get("keymap"),
            filter: String::new(),
            owner: String::new(),
            host: config::get("hostname"),
            brain,
            custom: String::new(),
            judge: 0,
            extra: String::new(),
            confirm: String::new(),
            ram,
            done: vec![],
            bytes: None,
            error: None,
            rx: None,
            tick: 0,
        }
    }

    fn steps(&self) -> Vec<Step> {
        let mut v = vec![Step::Welcome, Step::Keyboard];
        if self.full {
            v.push(Step::Disk);
        }
        v.extend([Step::Owner, Step::Host, Step::Brain]);
        if self.brain == BRAINS.len() {
            v.push(Step::Custom);
        }
        v.extend([Step::Judge, Step::Extra, Step::Summary, Step::Work, Step::Done]);
        v
    }

    fn go(&mut self, forward: bool) {
        let s = self.steps();
        let i = s.iter().position(|x| *x == self.step).unwrap_or(0);
        let j = if forward { (i + 1).min(s.len() - 1) } else { i.saturating_sub(1) };
        self.step = s[j];
        self.sel = match self.step {
            Step::Keyboard => self.filtered().iter().position(|(n, _)| *n == self.keymap).unwrap_or(0),
            Step::Disk => self.disk,
            Step::Brain => self.brain,
            Step::Judge => self.judge,
            _ => 0,
        };
        if self.step == Step::Judge && !forward {
            return;
        }
        if self.step == Step::Judge {
            // Kev 4B unless the RAM only fits the small one
            self.judge = usize::from(install::ram_needed(self.brain_bytes(), JUDGES[0].bytes) > self.ram);
            self.sel = self.judge;
        }
    }

    fn filtered(&self) -> Vec<(String, String)> {
        let f = self.filter.to_lowercase();
        self.keymaps.iter().filter(|(n, d)| f.is_empty() || n.contains(&f) || d.to_lowercase().contains(&f)).cloned().collect()
    }

    fn brain_bytes(&self) -> u64 {
        BRAINS.get(self.brain).map_or(0, |m| m.bytes)
    }

    fn brain_pick(&self) -> Option<Pick> {
        BRAINS.get(self.brain).map(Pick::of).or_else(|| Pick::parse(&self.custom))
    }

    fn plan(&self) -> Option<Plan> {
        Some(Plan {
            // a retry after the disk was prepared only downloads
            disk: if self.full && !install::have_data_partition() { Some(self.disks.get(self.disk)?.name.clone()) } else { None },
            esp_image: self.medium.as_ref().map(|m| m.1.clone()),
            keymap: self.keymap.clone(),
            owner: self.owner.clone(),
            hostname: self.host.clone(),
            brain: self.brain_pick()?,
            judge: Pick::of(&JUDGES[self.judge]),
            extra: self.extra.clone(),
        })
    }

    fn start(&mut self) {
        let Some(plan) = self.plan() else {
            self.error = Some("nothing to install: check the choices".into());
            return;
        };
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.error = None;
        self.done.clear();
        std::thread::spawn(move || {
            if let Err(e) = install::run(&plan, &tx) {
                crate::log(&format!("install: failed: {e}"));
                let _ = tx.send(Progress::Failed(e));
            }
        });
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        while let Ok(p) = rx.try_recv() {
            match p {
                Progress::Step(s) => {
                    self.bytes = None;
                    self.done.push(s);
                }
                Progress::Bytes(n, d, t, s) => self.bytes = Some((n, d, t, s)),
                Progress::Done => {
                    self.bytes = None;
                    self.step = Step::Done;
                }
                Progress::Failed(e) => {
                    self.bytes = None;
                    self.error = Some(e);
                }
            }
        }
    }

    /// One key. Returns false when the wizard is over.
    fn key(&mut self, code: KeyCode) -> bool {
        let text = |s: &mut String, max: usize| match code {
            KeyCode::Char(c) if s.chars().count() < max => s.push(c),
            KeyCode::Backspace => {
                s.pop();
            }
            _ => {}
        };
        let list = |sel: &mut usize, n: usize| match code {
            KeyCode::Up => *sel = (*sel + n.max(1) - 1) % n.max(1),
            KeyCode::Down => *sel = (*sel + 1) % n.max(1),
            _ => {}
        };
        match (self.step, code) {
            (Step::Work, KeyCode::Char('r')) if self.error.is_some() => self.start(),
            (Step::Work, KeyCode::Esc) if self.error.is_some() => {
                self.error = None;
                self.rx = None;
                self.step = Step::Summary;
            }
            (Step::Work, _) => {}
            (Step::Done, KeyCode::Enter) => return false,
            (Step::Done, _) => {}
            (_, KeyCode::Esc) => self.go(false),
            (Step::Keyboard, KeyCode::Enter) => {
                if let Some((n, _)) = self.filtered().get(self.sel) {
                    // applied at once: the next questions are typed with it
                    if crate::keyboard::apply(n).is_ok() {
                        self.keymap = n.clone();
                    }
                }
                self.filter.clear();
                self.go(true);
            }
            (Step::Keyboard, KeyCode::Up | KeyCode::Down) => {
                let n = self.filtered().len();
                list(&mut self.sel, n)
            }
            (Step::Keyboard, _) => {
                text(&mut self.filter, 30);
                self.sel = 0;
            }
            (Step::Disk, KeyCode::Enter) if !self.disks.is_empty() => {
                self.disk = self.sel;
                self.go(true);
            }
            (Step::Disk, _) => list(&mut self.sel, self.disks.len()),
            (Step::Brain, KeyCode::Enter) => {
                self.brain = self.sel;
                self.go(true);
            }
            (Step::Brain, _) => list(&mut self.sel, BRAINS.len() + 1),
            (Step::Judge, KeyCode::Enter) => {
                self.judge = self.sel;
                self.go(true);
            }
            (Step::Judge, _) => list(&mut self.sel, JUDGES.len()),
            (Step::Custom, KeyCode::Enter) if Pick::parse(&self.custom).is_some() => self.go(true),
            (Step::Custom, _) => text(&mut self.custom, 200),
            (Step::Owner, KeyCode::Enter) => self.go(true),
            (Step::Owner, _) => text(&mut self.owner, 40),
            (Step::Host, KeyCode::Enter) if crate::net::valid_hostname(&self.host) => self.go(true),
            (Step::Host, _) => text(&mut self.host, 63),
            (Step::Extra, KeyCode::Enter) => self.go(true),
            (Step::Extra, _) => text(&mut self.extra, 600),
            (Step::Summary, KeyCode::Enter) if !self.full || self.confirm == "erase" => {
                self.step = Step::Work;
                self.start();
            }
            (Step::Summary, _) if self.full => text(&mut self.confirm, 10),
            (Step::Welcome, KeyCode::Enter) => self.go(true),
            _ => {}
        }
        true
    }
}

/// Runs the wizard on `term` until the system is installed. A full install ends with a reboot.
pub fn run<B: Backend>(term: &mut Terminal<B>, ips: impl Fn() -> Vec<String>) {
    let mut s = Setup::new();
    let _ = term.clear();
    loop {
        crate::heartbeat("tui");
        s.poll();
        s.tick += 1;
        let net = ips();
        if term.draw(|f| draw(f, &s, &net)).is_err() {
            return;
        }
        if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(Event::Key(k)) = event::read() else {
            continue;
        };
        if k.kind == KeyEventKind::Press && !s.key(k.code) {
            break;
        }
    }
    if s.full {
        unsafe {
            libc::sync();
            libc::umount2(c"/data".as_ptr(), 0);
            libc::reboot(libc::RB_AUTOBOOT);
        }
    }
}

// ---------------------------------------------------------------- drawing

fn draw(f: &mut Frame, s: &Setup, net: &[String]) {
    f.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), f.area());
    let a = f.area();
    let w = a.width.min(92).saturating_sub(2);
    let h = a.height.min(32).saturating_sub(2);
    let card = Rect { x: a.x + (a.width - w) / 2, y: a.y + (a.height - h) / 2, width: w, height: h };
    let title = if s.full { "Install Daimon" } else { "Set up Daimon" };
    let blk = Block::bordered().border_type(border()).border_style(fg(BORDER)).style(Style::new().bg(BG)).title(Line::from(vec![
        Span::raw(" "),
        Span::styled(icon(Icon::Spark), fg(ACCENT)),
        Span::styled(format!("{title} {} ", env!("CARGO_PKG_VERSION")), fg(STRONG).add_modifier(Modifier::BOLD)),
        Span::styled(format!("· {} ", crate::splash::CODENAME), fg(FAINT)),
    ]));
    let inner = blk.inner(card);
    f.render_widget(blk, card);
    let inner = Rect { x: inner.x + 2, y: inner.y + 1, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(1) };
    let [dots, body, keys] = Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(1)]).areas(inner);

    // where we are: one dot per question
    let asked: Vec<Step> = s.steps().into_iter().filter(|x| !matches!(x, Step::Welcome | Step::Work | Step::Done | Step::Custom)).collect();
    let at = asked.iter().position(|x| *x == s.step);
    let mut d = vec![];
    for (i, _) in asked.iter().enumerate() {
        let (c, col) = match at {
            Some(a) if i < a => (g("●", "*"), ACCENT),
            Some(a) if i == a => (g("●", "*"), SKY),
            None if matches!(s.step, Step::Work | Step::Done) => (g("●", "*"), ACCENT),
            _ => (g("○", "-"), BORDER),
        };
        d.push(Span::styled(format!("{c} "), fg(col)));
    }
    f.render_widget(Paragraph::new(Line::from(d)), dots);

    let width = body.width as usize;
    let mut l: Vec<Line> = vec![];
    let head = |t: &str| Line::from(Span::styled(t.to_string(), fg(STRONG).add_modifier(Modifier::BOLD)));
    let dim = |t: String| Line::from(Span::styled(t, fg(MUTED)));
    let field = |v: &str, tick: usize| {
        Line::from(vec![
            Span::styled(format!(" {} ", g("❯", ">")), fg(SKY).add_modifier(Modifier::BOLD)),
            Span::styled(v.to_string(), fg(STRONG)),
            Span::styled(if tick / 5 % 2 == 0 { g("▏", "_") } else { " " }, fg(SKY)),
        ])
    };
    let row = |sel: bool, cols: Vec<(String, Color)>| {
        let mut v = vec![Span::styled(format!(" {} ", if sel { g("❯", ">") } else { " " }), fg(ACCENT))];
        for (i, (t, c)) in cols.into_iter().enumerate() {
            let st = if sel && i == 0 { fg(STRONG).add_modifier(Modifier::BOLD) } else { fg(c) };
            v.push(Span::styled(t, st));
        }
        Line::from(v)
    };
    let mut keys_hint = "Enter continue  ·  Esc back";
    match s.step {
        Step::Welcome => {
            l.push(head("Welcome. This is an operating system where the model is the system."));
            l.push(Line::raw(""));
            l.extend(
                wrap(
                    if s.full {
                        "A few questions, then Daimon installs itself on a disk of this machine and downloads its brain and its controller from Hugging Face."
                    } else {
                        "A few questions, then Daimon downloads its brain and its controller from Hugging Face."
                    },
                    width,
                )
                .into_iter()
                .map(dim),
            );
            l.push(Line::raw(""));
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
            l.push(Line::from(vec![Span::styled("  memory   ", fg(FAINT)), Span::styled(gb(s.ram), fg(TEXT))]));
            l.push(Line::from(vec![Span::styled("  cpu      ", fg(FAINT)), Span::styled(format!("{cpus} threads"), fg(TEXT))]));
            if s.full {
                l.push(Line::from(vec![Span::styled("  disks    ", fg(FAINT)), Span::styled(format!("{}", s.disks.len()), fg(TEXT))]));
            }
            match net.first() {
                Some(ip) => l.push(Line::from(vec![Span::styled("  network  ", fg(FAINT)), Span::styled(ip.clone(), fg(ACCENT))])),
                None => l.push(Line::from(vec![
                    Span::styled("  network  ", fg(FAINT)),
                    Span::styled(format!("{} waiting for an address: the downloads need the internet", spinner(s.tick)), fg(AMBER)),
                ])),
            }
            if s.full && s.medium.is_none() {
                l.push(Line::raw(""));
                l.push(Line::from(Span::styled("  The installation medium was not found: the disk can't be prepared.", fg(RED))));
            }
            keys_hint = "Enter start";
        }
        Step::Keyboard => {
            l.push(head("Keyboard layout"));
            l.push(dim("Type to filter. The layout applies at once.".into()));
            l.push(field(&s.filter, s.tick));
            l.push(Line::raw(""));
            let list = s.filtered();
            let rows = (body.height as usize).saturating_sub(5);
            let top = s.sel.saturating_sub(rows.saturating_sub(1));
            for (i, (n, d)) in list.iter().enumerate().skip(top).take(rows) {
                l.push(row(i == s.sel, vec![(format!("{n:<14}"), TEXT), (trunc(d, width.saturating_sub(18)), FAINT)]));
            }
            keys_hint = "↑↓ choose  ·  Enter confirm  ·  Esc back";
        }
        Step::Disk => {
            l.push(head("Where to install"));
            l.push(dim("The whole disk is used: everything on it will be erased.".into()));
            l.push(Line::raw(""));
            if s.disks.is_empty() {
                l.push(Line::from(Span::styled("  No disk of at least 8 GB found.", fg(RED))));
            }
            for (i, dk) in s.disks.iter().enumerate() {
                l.push(row(i == s.sel, vec![(format!("/dev/{:<10}", dk.name), TEXT), (format!("{:>10}   ", gb(dk.bytes)), MUTED), (dk.model.clone(), FAINT)]));
            }
            keys_hint = "↑↓ choose  ·  Enter confirm  ·  Esc back";
        }
        Step::Owner => {
            l.push(head("Your name"));
            l.push(dim("So the system knows how to call you. Optional.".into()));
            l.push(Line::raw(""));
            l.push(field(&s.owner, s.tick));
        }
        Step::Host => {
            l.push(head("Machine name"));
            l.push(dim("How this machine appears on the network: letters, digits and '-'.".into()));
            l.push(Line::raw(""));
            l.push(field(&s.host, s.tick));
            if !crate::net::valid_hostname(&s.host) {
                l.push(Line::from(Span::styled("  1-63 letters, digits or '-', not starting with '-'", fg(AMBER))));
            }
        }
        Step::Brain | Step::Judge => {
            let brain = s.step == Step::Brain;
            l.push(head(if brain { "The agent's model (the brain)" } else { "The controller's model" }));
            l.push(dim(if brain {
                format!("It thinks, talks and acts. This machine has {} of memory.", gb(s.ram))
            } else {
                "It judges every action and every memory, in code, before they happen.".into()
            }));
            l.push(Line::raw(""));
            let models = if brain { &BRAINS[..] } else { &JUDGES[..] };
            for (i, m) in models.iter().enumerate() {
                let need = if brain { install::ram_needed(m.bytes, JUDGES[1].bytes) } else { install::ram_needed(s.brain_bytes(), m.bytes) };
                let fits = need <= s.ram;
                l.push(row(
                    i == s.sel,
                    vec![
                        (format!("{:<17}", m.label), TEXT),
                        (format!("{:>8}   ", gb(m.bytes)), MUTED),
                        (format!("{} needs {:<9}", if fits { g("✓", "ok") } else { g("✗", "!!") }, gb(need)), if fits { ACCENT } else { AMBER }),
                        (format!(" {}", m.note), FAINT),
                    ],
                ));
            }
            if brain {
                l.push(row(s.sel == BRAINS.len(), vec![("Other GGUF from Hugging Face…".into(), TEXT)]));
                l.push(Line::raw(""));
                let note = "\"needs\" = the model, its working memory and the system, next to the smallest controller (Kev 0.8B).";
                l.extend(wrap(note, width).into_iter().map(dim));
            }
            keys_hint = "↑↓ choose  ·  Enter confirm  ·  Esc back";
        }
        Step::Custom => {
            l.push(head("Another model from Hugging Face"));
            l.push(dim("owner/repo/file.gguf, or the link to the file".into()));
            l.push(Line::raw(""));
            l.push(field(&s.custom, s.tick));
            if !s.custom.is_empty() && Pick::parse(&s.custom).is_none() {
                l.push(Line::from(Span::styled("  e.g. unsloth/Qwen3.5-4B-GGUF/Qwen3.5-4B-Q4_K_M.gguf", fg(AMBER))));
            }
        }
        Step::Extra => {
            l.push(head("Anything to add to the agent's instructions?"));
            l.push(dim("Optional, in English. Editable later in /data/daimon/system-extra.md.".into()));
            l.push(Line::raw(""));
            for (i, line) in wrap(&s.extra, width.saturating_sub(4)).into_iter().enumerate() {
                l.push(if i == 0 { field(&line, s.tick) } else { Line::from(Span::styled(format!("   {line}"), fg(STRONG))) });
            }
        }
        Step::Summary => {
            l.push(head("Ready"));
            l.push(Line::raw(""));
            let kv = |k: &str, v: String| Line::from(vec![Span::styled(format!("  {k:<14}"), fg(FAINT)), Span::styled(v, fg(TEXT))]);
            if let Some(dk) = s.disks.get(s.disk).filter(|_| s.full) {
                l.push(kv("disk", format!("/dev/{} · {} · {}", dk.name, gb(dk.bytes), dk.model)));
            }
            l.push(kv("keyboard", s.keymap.clone()));
            l.push(kv("your name", if s.owner.is_empty() { "-".into() } else { s.owner.clone() }));
            l.push(kv("machine", s.host.clone()));
            let brain = s.brain_pick().map_or("?".into(), |p| p.file);
            l.push(kv("brain", brain));
            l.push(kv("controller", JUDGES[s.judge].file.into()));
            l.push(kv("instructions", if s.extra.is_empty() { "-".into() } else { trunc(&s.extra, width.saturating_sub(16)) }));
            let total = s.brain_bytes() + JUDGES[s.judge].bytes;
            l.push(kv("download", if total > JUDGES[s.judge].bytes { gb(total) } else { format!("{} + your model", gb(total)) }));
            l.push(Line::raw(""));
            if s.full {
                l.push(Line::from(Span::styled(
                    format!("  Everything on /dev/{} will be erased.", s.disks.get(s.disk).map_or("?", |d| d.name.as_str())),
                    fg(RED).add_modifier(Modifier::BOLD),
                )));
                l.push(dim("  Type erase to continue:".into()));
                l.push(field(&s.confirm, s.tick));
                keys_hint = "type erase, then Enter  ·  Esc back";
            } else {
                keys_hint = "Enter start  ·  Esc back";
            }
        }
        Step::Work | Step::Done => {
            l.push(head(if s.step == Step::Done {
                "Done"
            } else if s.error.is_some() {
                "Stopped"
            } else {
                "Installing"
            }));
            l.push(Line::raw(""));
            let n = s.done.len();
            for (i, d) in s.done.iter().enumerate() {
                let current = i + 1 == n && s.step == Step::Work;
                let (mark, c) = if current && s.error.is_some() {
                    (g("✗", "x"), RED)
                } else if current {
                    (spinner(s.tick), SKY)
                } else {
                    (g("✓", "+"), ACCENT)
                };
                l.push(Line::from(vec![Span::styled(format!("  {mark} "), fg(c)), Span::styled(d.clone(), fg(if current { STRONG } else { MUTED }))]));
            }
            if let Some((name, done, total, speed)) = &s.bytes {
                let frac = *done as f64 / (*total).max(1) as f64;
                let eta = if *speed > 0.0 { (total.saturating_sub(*done)) as f64 / speed } else { 0.0 };
                l.push(Line::raw(""));
                let mut b = vec![Span::raw("    ")];
                b.extend(bar(width.saturating_sub(12).min(50), frac, ACCENT));
                b.push(Span::styled(format!(" {:>3.0}%", frac * 100.0), fg(STRONG)));
                l.push(Line::from(b));
                l.push(Line::from(Span::styled(
                    format!("    {name}  ·  {} of {}  ·  {:.1} MB/s  ·  {}m{:02}s left", gb(*done), gb(*total), speed / 1e6, eta as u64 / 60, eta as u64 % 60),
                    fg(FAINT),
                )));
            }
            if let Some(e) = &s.error {
                l.push(Line::raw(""));
                l.extend(wrap(e, width.saturating_sub(4)).into_iter().map(|x| Line::from(Span::styled(format!("  {x}"), fg(RED)))));
                keys_hint = "r retry (downloads resume)  ·  Esc back";
            } else if s.step == Step::Done {
                l.push(Line::raw(""));
                if s.full {
                    l.push(Line::from(Span::styled("  Daimon is installed. Remove the installation medium, then press Enter to restart.", fg(ACCENT))));
                    keys_hint = "Enter restart";
                } else {
                    l.push(Line::from(Span::styled("  Ready. Press Enter: the brain and the controller start now.", fg(ACCENT))));
                    keys_hint = "Enter start";
                }
            } else {
                keys_hint = "the first download is the controller, then the brain";
            }
        }
    }
    f.render_widget(Paragraph::new(l), body);
    f.render_widget(Paragraph::new(Span::styled(keys_hint, fg(FAINT))), keys);
}
