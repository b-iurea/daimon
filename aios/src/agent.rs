//! The agent: a tool-calling loop over llama-server's OpenAI API.
//! It runs in its own thread and reports everything it does as `Ev`s, so the UI can show it live.

use crate::config::{self, Scope};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const MAX_OUT: usize = 12_000;

pub fn llm_url() -> String {
    format!("http://127.0.0.1:{}", config::get("port"))
}

pub enum Cmd {
    Prompt(String),
    Reset,
    /// the owner's answer to Ev::Confirm
    Confirm(bool),
}

pub enum Ev {
    Think(String),
    Text(String),
    Tool(String, String),
    ToolOut(String),
    Info(String),
    Confirm(String),
    Ctx(u64),
    Progress(u64, u64),
    /// the controller started judging (kind, subject)
    Judging(&'static str, String),
    Judge(crate::judge::Report),
    Done(f64),
    Err(String),
    /// the instructions are loaded into the brain (true) or the warm-up gave up (false)
    Ready(bool),
}

const SYSTEM: &str = r#"You are Daimon. You are not an assistant running on a computer: you ARE this computer's operating system. The kernel boots you, you supervise every process, and the person talking to you is your owner. You have full root control through your tools. When asked to do something, do it.

# Where you are
{facts}

# How you are built
- Linux kernel + one Rust binary, /usr/bin/aios: it is PID 1 (supervisor), this console UI, and you.
- There is NO shell and NO coreutils. `run` executes a binary directly. Existing binaries: /usr/bin/aios, /usr/bin/llama-server.
- / lives in RAM and is rebuilt at every boot. /data is the only persistent disk. /proc and /sys work as on any Linux.
- Everything that runs is a module: a directory containing `cmd` (one line: program and args), optional `tty`, optional empty `disabled` file.
  Built-in modules: /etc/aios/modules. Persistent modules and overrides: /data/modules (same name wins).
  The supervisor rescans every 0.5s: create a module dir to start it, change `cmd` to restart it, add `disabled` to stop it. Crashes restart with backoff.
- Live state: /run/aios/modules (name pid restarts, pid "-" = down). Logs: /run/log/<module>.log; boot log: /run/log/aios.log. IPs: /run/aios/ip.<iface>.
- Your brain is module `llm` (llama-server, OpenAI API on the LAN). Your face is module `tui` (this console; you live inside it).
  Breaking either makes you unreachable: explain the risk and ask before touching them.

# Settings: /data/aios/config
Change them with config_set (validated). [restart] keys restart your brain: you wait ~10s automatically. [live] keys apply to your next reply.
{config}
Your system prompt can be replaced by writing /data/aios/system.md.
The owner can also type console commands: /help /config /set /restart /new /safe.

# Controller
Every action that changes the system is checked first by your controller, a small decision model (module `judge`).
If it judges an action risky or different from what the owner asked, the owner must confirm it. If the owner says no, do not retry: ask what they want.
The controller also enforces the memory rule below; it cannot be switched off.

# Long-term memory
You have a long-term memory that survives reboots: markdown notes under /data/memory, managed ONLY through the memory_* tools.
STRICT RULE, enforced by the system and impossible to bypass: you may only remember things about
  system = this OS/machine: configuration, settings, modules, problems, every change you make;
  self   = yourself: identity, behaviour, lessons learned running this machine;
  owner  = the owner as a person and their preferences for you and this system.
Never try to store anything else (projects, general knowledge, small talk, other people): it will be refused.
System changes are recorded automatically by the system itself: every setting in a "Setting <key>" note (current value + history), every file written, command run and reboot in the "Changes to this system" log. Don't duplicate them; save a system note yourself only for what the log can't know (why a change was made, a problem found).
Save on your own, without being asked, whenever you: learn an owner preference or fact about the owner; learn a lesson about yourself; learn why something on the system is the way it is.
One fact per note: a single plain English sentence that names its subject, following these patterns:
  owner:  "The owner wants the agent to <behaviour>."   (also for the owner's name: "The owner wants the agent to call him <name>.")
  system: "This system: <fact or change, with old -> new values>."
  self:   "The agent's own behaviour: <lesson or rule>."
Clear short title. Same title replaces the note: memory_read it first and merge. Before answering about the past or the owner, memory_search.
Current notes:
{memory}

# How to work
- Think briefly. A few short sentences of reasoning is enough for almost everything. Do not repeat the question, do not re-check what a tool already returned, do not weigh equivalent options: pick one and act.
- Use tools instead of guessing. For questions about the system, call `status` first.
- After a change, verify once (status, the module's log) and report the result.
- Your working language is English: replies, memory notes and tool arguments. If the owner explicitly asks for another reply language, use it and remember that preference. Memory notes stay in English.
- Plain text only: the console does not render markdown, tables or headings. Be short.
"#;

fn system_prompt() -> String {
    if !crate::safe_mode() {
        if let Ok(custom) = fs::read_to_string("/data/aios/system.md") {
            return custom;
        }
    }
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu = cpuinfo.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).unwrap_or("?").trim();
    let flags = cpuinfo.lines().find(|l| l.starts_with("flags")).unwrap_or("");
    let simd: Vec<&str> = ["avx2", "avx512f", "avx_vnni"].into_iter().filter(|f| flags.split_whitespace().any(|x| x == *f)).collect();
    let mem_gb = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().next()?.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1048576.0);
    let ips: Vec<String> = fs::read_dir("/run/aios")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            Some(format!("{} {}", n.strip_prefix("ip.")?, fs::read_to_string(e.path()).ok()?.trim()))
        })
        .collect();
    let c = config::load();
    let model = fs::read_link(&c["model"]).map_or(c["model"].clone(), |p| p.display().to_string());
    let facts = format!(
        "- Machine: {cpu}, {} cores ({}), {mem_gb:.1} GB RAM. No GPU acceleration: you run on the CPU, so every token you write costs the owner time.\n\
         - Network: {}. API for the LAN: port {}.\n\
         - Brain: {model}, context window {} tokens.",
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        simd.join(" "),
        if ips.is_empty() { "not configured yet".into() } else { ips.join(", ") },
        c["port"],
        c["ctx"],
    );
    SYSTEM.replace("{facts}", &facts).replace("{config}", &config::describe()).replace("{memory}", &crate::memory::prompt_index())
}

