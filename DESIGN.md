# OneFolder Design

13 Oct 2026 · Bora Okumusoglu. Source of truth: the Claude Doc at https://claude.ai/code/artifact/739fd373-13c9-4799-b525-b9c5158022ce; this file is its Markdown export.

## Overview

OneFolder is a single-binary Rust terminal app that drives rclone to sync local folders to OneDrive accounts and external drives, one-way or two-way, with a Mole-style interface. Version 1 targets macOS only; the code avoids macOS-only assumptions except in the volume-identity module.

Everything below was checked against rclone v1.75.2 on this Mac (`/opt/homebrew/bin/rclone`) by running copy and bisync against local folders. Facts marked **confirmed** come from those runs, not from memory.

**Engineering defaults** (made here so the rest of the design can rely on them):

- Config at `~/.config/onefolder/config.toml` (hand-editable). Machine-written state (last-run records, holds) lives separately in `~/.local/share/onefolder/state.json`, so a user edit of the config is never clobbered by a run.
- Per-run rclone logs at `~/.local/share/onefolder/logs/<job>/<destination-id>/<timestamp>.jsonl`; bisync working directories at `~/.local/share/onefolder/bisync/<job>/<destination-id>/`, one per destination so each two-way destination keeps its own state.
- Minimum rclone version 1.66.0 (first release with `--conflict-resolve`, `--resync-mode` and `--recover`); the doctor recommends the latest.
- rclone is always given absolute paths and `--color never`. **Confirmed:** relative paths make bisync abort with "path1 and path2 are out of sync", and without `--color never` the `msg` field of every JSON log line contains ANSI escape codes.
- Job excludes are passed as repeated `--exclude` flags, never as `--filters-file`. **Confirmed:** bisync aborts with "filters file md5 hash not found (must run --resync)" the first time a filters file is used and whenever it changes, which would force a full resync on every exclude edit.
- Two-way conflicts: the local side wins; the destination's copy is renamed with a `.conflict` suffix and stays on the destination only. Every conflict is listed in the summary block and in the run log, which is always kept.
- Non-interactive runs (`--json`, or stdout not a terminal) answer the folder-deletion prompt with "keep" unless `--folder-deletes` says otherwise.
- Build prerequisite: a Rust toolchain (`brew install rustup && rustup default stable`). Neither is installed on this Mac today; the first milestone installs Rust via rustup.

## Language and stack

**Decision: Rust** (your call, 13 Oct 2026). The app ships as one statically linked, compiled binary with no runtime, which is what you asked for, and the Rust ecosystem now covers every part of this design: ratatui for the screens, clap for the command line, tokio for the rclone subprocess and its output stream, serde for config and JSON, and cargo-dist for releases. cargo-dist is the piece that makes the later Homebrew install and update path cheap: it builds a universal macOS binary, publishes GitHub releases, writes the formula into a tap so `brew install` and `brew upgrade` work, and can add a self-update command through axoupdater when you want it. The trade-off accepted with Rust is that ratatui ships no form or picker widgets, so the account and job forms are built in-house (milestone 3 carries that cost), and compile cycles are slower than Go's. Mole's look is still the reference; only the toolkit differs.

| Concern | Crate | Why |
| --- | --- | --- |
| CLI, subcommands, completion | `clap` 4 (derive) + `clap_complete` | Generates bash/zsh/fish completion; typed flags for `--dry-run`, `--yes`, `--json` |
| TUI | `ratatui` + `crossterm` | Elm-style render loop; List, Gauge, Table, Paragraph, Block borders; alternate screen and raw mode |
| Text fields | `tui-input` | Single-line editing inside the in-house form component |
| Async runtime, subprocess, signals | `tokio` (process, io, sync, signal) | Spawn rclone, read stderr line by line on a task, send SIGINT on Ctrl-C |
| JSON | `serde` + `serde_json` | One `Deserialize` struct per rclone line shape; state file |
| Config | `toml_edit` (with `serde` feature) | Round-trips comments and unknown keys in the user's config |
| Errors | `anyhow` for the binary, `thiserror` for typed rclone errors | Error classification (`NeedsResync`, `TooManyDeletes`, `Locked`) is an enum |
| Paths | `directories` | `~/.config`, `~/.local/share` with platform fallbacks |
| macOS volume identity | `nix` (statfs) + `plist` | Mount point and device from statfs, UUID from `diskutil info -plist` |
| Tests | `cargo test`, `insta` (snapshots), `assert_cmd`, `tempfile`, `predicates` | Fixture-driven unit tests; golden snapshots of rendered screens; end-to-end CLI tests |
| Release | `cargo-dist` | Universal macOS binary, GitHub releases, Homebrew tap formula, optional self-updater |

One binary, no runtime dependency other than rclone on PATH. Minimum supported Rust version is the current stable; edition 2021. The crate name and binary are `onefolder`; the repository path is a placeholder until it exists on GitHub.

## Repository layout

A single crate. `src/cli/` holds thin clap handlers, every module with logic sits beside it under `src/`, unit tests live in the same files, and integration tests live under `tests/`.

```
onefolder/
├── Cargo.toml                # one crate, binary "onefolder"
├── src/
│   ├── main.rs               # clap parse, tokio runtime, dispatch
│   ├── cli/                  # one file per subcommand, flag parsing only
│   │   ├── mod.rs            # no-args → interactive main menu; --json/--dry-run globals
│   │   ├── accounts.rs       # list / add / remove / test / reconnect
│   │   ├── jobs.rs           # list / add / edit / remove / release
│   │   ├── run.rs            # run [job] --dry-run --yes --json --folder-deletes
│   │   ├── status.rs  doctor.rs  completion.rs
│   ├── config/               # TOML load/save (toml_edit), defaults, schema version, validation
│   ├── state/                # state.json: last-run records, holds; atomic writes
│   ├── model/                # Account, Job, Destination, RunRecord, Summary
│   ├── rclone/               # everything that touches the rclone binary
│   │   ├── detect.rs         # PATH lookup, version parse, minimum check
│   │   ├── remotes.rs        # config create / delete / show, about, reconnect
│   │   ├── exec.rs           # Command builder, tokio::process, stderr task, Ctrl-C
│   │   ├── events.rs         # JSON line → Event enum (Stats, Transfer, Delete, Plan, Error)
│   │   ├── listing.rs        # lsjson / size helpers
│   │   └── bisync.rs         # bisync flag set, plan parsing, error classification
│   ├── sync/                 # the semantics: Planner and Runner for one-way and two-way
│   │   ├── oneway.rs         # preview (copy --dry-run + dest listing) then copy
│   │   ├── twoway.rs         # dry-run plan → folder prompt → real bisync; resync; holds
│   │   ├── folders.rs        # deleted-folder detection from the plan
│   │   ├── reporter.rs       # Reporter trait: events out, prompt answers in
│   │   └── summary.rs        # per-destination summary accumulation
│   ├── volume/               # mod.rs: Identity + check(); macos.rs behind cfg(target_os)
│   ├── ui/                   # ratatui screens and theme
│   │   ├── theme.rs          # colours, glyphs (✓ ◎ → ➤ ● ○), borders
│   │   ├── form.rs           # in-house form component (fields, pickers, validation)
│   │   ├── menu.rs  accounts.rs  jobs.rs  jobform.rs
│   │   ├── progress.rs       # the run screen
│   │   ├── prompt.rs         # preview confirm, folder-deletion prompt
│   │   └── summary.rs        # bordered summary block (also used in plain mode)
│   └── jsonout/              # --json envelopes for non-interactive commands
├── tests/
│   ├── fixtures/rclone/      # recorded JSON log fixtures: copy, bisync plan, errors, stats
│   ├── fixtures/config/      # sample config files, including invalid ones
│   ├── snapshots/            # insta golden files for rendered screens
│   └── integration/          # runs only with ONEFOLDER_INTEGRATION=1 and rclone on PATH
├── docs/                     # ARCHITECTURE.md, screenshots
├── dist-workspace.toml       # cargo-dist: targets, Homebrew tap, installers
├── justfile                  # build, test, test-integration, lint, fixtures
├── DESIGN.md  README.md  TASKS.md  LICENSE (MIT)
└── rust-toolchain.toml
```

