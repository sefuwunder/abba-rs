//! Abba — Rust port. A quiet single-user markdown notepad.
//! Binds 127.0.0.1 (access control lives one layer up, in Deck's TOTP gate).
//! No sign-in, no communications except the read-only /api/sync/notes feed
//! and the one-way Relay notes-email ingest.

mod db;
mod http;
mod mind;

use db::{init_data_dir, kv_get, kv_set, now_iso};
use http::http_get;
use mind::{get_note, row_to_note, NoteRow};
use rusqlite::params;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const STATUSES: &[&str] = &["seed", "sprout", "motion", "decided", "resting"];
const INGEST_INTERVAL_SECS: u64 = 300;

type Db = Arc<Mutex<rusqlite::Connection>>;

// ---------- response helpers ----------

fn json_resp(v: &Value, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(v.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn err_resp(msg: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    json_resp(&json!({ "error": msg }), status)
}

fn text_resp(body: String, ctype: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", ctype).unwrap())
}

// ---------- static ----------

fn asset_version(public: &std::path::Path) -> u64 {
    let mut v: u64 = 0;
    for f in ["app.js", "styles.css", "index.html", "sw.js", "manifest.webmanifest",
              "icon-192.png", "icon-512.png", "icon-maskable-512.png", "apple-touch-icon.png"] {
        if let Ok(m) = fs::metadata(public.join(f)) {
            if let Ok(t) = m.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    v = v.max(d.as_secs());
                }
            }
        }
    }
    v
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url.split('?').nth(1).and_then(|q| {
        q.split('&').find_map(|pair| {
            let mut kv = pair.splitn(2, '=');
            match (kv.next(), kv.next()) {
                (Some(k), Some(v)) if k == key => Some(url_decode(v)),
                _ => None,
            }
        })
    })
}

fn url_decode(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h: String = chars.by_ref().take(2).collect();
            if let Ok(b) = u8::from_str_radix(&h, 16) {
                out.push(b as char);
            }
        } else if c == '+' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

// ---------- note json ----------

fn note_json(conn: &rusqlite::Connection, n: &NoteRow) -> Value {
    let tags: Value = serde_json::from_str(&n.tags).unwrap_or(json!([]));
    let mut stmt = conn.prepare("SELECT id, body, created_at, author_name FROM comments WHERE note_id = ? ORDER BY created_at ASC").unwrap();
    let comments: Vec<Value> = stmt.query_map(params![n.id], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "body": r.get::<_, String>(1)?,
            "createdAt": r.get::<_, String>(2)?,
            "author": r.get::<_, String>(3)?,
        }))
    }).unwrap().filter_map(|r| r.ok()).collect();
    json!({
        "id": n.id, "title": n.title, "body": n.body, "tags": tags, "status": n.status,
        "readMins": n.read_mins, "createdAt": n.created_at, "updatedAt": n.updated_at,
        "comments": comments,
    })
}

fn log_event(conn: &rusqlite::Connection, kind: &str, note_id: Option<i64>, meta: &Value) {
    conn.execute("INSERT INTO events (kind, note_id, meta, created_at) VALUES (?, ?, ?, ?)",
        params![kind, note_id, meta.to_string(), now_iso()]).ok();
}

fn read_body(req: &mut Request) -> String {
    let mut s = String::new();
    let _ = req.as_reader().read_to_string(&mut s);
    s
}

fn json_body(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(json!({}))
}

fn clamp_str(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

enum SqlVal {
    Text(String),
    Int(i64),
}
impl rusqlite::ToSql for SqlVal {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            SqlVal::Text(s) => Ok(rusqlite::types::ToSqlOutput::from(s.as_str())),
            SqlVal::Int(i) => Ok(rusqlite::types::ToSqlOutput::from(*i)),
        }
    }
}

// ---------- Relay notes-email ingest ----------

fn email_date_ms(e: &Value) -> i64 {
    match e.get("date") {
        Some(Value::Number(n)) => {
            let d = n.as_i64().unwrap_or(0);
            if d <= 0 { chrono::Utc::now().timestamp_millis() }
            else if d > 1_000_000_000_000 { d } else { d * 1000 }
        }
        Some(Value::String(s)) => {
            chrono::DateTime::parse_from_rfc3339(s.trim())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis())
        }
        _ => chrono::Utc::now().timestamp_millis(),
    }
}

