# crunchyroll-migrate

Migrate Crunchyroll profile data between accounts. Exports and imports watchlists, watch history, crunchylists, and ratings -- with a live terminal dashboard, automatic retries, and pre-filtering so nothing gets duplicated.

Built because Crunchyroll has no account merge or profile transfer feature, and the existing Google Apps Script tools break constantly (manual bearer tokens, no pagination, silent data loss).

## What it moves

| Data type | Export | Import | Notes |
|-----------|--------|--------|-------|
| Watchlist | Yes | Yes | Series and movies, favourite status |
| Watch history | Yes | Yes | Per-episode playhead and completion state |
| Crunchylists | Yes | Yes | Creates lists on target, populates items |
| Ratings | Yes | Yes | 1-5 star ratings on series and movies |

## Install

Download a binary from [Releases](../../releases), or build from source:

```bash
cargo install --git https://github.com/SeanLF/crunchyroll-migrate
```

Requires nightly Rust (edition 2024).

## Usage

Run with no arguments for an interactive menu:

```
$ crunchyroll-migrate

? What would you like to do?
> Migrate      Full flow: export -> diff -> confirm -> import
  Status       Show account info, profiles, and data counts
  Export       Export one profile's data to JSON files
  Import       Import from JSON files into a profile
  Diff         Compare exported data against target account
  Rename       Rename a profile on the account
```

Or use subcommands directly:

### Migrate (recommended)

Full flow: export from source, diff against target, confirm, import.

```bash
crunchyroll-migrate migrate \
  --source-email old@example.com \
  --target-email new@example.com
```

Prompts for passwords, profile selection, and confirmation interactively. Credentials can also be piped from a password manager:

```bash
crunchyroll-migrate migrate \
  --source-email old@example.com \
  --source-password "$(op read 'op://vault/crunchyroll-old/password')" \
  --source-profile Sean \
  --target-email new@example.com \
  --target-password "$(op read 'op://vault/crunchyroll-new/password')" \
  --target-profile Sean
```

### Export

```bash
crunchyroll-migrate export --output-dir ./backup
```

Produces four JSON files: `watchlist.json`, `watch_history.json`, `crunchylists.json`, `ratings.json`.

### Import

```bash
crunchyroll-migrate import --input-dir ./backup
```

Pre-filters against the target account so already-present items are skipped without making write calls. Use `--dry-run` to preview without changes.

### Diff

```bash
crunchyroll-migrate diff --input-dir ./backup
```

Shows what's in the export vs what's already on the target:

```
  Data Type        In Export  On Target    Missing  Already There
  ──────────────────────────────────────────────────────────────
  Watchlist               47         12         35             12
  History              1,234        200      1,034            200
  Crunchylists              3          1          2              1
  Ratings                 15          0         15              0
```

### Status

```bash
crunchyroll-migrate status
```

Shows account info, premium status, profiles, and data counts.

### Rename Profile

```bash
crunchyroll-migrate rename-profile --profile "Old Name" --new-name "New Name"
```

## Terminal dashboard

During export and import operations, a live TUI dashboard shows:

- Progress gauges per data type with ETA
- Scrollable log of processed items (Up/Down, PgUp/PgDn, Home/End)
- Running totals of added/skipped/failed
- Quit with `q` or Ctrl+C (gracefully stops the operation)

Falls back to simple line output when stdout is not a terminal (piped, CI).

## Resilience

- **Pre-filtering**: Diffs target account state before importing -- only missing items are written
- **Retry with backoff**: Transient errors (429, 5xx, timeouts) retry up to 5 times with exponential backoff
- **Cloudflare detection**: Pauses 60s on Cloudflare blocks before retrying
- **409 handling**: Duplicate adds are silently counted as "already present"
- **Parallel writes**: Watchlist and history use buffered concurrent requests (5 at a time) with per-request delays

## Export format

Each JSON file contains a metadata header and items array:

```json
{
  "metadata": {
    "profile_name": "Sean",
    "exported_at": "2026-02-18T12:00:00Z",
    "total_count": 47
  },
  "items": [...]
}
```

Files are written atomically (temp file + rename) to prevent corruption on interrupt.

## Development

```bash
# Build
cargo build

# Test (20 tests: model round-trips, retry logic, error classification, UI helpers)
cargo test

# Lint
cargo clippy --all-targets

# Format
cargo fmt
```

## License

MIT