Two rules keep the layers honest: only `rclone::exec` may spawn processes, and only `ui` may depend on ratatui. `sync` talks to the UI through the `Reporter` trait (progress events out, prompt answers in), which is what lets the integration tests drive a two-way run with a scripted answer to the folder-deletion prompt.

## Data model

Two files: a hand-editable config that describes accounts and jobs, and a machine-written state file that records what happened. Destination type is a first-class field; a job owns an ordered list of typed destinations, and every destination carries its own mode and its own last-run record.

**Config:** `~/.config/onefolder/config.toml`

```toml
version = 1

[[accounts]]
label      = "work"                 # what the user sees; unique
remote     = "onefolder-work"       # rclone remote name, always "onefolder-" + label
drive_type = "business"             # personal | business
added      = 2026-10-13T09:40:00+11:00

[[jobs]]
name     = "documents"              # unique; used on the command line
source   = "/Users/bora/Documents"  # must be absolute and readable
excludes = [".DS_Store", "node_modules/**", "*.tmp"]   # rclone filter syntax

  [[jobs.destinations]]
  id      = "d-7f3a"                # stable short id; names log and bisync directories
  type    = "onedrive"
  account = "work"
  path    = "Backups/Documents"     # relative to the drive root
  mode    = "two-way"               # one-way | two-way

  [[jobs.destinations]]
  id          = "d-91c0"
  type        = "local"
  path        = "/Volumes/Backup/Documents"
  mode        = "one-way"           # default for local destinations
  volume_uuid = "A1B2C3D4-0000-4E5F-8A9B-000000000001"
  volume_name = "Backup"
  mount_point = "/Volumes/Backup"
```

**State:** `~/.local/share/onefolder/state.json`, keyed by job name and destination id, written atomically (temp file plus rename) after every run.

```json
{
  "version": 1,
  "runs": {
    "documents/d-7f3a": {
      "started": "2026-10-13T09:41:02+11:00",
      "finished": "2026-10-13T09:43:40+11:00",
      "mode": "two-way", "dry_run": false,
      "outcome": "ok",                  // ok | partial | failed | cancelled | needs-resync
      "added": {"local": 2, "remote": 14}, "updated": {"local": 0, "remote": 3},
      "deleted": {"local": 1, "remote": 0},
      "bytes": 184329113, "errors": 0,
      "log": "logs/documents/d-7f3a/2026-10-13T094102.jsonl",
      "rclone_version": "1.75.2"
    }
  },
  "holds": {
    "documents/d-7f3a": ["Old projects/2019"]   // folders kept on the destination but no longer synced
  },
  "bisync": {
    "documents/d-7f3a": {"initialised": true, "last_resync": "2026-09-01T10:00:00+11:00"}
  }
}
```

Rules enforced by `config` on load and by the forms on entry:

- Account labels and job names are unique, lowercase, and match `[a-z0-9][a-z0-9-]*`, so they are safe in rclone remote names and file paths.
- A `onedrive` destination must reference an existing account; a `local` destination must have an absolute path and, if it is under `/Volumes/`, a recorded `volume_uuid`.
- A destination's `id` is generated once and never edited; renaming a job keeps its ids, so logs and bisync state survive the rename.
- Unknown keys are preserved on save; a config with a newer `version` than the binary understands is refused with a clear message.
- Removing an account that a job still uses is refused until the job is edited (`accounts remove --force` deletes the destinations too).