fn ingest_once(db: &Db) {
    let relay_base = env::var("RELAY_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3006".to_string());
    let relay_base = relay_base.trim().trim_end_matches('/').to_string();
    let wm: i64 = {
        let conn = db.lock().unwrap();
        kv_get(&conn, "notes_mail_watermark").and_then(|s| s.parse().ok()).unwrap_or(0)
    };
    let wm = if wm == 0 { mind::kv_watermark_default_ms() } else { wm };
    let url = format!("{}/api/relay/notes-emails?since={}", relay_base, wm);
    let resp = match http_get(&url, Duration::from_secs(20)) {
        Ok(r) => r,
        Err(_) => return, // quiet on Relay failure; never throws
    };
    if resp.status != 200 {
        return;
    }
    let body = match resp.body_str() { Ok(s) => s, Err(_) => return };
    let j: Value = match serde_json::from_str(body) { Ok(v) => v, Err(_) => return };
    let emails = match j.get("emails").and_then(|e| e.as_array()) {
        Some(a) => a.clone(),
        None => return,
    };
    let conn = db.lock().unwrap();
    let mut max_date = wm;
    for e in &emails {
        let mid = e.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
        let rid = match e.get("id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let key = mind::notes_email_dedupe_key(mid, &rid);
        if key.is_empty() {
            continue;
        }
        let seen: bool = conn.query_row(
            "SELECT 1 FROM ingested_mail WHERE message_id = ?", params![key], |_| Ok(()),
        ).is_ok();
        if seen {
            continue;
        }
        let subject = e.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let ebody = e.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let date_ms = email_date_ms(e);
        let (title, nbody, iso) = mind::notes_email_to_note(subject, ebody, date_ms);
        let read = mind::read_mins(&nbody);
        conn.execute(
            "INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
             VALUES (?, ?, ?, 'seed', ?, ?, ?)",
            params![title, nbody, "[\"from-email\"]", read, iso, iso],
        ).ok();
        let note_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO ingested_mail (message_id, note_id, ingested_at) VALUES (?, ?, ?)",
            params![key, note_id, now_iso()],
        ).ok();
        if date_ms > max_date {
            max_date = date_ms;
        }
    }
    if max_date > wm {
        kv_set(&conn, "notes_mail_watermark", &max_date.to_string()).ok();
    }
}

fn start_ingest(db: Db) {
    std::thread::spawn(move || {
        ingest_once(&db);
        loop {
            std::thread::sleep(Duration::from_secs(INGEST_INTERVAL_SECS));
            ingest_once(&db);
        }
    });
}

// ---------- routes ----------

