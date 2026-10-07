//! Long-term memory: plain markdown notes in an llm-wiki style layout, searched with BM25.
//!
//!   /data/memory/_index.md               generated here, never by the model
//!   /data/memory/<category>/wiki/<slug>.md   frontmatter + body
//!
//! STRICT RULE, enforced in code: only notes about the system, the agent itself, or the owner and
//! their preferences can be stored. Every save is judged by the controller (judge.rs), which only
//! sees the note, never the conversation, and fails closed. write_file cannot touch this tree.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/data/memory";
pub const CATEGORIES: [&str; 3] = ["system", "self", "owner"];

pub fn save(category: &str, title: &str, content: &str) -> Result<String, String> {
    if !CATEGORIES.contains(&category) {
        return Err(format!("category must be one of {CATEGORIES:?}"));
    }
    let (title, content) = (title.trim(), content.trim());
    if title.is_empty() || content.is_empty() {
        return Err("title and content are required".into());
    }
    let (allowed, reason) = crate::judge::memory_allowed(&format!("{title}. {content}")).map_err(|e| format!("REFUSED: {e}"))?;
    if !allowed {
        return Err(format!(
            "REFUSED by the memory rule: {reason}. If the note really is about this system, yourself or the owner's preferences, \
             rewrite it ONCE following the patterns in your instructions (\"The owner wants the agent to ...\", \"This system: ...\", \
             \"The agent's own behaviour: ...\"). Otherwise drop it and do not mention it again."
        ));
    }
    let path = save_in(Path::new(ROOT), category, title, content).map_err(|e| e.to_string())?;
    Ok(format!("saved {}", path.display()))
}

/// A note written by code from the owner's own answers (installer): skips the controller.
pub fn note(category: &str, title: &str, content: &str) {
    if let Err(e) = save_in(Path::new(ROOT), category, title, content) {
        crate::log(&format!("memory: {title}: {e}"));
    }
}

// ---------------------------------------------------------------- system changes
//
// Recorded by code, never by the model: they are about this system by construction, so they skip the
// controller, and the agent can't forget to write them. One note per setting (current value + history)
// and one changelog, newest first.

const CHANGELOG: &str = "Changes to this system";
const CHANGELOG_MAX: usize = 100;
const HISTORY_MAX: usize = 20;

/// `by` is "owner" (console) or "agent" (tools).
pub fn record_setting(key: &str, old: &str, new: &str, by: &str) {
    record_setting_in(Path::new(ROOT), key, old, new, by);
}

pub fn record_change(by: &str, what: &str) {
    record_change_in(Path::new(ROOT), by, what);
}

fn record_setting_in(root: &Path, key: &str, old: &str, new: &str, by: &str) {
    let title = format!("Setting {key}");
    let path = root.join("system").join("wiki").join(format!("{}.md", slug(&title)));
    let prev = parse(&fs::read_to_string(&path).unwrap_or_default()).map(|n| n.body).unwrap_or_default();
    let mut hist = vec![format!("- {} {old} -> {new} (by the {by})", now())];
    hist.extend(prev.lines().filter(|l| l.starts_with("- ")).map(String::from).take(HISTORY_MAX - 1));
    let body = format!("This system: {key} is {new} (since {}, set by the {by}).\n\nHistory, newest first:\n{}", today(), hist.join("\n"));
    if let Err(e) = save_in(root, "system", &title, &body) {
        crate::log(&format!("memory: setting {key}: {e}"));
    }
    record_change_in(root, by, &format!("set {key}: {old} -> {new}"));
}

fn record_change_in(root: &Path, by: &str, what: &str) {
    let path = root.join("system").join("wiki").join(format!("{}.md", slug(CHANGELOG)));
    let prev = parse(&fs::read_to_string(&path).unwrap_or_default()).map(|n| n.body).unwrap_or_default();
    let what: String = what.replace('\n', " ").chars().take(160).collect();
    let mut log = vec![format!("- {} · {by} · {what}", now())];
    log.extend(prev.lines().filter(|l| l.starts_with("- ")).map(String::from).take(CHANGELOG_MAX - 1));
    let body = format!("This system: log of the changes made to it, newest first.\n\n{}", log.join("\n"));
    if let Err(e) = save_in(root, "system", CHANGELOG, &body) {
        crate::log(&format!("memory: changelog: {e}"));
    }
}

fn slug(title: &str) -> String {
    let s: String = title.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    s.chars().take(60).collect()
}

