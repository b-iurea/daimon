//! The agent as a service: module `agent` runs the agent loop and serves it on a unix socket, one JSON object
//! per line. Windows (the console today, the LAN later) connect, send commands and all see the same events.
//!   in:  {"prompt":"..."}  {"confirm":true|false}  {"reset":true}  {"cancel":true}
//!   out: {"ev":"text","s":"..."} ... (see `ev_json`); a new window first gets {"ev":"sync"} and the conversation so far.

use crate::agent::{self, Cmd, Ev};
use crate::judge::{Report, Row};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const SOCK: &str = "/run/daimon/agent.sock";

// ---------------------------------------------------------------- wire format

pub fn ev_json(e: &Ev) -> Value {
    match e {
        Ev::User(s) => json!({"ev":"user","s":s}),
        Ev::Think(s) => json!({"ev":"think","s":s}),
        Ev::Text(s) => json!({"ev":"text","s":s}),
        Ev::Tool(name, args) => json!({"ev":"tool","name":name,"args":args}),
        Ev::ToolOut(s) => json!({"ev":"tool_out","s":s}),
        Ev::Info(s) => json!({"ev":"info","s":s}),
        Ev::Confirm(s) => json!({"ev":"confirm","s":s}),
        Ev::Answered(yes) => json!({"ev":"answered","yes":yes}),
        Ev::Ctx(n) => json!({"ev":"ctx","n":n}),
        Ev::Progress(done, total) => json!({"ev":"progress","done":done,"total":total}),
        Ev::Judging(kind, subject) => json!({"ev":"judging","kind":kind,"subject":subject}),
        Ev::Judge(r) => json!({"ev":"judge","kind":r.kind,"subject":r.subject,"score":r.score,"allowed":r.allowed,"verdict":r.verdict,"secs":r.secs,
            "rows": r.rows.iter().map(|w| json!({"question":w.question,"p":w.p,"rule":w.rule,"ok":w.ok})).collect::<Vec<_>>()}),
        Ev::Done(tok_s) => json!({"ev":"done","tok_s":tok_s}),
        Ev::Err(s) => json!({"ev":"err","s":s}),
        Ev::Ready(ok) => json!({"ev":"ready","ok":ok}),
        Ev::Sync => json!({"ev":"sync"}),
    }
}

pub fn parse_ev(v: &Value) -> Option<Ev> {
    let s = |k: &str| v[k].as_str().map(String::from);
    let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
    Some(match v["ev"].as_str()? {
        "user" => Ev::User(s("s")?),
        "think" => Ev::Think(s("s")?),
        "text" => Ev::Text(s("s")?),
        "tool" => Ev::Tool(s("name")?, s("args")?),
        "tool_out" => Ev::ToolOut(s("s")?),
        "info" => Ev::Info(s("s")?),
        "confirm" => Ev::Confirm(s("s")?),
        "answered" => Ev::Answered(v["yes"].as_bool()?),
        "ctx" => Ev::Ctx(v["n"].as_u64()?),
        "progress" => Ev::Progress(v["done"].as_u64()?, v["total"].as_u64()?),
        "judging" => Ev::Judging(s("kind")?, s("subject")?),
        "judge" => Ev::Judge(Report {
            kind: s("kind")?,
            subject: s("subject")?,
            rows: v["rows"]
                .as_array()?
                .iter()
                .map(|w| Row {
                    question: w["question"].as_str().unwrap_or("").into(),
                    p: w["p"].as_f64().unwrap_or(0.0),
                    rule: w["rule"].as_str().unwrap_or("").into(),
                    ok: w["ok"] == true,
                })
                .collect(),
            score: f("score"),
            allowed: v["allowed"].as_bool()?,
            verdict: s("verdict")?,
            secs: f("secs"),
        }),
        "done" => Ev::Done(f("tok_s")),
        "err" => Ev::Err(s("s")?),
        "ready" => Ev::Ready(v["ok"].as_bool()?),
        "sync" => Ev::Sync,
        _ => return None,
    })
}

fn cmd_json(c: &Cmd) -> Value {
    match c {
        Cmd::Prompt(p) => json!({"prompt":p}),
        Cmd::Confirm(yes) => json!({"confirm":yes}),
        Cmd::Reset => json!({"reset":true}),
        Cmd::Cancel => json!({"cancel":true}),
    }
}