fn tools() -> Value {
    let f = |name: &str, desc: &str, props: Value, req: &[&str]| {
        json!({"type":"function","function":{"name":name,"description":desc,
            "parameters":{"type":"object","properties":props,"required":req}}})
    };
    let path = json!({"type":"string","description":"absolute path"});
    let s = json!({"type":"string"});
    json!([
        f("status", "Snapshot of the whole system: modules, memory, load, network, disks, models, settings. Start here.", json!({}), &[]),
        f("read_file", "Read a file (large files: only the tail).", json!({"path": path}), &["path"]),
        f("write_file", "Create or overwrite a file, creating parent directories.", json!({"path": path, "content": s}), &["path", "content"]),
        f("list_dir", "List a directory.", json!({"path": path}), &["path"]),
        f(
            "run",
            "Execute a binary directly (no shell). 30s timeout. Returns exit code and output.",
            json!({"argv": {"type":"array","items":{"type":"string"}}}),
            &["argv"]
        ),
        f("config_set", "Change one setting in /data/aios/config (see the list in your instructions).", json!({"key": s, "value": s}), &["key", "value"]),
        f("restart_module", "Restart a running module by name.", json!({"name": s}), &["name"]),
        f(
            "memory_save",
            "Store one note in long-term memory (only system/self/owner topics; others are refused).",
            json!({"category": {"type":"string","enum":crate::memory::CATEGORIES}, "title": s, "content": s}),
            &["category", "title", "content"]
        ),
        f("memory_search", "Search long-term memory (keywords).", json!({"query": s}), &["query"]),
        f("memory_read", "Read one memory note by path.", json!({"path": s}), &["path"]),
        f("memory_forget", "Delete one memory note by path (when it is wrong or the owner asks).", json!({"path": s}), &["path"]),
        f("power", "Reboot or power off the machine.", json!({"action": {"type":"string","enum":["reboot","poweroff"]}}), &["action"]),
    ])
}

