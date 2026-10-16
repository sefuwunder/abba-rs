//! Abba's quiet intelligence. Deterministic and local: no network, no model.
//! Mirrors src/mind.ts. The UI renders its outputs as ordinary interface.

use crate::db::now_iso;
use chrono::{Datelike, Local, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

const STOP: &[&str] = &[
    "a","an","the","and","or","but","if","then","else","for","to","of","in","on","at","by","with",
    "from","as","is","are","was","were","be","been","being","it","its","this","that","these","those",
    "i","you","he","she","we","they","them","his","her","our","their","my","your","me","us","do",
    "does","did","will","would","can","could","should","have","has","had","not","no","yes","so",
    "such","than","too","very","just","about","into","over","after","before","up","down","out",
    "off","again","once","here","there","when","where","why","how","what","which","who","whom",
    "all","any","both","each","few","more","most","other","some","only","own","same",
];

pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() > 2 && !STOP.contains(&cur.as_str()) {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch);
        } else if ch == '-' || ch.is_whitespace() {
            flush(&mut cur, &mut out);
        } else {
            flush(&mut cur, &mut out); // other punctuation -> separator
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Quiet auto-title: first heading, else first sentence, else a calm fallback.
pub fn auto_title(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    for l in &lines {
        // #{1,3} heading
        let mut hashes = 0;
        for ch in l.chars() {
            if ch == '#' { hashes += 1; } else { break; }
        }
        if (1..=3).contains(&hashes) && l[hashes..].starts_with(' ') {
            return l[hashes..].trim().chars().take(80).collect();
        }
    }
    let first = lines.first().copied().unwrap_or("");
    // first sentence: up to the first [. ! ?] followed by whitespace
    let mut end = first.len();
    let bytes = first.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i] == b'.' || bytes[i] == b'!' || bytes[i] == b'?')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_whitespace()
        {
            end = i + 1;
            break;
        }
        i += 1;
    }
    let sentence = &first[..end];
    let clean: String = sentence
        .trim_start_matches(|c: char| c == '-' || c == '*' || c == '>' || c.is_ascii_digit() || c == '.' || c.is_whitespace())
        .trim()
        .to_string();
    if clean.is_empty() {
        return "Untitled thought".to_string();
    }
    if clean.chars().count() > 64 {
        let mut s: String = clean.chars().take(61).collect();
        // trimEnd before the ellipsis, like the TS version
        while s.ends_with(char::is_whitespace) { s.pop(); }
        s.push('…');
        s
    } else {
        clean
    }
}

pub fn read_mins(body: &str) -> i64 {
    let words = body.split_whitespace().count();
    ((words as f64) / 200.0).round().max(1.0) as i64
}

#[derive(Clone, Debug)]
pub struct NoteRow {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub tags: String,
    pub status: String,
    pub read_mins: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub fn row_to_note(r: &rusqlite::Row) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        id: r.get(0)?, title: r.get(1)?, body: r.get(2)?, tags: r.get(3)?,
        status: r.get(4)?, read_mins: r.get(5)?, created_at: r.get(6)?, updated_at: r.get(7)?,
    })
}

pub fn get_note(conn: &Connection, id: i64) -> Option<NoteRow> {
    conn.query_row("SELECT * FROM notes WHERE id = ?", params![id], row_to_note).ok()
}

fn keyword_set(n: &NoteRow) -> std::collections::HashSet<String> {
    let tags: Vec<String> = serde_json::from_str(&n.tags).unwrap_or_default();
    tokens(&format!("{} {} {}", n.title, n.body, tags.join(" "))).into_iter().collect()
}

/// Related ideas by shared language. Deterministic; ties break by recency then id.
pub fn related_ideas(note: &NoteRow, pool: &[NoteRow], limit: usize) -> Vec<NoteRow> {
    let mine = keyword_set(note);
    if mine.is_empty() {
        return vec![];
    }
    let mut scored: Vec<(&NoteRow, usize)> = vec![];
    for other in pool {
        if other.id == note.id {
            continue;
        }
        let theirs = keyword_set(other);
        let overlap = mine.iter().filter(|w| theirs.contains(*w)).count();
        if overlap >= 2 {
            scored.push((other, overlap));
        }
    }
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.updated_at.cmp(&a.0.updated_at))
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    scored.into_iter().take(limit).map(|(n, _)| n.clone()).collect()
}

