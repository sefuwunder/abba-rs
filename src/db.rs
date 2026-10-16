//! SQLite schema, data-dir bootstrap, and the tiny kv store.
//! Mirrors src/db.ts, including the legacy migrations so an existing
//! abba.db upgrades cleanly.

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection};
use std::fs;
use std::path::PathBuf;

pub fn data_dir(env_dir: Option<String>) -> PathBuf {
    match env_dir {
        Some(d) => PathBuf::from(d),
        None => match std::env::var("ABBA_DATA") {
            Ok(d) => PathBuf::from(d),
            Err(_) => PathBuf::from("./data"),
        },
    }
}

/// ISO-8601 UTC with millis, e.g. 2026-10-15T19:00:00.123Z
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn init_data_dir(dir: Option<String>) -> Result<(Connection, PathBuf), String> {
    let dir = data_dir(dir);
    fs::create_dir_all(&dir).map_err(|e| format!("data dir: {}", e))?;
    let db_path = dir.join("abba.db");
    let conn = Connection::open(&db_path).map_err(|e| format!("open db: {}", e))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("pragma: {}", e))?;
    migrate(&conn)?;
    Ok((conn, dir))
}

pub fn kv_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM kv WHERE key = ?", params![key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
}

pub fn kv_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO kv (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notes (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          title TEXT NOT NULL DEFAULT '',
          body TEXT NOT NULL DEFAULT '',
          tags TEXT NOT NULL DEFAULT '[]',
          status TEXT NOT NULL DEFAULT 'seed',
          read_mins INTEGER NOT NULL DEFAULT 1,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_notes_updated ON notes(updated_at);
        CREATE TABLE IF NOT EXISTS ingested_mail (
          message_id TEXT PRIMARY KEY,
          note_id INTEGER NOT NULL,
          ingested_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS kv (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS comments (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
          author_name TEXT NOT NULL DEFAULT '',
          body TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_comments_note ON comments(note_id);
        CREATE TABLE IF NOT EXISTS events (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          kind TEXT NOT NULL,
          note_id INTEGER REFERENCES notes(id) ON DELETE CASCADE,
          meta TEXT NOT NULL DEFAULT '{}',
          created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at);
        CREATE TABLE IF NOT EXISTS digests (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          week_key TEXT NOT NULL UNIQUE,
          payload TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS nudges_seen (
          nudge_key TEXT PRIMARY KEY,
          created_at TEXT NOT NULL
        );
        ",
    )
    .map_err(|e| format!("migrate: {}", e))?;
    // 2026-10-05: communications removed — drop leftovers from earlier versions.
    for t in ["imap_account", "shares", "shared_notes"] {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", t))
            .map_err(|e| format!("migrate drop: {}", e))?;
    }
    migrate_solo(conn)?;
    Ok(())
}

/// Collapse the old multi-member / mesh schema into the single-user model.
fn migrate_solo(conn: &Connection) -> Result<(), String> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(|e| e.to_string())?
        .query_map([], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())?;
    if !tables.iter().any(|t| t == "members") {
        return Ok(());
    }
    let cols = |t: &str| -> Result<Vec<String>, String> {
        conn.prepare(&format!("PRAGMA table_info({})", t))
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| e.to_string())
    };
    let run = |sql: &str| conn.execute_batch(sql).map_err(|e| e.to_string());

    run(
        "CREATE TABLE notes_new (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          title TEXT NOT NULL DEFAULT '',
          body TEXT NOT NULL DEFAULT '',
          tags TEXT NOT NULL DEFAULT '[]',
          status TEXT NOT NULL DEFAULT 'seed',
          read_mins INTEGER NOT NULL DEFAULT 1,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        INSERT INTO notes_new (id, title, body, tags, status, read_mins, created_at, updated_at)
          SELECT id, title, body, tags, status, read_mins, created_at, updated_at FROM notes;
        DROP TABLE notes;
        ALTER TABLE notes_new RENAME TO notes;
        CREATE INDEX idx_notes_updated ON notes(updated_at);",
    )?;
    if cols("comments")?.iter().any(|c| c == "member_id") {
        run(
            "CREATE TABLE comments_new (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
              author_name TEXT NOT NULL DEFAULT '',
              body TEXT NOT NULL,
              created_at TEXT NOT NULL
            );
            INSERT INTO comments_new (id, note_id, author_name, body, created_at)
              SELECT c.id, c.note_id, COALESCE(m.name, ''), c.body, c.created_at
              FROM comments c LEFT JOIN members m ON m.id = c.member_id;
            DROP TABLE comments;
            ALTER TABLE comments_new RENAME TO comments;
            CREATE INDEX idx_comments_note ON comments(note_id);",
        )?;
    }
    if cols("events")?.iter().any(|c| c == "member_id") {
        run(
            "CREATE TABLE events_new (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              kind TEXT NOT NULL,
              note_id INTEGER REFERENCES notes(id) ON DELETE CASCADE,
              meta TEXT NOT NULL DEFAULT '{}',
              created_at TEXT NOT NULL
            );
            INSERT INTO events_new (id, kind, note_id, meta, created_at)
              SELECT id, kind, note_id, meta, created_at FROM events;
            DROP TABLE events;
            ALTER TABLE events_new RENAME TO events;
            CREATE INDEX idx_events_created ON events(created_at);",
        )?;
    }
    if cols("digests")?.iter().any(|c| c == "circle_id") {
        run(
            "CREATE TABLE digests_new (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              week_key TEXT NOT NULL UNIQUE,
              payload TEXT NOT NULL,
              created_at TEXT NOT NULL
            );
            INSERT INTO digests_new (week_key, payload, created_at)
              SELECT week_key, payload, MIN(created_at) FROM digests GROUP BY week_key;
            DROP TABLE digests;
            ALTER TABLE digests_new RENAME TO digests;",
        )?;
    }
    if tables.iter().any(|t| t == "nudges_seen") && cols("nudges_seen")?.iter().any(|c| c == "member_id") {
        run(
            "CREATE TABLE nudges_seen_new (
              nudge_key TEXT PRIMARY KEY,
              created_at TEXT NOT NULL
            );
            INSERT OR IGNORE INTO nudges_seen_new (nudge_key, created_at)
              SELECT nudge_key, MIN(created_at) FROM nudges_seen GROUP BY nudge_key;
            DROP TABLE nudges_seen;
            ALTER TABLE nudges_seen_new RENAME TO nudges_seen;",
        )?;
    }
    for t in [
        "reactions", "circle", "members", "invited_users", "circle_peers", "mesh_seen",
        "mesh_blocked_nodes", "imap_accounts", "imap_account", "shares", "shared_notes",
    ] {
        run(&format!("DROP TABLE IF EXISTS {}", t))?;
    }
    run("DROP INDEX IF EXISTS idx_members_user_id")?;
    run("DROP INDEX IF EXISTS idx_notes_gid")?;
    run("DROP INDEX IF EXISTS idx_comments_rgid")?;
    Ok(())
}

#[cfg(test)]
pub fn init_test_db(conn: &Connection) {
    migrate(conn).expect("test migrate");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kv_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(kv_get(&conn, "k"), None);
        kv_set(&conn, "k", "v").unwrap();
        assert_eq!(kv_get(&conn, "k").as_deref(), Some("v"));
        kv_set(&conn, "k", "v2").unwrap();
        assert_eq!(kv_get(&conn, "k").as_deref(), Some("v2"));
    }
    #[test]
    fn now_iso_format() {
        let s = now_iso();
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
    }
}
