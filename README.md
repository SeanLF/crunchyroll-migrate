# crunchyroll-migrate

Copy Crunchyroll profile data between profiles and accounts. Exports and imports watchlists, watch history, crunchylists, and ratings, with a live terminal view, automatic retries, and pre-filtering so nothing gets duplicated.

## Try Crunchyroll's own transfer first

Crunchyroll now has an official [profile transfer](https://help.crunchyroll.com/article/creating-multiple-profiles) (avatar › Transfer Profile). It moves a whole profile, with its watch history and lists, to another account as a new profile, once the current account holder approves. If that's what you want, use it.

Use this tool when you need something it doesn't do:

| | Official transfer | crunchyroll-migrate |
|---|---|---|
| Source profile afterwards | Moved off the source account | Untouched (it's a copy) |
| Into an existing profile | No, arrives as a new profile | Yes, merges and skips what's already there |
| Primary profile | Can't be transferred | Works |
| Approval | Account holder approves by email, within 7 days | None; you sign in to both sides |
| Backup / preview | No | JSON export, `diff` before copying |

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

Requires Rust 1.85+ (edition 2024).

## Logging in

Crunchyroll no longer lets third-party tools log in with email and password. Instead, use the `etp_rt` cookie from a browser session:

1. Log in at [crunchyroll.com](https://www.crunchyroll.com).
2. Open devtools: **Application** (Chrome) or **Storage** (Firefox/Safari) > **Cookies** > `https://www.crunchyroll.com`.
3. Copy the value of `etp_rt`.

The tool prompts for it (input hidden), or takes it via `--cookie`. Pasting `etp_rt=...;` works too. The cookie expires; if login fails, grab a fresh one.

For a migration you need one cookie per account. Use a private window for the second account so logging in doesn't overwrite the first cookie.

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
crunchyroll-migrate migrate
```

Prompts for both cookies, profile selection, and confirmation interactively. Leave the target cookie empty to copy between profiles on the same account (for example into a new profile, which it offers to create). The intermediate export in `./migration` is deleted after a successful import; pass `--keep-data` to keep it as a backup. Everything can also be passed as flags:

```bash
crunchyroll-migrate migrate \
  --source-cookie "$OLD_ETP_RT" \
  --source-profile Sean \
  --target-cookie "$NEW_ETP_RT" \
  --target-profile Sean
```

### Export

```bash
crunchyroll-migrate export --output-dir ./backup
```

Produces four JSON files in `./export` (or `--output-dir`): `watchlist.json`, `watch_history.json`, `crunchylists.json`, `ratings.json`. The full path is printed when it finishes.

### Import

```bash
crunchyroll-migrate import --input-dir ./backup
```

Pre-filters against the target account so already-present items are skipped without making write calls. Use `--dry-run` to preview without changes.

### Diff

```bash
crunchyroll-migrate diff --input-dir ./backup
```

Shows what's in the export vs what's already on the target, per data type and per show:

```
 Diff Sean → Sean copy                                     ✓ Done 0m09s
 ✓ Sign in  ›  ✓ Read  ›  ✓ Compare

                On source   Already there   To copy
 Watchlist             10               0        10
 History              663               3       660
 Crunchylists           0               0         0
 Ratings                0               0         0

 ○ My Hero Academia                ★ watchlist  0/88 eps
 ○ Frieren: Beyond Journey's End                0/28 eps
 ✓ Gachiakuta                                   12 eps
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

## Terminal view

Every command runs in one view drawn in place in your terminal, which stays in scrollback when it's done. Its layout doesn't change during a run, so nothing flashes or jumps:

- A step tracker (`Source › Read › Target › Compare › Copy` for a migration) and one overall progress bar with an ETA
- A table per data type whose columns fill in as steps finish: source, target has, to copy, copied
- One row per show, in the order they'll be copied; finished shows scroll away while copying (`↑↓` to look around, `end` to follow again)
- Prompts (menu, cookie, profile, confirm) in a fixed strip at the bottom, with hidden cookie input and paste support
- Failures printed above the view so they stay readable; `q` or Ctrl+C stops (exit code 130)

Falls back to plain lines when stdin or stdout isn't a terminal (piped, CI).

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
# Set up git hooks (format + clippy on commit, tests on push)
git config core.hooksPath .githooks

# Build
cargo build

# Test (model round-trips, retry logic, error classification, cookie parsing, UI helpers)
cargo test

# Lint
cargo clippy --all-targets

# Format
cargo fmt
```

## License

MIT