// ---- digest ----

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DigestSection {
    pub heading: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Digest {
    #[serde(rename = "weekKey")]
    pub week_key: String,
    pub title: String,
    pub intro: String,
    pub sections: Vec<DigestSection>,
    pub empty: bool,
}

#[allow(dead_code)]
pub fn week_key_for(year: i32, month: u32, day: u32) -> String {
    let d = chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap();
    let iso = d.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

pub fn current_week_key() -> String {
    let d = Local::now().date_naive();
    let iso = d.iso_week();
    format!("{}-W{:02}", iso.year(), iso.week())
}

fn parse_digest_payload(week_key: &str, payload: &str) -> Digest {
    let p: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    Digest {
        week_key: week_key.to_string(),
        title: p.get("title").and_then(|v| v.as_str()).unwrap_or("Weekly letter").to_string(),
        intro: p.get("intro").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        sections: p.get("sections").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default(),
        empty: p.get("empty").and_then(|v| v.as_bool()).unwrap_or(true),
    }
}

/// Cached edition if it exists, fresh composition for the current week, else None.
pub fn get_digest(conn: &Connection, wk: Option<&str>) -> Option<Digest> {
    let key = wk.map(|s| s.to_string()).unwrap_or_else(current_week_key);
    let cached: Option<String> = conn
        .query_row("SELECT payload FROM digests WHERE week_key = ?", params![key], |r| r.get(0))
        .ok();
    if let Some(p) = cached {
        return Some(parse_digest_payload(&key, &p));
    }
    let cur = current_week_key();
    if wk.is_none() || wk == Some(cur.as_str()) {
        return Some(compose_digest(conn));
    }
    None
}

/// Every cached edition, newest first — the Weekly Letters shelf.
pub fn list_digests(conn: &Connection) -> Vec<serde_json::Value> {
    let mut stmt = conn.prepare("SELECT week_key, payload, created_at FROM digests ORDER BY week_key DESC").unwrap();
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    }).unwrap();
    rows.filter_map(|r| r.ok()).map(|(week_key, payload, created_at)| {
        let p: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("Weekly letter");
        serde_json::json!({ "weekKey": week_key, "createdAt": created_at, "title": title })
    }).collect()
}

const INTROS: &[&str] = &[
    "A quiet look back at what the circle was thinking about this week.",
    "What moved, what landed, and what's still simmering — this week in the circle.",
    "The week in ideas, gathered in one place.",
];