**Added in milestone 3** (13 Oct 2026, engineering defaults recorded after review). Volume identity fields on a local destination: `volume_uuid`, `volume_name`, `mount_point`, plus `volume_partition_uuid` (diskutil's DiskUUID, used when there is no VolumeUUID) and `volume_device` (device node, or `<fstype>:<share>` for a network volume); only the strongest proof is recorded, and nothing is recorded for the startup disk. A source on an external volume gets the same fields with a `source_` prefix. The `/Volumes/` rule is therefore: a path there needs `volume_uuid`, or `volume_partition_uuid`, or `volume_device` plus `volume_name`. Extra validation rules: a local destination may not be the source, be inside it, or contain it (lexical at load; the form and flags also follow symlinks); a job may not list the same destination twice; exclude patterns may not be empty; `--onedrive <account>:<path>:<mode>` requires the mode, and an empty path means the drive root. Changing a destination's target or source, or switching it to two-way, moves its bisync working directory aside and drops its bisync state so the next two-way run starts with a merge; the state's `bisync` record stores the `source` and `destination` pair for that check. Renaming a job rewrites state keys and log paths first, then moves `logs/<job>` and `bisync/<job>`, then saves the config. Removing a job drops its state entries and archives `bisync/<job>`; logs are kept. Folders are created only on a verified drive, or inside an existing parent when no drive is recorded.

## Driving rclone

rclone is a subprocess, never a library. Every call goes through one `rclone.Command` builder that adds the global flags, pipes stderr through the JSON event decoder, tees every raw line into the per-run log file, and maps exit codes to typed errors.

**Detection.** On start-up the app runs `rclone version` from PATH. Missing binary, or a version below 1.66.0, prints a friendly message and exits with code 2:

```
OneFolder needs rclone 1.66 or newer, and it is not on your PATH.

  Install it with Homebrew:   brew install rclone
  Then run onefolder again.
```

The app never installs, updates or downloads rclone itself.

**Remotes and authentication.** One rclone remote per account, named `onefolder-<label>`, living in rclone's own config file; the app stores no tokens. `accounts add` suspends the TUI and runs `rclone config create onefolder-<label> onedrive drive_type=<personal|business>` with the terminal attached, so rclone's own flow opens the browser, waits on `127.0.0.1:53682` for the token, and uses the account's default drive (confirmed in milestone 2: `config create` takes rclone's defaults for every follow-up question, so choosing another drive or a SharePoint library needs rclone's `--all` flow and is a later enhancement). Afterwards the app verifies with `rclone about onefolder-<label>:` and reads `rclone config show onefolder-<label>` to record the drive type. `accounts test` runs `about` again; a failure whose message contains `invalid_grant` or `token expired` suggests `onefolder accounts reconnect <label>`, which runs `rclone config reconnect`. `accounts remove` runs `rclone config delete`.

**Global flags on every transfer command** (confirmed on 1.75.2):

| Flag | Purpose |
| --- | --- |
| `--use-json-log` | One JSON object per line on stderr with `level`, `msg`, `time`, `source`; per-file lines add `object`, `objectType`, `size`; dry-run lines add `skipped`; stats lines add a `stats` object |
| `--stats 1s --stats-log-level NOTICE` | A stats line every second regardless of verbosity, feeding the progress bar |
| `-v` | Per-file lines (`Copied (new)`, `Copied (replaced existing)`, `Deleted`) and bisync's plan lines |
| `--color never` | Without it the `msg` field carries ANSI escape codes even in JSON mode |
| `--fast-list` | Required by the brief for OneDrive; only takes effect when the remote has `delta = true`, which the account form offers as an option because delta listing is efficient only from the drive root |
| `--exclude <pattern>` (repeated) | Job excludes, in rclone's filter syntax; `--exclude-from <tempfile>` when there are more than 20 |
| `--modify-window 2s` | OneDrive stores modification times to 1 s; local disks to nanoseconds |
| `--retries 3 --low-level-retries 10` | rclone defaults, stated so they are explicit |
| `--dry-run` | Added when the user passes `--dry-run`, and on every planning pass |

Modification times are preserved by default on both backends; the app never passes `--no-update-modtime`.

**Operations and the rclone commands behind them**

| Operation | Command |
| --- | --- |
| One-way preview | `rclone copy <src> <dst> --dry-run` plus `rclone lsjson -R --files-only <dst>` to tell new from changed |
| One-way transfer | `rclone copy <src> <dst>` |
| Two-way plan | `rclone bisync <src> <dst> --dry-run --force --workdir <wd>` (see Two-way sync for the full flag set) |
| Two-way transfer | `rclone bisync <src> <dst> --workdir <wd>` |
| First two-way run or recovery | `rclone bisync ... --resync --resync-mode path1` |
| Restore a folder locally | `rclone copy <dst>/<folder> <src>/<folder>` |
| Folder size for the prompt | `rclone size <dst>/<folder> --json` |
| Account check | `rclone about <remote>:` |

**Exit codes.** rclone's codes are mapped to outcomes (corrected in milestone 4 against rclone 1.75.2 and its documentation): 0 success; 1 uncategorised error, which is what per-file failures produce, so `partial` when files were transferred or failed and `failed` otherwise; 2 usage error (an app bug, reported as such); 3 and 4 source or destination not found; 5 and 6 retryable or less serious errors, `partial` with the per-file error list; 7 fatal; 8 and 10 transfer or duration limit; 9 nothing transferred (treated as success); death by signal or 130 is `cancelled`. Milestone 4 deviations recorded: the preview dry run uses `--retries 1` because retries inflate the checks count that the preview relies on (the real copy keeps `--retries 3`); more than 20 exclude patterns go to `copy` and `lsjson` through an `--exclude-from` file written beside the run log, but only when no pattern has leading or trailing whitespace or a carriage return, because rclone trims lines in that file and would change their meaning (bisync still never uses a filters file). bisync abort messages are classified by substring into `needs-resync`, `too-many-deletes`, `locked` and `filters-changed`, each with its own user-facing explanation.

**Logs.** Every raw stderr line is appended to the run's `.jsonl` file as it arrives, so a crash mid-run still leaves a usable log. The command line that was executed is the first line of the file. `status` prints the log path for the last run; `doctor` lists the log directory size and offers to prune files older than 30 days.

## Progress capture and rendering

rclone's stderr is the single source of truth for progress. A tokio task decodes it line by line into typed events and sends them over a channel to the ratatui render loop; the screen never scrapes text and never polls.

*(Diagram in the Claude Doc: run pipeline · 7 parts, one subprocess.)*

The runner owns the subprocess and the decisions; the decoder owns the parsing; the screen only renders events and answers prompts.

**Event types** produced by `rclone::events`:

- `Stats` from every line carrying a `stats` object: `bytes`, `totalBytes`, `transfers`, `totalTransfers`, `checks`, `totalChecks`, `speed`, `eta`, `errors`, `deletes`, `fatalError`, `lastError`, and the `transferring` list with each in-flight file's name, bytes and percentage.
- `Transfer` from `-v` lines whose `msg` starts with `Copied` (new, replaced existing, or server-side copy); carries `object` and `size`.
- `Delete` from `Deleted` and `Removing directory`.
- `Skipped` from dry-run lines (`skipped` field set): what would have happened.
- `Plan` from bisync's `- Path1 File was deleted - <path>` style lines, parsed by a regex into side, verb and relative path.
- `Phase` from bisync's milestone lines: `Building Path1 and Path2 listings`, `Path1 checking for diffs`, `Applying changes`, `Updating listings`.
- `Error` from any `level: error` line, keeping `object` and `msg`; `Fatal` when bisync prints `Bisync critical error` or `Bisync aborted`.
- `Unknown` for anything else, kept only in the log.

**Phases shown on screen.** For `copy` the phase is Listing until the first stats line, Comparing while `checks` grows and `transfers` is 0, then Transferring, then Finishing after the process exits and the summary is being written. For `bisync` the phase follows the `Phase` events above. The overall bar uses `bytes / totalBytes` once `totalBytes` is known; before that it is an indeterminate spinner with the running file count.

**Rendering.** The progress model holds the latest `Stats`, a ring buffer of the last 8 `Transfer`/`Delete` events, and the error count. It redraws on each event, throttled to 10 fps. When stdout is not a terminal, or `--json` is set, the same events are instead written as newline-delimited JSON (`{"event":"stats",...}`) and the final summary as one JSON object, which is also what the integration tests read.

**Ctrl-C.** The first Ctrl-C sends SIGINT to rclone and shows "Stopping rclone..."; rclone finishes the current chunk and exits. If it has not exited within 10 s the app sends SIGKILL. The run is recorded as `cancelled` with the counts from the last stats line, the summary block is still printed, and for a two-way run the next start passes `--recover` so bisync picks up cleanly from its `.lst-new` listings (confirmed: an aborted run leaves `*.lst-new` files that `--recover` reconciles). A second Ctrl-C quits immediately.

## One-way sync

One-way is additive: `rclone copy` from the local source to the destination. New and changed local files are uploaded; nothing on the destination is ever deleted; files that exist only on the destination are left alone. The one exception is a move the user confirmed (milestone 10): a file the user moved or renamed locally, whose identical content already sits on the destination under the old name, is moved there on the server instead of being uploaded again, so its content is kept under the new name. This holds for OneDrive and local destinations alike.

**Sequence for one destination**

1. Validate: source exists and is readable; for a local destination, the volume check passes (see Local external-drive destinations); for a OneDrive destination, `rclone about` succeeds.
2. Preview: run `rclone copy <src> <dst> --dry-run` and collect the `Skipped` events (confirmed: each carries `skipped: "copy"`, `object` and `size`). In parallel, list the destination once with `rclone lsjson -R --files-only` to build a path set. A skipped file whose path is absent from the set is **new**, otherwise **changed**. Sizes come from the dry-run lines, so no local stat pass is needed.
3. Show the preview and wait: counts and bytes of new and changed files, the first 10 paths of each (`l` opens the whole of every shortened list, scrollable and filterable, and returns to the question), and the exclude patterns in force. `--yes` skips this step; `--dry-run` stops after it and prints the summary block marked "dry run".
4. Transfer: `rclone copy <src> <dst>` with the global flags. `Transfer` events feed the file list; the preview's classification labels each as added or updated in the summary.
5. Summarise and record: the bordered block, then the state file.

A change is whatever rclone's default comparison says: size or modification time differs, within the 2 s modify window. A file that only changed its modification time is re-uploaded, which matches rclone's behaviour and keeps the two sides equal.

**Summary fields:** files added, files updated, bytes transferred, duration, average rate, files skipped as unchanged, and an error list with path and rclone's reason. The deleted row shows 0 with the note "one-way never deletes". If the destination was listed with more files than the source, a single line notes how many destination-only files were left alone, so the user is reminded that one-way does not mirror.

## Two-way sync

Two-way is `rclone bisync` with the local folder as Path1 and the destination as Path2. Changes on either side propagate. Deletions propagate, with the folder prompt described in the next section as the single exception. Every two-way destination has its own bisync working directory, so each pair keeps independent listings.

**Flags on every bisync call**

| Flag | Reason |
| --- | --- |
| `--workdir ~/.local/share/onefolder/bisync/<job>/<dest-id>` | Isolates listings and the lock per destination |
| `--conflict-resolve path1 --conflict-loser num --conflict-suffix conflict-{DateOnly}` | Local wins a conflict; the destination's version is kept as `name.conflict-2026-10-13.ext` and stays on the destination only: the conflict suffix pattern is excluded from the sync, so it is never copied back. Every conflict is listed in the summary block and in the run log |
| `--create-empty-src-dirs` | Folder creation and deletion of empty folders propagate |
| `--resilient --recover` | Retry after minor errors and recover from an interrupted run without a resync |
| `--max-delete 50` (bisync's own percentage flag, default) | Keep the safety net unless the user has confirmed the deletions |
| `--compare size,modtime` (default) | OneDrive's QuickXorHash would require hashing every local file; not worth it for a sync |
| `--exclude ...` from the job | Never `--filters-file` (confirmed to force a resync on change) |

**Sequence for one destination**

1. Validate as for one-way, plus: if the working directory has no `.lst` listings, this is a first run and needs `--resync` (step 2). If a `.lck` file exists, read the PID in it; if that process is alive, refuse with "another OneFolder or rclone run is syncing this destination"; if not, say the lock is stale and offer to remove it.
2. First run: explain and ask. "This is the first two-way run for this destination. OneFolder will merge both sides: files that exist on only one side are copied to the other, and where the same file differs the local copy wins (the other copy is kept with a .conflict suffix). Nothing is deleted during a first run." On yes, run `bisync --resync --resync-mode path1`, then stop with the summary; deletions start propagating from the second run.
3. Plan: `bisync --dry-run --force` with the flags above. `--force` is needed because bisync applies its too-many-deletes check even in a dry run and stops before listing the plan (confirmed); the dry run changes nothing. Parse `Plan` events into: new, modified and deleted files per side, and conflicts.
4. Detect deleted local folders from the plan (next section). Prompt if there are any.
5. Preview: show counts and bytes per direction, the deletions that will happen on each side (destination-side deletions are always listed), conflicts, and the folder decisions. `--yes` skips the confirmation but never the folder prompt. `--dry-run` stops here.
6. Transfer: `bisync` without `--dry-run`, plus `--max-delete 100` only if the plan's deletions exceeded 50% and the user confirmed the preview showing those exact counts, plus `--exclude` entries for folders on hold.
7. Summarise and record. On a `Bisync critical error` the state gets `needs-resync` and the next run starts at step 2 with an explanation of what happened.

**Error handling in words, not rclone output**

- "cannot find prior listings" or "out of sync": the app says the sync state for this destination was lost or damaged, names the log, and offers the resync path from step 2.
- Too many deletes in the real run (should not happen after step 6, but is caught): the app shows the percentage and the count and offers to re-run with the safety limit lifted.
- Empty side: bisync refuses to sync to an empty folder without a resync; the app explains that the folder looks empty and asks whether the drive is mounted before offering a resync.
- Interrupted run: handled by `--recover`; the user only sees "recovering from the interrupted run on 13 Oct".

**Added in milestone 5** (13 Oct 2026, recorded after review against rclone 1.75.2). Conflicts: bisync's own `--conflict-loser num` cannot see conflict copies that are excluded from the sync, so it overwrites an earlier same-day copy; OneFolder therefore renames the destination's copy itself with `rclone moveto` before the real run, to `name.conflict-<YYYY-MM-DD>-<n>.ext` where n is the first number free on both sides (checked with unfiltered listings; the extension is the part after the last dot, so `arch.tar.gz` becomes `arch.tar.conflict-2026-10-13-1.gz`). bisync then sees the file changed locally and gone on the destination and simply uploads, so nothing is copied back. The exclude pattern is `*.conflict-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*` and is shown in the preview. bisync keeps a fallback suffix `race-conflict-{DateOnly}-` for a file that changes between the plan and the run; that name is deliberately not excluded, so such a copy syncs to both sides. On a first run `--resync` keeps no loser, so OneFolder runs `--resync --dry-run` first and renames each destination file that would be overwritten the same way. Plan paths are taken verbatim (leading and trailing spaces are significant); an unreadable prior listing fails closed into the merge offer. The max-delete rule is strictly greater than 50%; held folders and conflict copies count as deletions for bisync, and the limit is lifted without asking only when those alone push it over. The folder prompt highlights Keep by default. Ctrl-C on bisync waits up to 45 seconds for it to save its listings and release its lock (a second Ctrl-C kills it, and OneFolder removes the stale lock); a lock counts as live only if its PID is alive, is an rclone process, and was renewed after boot; `--yes` removes a stale lock without asking and still prints the first-run explanation. A `--dry-run` on a terminal never asks the folder prompt. The "more than half deleted, no terminal" refusal exits 1. bisync's working file names embed both paths and must fit 255 bytes, reported as "names too long".

**Several two-way destinations on one job.** Destinations run in sequence, each against its own listings. A change that arrives from destination A during this run is on local disk by the time destination B runs, so B receives it in the same run if B comes later in the list, and otherwise on the next run. The README states the simple rule: a change from one destination reaches the others no later than the next run.

## Folder-deletion prompt (two-way)

When a whole folder has vanished from the local side since the last run, the app asks before bisync is allowed to delete it on the destination. Deleted files, and anything deleted on the destination side, propagate without a prompt.

**Detection.** The dry-run plan lists every local deletion as a `Plan` event with side Path1, verb "File was deleted" and a path relative to the root (confirmed; the later "Queue delete" lines use absolute destination paths and are not needed). The algorithm:

1. Take the set D of relative paths from Path1 "File was deleted" events.
2. For each path, walk up its parent directories. The highest ancestor that no longer exists on local disk (`os.Stat` fails with not-exist) is that file's "deleted folder". Files whose parent still exists are plain file deletions and need no prompt.
3. Group by deleted folder. A folder qualifies for the prompt when every file the previous listing had under it is in D, which is checked against the prior `.path1.lst` listing in the working directory (confirmed format: one line per file with size, modtime and quoted path). If some files under it survived locally, it was not a whole-folder deletion and is treated as file deletions.
4. For each qualifying folder, run `rclone size <dst>/<folder> --json` to get the remote file count and bytes to show.

**The prompt**, one per folder, before the preview:

- **Delete on the destination.** The deletion is left in the plan; bisync removes the files and the folder.
- **Keep on the destination and stop syncing it.** The folder is added to the destination's hold list in the state file, and every later bisync for this destination gets `--exclude /<folder>/**`. Confirmed on 1.75.2: excluding the folder on this run leaves it untouched on the destination, and bisync does not complain when a plain `--exclude` differs between runs. The hold is listed by `status` and in the job's edit form, and can be released with `onefolder jobs release <job> <folder>`. Releasing it makes the folder look new on the destination, so the next run copies it back locally; the user is told this when they release.
- **Restore locally.** Before the real bisync the app runs `rclone copy <dst>/<folder> <src>/<folder>`, so the folder is back on both sides and bisync sees no change for it.

`Skip this run` is not offered as a separate choice because bisync has no way to defer a single deletion: whatever the listings record at the end of the run is the new baseline. The hold is the honest version of "skip": it keeps the folder safe and makes the decision visible and reversible.

**Non-interactive runs.** `--folder-deletes=ask|delete|keep|restore` controls the answer; the default is `ask` on a terminal and `keep` when `--json` is set or stdout is not a terminal, so an unattended run can never delete a folder on the destination. `--yes` on its own does not answer this prompt.

**Edge cases handled**

- A folder renamed locally shows up as a deleted folder plus new files elsewhere; the prompt mentions "this may be a rename" when the plan also contains the same number of new local files, and "delete" is still the right answer since bisync will upload the renamed copy.
- Nested deleted folders prompt once, for the highest one.
- More than 20 deleted folders in one run: the prompt switches to a list with a single answer for all, plus a per-folder override.
- The folder prompt and the too-many-deletes limit interact: if the user chooses delete and the deletions exceed 50%, the preview says so in red and the real run gets `--max-delete 100` only after the preview is confirmed.

## Local external-drive destinations

A local destination is `rclone`'s `local` backend pointed at a path on another volume. The danger is specific to macOS: when the drive is unplugged, `/Volumes/<name>` is either absent or an empty folder on the boot disk, and a two-way sync would read that as "everything was deleted on the destination" and propagate it. One-way is safer but would still fill the boot disk with a copy. So the app proves it is looking at the right drive before every run.

**At setup** (`jobs add`, local destination): the app resolves the path, finds its mount point by walking up until `statfs` reports a different device, and records three things: `mount_point`, `volume_name` (from `diskutil info -plist <mount point>`, key `VolumeName`), and `volume_uuid` (key `VolumeUUID`). If `diskutil` reports no UUID (some exFAT drives), the device node and volume name are recorded instead and the user is told the check will be weaker.

**Before every run**, for the destination and for a source that is itself on an external volume:

1. The path must exist and be a directory.
2. `statfs` on the path must report the recorded `mount_point` as its mount point, not `/`.
3. `diskutil info -plist` on that mount point must report the recorded `volume_uuid` (or, failing that, device node and name).
4. The volume must not be read-only for a destination.

Any failure stops the run with a message such as:

```
✗ Backup drive not mounted
  Destination /Volumes/Backup/Documents expects volume "Backup"
  (UUID A1B2C3D4-...). /Volumes/Backup is currently a folder on
  Macintosh HD. Plug the drive in and run again.
```

`doctor` runs the same check for every local destination and reports mounted / not mounted / wrong volume. **Added in milestone 6** (13 Oct 2026): a drive that is mounted somewhere other than the recorded mount point is reported as "mounted somewhere else" with the actual mount point (found with one `diskutil info -plist <uuid>` call) and the single fix is to change the destination's path in `jobs edit`, which records the drive again; `jobs edit --rerecord <id>` never replaces a recorded drive with none, and `--forget-drive <id>` is the only way to drop one. Every writer of state.json takes a shared file lock on `state.lock`, because runs of different jobs may overlap. Each job has a run lock at `runs/<job>.lock` (created atomically with the PID; stale when the PID is dead, not a OneFolder process, or older than the last boot) that `run`, `--dry-run`, `jobs edit`, `remove` and `release` all take without waiting. Dry runs are recorded under a separate `last_dry_run` map so they never overwrite the last real run. `status` makes no network call. Exit codes for a run the user declined at the preview or stopped with Ctrl-C are 1, so scripts notice nothing was synced. On Linux later, step 3 becomes `findmnt -no UUID`; the interface in `volume` takes a path and returns an `Identity`, so the macOS implementation is a module behind cfg(target\_os).

**Added in milestone 14** (15 Oct 2026, Linux and WSL): the Linux implementation reads the mount of a path from `/proc/self/mountinfo` (the longest mount point that holds the path and has its device, so a file system mounted over the drive later hides it) and the drive's identity from `lsblk -J` (UUID, else PARTUUID, else device and label), unprivileged, rather than `findmnt`; the mount's root (a btrfs subvolume, a bind-mounted folder) is recorded and compared too. On Linux every mount is a drive, recorded and checked, except the system disk: `/`, the standard system mount points (`/boot`, `/home`, `/var`, `/tmp` and the like) and file systems with no disk behind them (`tmpfs`, `overlay`, `squashfs`), as `/` and `/System/Volumes/...` on macOS. Under WSL, Windows drives are drvfs mounts, identified by their Windows volume GUID through PowerShell's `Get-Volume` (a check may take up to 30 seconds there), and `/mnt/c` is the system disk. Move detection compares names ignoring case where either side ignores it, and on Linux only known file systems (ext4, btrfs, xfs and the like, without the casefold flag) count as telling case apart, so a case-only rename between two ext4 folders is a move and one onto a FAT drive or a network share never is. See docs/ARCHITECTURE.md, "Platforms".

## Screens and commands

The tone follows Mole: a bold title, one blank line, calm sections with `➤` for the current item, `✓` done, `◎` skipped with a reason, `→` for an action taken, `●`/`○` for selection, `█`/`░` bars, and a summary block framed by rules. Arrow keys and `j`/`k` move, `Enter` selects, `Esc`/`q` go back, `/` filters lists, `?` shows keys. Colour is used sparingly: one accent for the cursor and bars, green for success, yellow for skipped and warnings, red for errors; everything else is the terminal's default foreground.

**Commands**

| Command | Behaviour | Flags |
| --- | --- | --- |
| `onefolder` | Interactive main menu |  |
| `onefolder accounts [list\|add\|remove\|test\|reconnect] [label]` | Manage OneDrive accounts; `list` and `test` support `--json` | `--force` on remove |
| `onefolder jobs [list\|add\|edit\|remove\|release] [name]` | Manage jobs; `add`/`edit` open the form; `release` lifts a hold | `--json` on list |
| `onefolder run [job] [--dest id]` | Run one job, or pick from a list; all destinations in order, or one | `--dry-run`, `--yes`, `--json`, `--folder-deletes`, `--resync` |
| `onefolder status [job]` | Last run per destination: when, outcome, counts, log path | `--json` |
| `onefolder doctor` | rclone, accounts, paths, volumes, state, log size | `--json`, `--prune-logs` |
| `onefolder completion <shell>` | bash, zsh, fish |  |
| `onefolder version` | Version, commit, rclone version found | `--json` |

`--dry-run` is accepted on every command that could change files or config. `--json` makes a command non-interactive: no prompts, no TUI, one JSON document (or NDJSON for `run`) on stdout, exit code 0 on success, 1 on partial, 2 on failure or refusal.

**Main menu** (`onefolder`)

```
OneFolder

⚙ rclone 1.75.2 · 2 accounts · 3 jobs · last run 2h ago

➤ Run a job
  Jobs
  Accounts
  Status
  Doctor
  Quit

↑↓ j/k move · enter select · q quit
```

**Jobs list** (`onefolder jobs`)

```
Jobs  3 jobs · 4 destinations

➤ documents      ~/Documents
      → work:Backups/Documents          two-way   ✓ 2h ago
      → /Volumes/Backup/Documents       one-way   ◎ not mounted
  photos         ~/Pictures/Photos
      → personal:Photos                 one-way   ✓ yesterday
  notes          ~/Notes
      → work:Notes                      two-way   ✗ failed 3d ago

enter run · a add · e edit · d remove · q back
```

**Run progress** (`onefolder run documents`)

```
Run documents

→ work:Backups/Documents   two-way   destination 1 of 2

  Transferring · 42 of 118 files
  ██████████████░░░░░░░░░░░░░░░░░░░░  41%   312.4 MB / 760.1 MB
  12.8 MB/s · ETA 35s · errors 0

  ✓ Projects/2026/brief.docx                       1.2 MB
  ✓ Projects/2026/budget.xlsx                     248 KB
  ↑ Projects/2026/deck.pptx                        62%
  ↓ Shared/meeting-notes.md                        14 KB   from OneDrive
  ✗ Projects/2024/old.mov   OneDrive: file too large (250 GiB limit)

ctrl-c stop
```

**Preview before a one-way transfer**

```
→ /Volumes/Backup/Documents   one-way

  New         37 files   412.6 MB
  Changed      5 files    18.2 MB
  Unchanged  981 files
  Excluded   .DS_Store, node_modules/**, *.tmp

  Nothing will be deleted on the destination.

  Continue? [Y/n]
```

**Folder-deletion prompt (two-way)**

```
➤ Folder deleted locally

  Old projects/2019
  On OneDrive: 1,284 files, 3.4 GB

  Not on your Mac any more. What should happen on OneDrive?

  ● Delete it on OneDrive too
  ○ Keep it on OneDrive and stop syncing this folder
  ○ Restore it to this Mac from OneDrive

  ↑↓ choose · enter confirm · esc cancel run
```

**Summary block** (one per destination, Mole-style)

```
======================================================================
documents → work:Backups/Documents   two-way   complete
Added: 14 to OneDrive, 2 to Mac | Updated: 3 | Deleted: 0 on OneDrive, 1 on Mac
Transferred: 312.4 MB in 1m 38s (3.2 MB/s) | Errors: 1
  ✗ Projects/2024/old.mov   OneDrive: file too large (250 GiB limit)
Log: ~/.local/share/onefolder/logs/documents/d-7f3a/2026-10-13T094102.jsonl
======================================================================
```

**Doctor** (`onefolder doctor`)

```
Doctor

➤ rclone
  ✓ /opt/homebrew/bin/rclone · 1.75.2 (minimum 1.66)

➤ Accounts
  ✓ work        business   token ok   1.2 TB free
  ✗ personal    personal   token expired · run: onefolder accounts reconnect personal

➤ Jobs
  ✓ documents   source ok
      ✓ work:Backups/Documents       reachable
      ◎ /Volumes/Backup/Documents    not mounted (expects volume "Backup")
  ✓ photos      source ok

➤ Data
  ✓ config ~/.config/onefolder/config.toml (version 1)
  ✓ state ~/.local/share/onefolder/state.json
  ✓ logs 48 files, 12.4 MB · onefolder doctor --prune-logs removes those older than 30 days

======================================================================
1 problem · 1 warning
======================================================================
```

## Testing strategy

Three layers, all run by `just test` (unit and snapshot) and `just test-integration` (needs rclone on PATH); a milestone is not done until both pass and `cargo clippy` is clean.

**Unit tests** (`cargo test`, no rclone needed)

- `config`: round-trip of the sample config, every validation rule, unknown keys and comments preserved, newer-version refusal, migration hook.
- `model`: destination defaults (local defaults to one-way), id generation, rename keeps ids.
- `rclone::events`: recorded JSON fixtures under `tests/fixtures/rclone/`, captured from real rclone 1.75.2 runs during the build: copy dry-run, copy real run with stats, bisync resync, bisync dry-run plan with deletions on both sides, too-many-deletes abort, missing-listings abort, filters-file abort, lock-file abort, a run with per-file errors, and a stats line with an in-flight `transferring` list. Each fixture has an expected event sequence as an insta snapshot.
- `rclone::bisync`: error classification by substring for every abort message seen in the fixtures; `.path1.lst` listing parser.
- `sync::folders`: deleted-folder detection over synthetic plans and listings: whole folder, partial folder, nested folders, rename-shaped plan, more than 20 folders.
- `volume`: the mount-point walk and the `diskutil` plist parser over recorded plist output; the "unplugged drive" case is simulated with a temp directory whose recorded identity does not match.
- `ui`: insta snapshots of the menu, jobs list, preview, folder prompt and summary block rendered into a `TestBackend` buffer at 80 and 120 columns; key handling for arrows and vim keys.

**Integration tests** (`ONEFOLDER_INTEGRATION=1 cargo test --test integration`)

Each test gets a temp directory with a private `RCLONE_CONFIG`, a source folder, and two destinations: a plain local path and an rclone alias remote named `onefolder-test` pointing at another temp folder, so the OneDrive code path (`remote:path` addressing, `about`, `lsjson` on a remote) is exercised without a Microsoft account. Files are seeded with `rclone copy` so modification times match (confirmed: `cp -R` without `-p` makes the first bisync fail on modtime). The scripted `Reporter` answers prompts.

- One-way: new and changed files copied, destination-only files untouched, excludes honoured, `--dry-run` changes nothing, preview counts match what was transferred, summary counts match the filesystem.
- Two-way: first run requires and performs resync; second run propagates adds, updates and file deletes both ways; a conflict keeps both copies with local winning; a destination-side deletion propagates to local without a prompt; Ctrl-C mid-run (SIGINT to the test's own rclone) followed by a recovery run.
- Folder prompt: delete a local folder, run with each of the three answers, assert the destination and local trees afterwards; `keep` adds a hold and the next run leaves the folder alone; `release` brings it back; non-interactive default is `keep`.
- Safety: deleting more than half the files triggers the confirmation path; a stale `.lck` is detected and removed; a damaged working directory produces the resync offer.
- Multi-destination: a job with a local and an alias destination runs both in order and writes two run records; the second destination still runs after the first fails.
- Volume check: a destination whose recorded identity does not match the temp directory refuses to run with the expected message.
- `--json` output of `run`, `status` and `doctor` validates against the documented shapes.

**Continuous integration.** GitHub Actions on macOS runners: `cargo fmt --check`, `cargo clippy -D warnings`, unit and snapshot tests, then `brew install rclone` and the integration suite. Fixtures are regenerated with `just fixtures`, which re-runs the recording script against the installed rclone and fails if the event parser's output changes, so a new rclone release that alters its log lines is caught deliberately rather than in a user's run.

## Milestones

Seven milestones, each delivered by an Opus implementation agent and checked by a separate Opus review agent against this document before I accept it. Milestones 2 and 3 run in parallel; 4 and 5 share the runner and progress screen, so 5 starts when 4's runner is merged. The task list below is the live tracker: I will update it as work moves, and it goes into the repo as `TASKS.md` so you can see it from the terminal too.

| # | Milestone | Scope | Done when |
| --- | --- | --- | --- |
| 0 | Design | This document; `DESIGN.md` committed | You approve it |
| 1 | Scaffold | Rust toolchain via rustup, crate with clap skeleton and every subcommand stubbed, config load/save and validation, rclone detection, `doctor`, `version`, theme and summary block, LICENSE, justfile, CI | `onefolder doctor` reports rclone and config; unit tests for config and detection pass |
| 2 | Accounts | add (browser flow via rclone), list, remove, test, reconnect; `--json` | A real OneDrive account can be added on your Mac and `doctor` shows it; tests use the alias remote |
| 3 | Jobs | Data model with typed destinations, in-house form component, interactive add/edit, list, remove, volume identity capture for local destinations | Jobs with both destination types can be created and edited; config round-trips |
| 4 | One-way run | Event decoder with fixtures, runner, preview, progress screen, summary, `--dry-run`, `--yes`, `--json`, Ctrl-C, per-run logs, state records | Integration suite for one-way passes against both destination types |
| 5 | Two-way run | bisync flag set, resync flow, plan parsing, folder-deletion prompt with the three answers and holds, conflict reporting, error classification, `jobs release` | Integration suite for two-way and the folder prompt passes |
| 6 | Multi-destination and volumes | Sequencing, per-destination summaries, volume check before every run, `status` | Multi-destination and volume-check tests pass; unplugged-drive refusal demonstrated |
| 7 | Polish and release | Main menu, completion, README, ARCHITECTURE.md, cargo-dist config, Homebrew tap formula, release build, log pruning | A tagged release produces a universal macOS binary and `brew install <tap>/onefolder` works; README reviewed by you |

*(Diagram in the Claude Doc: milestone roadmap · 8 steps, 1 parallel pair.)*

Every arrow is a gate: tests pass and an Opus review against this design before I accept the work. Milestone 0 is where we are now; nothing below it starts until you say go.

After each milestone you get a short summary in chat with the command to try it, and the task list is updated. Anything that would change these semantics or the screens comes back to you as a question first.

**Added in milestone 7** (13 Oct 2026). Logs are never deleted automatically (decision 2); `doctor --prune-logs` is explicit, removes logs older than 90 days or beyond the newest 50 per destination, and never removes the last run or last dry run of a destination, a log that records a conflict, or logs of jobs and destinations no longer in the config (this replaces the earlier "offer to prune after 30 days" wording). The Homebrew formula declares `depends_on "rclone"`; OneFolder still never installs rclone itself. `accounts remove` needs `--yes` or `--force` whenever it cannot ask, including with `--json`. Ctrl-C quits from any screen with the terminal restored (exit 130), except on the run screen where the first press stops the run. Volume checks and the mount probes used by the menu and jobs list give up after 10 s and 2 s respectively and report "not answering". Release: cargo-dist builds aarch64 and x86_64 archives, a custom publish job joins them into a universal binary with `lipo`, the formula goes to `OWNER/homebrew-onefolder`, and the toolchain is pinned to 1.93.1.

**Added in milestone 8** (13 Oct 2026, after Bora's first real run on a 62,000-file folder). Built-in excludes: every rclone call that walks a tree gets `rclone::BUILTIN_EXCLUDES` (`.DS_Store`, `._*`, `.localized`, `.Spotlight-V100/**`, `.Trashes/**`, `.fseventsd/**`, `.TemporaryItems/**`, `.DocumentRevisions-V100/**`, `[Tt]humbs.db`, `[Ee]hthumbs.db`, `[Ee]hthumbs_vista.db`, `[Dd]esktop.ini`, `$RECYCLE.BIN/**`, `System Volume Information/**`) and, unless the job has `copy_hidden = true` (config key, default false, written only when true or already present; form field "Copy hidden files", flag `--copy-hidden yes|no`), `HIDDEN_EXCLUDES` (`.*`, `.*/**`). The effective list is built-ins, hidden, then the job's own patterns with duplicates of the built-ins dropped; the preview shows the job's own patterns, "Hidden files: not copied/copied" and an "Always excluded" summary; the JSON preview and plan carry `copy_hidden` and `effective_excludes`. Bisync always gets repeated `--exclude` flags (the exclude-from path was removed for bisync); copy and lsjson still switch to `--exclude-from` above 20 patterns. Conflict naming listings stay unfiltered. Two-way upgrade rule: entries present in a prior listing but now excluded appear to bisync as deleted on both sides; they delete nothing and need no resync, but they are phantoms, so the more-than-half check counts them out of both the deletions and the totals (held folders likewise), the limit is lifted silently only when phantoms alone push bisync over 50%, and folder detection drops them so a hidden or junk-only folder deleted locally gets no prompt. The folder prompt's Delete answer now also removes what the sync could not see: after a fully successful bisync, if the folder is still gone locally, still exists on the destination, and a filtered listing (excludes only, so conflict copies count) finds nothing the sync handles, OneFolder runs an unfiltered `rclone purge` on it, logs `folder-removed`, and lists it in the summary as `removed_folders`; otherwise the folder stays and a note says why. Restore and the prompt's size measurement use per-folder filters (`rclone::folder_filter_args`: the excludes, then `+ /<folder>/**`, then `- **`) so excluded content never comes back. Preview progress: the dry run's stats (`checks`, `listed`, `transfers`, `bytes`) and a streaming `lsjson` reader feed live figures once a second (900 ms tolerance) on the run screen for Previewing and Planning, one plain-text line per 10 s without a terminal, and a `progress` NDJSON event; counts use thousands separators throughout. Known limits recorded: `._*` also drops a deliberately named `._file`; Finder's `Icon\r` is not excluded; bisync's `--create-empty-src-dirs` still creates an empty folder for a folder whose only contents are excluded; previously synced hidden files on a two-way destination stop syncing and drift apart, and turning `copy_hidden` on later can produce conflict copies.

**Added after milestone 8** (14 Oct 2026, from Bora's Engineering preview showing "and 29,142 more"). The run's confirmation screens (the one-way preview and the two-way plan) carry the full lists they shortened: `ui::prompt::FullList` holds each cut list with its title, total bytes and files, and `preview_screen`/`plan_screen` return the screen body and those lists together so the key text and the data cannot be separated. The shortened-list line reads `and N more · l whole list`, and the hint gains `l whole list` only when something was cut. `l` opens a view of every shortened list in full, in the order the confirmation shows them, with the count and size in each heading, the current heading pinned while scrolling, a position line (`1–38 of 29,152`, with thousands separators), `↑↓`/`j k`, PageUp/PageDown/space, `g`/`G`, and a `/` filter that matches case-insensitively on NFC-normalised paths (Enter keeps the filter, Esc clears it, a second Esc closes). Inside the view `y`, `n` and Enter answer nothing, so reading the list cannot start a copy; Ctrl-C still answers no as it does on the confirmation. `Esc`, `q` or `l` return to the question with its scroll untouched. Only the rows on screen are built per draw, and paths are shortened from the front by display width (`unicode_width`), so 100,000 paths draw in about a millisecond and a filter keystroke is one pass over cached lowercase paths. Plain output, `--dry-run` printouts and the NDJSON preview are unchanged (still the first 10 paths). Yes/no questions and the resync offer have no `l`. Known limit: the hint line is cut below 76 columns.

**Added in milestone 10** (14 Oct 2026, asked by Bora: moved files were re-uploaded and the old copies left as duplicates). Move detection for one-way runs. rclone's `--track-renames` works only with `sync`, so OneFolder detects moves itself. The preview now runs three things side by side: the dry run, the destination listing (with `--hash-type quickxor` on OneDrive, where the hash comes free with the listing; never on a local destination, where it would read every file) and a listing of the local source with the same excludes, so destination-only files are an exact set, compared NFC-normalised and case-folded because every destination OneFolder supports is case-insensitive. `sync::moves` pairs each new file (size above 0) with destination-only files of the same size, confirms identical content by QuickXorHash (`rclone hashsum quickxor --files-from-raw -` on the source, and on a local destination, for the candidates only), breaks ties by the longest matching relative path under a renamed parent, then modification time within 2 s, then leaf name, accepts a pair only when it is the unique best for both files, uses each destination file once, and uploads anything still tied; a file that moved and changed is simply new. Whole-folder renames (two or more files, every listed file under the old folder mapping to the new one with the same relative path, nothing under the new folder) are one `rclone moveto` of the folder, re-checked at execution against an unfiltered listing of the old folder by path; otherwise files move one by one, four at a time, after their target parents are created one by one. The preview gains a `Moved` row ("already on OneDrive under another name") and a `Moved files` section of `old → new` lines (folder groups with a file count), both in the whole-list view; the question `Move them on OneDrive instead of uploading them again? [Y/n]` comes before `Continue?`; Enter or y moves, n, Esc or q uploads again (never skips the destination), Ctrl-C cancels. Flag `--moves ask|move|copy`: no flag asks on a terminal and moves with `--yes` or without a terminal; an explicit `ask` without a terminal uploads again and says so; `--dry-run` moves nothing and shows what would move. Moves run as a `Moving` step before the copy with live figures, each logged (`move`, `moved`, `move-failed`, `moves-found`, `moves-decided`, `move-folder-split`, `moves-not-checked`); a failed move becomes a per-file error and the copy uploads that file; the first Ctrl-C lets running moves finish and starts no more, a second aborts them. The summary gets `Moved: N files, X (on OneDrive, no upload)` and the JSON `moved` only when something moved; the Deleted row stays `0 (one-way never deletes)`, since a move keeps the content. The preview's "Nothing will be deleted" line says moved files keep their content under the new name, and the left-alone count leaves out the files that move. Known limits: identical twins, zero-byte files and anything still ambiguous are uploaded and their old copies stay; a copy-and-delete "move" whose time differs by more than 2 s is moved and then has its time updated by the copy; the summary's Unchanged count does not include files just moved; a destination-only file edited remotely between preview and confirmation would be moved and then overwritten by the copy. Two-way runs do not detect moves (bisync removes the old copy on the other side, so they re-upload but leave no duplicates); decided 16 Oct 2026 with Bora not to add it: a rare case, hard to test, and the user can rename the file in both places.

**Added in milestone 11** (14 Oct 2026, after Bora's exclude `_0RAG/*` still let `_0RAG/backend/...` through). rclone's filter syntax never lets `*` cross a `/`, so `folder/*` matched only the files directly inside the folder and a bare `folder` matched nothing but a file of that name (confirmed on 1.75.2). Job patterns are therefore widened in `rclone::widen_exclude` before they reach rclone, in what rclone receives only: config, the edit form and the preview's Excluded row keep them as typed, and the JSON `effective_excludes` shows the widened list. Rules: whitespace at both ends is trimmed first (rclone itself trimmed only the last `--exclude` value, so a pattern's meaning depended on its position); a pattern ending in `/*` becomes `/**` unless nothing but slashes precedes it (`/*` stays the root's files); a pattern with none of `* ? [ { \` that does not end in `/` and is not `.` or `..` (after one optional leading `/`) yields itself and itself plus `/**`, so `_0RAG`, `a/b` and `/Archive/2019` each leave out the folder and everything in it as well as a file of that name; everything else (wildcards, trailing `/`, `/**`) passes through. Built-in and hidden patterns are not widened. Duplicates are dropped and order kept; widening can push a job over the 20-flag limit into the `--exclude-from` file, which holds the widened list too. Bisync accepts the changed flags without a resync, and files newly excluded on a two-way destination are phantoms that delete nothing (integration-tested). The directory rule `folder/**` also lets rclone skip walking the folder.

**Added in milestone 12** (15 Oct 2026, after Bora saw his shell history flash back between screens). The interactive session (`onefolder` with no arguments) takes raw mode and the alternate screen once, in `ui::session::start` from `cli::menu`, and leaves them once when the menu quits, on Ctrl-C, on an error, or on SIGTERM/SIGHUP (the signal handler is installed at session start, not at the first run). Every screen gets its terminal from `ui::screen()`: inside a session it borrows the session's terminal (a static slot; a second borrow while one is out is an error, never a second alternate screen); outside one it initialises its own and restores on drop, so direct subcommands behave as before. Text the session would have printed (summary blocks, status, doctor, listings, notes, errors) goes through `ui::out`/`ui::err` onto a page decoded back into tones and shown on a scrollable output screen (2-column margin, starts at the end, ↑↓ j/k PgUp/PgDn g/G, any other key back, hint `esc back · ↑↓ pgup/pgdn scroll · g/G top/bottom`; rule lines are clipped, paths are never split with an indent); pending keys are drained before it opens, and Status and Doctor show `Checking…` while they work. The job picker, label prompts and confirmations draw on the session terminal; rclone's attached sign-in gets the terminal handed over with the page shown. Inside a session a run never falls back to the plain reporter. A transcript of every summary block, multi-destination block, problem and skipped line is printed to the shell, from column 0, when the session ends, so the Log paths stay in the scrollback; nothing is printed if nothing ran. Known limits: a panic in a screen thread leaves the alternate screen through ratatui's hook while the session continues; the signal hook can in theory interleave with a frame being drawn in the instant before exit.

**Added in milestone 13** (15 Oct 2026, asked by Bora, modelled on Mole's disk browser). Folder fields (the source, a local destination folder, a OneDrive destination folder) get Tab completion and a browse screen. Completion (`ui::complete`): the last path segment is matched against the folders in its parent, NFC and case-folded, hidden names only after a dot, sorted; Tab inserts the first match plus `/`, Tab and Shift-Tab cycle with wrap, any other key carries on typing; `~` stays as typed; the hint shows `2 of 3: Desktop, [Documents], Downloads`, `no matching folder`, or for OneDrive `listing…` while `rclone lsjson --dirs-only` of the parent runs in the background (cached for the form; a Tab for another parent replaces the pending request). Inside a folder field edit, Tab no longer moves to the next field; Enter ends the edit, ↑↓ move on. Browse (`ui::browse`): `b` on a folder field or Ctrl-B while editing (which commits the text first); a full screen on the form's terminal with `Choose a folder`, the free space (`statvfs`, or `rclone about --json`), the current folder, one row per subfolder with a bar relative to the rows shown, the name and the size, and `N files here, X` under the list. Keys: ↑↓ j/k, Enter/→/l open, h/←/Backspace up, Space chooses the highlighted folder, c the current one, `.` toggles hidden folders, `/` filters, r refreshes, g/G, PgUp/PgDn, `~` home and `v` /Volumes (local), Esc back; Ctrl and Alt keys are ignored. Local sizes: a listing thread per opened folder and a pool of four walkers (apparent sizes, files only, no symlinks, one file system, unreadable parts shown as `+`), running totals at most every 100 ms with a dim `…`, a generation number per opened folder so stale totals are dropped, cancellation through an AtomicBool checked between directories and every 512 entries, a size cache for the life of the form; a subfolder that is a mount point is never walked and shows `used X` from statvfs, scaled against other volumes only. OneDrive: one tokio task per folder runs `lsjson` then `rclone size --json` two at a time with kill_on_drop; leaving aborts them. `rclone::exec` now registers the pids of capture children and the exit hook kills them, so Ctrl-C leaves no `rclone size` behind (the run's own rclone, the sign-in and the move children are not registered). Known limits: completion works on the whole text, not the text before the cursor; emoji names can sit one column off; walking `~` can meet macOS privacy prompts for other apps' containers.

## Decisions for you

Everything else in this document is an engineering default I am happy to own. These five change what the user sees, so they are yours. My recommendation is first in each case.

1. **Rust.** Decided 13 Oct 2026: Rust, compiled release binaries, Homebrew install and update path via cargo-dist in milestone 7, self-update command later.
2. **Conflict rule in two-way sync.** Decided 13 Oct 2026: local wins; the destination's copy is kept on the destination only, renamed with a `.conflict-<date>` suffix and excluded from the sync; every conflict appears in the summary block and the run log, and logs are always kept.
3. **"Keep" in the folder prompt becomes a hold.** Decided 13 Oct 2026: confirmed.
4. **Unattended default for the folder prompt is keep.** Decided 13 Oct 2026: keep. A cron or scripted `--json` run never deletes a folder on the destination unless `--folder-deletes=delete` is passed explicitly.
5. **Delta listing on OneDrive is opt-in per account.** Engineering default taken 13 Oct 2026 so coding could start: off by default, opt-in switch in `accounts add`. Background: `--fast-list` is always passed, as the brief asks, but it only does anything when the remote has `delta = true`, which makes every listing start from the drive root. Recommended: off by default, with a switch in `accounts add` for people who sync a folder near the root of a large drive.

Also for your awareness, not a decision: the Rust toolchain was installed via rustup on 13 Oct 2026 (rustc 1.93.1). The GitHub repository does not exist yet; the Homebrew tap needs one, so the repository path is the placeholder `OWNER/onefolder` until you tell me the real name.
