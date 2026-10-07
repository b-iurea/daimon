//! /data/aios/config: `key = value` lines. One table defines every knob, its default and how to validate it.
//! Server keys restart the `llm` module; request keys apply to the next message.

use std::collections::HashMap;
use std::fs;

pub const PATH: &str = "/data/aios/config";

#[derive(PartialEq)]
pub enum Scope {
    Server,
    Judge,
    Request,
    /// applied by the system the moment it is set (and at every boot)
    Instant,
}

pub struct Key {
    pub name: &'static str,
    pub default: &'static str,
    pub scope: Scope,
    /// allowed values, or [] for numbers, or ["*"] for free text
    pub allowed: &'static [&'static str],
    pub help: &'static str,
}

use Scope::*;
pub const KEYS: &[Key] = &[
    Key { name: "model", default: "/data/models/current.gguf", scope: Server, allowed: &["*"], help: "GGUF file the brain loads" },
    Key { name: "ctx", default: "32768", scope: Server, allowed: &[], help: "context window in tokens (model max 131072); more = more RAM" },
    Key { name: "kv_cache", default: "f16", scope: Server, allowed: &["f16", "q8_0"], help: "KV cache precision; q8_0 halves its RAM" },
    Key { name: "threads", default: "0", scope: Server, allowed: &[], help: "CPU threads for inference, 0 = auto" },
    Key { name: "port", default: "8080", scope: Server, allowed: &[], help: "OpenAI-compatible API port (LAN)" },
    Key { name: "extra_args", default: "", scope: Server, allowed: &["*"], help: "raw extra llama-server arguments" },
    Key { name: "judge_model", default: "/data/models/judge.gguf", scope: Judge, allowed: &["*"], help: "decision model of the controller (System 1)" },
    Key { name: "judge_port", default: "8081", scope: Judge, allowed: &[], help: "controller port (localhost only)" },
    Key { name: "keymap", default: "us", scope: Instant, allowed: &["*"], help: "keyboard layout: it, us, gb, de, fr, es, ...; a wrong name lists all" },
    Key { name: "hostname", default: "daimon", scope: Instant, allowed: &["*"], help: "machine name on the network" },
    Key { name: "ui_font", default: "auto", scope: Instant, allowed: &["auto", "17", "19", "24", "30"], help: "screen font height (px), auto = by resolution" },
    Key {
        name: "controller",
        default: "on",
        scope: Request,
        allowed: &["on", "off"],
        help: "check risky/off-request actions and ask the owner (the memory rule is always on)",
    },
    Key { name: "thinking", default: "on", scope: Request, allowed: &["on", "off"], help: "reason before answering" },
    Key { name: "thinking_budget", default: "512", scope: Request, allowed: &[], help: "max reasoning tokens per reply, -1 = unlimited" },
    Key { name: "temperature", default: "1.0", scope: Request, allowed: &[], help: "sampling temperature" },
    Key { name: "top_p", default: "0.95", scope: Request, allowed: &[], help: "nucleus sampling" },
    Key { name: "min_p", default: "0.0", scope: Request, allowed: &[], help: "min-p sampling" },
    Key { name: "repeat_penalty", default: "1.05", scope: Request, allowed: &[], help: "penalty against loops" },
    Key { name: "max_steps", default: "16", scope: Request, allowed: &[], help: "max tool calls per request" },
];

pub fn key(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name == name)
}

/// Current values: defaults overlaid with the file. In safe mode the file is ignored.
pub fn load() -> HashMap<String, String> {
    let mut m: HashMap<String, String> = KEYS.iter().map(|k| (k.name.into(), k.default.into())).collect();
    if crate::safe_mode() {
        return m;
    }
    for line in fs::read_to_string(PATH).unwrap_or_default().lines() {
        let line = line.split('#').next().unwrap_or("");
        if let Some((k, v)) = line.split_once('=') {
            if key(k.trim()).is_some() {
                m.insert(k.trim().into(), v.trim().into());
            }
        }
    }
    m
}

pub fn get(name: &str) -> String {
    load().remove(name).unwrap_or_default()
}

pub fn num(name: &str) -> f64 {
    get(name).parse().unwrap_or_else(|_| key(name).and_then(|k| k.default.parse().ok()).unwrap_or(0.0))
}

/// Validates and persists one key. Returns its scope so callers know whether to restart the brain.
pub fn set(name: &str, value: &str) -> Result<&'static Scope, String> {
    let k = key(name).ok_or_else(|| format!("unknown key '{name}'. Keys: {}", KEYS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")))?;
    let v = value.trim();
    match k.allowed {
        [] if v.parse::<f64>().is_err() => return Err(format!("{name} must be a number")),
        ["*"] | [] => {}
        a if !a.contains(&v) => return Err(format!("{name} must be one of: {}", a.join(", "))),
        _ => {}
    }
    let mut m = load();
    m.insert(name.into(), v.into());
    save(&m)?;
    Ok(&k.scope)
}

pub fn reset() -> Result<(), String> {
    save(&KEYS.iter().map(|k| (k.name.into(), k.default.into())).collect())
}

fn save(m: &HashMap<String, String>) -> Result<(), String> {
    let mut out = String::from("# Daimon settings. Edit here, with /set <key> <value> on the console, or ask the agent.\n");
    for (title, scope) in [
        ("brain (changing these restarts the llm module)", Server),
        ("controller (changing these restarts the judge module)", Judge),
        ("system (applied at once and at boot)", Instant),
        ("per request (apply to the next message)", Request),
    ] {
        out += &format!("\n# --- {title}\n");
        for k in KEYS.iter().filter(|k| k.scope == scope) {
            out += &format!("{} = {}\n# {}\n", k.name, m.get(k.name).map_or(k.default, String::as_str), k.help);
        }
    }
    let _ = fs::create_dir_all("/data/aios");
    fs::write(PATH, out).map_err(|e| e.to_string())
}

/// `name = value  (help)` lines, for the agent and for /config.
pub fn describe() -> String {
    let m = load();
    KEYS.iter()
        .map(|k| {
            let scope = match k.scope {
                Server => "restart",
                Judge => "restart judge",
                Request => "live",
                Instant => "instant",
            };
            format!("{} = {}   [{scope}] {}\n", k.name, m[k.name], k.help)
        })
        .collect()
}

/// Exec llama-server with the configured flags (the `llm` module's cmd is `aios llm`).
pub fn exec_llm() -> ! {
    use std::os::unix::process::CommandExt;
    let c = load();
    let mut args: Vec<String> = vec![
        "-m".into(),
        c["model"].clone(),
        "--host".into(),
        "0.0.0.0".into(),
        "--port".into(),
        c["port"].clone(),
        "-c".into(),
        c["ctx"].clone(),
        "--jinja".into(),
    ];
    if c["kv_cache"] != "f16" {
        // quantized V cache needs flash attention
        args.extend(["-fa", "on", "-ctk", &c["kv_cache"], "-ctv", &c["kv_cache"]].map(String::from));
    }
    if c["threads"] != "0" {
        args.extend(["-t".into(), c["threads"].clone()]);
    }
    args.extend(c["extra_args"].split_whitespace().map(String::from));
    eprintln!("aios llm: llama-server {}", args.join(" "));
    let e = std::process::Command::new("/usr/bin/llama-server").args(&args).exec();
    eprintln!("aios llm: exec failed: {e}");
    std::process::exit(1);
}