pub fn compose_digest(conn: &Connection) -> Digest {
    let wk = current_week_key();
    let cached: Option<String> = conn
        .query_row("SELECT payload FROM digests WHERE week_key = ?", params![wk], |r| r.get(0))
        .ok();
    if let Some(p) = cached {
        return parse_digest_payload(&wk, &p);
    }
    let since = (Utc::now() - chrono::Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let mut stmt = conn.prepare("SELECT * FROM notes WHERE created_at >= ? ORDER BY created_at DESC").unwrap();
    let notes: Vec<NoteRow> = stmt.query_map(params![since], row_to_note).unwrap().filter_map(|r| r.ok()).collect();

    let mut stmt = conn.prepare(
        "SELECT e.meta, n.title, e.note_id FROM events e JOIN notes n ON n.id = e.note_id
         WHERE e.kind = 'status' AND e.created_at >= ? ORDER BY e.created_at DESC").unwrap();
    let status_moves: Vec<(String, String, i64)> = stmt.query_map(params![since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    let mut stmt = conn.prepare(
        "SELECT DISTINCT n.id, n.title FROM comments c JOIN notes n ON n.id = c.note_id
         WHERE c.created_at >= ? ORDER BY c.created_at DESC LIMIT 8").unwrap();
    let commented: Vec<(i64, String)> = stmt.query_map(params![since], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    }).unwrap().filter_map(|r| r.ok()).collect();

    let mut sections: Vec<DigestSection> = vec![];
    let fresh: Vec<&NoteRow> = notes.iter().filter(|n| n.status == "seed" || n.status == "sprout").collect();
    if !fresh.is_empty() {
        sections.push(DigestSection {
            heading: "New seeds".to_string(),
            lines: fresh.iter().take(6).map(|n| format!("**{}**", n.title)).collect(),
        });
    }
    {
        let mut seen = std::collections::HashSet::new();
        let mut lines: Vec<String> = vec![];
        for (meta, title, note_id) in &status_moves {
            let to: String = serde_json::from_str::<serde_json::Value>(meta)
                .ok().and_then(|v| v.get("to").and_then(|t| t.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            if to != "motion" && to != "decided" {
                continue;
            }
            if !seen.insert(*note_id) {
                continue;
            }
            lines.push(if to == "decided" {
                format!("**{}** landed — a decision was made", title)
            } else {
                format!("**{}** is in motion", title)
            });
            if lines.len() >= 6 {
                break;
            }
        }
        if !lines.is_empty() {
            sections.push(DigestSection { heading: "What moved".to_string(), lines });
        }
    }
    if !commented.is_empty() {
        sections.push(DigestSection {
            heading: "Worth a re-read".to_string(),
            lines: commented.iter().map(|(_, t)| format!("**{}** — the conversation kept going", t)).collect(),
        });
    }
    // Quiet ones: untouched 14+ days, gentle, never shaming
    let cutoff = (Utc::now() - chrono::Duration::days(14)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut stmt = conn.prepare(
        "SELECT * FROM notes WHERE status IN ('seed','sprout') AND updated_at < ? ORDER BY updated_at ASC LIMIT 4").unwrap();
    let quiet: Vec<NoteRow> = stmt.query_map(params![cutoff], row_to_note).unwrap().filter_map(|r| r.ok()).collect();
    if !quiet.is_empty() {
        sections.push(DigestSection {
            heading: "Still simmering".to_string(),
            lines: quiet.iter().map(|n| format!("**{}** — planted a while back, still simmering", n.title)).collect(),
        });
    }

    let last = wk.chars().last().map(|c| c as usize).unwrap_or(0);
    let digest = Digest {
        week_key: wk.clone(),
        title: "This week in your notepad".to_string(),
        intro: INTROS[last % INTROS.len()].to_string(),
        sections,
        empty: false,
    };
    let digest = Digest { empty: digest.sections.is_empty(), ..digest };
    let payload = serde_json::json!({
        "title": digest.title, "intro": digest.intro,
        "sections": digest.sections, "empty": digest.empty,
    }).to_string();
    conn.execute("INSERT INTO digests (week_key, payload, created_at) VALUES (?, ?, ?)",
        params![wk, payload, now_iso()]).ok();
    digest
}

// ---- nudges ----

#[derive(Clone, Debug, Serialize)]
pub struct Nudge {
    pub key: String,
    pub text: String,
    #[serde(rename = "noteId", skip_serializing_if = "Option::is_none")]
    pub note_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

pub fn nudges_for(conn: &Connection) -> Vec<Nudge> {
    let mut out: Vec<Nudge> = vec![];
    let seen: std::collections::HashSet<String> = conn.prepare("SELECT nudge_key FROM nudges_seen").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .filter_map(|r| r.ok()).collect();
    let cutoff = (Utc::now() - chrono::Duration::days(9)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut stmt = conn.prepare(
        "SELECT id, title FROM notes WHERE status = 'sprout' AND updated_at < ? ORDER BY updated_at ASC LIMIT 2").unwrap();
    let stale: Vec<(i64, String)> = stmt.query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap().filter_map(|r| r.ok()).collect();
    for (id, title) in stale {
        let key = format!("stale-{}", id);
        if !seen.contains(&key) {
            out.push(Nudge {
                key,
                text: format!("“{}” has been sprouting for a while — still alive, or time to let it rest?", title),
                note_id: Some(id),
                action: Some("rest".to_string()),
            });
        }
    }
    out.truncate(3);
    out
}

pub fn dismiss_nudge(conn: &Connection, key: &str) {
    conn.execute("INSERT OR IGNORE INTO nudges_seen (nudge_key, created_at) VALUES (?, ?)",
        params![key, now_iso()]).ok();
}

// ---- daily task letter ----

#[derive(Clone, Debug)]
pub struct TaskItem {
    pub text: String,
    pub done: bool,
}

/// Parse markdown checklist items, skipping fenced code blocks.
pub fn parse_tasks(body: &str) -> Vec<TaskItem> {
    let mut out = Vec::new();
    let mut in_pre = false;
    for raw in body.split('\n') {
        if raw.starts_with("```") {
            in_pre = !in_pre;
            continue;
        }
        if in_pre {
            continue;
        }
        // [-*] [x] text
        let t = raw.trim_start();
        let marker = if t.starts_with("- ") || t.starts_with("* ") { &t[2..] } else { continue };
        let rest = marker.trim_start();
        if !rest.starts_with('[') || rest.len() < 4 {
            continue;
        }
        let mark = rest.chars().nth(1);
        if rest.chars().nth(2) != Some(']') || rest.chars().nth(3) != Some(' ') {
            continue;
        }
        match mark {
            Some(' ') | Some('x') | Some('X') => {
                let text: String = rest[4..].trim().chars().take(120).collect();
                out.push(TaskItem { text, done: mark != Some(' ') });
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct DailyLetter {
    #[serde(rename = "dayKey")]
    pub day_key: String,
    pub title: String,
    pub subtitle: String,
    pub intro: String,
    pub sections: Vec<DigestSection>,
    pub empty: bool,
    #[serde(rename = "emptyText")]
    pub empty_text: String,
}

const DAY_INTROS: &[&str] = &[
    "Your tasks, gathered in one place.",
    "What's open, what's done — a quiet accounting.",
    "The day's tasks, without the noise.",
];

/// The day's task intelligence as a personal letter. Composed fresh on read.
pub fn compose_daily_letter(conn: &Connection) -> DailyLetter {
    let now = Local::now();
    let dk = format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day());
    let mut stmt = conn.prepare("SELECT * FROM notes ORDER BY updated_at DESC").unwrap();
    let notes: Vec<NoteRow> = stmt.query_map([], row_to_note).unwrap().filter_map(|r| r.ok()).collect();
    let with_tasks: Vec<(&NoteRow, Vec<TaskItem>)> = notes.iter()
        .map(|n| (n, parse_tasks(&n.body)))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    let week_ago = Utc::now() - chrono::Duration::days(7);
    let mut open_lines: Vec<String> = vec![];
    let mut done_lines: Vec<String> = vec![];
    for (n, tasks) in &with_tasks {
        let done_count = tasks.iter().filter(|t| t.done).count();
        if tasks.iter().any(|t| !t.done) {
            let items: Vec<&str> = tasks.iter().filter(|t| !t.done).map(|t| t.text.as_str()).collect();
            let shown = items.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            let more = if items.len() > 5 { format!(", +{} more", items.len() - 5) } else { String::new() };
            let title = if n.title.is_empty() { "Untitled" } else { &n.title };
            open_lines.push(format!("**{}** · {} of {} done — {}{}", title, done_count, tasks.len(), shown, more));
        } else {
            let updated_ok = chrono::DateTime::parse_from_rfc3339(&n.updated_at)
                .map(|dt| dt.with_timezone(&Utc) >= week_ago)
                .unwrap_or(false);
            if updated_ok {
                let title = if n.title.is_empty() { "Untitled" } else { &n.title };
                done_lines.push(format!("**{}** · all {} done", title, tasks.len()));
            }
        }
    }
    let mut sections = vec![];
    if !open_lines.is_empty() {
        sections.push(DigestSection { heading: "Still open".to_string(), lines: open_lines });
    }
    if !done_lines.is_empty() {
        sections.push(DigestSection { heading: "Wrapped up".to_string(), lines: done_lines });
    }
    let subtitle = now.format("%A, %B %-d").to_string();
    DailyLetter {
        day_key: dk,
        title: "Today".to_string(),
        subtitle,
        intro: DAY_INTROS[now.day() as usize % DAY_INTROS.len()].to_string(),
        sections,
        empty: with_tasks.is_empty(),
        empty_text: "No tasks on your plate.\nEnjoy the clear desk.".to_string(),
    }
}

// ---- Relay notes-email ingest mapping (pure part) ----

/// Dedupe key: message_id, falling back to relay's id. Empty when unidentifiable.
pub fn notes_email_dedupe_key(message_id: &str, relay_id: &str) -> String {
    let mid = message_id.trim();
    if !mid.is_empty() {
        return mid.to_string();
    }
    let rid = relay_id.trim();
    if !rid.is_empty() {
        return format!("relay:{}", rid);
    }
    String::new()
}

/// Pure mapping: Relay Notes-folder email → Abba note fields.
pub fn notes_email_to_note(subject: &str, body: &str, date_ms: i64) -> (String, String, String) {
    let title_src = {
        let s = subject.trim();
        if !s.is_empty() {
            s.to_string()
        } else {
            let b = body.trim();
            if b.is_empty() { "Untitled note".to_string() } else { b.chars().take(60).collect() }
        }
    };
    let title: String = title_src.chars().take(200).collect();
    let iso = chrono::DateTime::<Utc>::from_timestamp_millis(date_ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    (title, body.to_string(), iso)
}

pub fn kv_watermark_default_ms() -> i64 {
    (Utc::now() - chrono::Duration::days(7)).timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn memdb() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::db::init_test_db(&c);
        c
    }

    #[test]
    fn tokens_basic() {
        let t = tokens("The Quick brown-fox jumps 42 times!");
        assert!(t.contains(&"quick".to_string()));
        assert!(t.contains(&"brown".to_string()));
        assert!(!t.contains(&"the".to_string()));
        assert!(!t.contains(&"42".to_string())); // len 2
    }
    #[test]
    fn auto_title_heading() {
        assert_eq!(auto_title("# Hello world\nbody"), "Hello world");
        assert_eq!(auto_title("Just a first sentence. Second."), "Just a first sentence.");
        assert_eq!(auto_title(""), "Untitled thought");
    }
    #[test]
    fn read_mins_min_one() {
        assert_eq!(read_mins("hi"), 1);
        assert_eq!(read_mins(&"word ".repeat(400)), 2);
    }
    #[test]
    fn related_ideas_overlap() {
        let a = NoteRow { id: 1, title: "garden tomatoes".into(), body: "planting tomatoes in the garden soil".into(), tags: "[]".into(), status: "seed".into(), read_mins: 1, created_at: "".into(), updated_at: "2026-10-02T00:00:00.000Z".into() };
        let b = NoteRow { id: 2, title: "tomato garden pests".into(), body: "aphids on the garden tomatoes".into(), tags: "[]".into(), status: "seed".into(), read_mins: 1, created_at: "".into(), updated_at: "2026-10-03T00:00:00.000Z".into() };
        let c = NoteRow { id: 3, title: "unrelated finance".into(), body: "quarterly earnings report numbers".into(), tags: "[]".into(), status: "seed".into(), read_mins: 1, created_at: "".into(), updated_at: "2026-10-04T00:00:00.000Z".into() };
        let rel = related_ideas(&a, &[b.clone(), c.clone()], 3);
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].id, 2);
    }
    #[test]
    fn week_key_iso() {
        // 2026-10-15 is a Thursday in ISO week 42
        assert_eq!(week_key_for(2026, 10, 15), "2026-W42");
    }
    #[test]
    fn parse_tasks_basic() {
        let body = "- [ ] buy milk\n- [x] done thing\n```\n- [ ] not a task\n```\n* [X] star done";
        let t = parse_tasks(body);
        assert_eq!(t.len(), 3);
        assert!(!t[0].done && t[0].text == "buy milk");
        assert!(t[1].done);
        assert!(t[2].done);
    }
    #[test]
    fn digest_and_nudges() {
        let c = memdb();
        c.execute("INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
                   VALUES ('Seed idea','body text here','[]','seed',1,?,?)",
            params![now_iso(), now_iso()]).unwrap();
        let d = compose_digest(&c);
        assert!(!d.week_key.is_empty());
        assert!(d.sections.iter().any(|s| s.heading == "New seeds"));
        // second call reads the cached edition
        let d2 = get_digest(&c, None).unwrap();
        assert_eq!(d2.week_key, d.week_key);
        // stale sprout nudge
        let old = (Utc::now() - chrono::Duration::days(10)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        c.execute("INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
                   VALUES ('Old sprout','body','[]','sprout',1,?,?)", params![old.clone(), old]).unwrap();
        let n = nudges_for(&c);
        assert_eq!(n.len(), 1);
        assert!(n[0].key.starts_with("stale-"));
        dismiss_nudge(&c, &n[0].key.clone());
        assert!(nudges_for(&c).is_empty());
    }
    #[test]
    fn daily_letter_sections() {
        let c = memdb();
        let t = now_iso();
        c.execute("INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
                   VALUES ('Chores','- [ ] mop\n- [x] sweep','[]','seed',1,?,?)", params![t.clone(), t]).unwrap();
        let l = compose_daily_letter(&c);
        assert!(!l.empty);
        assert!(l.sections.iter().any(|s| s.heading == "Still open"));
        assert!(l.sections[0].lines[0].contains("mop"));
    }
    #[test]
    fn dedupe_key_fallback() {
        assert_eq!(notes_email_dedupe_key("abc", "1"), "abc");
        assert_eq!(notes_email_dedupe_key("", "77"), "relay:77");
        assert_eq!(notes_email_dedupe_key("", ""), "");
    }
}