fn parse_cmd(line: &str) -> Option<Cmd> {
    let v: Value = serde_json::from_str(line).ok()?;
    if let Some(p) = v["prompt"].as_str() {
        Some(Cmd::Prompt(p.into()))
    } else if let Some(yes) = v["confirm"].as_bool() {
        Some(Cmd::Confirm(yes))
    } else if v["reset"] == true {
        Some(Cmd::Reset)
    } else if v["cancel"] == true {
        Some(Cmd::Cancel)
    } else {
        None
    }
}

// ---------------------------------------------------------------- server (module `agent`)

/// The conversation so far (replayed to every window that connects) and the windows connected now.
#[derive(Default)]
struct Hub {
    history: Vec<String>,
    windows: Vec<UnixStream>,
}

// ponytail: history is every event since the last /new, capped; a very long conversation loses its start on replay
const HISTORY_MAX: usize = 50_000;

/// `daimon agent [socket]`: the module's entry point.
pub fn serve(path: &str) -> ! {
    let _ = std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("/")));
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("daimon agent: {path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("daimon agent: serving {path}");
    let cancel = Arc::new(AtomicBool::new(false));
    let (ev_tx, ev_rx) = channel();
    let cmds = agent::spawn(ev_tx, cancel.clone());
    hub(listener, ev_rx, cmds, cancel);
    eprintln!("daimon agent: listener closed");
    std::process::exit(1);
}

/// Broadcasts the agent's events to every window and feeds the windows' commands to the agent.
fn hub(listener: UnixListener, events: Receiver<Ev>, cmds: Sender<Cmd>, cancel: Arc<AtomicBool>) {
    let hub = Arc::new(Mutex::new(Hub::default()));
    let h = hub.clone();
    std::thread::spawn(move || {
        for e in events {
            let line = format!("{}\n", ev_json(&e));
            let mut h = h.lock().unwrap_or_else(|p| p.into_inner());
            match e {
                Ev::Sync => h.history.clear(),
                // progress is only meaningful live
                Ev::Progress(..) => {}
                _ => h.history.push(line.clone()),
            }
            if h.history.len() > HISTORY_MAX {
                h.history.drain(..HISTORY_MAX / 5);
            }
            // a window that can't keep up (1 s write timeout) is dropped; it reconnects and gets the replay
            h.windows.retain_mut(|w| w.write_all(line.as_bytes()).is_ok());
        }
    });
    for s in listener.incoming().flatten() {
        let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
        if let Ok(mut w) = s.try_clone() {
            // under the lock: no event is lost or sent twice between the replay and the live flow
            let mut h = hub.lock().unwrap_or_else(|p| p.into_inner());
            let replay = format!("{}\n{}", ev_json(&Ev::Sync), h.history.concat());
            if w.write_all(replay.as_bytes()).is_ok() {
                h.windows.push(w);
            }
        }
        let (cmds, cancel) = (cmds.clone(), cancel.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                match parse_cmd(&line) {
                    Some(Cmd::Cancel) => cancel.store(true, Ordering::Relaxed),
                    Some(c) => {
                        let _ = cmds.send(c);
                    }
                    None => eprintln!("daimon agent: bad command: {line}"),
                }
            }
        });
    }
}

// ---------------------------------------------------------------- client (a window)

/// A window's connection to the agent: reconnects on its own; events arrive on the channel given to `connect`.
pub struct Link {
    out: Arc<Mutex<Option<UnixStream>>>,
}