fn handle(method: &Method, url: &str, body: &str, db: &Db, public: &std::path::Path) -> Response<std::io::Cursor<Vec<u8>>> {
    let path = url.split('?').next().unwrap_or("").to_string();
    macro_rules! respond {
        ($r:expr) => { return $r };
    }

    // static
    if *method == Method::Get && (path == "/" || path == "/index.html") {
        let v = asset_version(public);
        match fs::read_to_string(public.join("index.html")) {
            Ok(h) => respond!(text_resp(h.replace("__V__", &v.to_string()), "text/html; charset=utf-8", 200)),
            Err(_) => respond!(err_resp("Not found.", 404)),
        }
    }
    if *method == Method::Get && path == "/sw.js" {
        let v = asset_version(public);
        match fs::read_to_string(public.join("sw.js")) {
            Ok(s) => respond!(text_resp(s.replace("__V__", &v.to_string()), "application/javascript", 200)),
            Err(_) => respond!(err_resp("Not found.", 404)),
        }
    }
    if *method == Method::Get && path == "/manifest.webmanifest" {
        match fs::read_to_string(public.join("manifest.webmanifest")) {
            Ok(s) => respond!(text_resp(s, "application/manifest+json", 200)),
            Err(_) => respond!(err_resp("Not found.", 404)),
        }
    }
    for icon in ["icon-192.png", "icon-512.png", "icon-maskable-512.png", "apple-touch-icon.png", "favicon.svg"] {
        if *method == Method::Get && path == format!("/{}", icon) {
            match fs::read(public.join(icon)) {
                Ok(bytes) => {
                    let ctype = if icon.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
                    respond!(Response::from_data(bytes)
                        .with_header(Header::from_bytes("Content-Type", ctype).unwrap()));
                }
                Err(_) => respond!(err_resp("Not found.", 404)),
            }
        }
    }
    for (p, ctype) in [("/app.js", "application/javascript"), ("/styles.css", "text/css"),
                       ("/favicon.svg", "image/svg+xml"), ("/vendor/mermaid.min.js", "application/javascript")] {
        if *method == Method::Get && path == p {
            match fs::read_to_string(public.join(p.trim_start_matches('/'))) {
                Ok(s) => respond!(text_resp(s, ctype, 200)),
                Err(_) => respond!(err_resp("Not found.", 404)),
            }
        }
    }

    let conn = db.lock().unwrap();

    // status
    if *method == Method::Get && path == "/api/status" {
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0)).unwrap_or(0);
        respond!(json_resp(&json!({ "ok": true, "notes": n }), 200));
    }

    // notes collection
    if *method == Method::Get && path == "/api/notes" {
        let mut stmt = conn.prepare("SELECT * FROM notes ORDER BY updated_at DESC").unwrap();
        let notes: Vec<Value> = stmt.query_map([], row_to_note).unwrap()
            .filter_map(|r| r.ok()).map(|n| note_json(&conn, &n)).collect();
        respond!(json_resp(&json!({ "notes": notes }), 200));
    }
    if *method == Method::Post && path == "/api/notes" {
        let b = json_body(body);
        let note_body = clamp_str(b.get("body").and_then(|v| v.as_str()).unwrap_or(""), 60000);
        if note_body.trim().is_empty() {
            respond!(err_resp("Write something first — even a fragment.", 400));
        }
        let title_raw = b.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
        let title = if title_raw.is_empty() { mind::auto_title(&note_body) } else { clamp_str(title_raw, 120) };
        let tags: Vec<String> = b.get("tags").and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).map(|t| clamp_str(t, 30)).take(8).collect())
            .unwrap_or_default();
        let t = now_iso();
        conn.execute(
            "INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
             VALUES (?, ?, ?, 'seed', ?, ?, ?)",
            params![title, note_body, serde_json::to_string(&tags).unwrap(), mind::read_mins(&note_body), t, t],
        ).unwrap();
        let id = conn.last_insert_rowid();
        let note = get_note(&conn, id).unwrap();
        respond!(json_resp(&json!({ "note": note_json(&conn, &note) }), 201));
    }

    // notes item + subpaths
    if path.starts_with("/api/notes/") {
        let rest = path.trim_start_matches("/api/notes/");
        let mut parts = rest.splitn(2, '/');
        let id: i64 = match parts.next().unwrap_or("").parse() {
            Ok(i) => i,
            Err(_) => { respond!(err_resp("Not found.", 404)) }
        };
        let sub = format!("/{}", parts.next().unwrap_or(""));
        let sub = if sub == "/" { String::new() } else { sub };
        let note = match get_note(&conn, id) {
            Some(n) => n,
            None => { respond!(err_resp("Not found.", 404)) }
        };

        if *method == Method::Get && sub.is_empty() {
            respond!(json_resp(&json!({ "note": note_json(&conn, &note) }), 200));
        }
        if *method == Method::Get && sub == "/related" {
            let mut stmt = conn.prepare("SELECT * FROM notes WHERE id != ?").unwrap();
            let pool: Vec<NoteRow> = stmt.query_map(params![id], row_to_note).unwrap()
                .filter_map(|r| r.ok()).collect();
            let related: Vec<Value> = mind::related_ideas(&note, &pool, 3).iter()
                .map(|n| json!({ "id": n.id, "title": n.title })).collect();
            respond!(json_resp(&json!({ "related": related }), 200));
        }
        if *method == Method::Get && sub == "/export" {
            let md = format!("# {}\n\n{}\n\n---\n*{} · {}*\n",
                note.title, note.body, &note.created_at[..10.min(note.created_at.len())], note.status);
            respond!(Response::from_string(md)
                .with_header(Header::from_bytes("Content-Type", "text/markdown").unwrap())
                .with_header(Header::from_bytes("Content-Disposition",
                    format!("attachment; filename=\"abba-{}.md\"", note.id).as_str()).unwrap()));
        }
        if *method == Method::Patch && sub.is_empty() {
            let b = json_body(body);
            let mut updates: Vec<String> = vec![];
            let mut vals: Vec<SqlVal> = vec![];
            if let Some(t) = b.get("title").and_then(|v| v.as_str()) {
                if !t.trim().is_empty() {
                    updates.push("title = ?".to_string());
                    vals.push(SqlVal::Text(clamp_str(t.trim(), 120)));
                }
            }
            if let Some(nb) = b.get("body").and_then(|v| v.as_str()) {
                if !nb.trim().is_empty() {
                    let nb = clamp_str(nb, 60000);
                    let rm = mind::read_mins(&nb);
                    updates.push("body = ?".to_string());
                    vals.push(SqlVal::Text(nb));
                    updates.push("read_mins = ?".to_string());
                    vals.push(SqlVal::Int(rm));
                }
            }
            if let Some(tags) = b.get("tags").and_then(|v| v.as_array()) {
                let ts: Vec<String> = tags.iter().filter_map(|t| t.as_str()).map(|t| clamp_str(t, 30)).take(8).collect();
                updates.push("tags = ?".to_string());
                vals.push(SqlVal::Text(serde_json::to_string(&ts).unwrap()));
            }
            if let Some(st) = b.get("status").and_then(|v| v.as_str()) {
                if STATUSES.contains(&st) && st != note.status {
                    updates.push("status = ?".to_string());
                    vals.push(SqlVal::Text(st.to_string()));
                    log_event(&conn, "status", Some(note.id), &json!({ "from": note.status, "to": st }));
                }
            }
            if updates.is_empty() {
                respond!(err_resp("Nothing to change.", 400));
            }
            updates.push("updated_at = ?".to_string());
            vals.push(SqlVal::Text(now_iso()));
            vals.push(SqlVal::Int(id));
            let sql = format!("UPDATE notes SET {} WHERE id = ?", updates.join(", "));
            let params_refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            conn.execute(&sql, params_refs.as_slice()).unwrap();
            let fresh = get_note(&conn, id).unwrap();
            respond!(json_resp(&json!({ "note": note_json(&conn, &fresh) }), 200));
        }
        if *method == Method::Delete && sub.is_empty() {
            conn.execute("DELETE FROM notes WHERE id = ?", params![id]).ok();
            respond!(json_resp(&json!({ "ok": true }), 200));
        }
        if *method == Method::Post && sub == "/comments" {
            let b = json_body(body);
            let cbody = clamp_str(b.get("body").and_then(|v| v.as_str()).unwrap_or("").trim(), 5000);
            if cbody.is_empty() {
                respond!(err_resp("Write something first.", 400));
            }
            let author = clamp_str(b.get("author").and_then(|v| v.as_str()).unwrap_or("").trim(), 40);
            conn.execute("INSERT INTO comments (note_id, author_name, body, created_at) VALUES (?, ?, ?, ?)",
                params![id, author, cbody, now_iso()]).unwrap();
            let cid = conn.last_insert_rowid();
            log_event(&conn, "comment", Some(id), &json!({}));
            let fresh = get_note(&conn, id).unwrap();
            respond!(json_resp(&json!({ "note": note_json(&conn, &fresh), "commentId": cid }), 201));
        }
        if *method == Method::Delete && sub.starts_with("/comments/") {
            let cid: i64 = sub.trim_start_matches("/comments/").parse().unwrap_or(0);
            if cid == 0 {
                respond!(err_resp("Not found.", 404));
            }
            conn.execute("DELETE FROM comments WHERE id = ? AND note_id = ?", params![cid, id]).ok();
            let fresh = get_note(&conn, id).unwrap();
            respond!(json_resp(&json!({ "note": note_json(&conn, &fresh) }), 200));
        }
        respond!(err_resp("Not found.", 404));
    }

    // letters
    if *method == Method::Get && path == "/api/digests" {
        respond!(json_resp(&json!({ "digests": mind::list_digests(&conn) }), 200));
    }
    if *method == Method::Get && path == "/api/digest/today" {
        respond!(json_resp(&json!({ "digest": mind::compose_daily_letter(&conn) }), 200));
    }
    if *method == Method::Get && path == "/api/digest" {
        match mind::get_digest(&conn, query_param(&url, "week").as_deref()) {
            Some(d) => respond!(json_resp(&json!({ "digest": d }), 200)),
            None => respond!(err_resp("That letter isn't on the shelf.", 404)),
        }
    }
    if *method == Method::Get && path == "/api/nudges" {
        respond!(json_resp(&json!({ "nudges": mind::nudges_for(&conn) }), 200));
    }
    if *method == Method::Post && path == "/api/nudges/dismiss" {
        let b = json_body(body);
        if let Some(k) = b.get("key").and_then(|v| v.as_str()) {
            mind::dismiss_nudge(&conn, k);
        }
        respond!(json_resp(&json!({ "ok": true }), 200));
    }

    // sync feed (read-only, for Switchboard)
    if *method == Method::Get && path == "/api/sync/notes" {
        let since_raw = query_param(&url, "since").unwrap_or_default();
        let mut since_ms: i64 = since_raw.parse().unwrap_or(0);
        if since_ms > 0 && since_ms < 1_000_000_000_000 {
            since_ms *= 1000;
        }
        let since = if since_ms > 0 {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(since_ms)
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
        } else {
            "1970-01-01T00:00:00.000Z".to_string()
        };
        let mut stmt = conn.prepare(
            "SELECT id, title, status, updated_at FROM notes WHERE updated_at >= ? ORDER BY updated_at ASC LIMIT 500").unwrap();
        let notes: Vec<Value> = stmt.query_map(params![since], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "title": r.get::<_, String>(1)?,
                "folder": "Notepad",
                "updated_at": r.get::<_, String>(3)?,
                "state": r.get::<_, String>(2)?,
            }))
        }).unwrap().filter_map(|r| r.ok()).collect();
        respond!(json_resp(&json!({ "notes": notes }), 200));
    }

    // backup
    if *method == Method::Get && path == "/api/backup" {
        let mut stmt = conn.prepare("SELECT * FROM notes ORDER BY id").unwrap();
        let notes: Vec<Value> = stmt.query_map([], row_to_note).unwrap().filter_map(|r| r.ok())
            .map(|n| json!({
                "id": n.id, "title": n.title, "body": n.body,
                "tags": serde_json::from_str::<Value>(&n.tags).unwrap_or(json!([])),
                "status": n.status, "read_mins": n.read_mins,
                "created_at": n.created_at, "updated_at": n.updated_at,
            })).collect();
        let mut stmt = conn.prepare("SELECT id, note_id, author_name, body, created_at FROM comments ORDER BY id").unwrap();
        let comments: Vec<Value> = stmt.query_map([], |r| {
            Ok(json!({ "id": r.get::<_, i64>(0)?, "note_id": r.get::<_, i64>(1)?,
                       "author_name": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?,
                       "created_at": r.get::<_, String>(4)? }))
        }).unwrap().filter_map(|r| r.ok()).collect();
        let mut stmt = conn.prepare("SELECT week_key, payload, created_at FROM digests ORDER BY week_key").unwrap();
        let digests: Vec<Value> = stmt.query_map([], |r| {
            Ok(json!({ "week_key": r.get::<_, String>(0)?, "payload": r.get::<_, String>(1)?,
                       "created_at": r.get::<_, String>(2)? }))
        }).unwrap().filter_map(|r| r.ok()).collect();
        let payload = json!({ "app": "abba", "version": 1, "exportedAt": now_iso(),
                              "notes": notes, "comments": comments, "digests": digests });
        respond!(Response::from_string(serde_json::to_string_pretty(&payload).unwrap())
            .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
            .with_header(Header::from_bytes("Content-Disposition", "attachment; filename=\"abba-backup.json\"").unwrap()));
    }
    if *method == Method::Post && path == "/api/backup" {
        let b = json_body(body);
        let notes = match (b.get("app").and_then(|v| v.as_str()), b.get("notes").and_then(|v| v.as_array())) {
            (Some("abba"), Some(n)) => n.clone(),
            _ => { respond!(err_resp("That's not an Abba backup.", 400)) }
        };
        let t = now_iso();
        let mut imported = 0;
        for n in notes.iter().take(5000) {
            let nbody = clamp_str(n.get("body").and_then(|v| v.as_str()).unwrap_or(""), 60000);
            if nbody.trim().is_empty() {
                continue;
            }
            let title_raw = n.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let title = if title_raw.is_empty() { mind::auto_title(&nbody) } else { clamp_str(title_raw, 120) };
            let tags: Vec<String> = n.get("tags").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).take(8).collect())
                .unwrap_or_default();
            let status = n.get("status").and_then(|v| v.as_str()).unwrap_or("seed");
            let status = if STATUSES.contains(&status) { status } else { "seed" };
            let created = n.get("created_at").and_then(|v| v.as_str()).unwrap_or(&t);
            let updated = n.get("updated_at").and_then(|v| v.as_str()).unwrap_or(&t);
            conn.execute(
                "INSERT INTO notes (title, body, tags, status, read_mins, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![title, nbody, serde_json::to_string(&tags).unwrap(),
                        status, mind::read_mins(&nbody), created, updated],
            ).ok();
            imported += 1;
        }
        respond!(json_resp(&json!({ "ok": true, "imported": imported }), 200));
    }

    respond!(err_resp("Not found.", 404));
}