/// Same title = same file: saving again replaces the note (the model should read it first and merge).
fn save_in(root: &Path, category: &str, title: &str, content: &str) -> std::io::Result<PathBuf> {
    let dir = root.join(category).join("wiki");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", slug(title)));
    let created = parse(&fs::read_to_string(&path).unwrap_or_default()).map(|n| n.created).unwrap_or_else(today);
    fs::write(&path, format!("---\ntitle: {title}\ncategory: {category}\ncreated: {created}\nupdated: {}\n---\n{content}\n", today()))?;
    write_index(root)?;
    Ok(path)
}

pub fn forget(path: &str) -> Result<String, String> {
    let p = inside(path)?;
    fs::remove_file(&p).map_err(|e| e.to_string())?;
    write_index(Path::new(ROOT)).map_err(|e| e.to_string())?;
    Ok(format!("forgot {}", p.display()))
}

pub fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(inside(path)?).map_err(|e| e.to_string())
}

/// Resolves a note path (absolute or relative to ROOT) and refuses anything outside the memory tree.
fn inside(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    let p = if p.is_absolute() { p.to_path_buf() } else { Path::new(ROOT).join(p) };
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) || !p.starts_with(ROOT) {
        return Err(format!("path must be inside {ROOT}"));
    }
    Ok(p)
}

/// True for paths the model may not write with generic file tools.
pub fn protected(path: &str) -> bool {
    let p = Path::new(path);
    // resolve symlinks in the existing part of the path too
    let real = p.parent().and_then(|d| fs::canonicalize(d).ok()).is_some_and(|d| d.starts_with(ROOT));
    real || p.starts_with(ROOT) || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

struct Note {
    path: PathBuf,
    title: String,
    category: String,
    created: String,
    updated: String,
    body: String,
}

fn parse(text: &str) -> Option<Note> {
    let rest = text.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let f: HashMap<&str, &str> = front.lines().filter_map(|l| l.split_once(": ")).collect();
    Some(Note {
        path: PathBuf::new(),
        title: f.get("title")?.to_string(),
        category: f.get("category").unwrap_or(&"").to_string(),
        created: f.get("created").unwrap_or(&"").to_string(),
        updated: f.get("updated").unwrap_or(&"").to_string(),
        body: body.trim().to_string(),
    })
}

fn notes(root: &Path) -> Vec<Note> {
    let mut v = vec![];
    for cat in CATEGORIES {
        let Ok(rd) = fs::read_dir(root.join(cat).join("wiki")) else {
            continue;
        };
        for e in rd.flatten() {
            if let Some(mut n) = fs::read_to_string(e.path()).ok().as_deref().and_then(parse) {
                n.path = e.path();
                v.push(n);
            }
        }
    }
    v.sort_by(|a, b| (&a.category, &a.title).cmp(&(&b.category, &b.title)));
    v
}

fn first_line(body: &str) -> String {
    let l = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if l.chars().count() > 100 { format!("{}…", l.chars().take(100).collect::<String>()) } else { l.to_string() }
}

fn write_index(root: &Path) -> std::io::Result<()> {
    let mut out = String::from("# Daimon memory (generated by aios, do not edit)\n");
    let all = notes(root);
    for cat in CATEGORIES {
        out += &format!("\n## {cat}\n");
        for n in all.iter().filter(|n| n.category == cat) {
            let rel = n.path.strip_prefix(root).unwrap_or(&n.path).display();
            out += &format!("- [{}]({rel}) {} ({})\n", n.title, first_line(&n.body), n.updated);
        }
    }
    fs::write(root.join("_index.md"), out)
}

/// What goes into the system prompt: one line per note.
pub fn prompt_index() -> String {
    let all = notes(Path::new(ROOT));
    if all.is_empty() {
        return "(empty)".into();
    }
    // ponytail: whole index in the prompt; switch to "top-N by recency" once it passes ~60 notes
    all.iter()
        .map(|n| format!("- [{}] {}: {} ({})\n", n.category, n.title, first_line(&n.body), n.path.strip_prefix(ROOT).unwrap_or(&n.path).display()))
        .collect()
}

pub fn count() -> usize {
    notes(Path::new(ROOT)).len()
}

// ---------------------------------------------------------------- BM25

/// Lowercase alphanumeric words, cut to 6 chars as a poor man's stemmer
/// (preferred/preferences -> prefer, configured/configuration -> config).
// ponytail: prefix stemming; a real English stemmer if recall disappoints
fn terms(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(|w| w.chars().take(6).collect()).collect()
}

pub fn search(query: &str) -> String {
    let hits = search_in(Path::new(ROOT), query, 5);
    if hits.is_empty() {
        return "no matching notes".into();
    }
    hits.iter().map(|(score, n)| format!("{} [{}] {} (score {score:.2})\n  {}\n", n.path.display(), n.category, n.title, first_line(&n.body))).collect()
}

fn search_in(root: &Path, query: &str, k: usize) -> Vec<(f64, Note)> {
    let docs = notes(root);
    // title counts twice: it is the densest description of a note
    let toks: Vec<Vec<String>> = docs.iter().map(|n| terms(&format!("{0} {0} {1}", n.title, n.body))).collect();
    let n = docs.len() as f64;
    let avg = toks.iter().map(Vec::len).sum::<usize>() as f64 / n.max(1.0);
    let mut df: HashMap<&str, f64> = HashMap::new();
    for t in &toks {
        let mut seen: Vec<&str> = t.iter().map(String::as_str).collect();
        seen.sort_unstable();
        seen.dedup();
        for w in seen {
            *df.entry(w).or_default() += 1.0;
        }
    }
    let q = terms(query);
    let (k1, b) = (1.2, 0.75);
    let mut scored: Vec<(f64, Note)> = docs
        .into_iter()
        .zip(&toks)
        .map(|(note, t)| {
            let len = t.len() as f64;
            let s: f64 = q
                .iter()
                .map(|w| {
                    let tf = t.iter().filter(|x| *x == w).count() as f64;
                    let d = df.get(w.as_str()).copied().unwrap_or(0.0);
                    let idf = ((n - d + 0.5) / (d + 0.5) + 1.0).ln();
                    idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * len / avg.max(1.0)))
                })
                .sum();
            (s, note)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);
    scored
}