impl Link {
    pub fn connect(path: &str, events: Sender<Ev>) -> Link {
        let out = Arc::new(Mutex::new(None));
        let (o, path) = (out.clone(), path.to_string());
        std::thread::spawn(move || {
            loop {
                if let Ok(s) = UnixStream::connect(&path) {
                    *o.lock().unwrap_or_else(|p| p.into_inner()) = s.try_clone().ok();
                    for line in BufReader::new(s).lines().map_while(Result::ok) {
                        let Some(e) = serde_json::from_str(&line).ok().as_ref().and_then(parse_ev) else {
                            continue;
                        };
                        if events.send(e).is_err() {
                            return;
                        }
                    }
                    *o.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    if events.send(Ev::Err("lost the agent (module agent); reconnecting".into())).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        Link { out }
    }

    pub fn up(&self) -> bool {
        self.out.lock().is_ok_and(|o| o.is_some())
    }

    pub fn send(&self, c: Cmd) -> Result<(), String> {
        let mut o = self.out.lock().map_err(|e| e.to_string())?;
        let s = o.as_mut().ok_or("the agent is not running (module agent, see /run/log/agent.log)")?;
        s.write_all(format!("{}\n", cmd_json(&c)).as_bytes()).map_err(|e| format!("agent: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn all_events() -> Vec<Ev> {
        let row = Row { question: "Does it do what the owner asked?".into(), p: 0.93, rule: "needs >= 60%".into(), ok: true };
        let r =
            Report { kind: "action check".into(), subject: "power {}".into(), rows: vec![row], score: 0.86, allowed: false, verdict: "asks".into(), secs: 7.8 };
        vec![
            Ev::User("hi".into()),
            Ev::Think("t".into()),
            Ev::Text("x\ny".into()),
            Ev::Tool("run".into(), "{\"argv\":[]}".into()),
            Ev::ToolOut("o".into()),
            Ev::Info("i".into()),
            Ev::Confirm("c".into()),
            Ev::Answered(true),
            Ev::Ctx(42),
            Ev::Progress(1, 2),
            Ev::Judging("memory gate".into(), "note".into()),
            Ev::Judge(r),
            Ev::Done(14.5),
            Ev::Err("e".into()),
            Ev::Ready(false),
            Ev::Sync,
        ]
    }

    #[test]
    fn every_event_survives_the_wire() {
        for e in all_events() {
            let v = ev_json(&e);
            let back = parse_ev(&serde_json::from_str(&v.to_string()).unwrap()).unwrap_or_else(|| panic!("{v}"));
            assert_eq!(ev_json(&back), v);
        }
        for c in [Cmd::Prompt("p".into()), Cmd::Confirm(false), Cmd::Reset, Cmd::Cancel] {
            assert_eq!(cmd_json(&parse_cmd(&cmd_json(&c).to_string()).unwrap()), cmd_json(&c));
        }
        assert!(parse_cmd("{\"what\":1}").is_none() && parse_cmd("nonsense").is_none());
    }

    fn recv(rx: &Receiver<Ev>) -> Value {
        ev_json(&rx.recv_timeout(Duration::from_secs(5)).expect("event"))
    }

    fn wait(what: &str, ok: impl Fn() -> bool) {
        let t = Instant::now();
        while !ok() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Two windows on one agent: both see the flow, either can drive it, a late one gets the replay, /new resets it.
    #[test]
    fn windows_share_one_agent() {
        let path = std::env::temp_dir().join(format!("daimon-link-{}.sock", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        let (agent_ev, events) = channel();
        let (cmd_tx, cmds) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || hub(listener, events, cmd_tx, c));

        let (tx1, rx1) = channel();
        let one = Link::connect(path, tx1);
        assert_eq!(recv(&rx1)["ev"], "sync");
        wait("first window", || one.up());
        agent_ev.send(Ev::User("hi".into())).unwrap();
        agent_ev.send(Ev::Progress(1, 2)).unwrap();
        agent_ev.send(Ev::Text("hello".into())).unwrap();
        assert_eq!(recv(&rx1)["ev"], "user");
        assert_eq!(recv(&rx1)["ev"], "progress");
        assert_eq!(recv(&rx1)["s"], "hello");

        // a console restarting mid-turn: the replay has the turn so far, without the live-only progress
        let (tx2, rx2) = channel();
        let two = Link::connect(path, tx2);
        assert_eq!(recv(&rx2)["ev"], "sync");
        assert_eq!(recv(&rx2)["s"], "hi");
        assert_eq!(recv(&rx2)["s"], "hello");
        wait("second window", || two.up());
        agent_ev.send(Ev::Confirm("risky: power {}".into())).unwrap();
        assert_eq!(recv(&rx1)["ev"], "confirm");
        assert_eq!(recv(&rx2)["ev"], "confirm");

        // either window answers; cancel goes straight to the flag
        two.send(Cmd::Confirm(true)).unwrap();
        assert!(matches!(cmds.recv_timeout(Duration::from_secs(5)), Ok(Cmd::Confirm(true))));
        one.send(Cmd::Prompt("next".into())).unwrap();
        assert!(matches!(cmds.recv_timeout(Duration::from_secs(5)), Ok(Cmd::Prompt(p)) if p == "next"));
        one.send(Cmd::Cancel).unwrap();
        wait("cancel", || cancel.load(Ordering::Relaxed));

        // /new: history restarts from the sync
        agent_ev.send(Ev::Sync).unwrap();
        agent_ev.send(Ev::Ready(true)).unwrap();
        assert_eq!(recv(&rx1)["ev"], "sync");
        assert_eq!(recv(&rx1)["ev"], "ready");
        let (tx3, rx3) = channel();
        let _three = Link::connect(path, tx3);
        assert_eq!(recv(&rx3)["ev"], "sync");
        assert_eq!(recv(&rx3)["ev"], "ready");
        let _ = std::fs::remove_file(path);
    }
}