static SINK: std::sync::OnceLock<Sender<Ev>> = std::sync::OnceLock::new();

/// Lets code deep in a tool (the controller, called from memory_save) report to the console flow.
pub fn emit(e: Ev) {
    if let Some(s) = SINK.get() {
        let _ = s.send(e);
    }
}

/// Starts the agent thread. Send commands in, read events out; set `cancel` to stop the current turn.
pub fn spawn(events: Sender<Ev>, cancel: Arc<AtomicBool>) -> Sender<Cmd> {
    let _ = SINK.set(events.clone());
    let (tx, rx) = channel();
    std::thread::spawn(move || run(rx, events, cancel));
    tx
}

fn run(cmds: Receiver<Cmd>, ev: Sender<Ev>, cancel: Arc<AtomicBool>) {
    // `for cmd in &cmds` below would hold the receiver; iterate manually so tools can ask for confirmation
    // Built once per conversation: a stable prefix keeps llama-server's prompt cache warm.
    let mut msgs: Vec<Value> = vec![];
    prewarm(&mut msgs, &ev, &cancel);
    while let Ok(cmd) = cmds.recv() {
        let prompt = match cmd {
            Cmd::Reset => {
                msgs.clear();
                let _ = ev.send(Ev::Ctx(0));
                prewarm(&mut msgs, &ev, &cancel);
                continue;
            }
            Cmd::Prompt(p) => p,
            Cmd::Confirm(_) => continue,
        };
        if msgs.is_empty() {
            msgs.push(json!({"role":"system","content":system_prompt()}));
        }
        cancel.store(false, Ordering::Relaxed);
        msgs.push(json!({"role":"user","content":prompt}));
        for _ in 0..config::num("max_steps").max(1.0) as usize {
            if trim(&mut msgs) {
                let _ = ev.send(Ev::Info("-- context nearly full: dropped the oldest messages".into()));
            }
            let (msg, calls, tok_s) = match step(&msgs, &ev, &cancel) {
                Ok(r) => r,
                Err(e) => {
                    let _ = ev.send(Ev::Err(e));
                    break;
                }
            };
            msgs.push(msg);
            if calls.is_empty() || cancel.load(Ordering::Relaxed) {
                let _ = ev.send(Ev::Done(tok_s));
                break;
            }
            for (id, name, args) in calls {
                let _ = ev.send(Ev::Tool(name.clone(), args.clone()));
                let out = if controller_allows(&prompt, &name, &args, &cmds, &ev) {
                    call(&name, &args)
                } else {
                    "denied by the owner: do not retry this action, ask what they want instead".to_string()
                };
                let _ = ev.send(Ev::ToolOut(out.clone()));
                msgs.push(json!({"role":"tool","tool_call_id":id,"content":out}));
            }
        }
    }
}