fn today() -> String {
    now()[..10].to_string()
}

/// "YYYY-MM-DD HH:MM" UTC from the system clock (civil-from-days, no date crate needed).
fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", t / 3600, t / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_index_search() {
        let root = std::env::temp_dir().join(format!("aios-mem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        save_in(&root, "owner", "Preferred language", "The owner prefers short answers in Italian.").unwrap();
        save_in(&root, "system", "Context raised", "Raised ctx to 49152 at the owner's request.").unwrap();
        let p = save_in(&root, "system", "Network", "eth0 gets its address via DHCP.").unwrap();
        save_in(&root, "system", "Network", "eth0 via DHCP, gateway 10.0.2.2.").unwrap(); // same title replaces
        assert_eq!(notes(&root).len(), 3);
        assert!(fs::read_to_string(&p).unwrap().contains("gateway"));
        let idx = fs::read_to_string(root.join("_index.md")).unwrap();
        assert!(idx.contains("## owner") && idx.contains("[Preferred language]"));
        let hits = search_in(&root, "which language do I prefer for answers?", 5);
        assert_eq!(hits[0].1.title, "Preferred language");
        assert_eq!(search_in(&root, "network configuration dhcp", 5)[0].1.title, "Network");
        assert!(search_in(&root, "zzzz", 5).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn system_changes_are_recorded() {
        let root = std::env::temp_dir().join(format!("aios-chg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        record_setting_in(&root, "keymap", "us", "it", "owner");
        record_setting_in(&root, "keymap", "it", "de", "agent");
        record_change_in(&root, "agent", "wrote /data/modules/x/disabled (0 bytes)");
        let all = notes(&root);
        assert_eq!(all.len(), 2);
        let k = all.iter().find(|n| n.title == "Setting keymap").unwrap();
        assert!(k.body.starts_with("This system: keymap is de"));
        let h: Vec<&str> = k.body.lines().filter(|l| l.starts_with("- ")).collect();
        assert!(h.len() == 2 && h[0].ends_with("it -> de (by the agent)") && h[1].ends_with("us -> it (by the owner)"));
        let c = all.iter().find(|n| n.title == CHANGELOG).unwrap();
        let l: Vec<&str> = c.body.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(l.len(), 3);
        assert!(l[0].contains("· agent · wrote") && l[2].contains("· owner · set keymap: us -> it"));
        assert_eq!(search_in(&root, "keyboard layout keymap", 5)[0].1.title, "Setting keymap");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_fenced() {
        assert!(inside("/data/memory/owner/wiki/x.md").is_ok());
        assert!(inside("owner/wiki/x.md").is_ok());
        assert!(inside("/data/memory/../aios/config").is_err());
        assert!(inside("/etc/passwd").is_err());
        assert!(protected("/data/memory/owner/wiki/x.md"));
        assert!(protected("/data/x/../memory/y"));
        assert!(!protected("/data/aios/config"));
        assert_eq!(slug("Preferred language!"), "preferred-language");
        assert_eq!(today().len(), 10);
    }
}
