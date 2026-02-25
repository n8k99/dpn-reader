# dpn-reader

Newsboat-inspired Rust TUI RSS reader for DragonPunk Noir workflow.

Core behavior:
- no unread counters in the UI
- feed view + global chronological view
- `j/k` or `↑/↓` = list navigation
- `n` = next unread
- `c` = save comment to Postgres `documents` under:
  `Areas/Eckenrode Muziekopname/Executive/Thought Police/`
- `Shift+R` = refresh feeds now
- auto-refresh runs every 900 seconds by default

## DPN + Transparency

- Foreground-only DPN accents (no forced background colors), so Kitty transparency remains visible.
- Optional style hinting from `~/.newsboat/config` if present (fg-only mapping).

## Images (Kitty)

In article view:
- press `i` to display the first image found in feed content via `kitty +kitten icat`

## Run

```bash
cd /home/n8k99/dpn-reader
cargo run
```

## Feed sources

Priority order:
1. `DPN_READER_OPML` env var path
2. `./feeds.opml` in repo
3. `DPN_READER_URLS` env var path
4. `~/.newsboat/urls`

## Environment

SQLite cache:
- `DPN_READER_DB` (default: `~/.local/share/dpn-reader/dpn_reader.db`)
- `DPN_READER_AUTO_REFRESH_SECS` (default: `900`)

Postgres (comment save target):
- `PG_HOST` (default `127.0.0.1`)
- `PG_PORT` (default `5432`)
- `PG_USER` (default `chronicle`)
- `PG_PASSWORD` (default `chronicle2026`)
- `PG_DATABASE` (default `master_chronicle`)

## Notes

- No schema migration is performed.
- Feed/article cache is local SQLite.
- Comments persist using existing `documents` table contract.