fn main() {
    let port: u16 = env::var("ABBA_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3013);
    let (conn, data_dir) = init_data_dir(None).unwrap_or_else(|e| {
        eprintln!("abba-rs: {}", e);
        std::process::exit(1);
    });
    let db: Db = Arc::new(Mutex::new(conn));
    let public = std::path::PathBuf::from("./public");

    start_ingest(db.clone());

    let server = Server::http(format!("127.0.0.1:{}", port)).unwrap_or_else(|e| {
        eprintln!("abba-rs: cannot bind 127.0.0.1:{}: {}", port, e);
        std::process::exit(1);
    });
    println!("Abba (rust) listening on http://127.0.0.1:{}  (single-user; access via Deck)", port);
    println!("data: {}", data_dir.display());

    for mut req in server.incoming_requests() {
        let db = db.clone();
        let public = public.clone();
        std::thread::spawn(move || {
            let method = req.method().clone();
            let url = req.url().to_string();
            let body = read_body(&mut req);
            let resp = handle(&method, &url, &body, &db, &public);
            let _ = req.respond(resp);
        });
    }
}

#[cfg(test)]
mod api_tests {
    // Integration tests spawn the real binary over HTTP.
    use crate::http::http_request;
    use std::process::{Child, Command};
    use std::time::{Duration, SystemTime};

    fn bin() -> String {
        // CARGO_BIN_EXE_<name>: dashes become underscores
        let from_env = std::env::var("CARGO_BIN_EXE_abba_rs").ok();
        let fallback = "./target/debug/abba-rs".to_string();
        for cand in from_env.iter().chain(std::iter::once(&fallback)) {
            if std::path::Path::new(cand).exists() {
                return cand.clone();
            }
        }
        panic!("abba-rs binary not found: run `cargo build` before `cargo test`");
    }

    struct Server {
        child: Child,
        base: String,
        dir: String,
    }
    impl Server {
        fn start() -> Self {
            let dir = format!("/tmp/abba-rs-test-{}", SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos());
            std::fs::create_dir_all(&dir).unwrap();
            // pick a free port
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let child = Command::new(bin())
                .env("ABBA_DATA", &dir)
                .env("ABBA_PORT", port.to_string())
                .env("RELAY_URL", "http://127.0.0.1:1") // unreachable: ingest skips quietly
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn abba-rs");
            let base = format!("http://127.0.0.1:{}", port);
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                if http_request("GET", &format!("{}/api/status", base), None, Duration::from_secs(1)).map(|r| r.status == 200).unwrap_or(false) {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    panic!("server did not start");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Server { child, base, dir }
        }
        fn api(&self, method: &str, path: &str, body: Option<&str>) -> (u16, serde_json::Value) {
            let r = http_request(method, &format!("{}{}", self.base, path),
                body.map(|b| b.as_bytes()), Duration::from_secs(5)).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or(serde_json::json!({}));
            (r.status, v)
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn notes_crud() {
        let s = Server::start();
        let (st, v) = s.api("GET", "/api/status", None);
        assert_eq!(st, 200);
        assert_eq!(v["notes"], 0);

        // create
        let (st, v) = s.api("POST", "/api/notes",
            Some(r##"{"body":"# Weekly thought\n\nA quiet idea about gardens.","tags":["garden"]}"##));
        assert_eq!(st, 201);
        assert_eq!(v["note"]["title"], "Weekly thought");
        assert_eq!(v["note"]["tags"], serde_json::json!(["garden"]));
        let id = v["note"]["id"].as_i64().unwrap();

        // validation
        let (st, _) = s.api("POST", "/api/notes", Some(r#"{"body":"   "}"#));
        assert_eq!(st, 400);

        // list + get
        let (st, v) = s.api("GET", "/api/notes", None);
        assert_eq!(st, 200);
        assert_eq!(v["notes"].as_array().unwrap().len(), 1);
        let (st, v) = s.api("GET", &format!("/api/notes/{}", id), None);
        assert_eq!(st, 200);
        assert_eq!(v["note"]["id"], id);
        let (st, _) = s.api("GET", "/api/notes/999999", None);
        assert_eq!(st, 404);

        // patch status (logs event)
        let (st, v) = s.api("PATCH", &format!("/api/notes/{}", id), Some(r#"{"status":"sprout"}"#));
        assert_eq!(st, 200);
        assert_eq!(v["note"]["status"], "sprout");
        let (st, _) = s.api("PATCH", &format!("/api/notes/{}", id), Some(r#"{}"#));
        assert_eq!(st, 400);

        // related
        let (st, v) = s.api("GET", &format!("/api/notes/{}/related", id), None);
        assert_eq!(st, 200);
        assert!(v["related"].as_array().unwrap().is_empty());

        // comments
        let (st, v) = s.api("POST", &format!("/api/notes/{}/comments", id),
            Some(r#"{"body":"a kind comment","author":"me"}"#));
        assert_eq!(st, 201);
        let cid = v["commentId"].as_i64().unwrap();
        assert_eq!(v["note"]["comments"].as_array().unwrap().len(), 1);
        let (st, v) = s.api("DELETE", &format!("/api/notes/{}/comments/{}", id, cid), None);
        assert_eq!(st, 200);
        assert!(v["note"]["comments"].as_array().unwrap().is_empty());

        // export
        let r = http_request("GET", &format!("{}/api/notes/{}/export", s.base, id), None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.header("Content-Type").unwrap_or("").contains("markdown"));

        // letters + nudges
        let (st, _) = s.api("GET", "/api/digest/today", None);
        assert_eq!(st, 200);
        let (st, v) = s.api("GET", "/api/digests", None);
        assert_eq!(st, 200);
        assert!(v["digests"].as_array().is_some());
        let (st, _) = s.api("GET", "/api/nudges", None);
        assert_eq!(st, 200);
        let (st, v) = s.api("POST", "/api/nudges/dismiss", Some(r#"{"key":"k"}"#));
        assert_eq!(st, 200);
        assert_eq!(v["ok"], true);

        // sync feed
        let (st, v) = s.api("GET", "/api/sync/notes?since=0", None);
        assert_eq!(st, 200);
        assert_eq!(v["notes"].as_array().unwrap().len(), 1);
        assert_eq!(v["notes"][0]["folder"], "Notepad");

        // backup roundtrip
        let r = http_request("GET", &format!("{}/api/backup", s.base), None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 200);
        let backup = String::from_utf8(r.body).unwrap();
        let (st, _) = s.api("DELETE", &format!("/api/notes/{}", id), None);
        assert_eq!(st, 200);
        let (st, v) = s.api("POST", "/api/backup", Some(&backup));
        assert_eq!(st, 200);
        assert_eq!(v["imported"], 1);
        let (st, _) = s.api("POST", "/api/backup", Some(r#"{"app":"nope","notes":[]}"#));
        assert_eq!(st, 400);

        // static
        let r = http_request("GET", &format!("{}/", s.base), None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 200);
        assert!(r.header("Content-Type").unwrap_or("").contains("text/html"));
        let r = http_request("GET", &format!("{}/app.js", s.base), None, Duration::from_secs(5)).unwrap();
        assert_eq!(r.status, 200);
    }
}