/// The controller judges every mutating action; off-request or risky ones need the owner's yes.
/// If the controller is down, every mutating action needs the owner's yes.
fn controller_allows(request: &str, tool: &str, args: &str, cmds: &Receiver<Cmd>, ev: &Sender<Ev>) -> bool {
    if !crate::judge::is_mutating(tool) || config::get("controller") == "off" {
        return true;
    }
    let a: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    // the controller reports its questions and answers to the flow itself (judge::Report)
    let why = match crate::judge::check_action(request, tool, &a) {
        Ok(None) => return true,
        Ok(Some(why)) => why,
        Err(_) => "the controller is unavailable".into(),
    };
    let _ = ev.send(Ev::Confirm(format!("{why}: {tool} {args}  -- allow? [y/n]")));
    // ponytail: blocks the agent thread until the owner answers; a timeout could default to "no"
    loop {
        match cmds.recv() {
            Ok(Cmd::Confirm(yes)) => return yes,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// Feeds system prompt + tools to the brain ahead of time, so on CPU the first question
/// doesn't wait for ~2k tokens of instructions to be processed.
fn prewarm(msgs: &mut Vec<Value>, ev: &Sender<Ev>, cancel: &AtomicBool) {
    if wait_ready(ev, cancel).is_err() {
        let _ = ev.send(Ev::Ready(false));
        return;
    }
    msgs.push(json!({"role":"system","content":system_prompt()}));
    let mut b = body(&[msgs[0].clone(), json!({"role":"user","content":"."})]);
    b["stream"] = json!(false);
    b["max_tokens"] = json!(1);
    let _ = ev.send(Ev::Info("-- loading instructions into the brain...".into()));
    let r = ureq::post(&format!("{}/v1/chat/completions", llm_url())).timeout(Duration::from_secs(900)).send_string(&b.to_string());
    let _ = ev.send(Ev::Info(if r.is_ok() { "-- ready." } else { "-- warm-up failed, the first reply will be slower" }.into()));
    let _ = ev.send(Ev::Ready(r.is_ok()));
}

/// Request body with the current settings.
fn body(msgs: &[Value]) -> Value {
    let c = config::load();
    let n = |k: &str| c[k].parse::<f64>().unwrap_or(0.0);
    let thinking = c["thinking"] == "on";
    let mut body = json!({"messages": msgs, "tools": tools(), "stream": true, "return_progress": true,
        "temperature": n("temperature"), "top_p": n("top_p"), "min_p": n("min_p"), "repeat_penalty": n("repeat_penalty"),
        "chat_template_kwargs": {"enable_thinking": thinking}});
    if thinking && n("thinking_budget") >= 0.0 {
        body["reasoning_budget_tokens"] = json!(n("thinking_budget") as i64);
    }
    body
}

/// Drops the oldest turns once the history nears the context window.
/// ponytail: tokens estimated as chars/3; ask the server to tokenize if this misjudges
fn trim(msgs: &mut Vec<Value>) -> bool {
    let ctx = config::num("ctx");
    let est = |m: &[Value]| m.iter().map(|v| v.to_string().len()).sum::<usize>() as f64 / 3.0;
    if est(msgs) < ctx * 0.7 {
        return false;
    }
    while est(msgs) > ctx * 0.5 {
        // the oldest turn = from msgs[1] up to (not including) the next user message
        let Some(next) = msgs.iter().skip(2).position(|m| m["role"] == "user") else {
            break;
        };
        msgs.drain(1..next + 2);
    }
    true
}

/// Waits for the brain to answer /health (it may be restarting after a config change).
fn wait_ready(ev: &Sender<Ev>, cancel: &AtomicBool) -> Result<(), String> {
    let start = Instant::now();
    let mut told = false;
    loop {
        if ureq::get(&format!("{}/health", llm_url())).timeout(Duration::from_secs(2)).call().is_ok() {
            return Ok(());
        }
        if cancel.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(180) {
            return Err("the llm module is not answering (see /run/log/llm.log)".into());
        }
        if !told {
            let _ = ev.send(Ev::Info("-- waiting for the brain to be ready...".into()));
            told = true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

type Call = (String, String, String); // id, name, arguments json

/// One streamed completion. Returns the assistant message to keep in history, its tool calls, tok/s.
fn step(msgs: &[Value], ev: &Sender<Ev>, cancel: &AtomicBool) -> Result<(Value, Vec<Call>, f64), String> {
    let body = body(msgs).to_string();
    // A brain restart (config_set) can still answer /health while it shuts down: retry connection failures.
    let mut tries = 0;
    let resp = loop {
        wait_ready(ev, cancel)?;
        match ureq::post(&format!("{}/v1/chat/completions", llm_url()))
            .timeout(Duration::from_secs(900))
            .set("Content-Type", "application/json")
            .send_string(&body)
        {
            Ok(r) => break r,
            Err(ureq::Error::Status(c, r)) => {
                return Err(format!("llm {c}: {}", r.into_string().unwrap_or_default()));
            }
            Err(e) if tries >= 5 => return Err(format!("llm unreachable: {e}")),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };
    let (mut text, mut calls, mut tok_s) = (String::new(), Vec::<Call>::new(), 0.0);
    for line in BufReader::new(resp.into_reader()).lines() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let pp = &v["prompt_progress"];
        if let (Some(done), Some(total)) = (pp["processed"].as_u64(), pp["total"].as_u64()) {
            let _ = ev.send(Ev::Progress(done, total));
        }
        let t = &v["timings"];
        if let Some(s) = t["predicted_per_second"].as_f64() {
            tok_s = s;
            let used = ["cache_n", "prompt_n", "predicted_n"].iter().filter_map(|k| t[k].as_u64()).sum();
            let _ = ev.send(Ev::Ctx(used));
        }
        let d = &v["choices"][0]["delta"];
        if let Some(s) = d["reasoning_content"].as_str() {
            let _ = ev.send(Ev::Think(s.into()));
        }
        if let Some(s) = d["content"].as_str() {
            text.push_str(s);
            let _ = ev.send(Ev::Text(s.into()));
        }
        for tc in d["tool_calls"].as_array().into_iter().flatten() {
            let i = tc["index"].as_u64().unwrap_or(0) as usize;
            if calls.len() <= i {
                calls.resize(i + 1, Default::default());
            }
            let c = &mut calls[i];
            c.0 += tc["id"].as_str().unwrap_or("");
            c.1 += tc["function"]["name"].as_str().unwrap_or("");
            c.2 += tc["function"]["arguments"].as_str().unwrap_or("");
        }
    }
    let tc: Vec<Value> = calls.iter().map(|(id, n, a)| json!({"id":id,"type":"function","function":{"name":n,"arguments":a}})).collect();
    let mut msg = json!({"role":"assistant","content":text});
    if !tc.is_empty() {
        msg["tool_calls"] = Value::Array(tc);
    }
    Ok((msg, calls, tok_s))
}

fn clip(mut s: String) -> String {
    if s.len() > MAX_OUT {
        let mut cut = s.len() - MAX_OUT;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("…(truncated, showing tail)\n{}", &s[cut..]);
    }
    s
}

/// Change a setting; brain settings restart `llm`. Shared by the agent and the /set console command.
/// `by`: "owner" (console) or "agent" (tool); the change is recorded in the system memory.
pub fn set_config(key: &str, value: &str, by: &str) -> Result<String, String> {
    let old = config::get(key);
    // apply first: a value the system cannot apply is never persisted
    let applied = match key {
        "keymap" => Some(crate::keyboard::apply(value.trim())?),
        _ => None,
    };
    let scope = config::set(key, value)?;
    if old != value.trim() {
        crate::memory::record_setting(key, &old, value.trim(), by);
    }
    match scope {
        Scope::Server => crate::restart_module("llm").map(|_| format!("{key} = {value}; brain restarting to apply it")),
        Scope::Judge => crate::restart_module("judge").map(|_| format!("{key} = {value}; controller restarting to apply it")),
        Scope::Request => Ok(format!("{key} = {value}; applies from the next reply")),
        Scope::Instant => Ok(applied.unwrap_or_else(|| format!("{key} = {value}"))),
    }
}

pub fn call(name: &str, args: &str) -> String {
    let a: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let path = a["path"].as_str().unwrap_or("");
    let s = |k: &str| a[k].as_str().map(String::from).unwrap_or_else(|| a[k].to_string());
    let r: Result<String, String> = match name {
        "status" => Ok(status()),
        "read_file" => fs::read(path).map(|b| clip(String::from_utf8_lossy(&b).into_owned())).map_err(|e| e.to_string()),
        "write_file" if crate::memory::protected(path) => Err("refused: /data/memory is written only through memory_save".into()),
        "write_file" => {
            let content = a["content"].as_str().unwrap_or("");
            if let Some(dir) = std::path::Path::new(path).parent() {
                let _ = fs::create_dir_all(dir);
            }
            fs::write(path, content).map_err(|e| e.to_string()).map(|_| {
                crate::memory::record_change("agent", &format!("wrote {path} ({} bytes)", content.len()));
                format!("wrote {} bytes", content.len())
            })
        }
        // small models list files they mean to read: just read them
        "list_dir" if std::path::Path::new(path).is_file() => return call("read_file", args),
        "list_dir" => fs::read_dir(path).map_err(|e| e.to_string()).map(|rd| {
            let mut v: Vec<String> = rd
                .flatten()
                .map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    match e.metadata() {
                        Ok(m) if m.is_dir() => format!("{n}/"),
                        Ok(m) => format!("{n} {}", m.len()),
                        Err(_) => n,
                    }
                })
                .collect();
            v.sort();
            clip(v.join("\n"))
        }),
        "run" => {
            let argv: Vec<String> = a["argv"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
            // ponytail: every run is logged, read-only ones too; filter if the changelog gets noisy
            let out = run_cmd(&argv);
            if let Ok(o) = &out {
                crate::memory::record_change("agent", &format!("ran `{}` ({})", argv.join(" "), o.lines().next().unwrap_or("")));
            }
            out
        }
        "config_set" => set_config(&s("key"), &s("value"), "agent"),
        "restart_module" => crate::restart_module(&s("name")),
        "memory_save" => crate::memory::save(&s("category"), &s("title"), &s("content")),
        "memory_search" => Ok(crate::memory::search(&s("query"))),
        "memory_read" => crate::memory::read(path),
        "memory_forget" => crate::memory::forget(path),
        "power" => {
            let cmd = if a["action"] == "poweroff" { libc::RB_POWER_OFF } else { libc::RB_AUTOBOOT };
            crate::memory::record_change("agent", if cmd == libc::RB_POWER_OFF { "powered off" } else { "rebooted" });
            unsafe {
                libc::sync();
                libc::reboot(cmd);
            }
            Err(std::io::Error::last_os_error().to_string())
        }
        _ => Err(format!("unknown tool {name}")),
    };
    r.unwrap_or_else(|e| format!("error: {e}"))
}

fn status() -> String {
    let rd = |p: &str| fs::read_to_string(p).unwrap_or_default();
    let mem: String = rd("/proc/meminfo").lines().filter(|l| l.starts_with("Mem")).map(|l| format!("{l}\n")).collect();
    let ips: String = fs::read_dir("/run/aios")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("ip."))
        .map(|e| format!("{} {}", e.file_name().to_string_lossy(), rd(&e.path().to_string_lossy())))
        .collect();
    let models: String = fs::read_dir("/data/models")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| format!("{} {} MB\n", e.file_name().to_string_lossy(), e.metadata().map_or(0, |m| m.len() >> 20)))
        .collect();
    format!(
        "modules (name pid restarts; pid '-' = down){}:\n{}\n{mem}\nloadavg: {}cpus: {}\nnetwork:\n{ips}\ndisks (/proc/partitions):\n{}\nmodels in /data/models:\n{models}\nsettings:\n{}",
        if crate::safe_mode() { " [SAFE MODE: /data/modules and config ignored]" } else { "" },
        rd("/run/aios/modules"),
        rd("/proc/loadavg"),
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        rd("/proc/partitions"),
        config::describe(),
    )
}

fn run_cmd(argv: &[String]) -> Result<String, String> {
    let (prog, rest) = argv.split_first().ok_or("empty argv")?;
    let out = "/tmp/.agent-run.out";
    let f = fs::File::create(out).map_err(|e| e.to_string())?;
    let mut child =
        Command::new(prog).args(rest).stdin(Stdio::null()).stdout(f.try_clone().map_err(|e| e.to_string())?).stderr(f).spawn().map_err(|e| e.to_string())?;
    let start = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s.code().map_or("signal".into(), |c| c.to_string());
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            break "timeout".into();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = String::from_utf8_lossy(&fs::read(out).unwrap_or_default()).into_owned();
    Ok(clip(format!("exit {code}\n{text}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_drops_oldest_turns_keeps_system() {
        // ctx comes from defaults (32768) since /data/aios/config doesn't exist here
        let big = "x".repeat(20_000);
        let mut m = vec![json!({"role":"system","content":"sys"})];
        for i in 0..6 {
            m.push(json!({"role":"user","content":format!("q{i}")}));
            m.push(json!({"role":"assistant","content":big}));
        }
        assert!(trim(&mut m));
        assert_eq!(m[0]["content"], "sys");
        assert_eq!(m[1]["role"], "user");
        assert_eq!(m.last().unwrap()["content"], big);
        assert!(m.len() < 13);
    }
}
