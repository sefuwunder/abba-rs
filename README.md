# abba-rs

A quiet single-user markdown notepad for an executive. Rust port of [abba](https://github.com/sefuwunder/abba).

No sign-in: binds `127.0.0.1` only (access control lives one layer up, in Deck's TOTP gate). No AI, no network calls in the hot path — the "mind" is deterministic and local: auto-titles, reading-time estimates, related ideas by shared language, a weekly letter composed from the week's events, gentle nudges, and a daily task letter parsed from markdown checklists.

## Run

```bash
cargo run --release
# or
ABBA_PORT=3013 ABBA_DATA=./data ./target/release/abba-rs
```

Then open http://127.0.0.1:3013 (via Deck).

The only network the server ever initiates is a quiet 5-minute poll of Relay's
`GET /api/relay/notes-emails` (`RELAY_URL`, default `http://127.0.0.1:3006`) for
one-way email-to-note ingest. Nothing is ever written back.

Point `ABBA_DATA` at the Bun version's data dir to carry notes over — the
SQLite schema (including the legacy single-user migration) is identical.

## API

Same as the Bun original: notes CRUD (`GET/POST /api/notes`, `GET/PATCH/DELETE /api/notes/:id`),
`/related`, `/export`, comments, `/api/digests`, `/api/digest/today`, `/api/digest?week=`,
`/api/nudges`, read-only `/api/sync/notes?since=` (Switchboard feed), and JSON
`/api/backup` export/import. The `public/` frontend is unchanged.

## Build

```bash
cargo build       # the HTTP integration test spawns this binary
cargo test        # 13 tests: mind unit tests + full HTTP integration test
cargo build --release
```

Deps: `tiny_http` (thread-per-request server), `rusqlite` (bundled SQLite),
`serde`/`serde_json`, `chrono`. The frontend in `public/` is served from disk.
