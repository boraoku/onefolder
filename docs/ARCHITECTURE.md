# OneFolder architecture

This is a map of the code for people working on OneFolder. The design and the reasons behind it are in [DESIGN.md](../DESIGN.md); where the two differ, the code and the "Added in milestone N" paragraphs of DESIGN.md record the later decision.

OneFolder is one Rust crate that builds one binary, `onefolder`. rclone is always a subprocess, never a library. The binary has no runtime dependency other than rclone 1.66 or newer on `PATH`.

## Module map

```
src/
├── main.rs              clap parse, tokio runtime, dispatch to cli
├── cli/                 thin command handlers: flags in, calls out
│   ├── mod.rs           the Cli and Command enums, global --json/--dry-run, the no-args menu, version
│   ├── accounts.rs      accounts list / add / remove / test / reconnect
│   ├── jobs.rs          jobs list / add / edit / remove / release, from the form or from flags
│   ├── run.rs           run [job] --dest --yes --folder-deletes --resync --moves; output choice and exit codes
│   ├── status.rs        status [job]: last runs, holds, drives and remotes, no network calls
│   ├── doctor.rs        doctor: rclone, config, accounts, jobs, drives, data files; --prune-logs
│   └── completion.rs    completion bash|zsh|fish through clap_complete
├── config/
│   ├── mod.rs           config.toml load, validate and save (toml_edit keeps comments and unknown keys)
│   ├── paths.rs         config and data directories, ONEFOLDER_CONFIG_DIR / ONEFOLDER_DATA_DIR, ~ handling
│   ├── sync.rs          writes the typed Config back into the toml_edit document, matched by identity
│   └── tests.rs         config unit tests over tests/fixtures/config
├── state/
│   ├── mod.rs           state.json: runs, last_dry_run, holds, bisync; state.lock; atomic writes
│   ├── lock.rs          the per-job run lock runs/<job>.lock, and inspect() for status and doctor
│   └── prune.rs         run log pruning, only for doctor --prune-logs (never automatic)
├── model/
│   └── mod.rs           Account, Job, Destination, Mode, RunRecord, Summary, Outcome; the on-disk shapes
├── rclone/              everything that touches the rclone binary
│   ├── mod.rs           the global flag set, copy and moveto arguments, the built-in and effective excludes
│   ├── detect.rs        PATH lookup, `rclone version` parse, minimum version check
│   ├── remotes.rs       config create / delete / show / reconnect, about, listremotes
│   ├── exec.rs          the only place that starts processes; streaming stderr and stdout, run log, exit codes, Ctrl-C
│   ├── events.rs        JSON log line to Event
│   ├── listing.rs       lsjson (read line by line, counting entries as they come, hashes when asked), hashsum, size and mkdir helpers
│   └── bisync.rs        bisync flag set, plan and listing parsers, lock files, abort classification
├── sync/                the semantics, free of any UI dependency
│   ├── mod.rs           module root; NFC normalisation for matching paths to listings
│   ├── oneway.rs        validate, preview (copy --dry-run plus both listings, move detection), moves, copy, record
│   ├── moves.rs         moved-file detection: candidates by size, pairs by hash, folder and file groups
│   ├── twoway.rs        validate, merge (resync), plan, folder prompt, preview, bisync, record
│   ├── twoway/tests.rs  unit tests for twoway.rs
│   ├── plan.rs          what a two-way run will do, read from bisync's dry runs; conflict names
│   ├── folders.rs       deleted-folder detection from the plan and the prior .path1.lst listing
│   ├── reporter.rs      the Reporter trait (events out, answers in) and the Scripted test reporter
│   └── summary.rs       accumulates events into per-destination counts, bytes, errors and outcome
├── volume/
│   ├── mod.rs           Identity, identify() and check() (on a thread, 10 s limit), mount_state() for lists (2 s), guarded() file checks, symlink resolution, messages
│   └── macos.rs         statfs and `diskutil info -plist`, behind cfg(target_os = "macos")
├── ui/                  everything the user sees; the only layer using ratatui and crossterm
│   ├── mod.rs           terminal width, display-width helpers, fitting lines to the width, quoting, out/err/pause, restoring the terminal
│   ├── theme.rs         glyphs, colours, rules and borders
│   ├── form.rs          the in-house form component (text, path, picker, toggle, list, rows, buttons); folder fields complete and browse
│   ├── complete.rs      Tab completion of folder names: split, match, cycle, one local read_dir
│   ├── browse/          the folder browser opened from a folder field
│   │   ├── mod.rs       Browser state (folder, rows, sizes, filter, hidden toggle), keys, drawing; Update and the Source trait
│   │   ├── local.rs     LocalSource: a listing thread per folder and up to four size walkers (std::fs), cancel flags, statvfs free and used space, mount points
│   │   ├── remote.rs    RemoteSource: rclone lsjson, then rclone size two at a time on the runtime; rclone about for free space
│   │   └── tests.rs     unit tests and snapshots of the browser
│   ├── jobform.rs       the job add and edit form with typed destinations; runs the browser and the OneDrive listings for completion
│   ├── menu.rs          the main menu shown by `onefolder` with no arguments, and the `?` keys overlay
│   ├── accounts.rs      accounts list (printed and interactive, with `/` filter), test results, prompts
│   ├── jobs.rs          the jobs list, printed and interactive (with `/` filter)
│   ├── status.rs        the text of `onefolder status`
│   ├── progress.rs      the run screen and TuiReporter
│   ├── prompt.rs        one-way preview, two-way plan, merge offer, questions, folder prompt, whole-list view
│   ├── session.rs       the interactive session's one terminal, the screen guard, the output page and text screen, the transcript
│   ├── plain.rs         PlainReporter for runs whose output is not a terminal
│   └── summary.rs       the bordered summary block, shared by the screen and plain output
└── jsonout/             --json output
    ├── mod.rs           the Envelope every non-interactive command prints
    ├── accounts.rs      data documents of the accounts commands
    ├── run.rs           JsonReporter: run --json as newline-delimited events
    └── status.rs        data document of status --json
```

Outside `src/`: `build.rs` records the short git commit for `onefolder version` (or `unknown` outside a checkout); `scripts/record-fixtures.sh` records rclone fixtures; `scripts/demo-unplugged-drive.sh` demonstrates the unplugged-drive refusal on a real APFS disk image; `scripts/universal.sh` joins the two macOS release archives into the universal one with `lipo`; `justfile` holds the developer tasks; `dist-workspace.toml` configures the release.

## Layer rules

1. **Only `rclone::exec` spawns processes.** That covers rclone, `diskutil`, `sysctl` (boot time, for lock staleness), `ps -p <pid> -o comm=` (whether a lock's process is still OneFolder or rclone), `stty` and `brew`. Everything else asks `exec` through `run_capture` (run to completion, collect output, with a time limit where the caller sets one), `run_capture_lines` (the same, handing each line of stdout over as it arrives, used for the destination listing), `run_capture_input` (the same, with text on standard input, for `hashsum --files-from-raw -`), `run_capture_detached` (the same, in its own process group so Ctrl-C cannot cut it in half, for each move of moved files), `run_attached` (hand the terminal to the child, used for rclone's browser sign-in) or `run_streaming` (a transfer, decoded line by line). Test code may start its own processes.
2. **Only `src/ui` depends on ratatui and crossterm.** `cli` calls into `ui` for screens and prompts; nothing in `sync`, `rclone`, `config`, `state` or `volume` knows a terminal exists.
3. **`sync` talks to the UI only through the `Reporter` trait** in `sync/reporter.rs`: progress, previews, plans, conflicts and problems go out; confirmations and folder answers come back. Four implementations exist: `ui::progress::TuiReporter` (the run screen), `ui::plain::PlainReporter` (no terminal), `jsonout::run::JsonReporter` (`--json`) and `sync::reporter::Scripted` (tests, answering from a script).
4. **`cli` handlers stay thin.** They parse flags, load config and state, pick the output, call the module that does the work and turn the result into an exit code.
5. **Text and JSON come from the same values.** Functions such as `ui::summary::summary_lines`, `ui::jobs::list_lines` and the `*_lines` functions in `ui::prompt` decide the text and tone once; the screen, the plain renderer and the `--dry-run` printout draw from them. `jsonout` documents are the same data that the text is rendered from.

## rclone command inventory

Every rclone call, with its flags. Paths are always absolute or `remote:path`.

The **global flags** (`rclone::GLOBAL_FLAGS`) go on every transfer command:

```
--use-json-log --stats 1s --stats-log-level NOTICE -v --color never
--modify-window 2s --retries 3 --low-level-retries 10
```

On a planning pass (one-way preview, two-way plan, merge dry run) `--retries 1` replaces `--retries 3`, because retries inflate the checks count the preview relies on.

**Excludes.** Every rclone call that filters gets the same effective list, assembled in one place, `rclone::effective_excludes` (through `sync::oneway::excludes_of` for a job), in this order:

1. `rclone::BUILTIN_EXCLUDES`, files macOS and Windows make for themselves, left out of every job, every destination, one-way and two-way, whatever the job says: `.DS_Store`, `._*`, `.localized`, `.Spotlight-V100/**`, `.Trashes/**`, `.fseventsd/**`, `.TemporaryItems/**`, `.DocumentRevisions-V100/**`, `[Tt]humbs.db`, `[Ee]hthumbs.db`, `[Ee]hthumbs_vista.db`, `[Dd]esktop.ini` (patterns are case-sensitive; macOS and OneDrive are not), `$RECYCLE.BIN/**`, `System Volume Information/**`;
2. `rclone::HIDDEN_EXCLUDES`, `.*` and `.*/**` (every hidden file, and everything inside a hidden folder, at any depth), unless the job has `copy_hidden = true`;
3. the job's own `excludes`, each trimmed and widened by `rclone::widen_exclude` so a folder it names is left out whole (`folder/*` becomes `folder/**`, though `/*` stays; a name or path with no wildcard and no trailing `/` gives itself and itself with `/**`; anything else is passed on as it is; the trim because rclone trims the trailing spaces of the last `--exclude` flag only), less any already in the list (the preview lists the job's patterns as typed, less those already built in, as the job's own, `rclone::own_excludes`; the JSON `effective_excludes` has the widened ones).

A pattern without a leading `/` matches a whole name at any depth and `*` never matches a `/` (rclone's filtering rules, confirmed with a real copy on 1.75.2). The two-way calls add one `/<folder>/**` per held folder and the conflict pattern after these. The one-way preview, the destination listing, the copy, the folder restore, the merge's listing of this Mac and every bisync call all use the list, so they all see the same files. Leaving a file out never deletes it: on an existing two-way destination, newly excluded files drop out of both of bisync's listings, which deletes nothing on either side and needs no resync (confirmed on 1.75.2 and in the integration suite); bisync counts those entries against its 50% delete limit, and `sync::plan` lifts the limit without asking when they alone push it over, as it does for holds. Entries gone from both listings never count towards a real deletion: the more-than-half check measures real deletions against the prior listing less those entries (and less a folder kept this run), so a mass deletion is still asked about, and `sync::folders` leaves them out before looking for deleted folders, so a hidden or junk-only folder deleted locally brings no prompt.

**One folder, as the sync sees it.** The folder prompt's size, the restore of a folder and the check before removing a deleted folder's leftovers run on the destination's root with `rclone::folder_filter_args`: `--filter "- <pattern>"` per exclude, then `--filter "+ /<folder>/**"` and `--filter "- **"`. Rooted there, each pattern means what it means in the sync; with the folder as the root, a hidden folder's own content or a `/`-anchored pattern would slip through.

The arguments (`rclone::exclude_args`) are repeated `--exclude <pattern>`. For `copy` and `lsjson`, more than 20 patterns go in an `--exclude-from <file>` written beside the run log (`Recorder::exclude_file`), but only when no pattern starts with `#` or `;`, has leading or trailing spaces, or contains a line break, since rclone reads those differently in a file (the job's own patterns arrive trimmed, so the spaces check is a guard only). The built-in and hidden patterns are 16, so a job with five patterns of its own crosses that line. bisync always gets repeated `--exclude` flags, however many patterns there are: never a `--filters-file`, whose hash bisync checks, and never an `--exclude-from` file either.

**`--fast-list`** is added for every OneDrive destination (it only takes effect when the remote has `delta = true`) and never for a local one.

| Purpose | Command | Built in |
| --- | --- | --- |
| Detection | `rclone version` | `rclone/detect.rs` |
| Add an account | `rclone config create --no-output onefolder-<label> onedrive drive_type=<personal\|business> [delta=true]`, terminal attached | `rclone/remotes.rs` `create_args` |
| Sign in again | `rclone config reconnect onefolder-<label>:`, terminal attached | `rclone/remotes.rs` `reconnect` |
| Remove an account | `rclone config delete onefolder-<label>` | `rclone/remotes.rs` `delete` |
| Read an account's settings | `rclone config show onefolder-<label>` | `rclone/remotes.rs` `show` |
| List remotes (status, doctor) | `rclone listremotes` | `rclone/remotes.rs` `list` |
| Account check and quota | `rclone about onefolder-<label>: --json` | `rclone/remotes.rs` `about` |
| One-way preview | `rclone copy <src> <dst>` + global flags (`--retries 1`) + excludes + `[--fast-list]` + `--dry-run` | `rclone/mod.rs` `copy_args` |
| Destination listing for the preview | `rclone lsjson -R --files-only --color never [--fast-list] [--hash-type quickxor]` + excludes + `<dst>`, stdout read line by line (`exec::run_capture_lines`) so the entries listed so far can be shown; the hash only for a OneDrive destination, which lists it from the file's metadata at no cost (on a local folder it would read every file) | `rclone/listing.rs` `list_files` |
| Source listing for the preview (destination-only files, move detection) | `rclone lsjson -R --files-only --color never` + excludes + `<src>` | `rclone/listing.rs` `list_files` |
| Hashes of move candidates | `rclone hashsum quickxor --files-from-raw - --color never <root>`, the candidate paths on standard input; on the source, and on a local destination (a OneDrive destination's hashes come from its listing) | `rclone/listing.rs` `hashsum` |
| Move a moved file or folder on the destination | `rclone moveto <dst>/<old> <dst>/<new>` + global flags, at most four at a time, folders first; before a folder move, an unfiltered `lsjson -R --files-only` of the old folder (nothing else may be in it) and `lsjson --max-depth 1 --dirs-only` of the new one (it must not exist yet) | `rclone/mod.rs` `moveto_args`, `sync/oneway.rs` `run_moves` |
| One-way transfer | `rclone copy <src> <dst>` + global flags + excludes + `[--fast-list]` | `rclone/mod.rs` `copy_args` |
| Restore a folder locally (folder prompt) | `rclone copy <dst> <src>` + global flags + `[--fast-list]` + the folder filters (excludes and the conflict pattern) | `rclone/mod.rs` `copy_args`, `folder_filter_args` |
| What is left of a deleted folder | `rclone lsjson -R --files-only --color never [--fast-list]` + the folder filters (excludes only, so a conflict copy counts) + `<dst>` | `rclone/listing.rs` `list_filtered` |
| Remove a deleted folder's leftovers | `rclone purge --color never <dst>/<folder>`, no filter: only for a folder answered Delete, after a clean bisync, when the folder is still gone locally and nothing the sync handles is left in it; logged as `folder-removed` and named in the summary | `rclone/listing.rs` `purge` |
| Rename a conflict copy on the destination | `rclone moveto <from> <to>` + global flags | `rclone/mod.rs` `moveto_args` |
| Folder size for the prompt | `rclone size --json --color never` + the folder filters + `<dst>` | `rclone/listing.rs` `size` |
| Names in one folder, unfiltered (free conflict numbers) | `rclone lsjson --files-only --max-depth 1 --color never <folder>` | `rclone/listing.rs` `names_in` |
| Does a folder exist | `rclone lsjson --max-depth 1 --dirs-only --color never <folder>` (exit 3 when missing) | `rclone/listing.rs` `exists` |
| Create a folder | `rclone mkdir --color never <folder>` | `rclone/listing.rs` `mkdir` |
| Two-way plan | `rclone bisync` + bisync flags + `--dry-run --force` | `rclone/bisync.rs` `args(Call::Plan)` |
| Two-way transfer | `rclone bisync` + bisync flags `[--force]`, `--max-delete 100` when lifted | `rclone/bisync.rs` `args(Call::Sync)` |
| Merge (first run, recovery) | `rclone bisync` + bisync flags + `--resync --resync-mode path1 [--dry-run]` | `rclone/bisync.rs` `args(Call::Resync)` |

The **bisync flags**, after the global flags, in this order:

```
--workdir <data>/bisync/<job>/<dest-id>
--conflict-resolve path1 --conflict-loser num
--conflict-suffix race-conflict-{DateOnly}- --suffix-keep-extension
--create-empty-src-dirs --resilient --recover
--max-delete 50 --compare size,modtime
--exclude <built-in, hidden and job excludes> --exclude /<held folder>/** --exclude '*.conflict-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*'
[--fast-list]
```

Notes on the two-way calls:

- `--force` on the plan is needed because bisync applies its too-many-deletes check even in a dry run. On the real run it is passed only after the user confirmed a preview in which every file changed.
- `--max-delete 100` replaces 50 only after the user confirmed a preview showing more than half a side deleted, or when held folders and conflict copies alone push it over.
- OneFolder renames a conflict's destination copy itself with `moveto`, to `name.conflict-<YYYY-MM-DD>-<n>.ext` (`sync::plan::conflict_name`), before the real run; bisync's own suffix only applies to a file that changes between plan and run, and that name is deliberately outside the excluded pattern. Before a merge, `--resync --dry-run` finds destination files that would be overwritten so they can be renamed the same way.
- The expected snapshot of every bisync call is `tests/snapshots/bisync_flags.snap`.

Outside rclone, `exec` also runs `/usr/sbin/diskutil info -plist <mount point or UUID>` for volume identity (10 second limit; a timeout reads "did not answer in 10 seconds, is a network drive hanging?"), `/usr/sbin/sysctl -n kern.boottime`, `/bin/ps -p <pid> -o comm=` for lock staleness, `/bin/stty` to restore the terminal, and `brew info --json=v2 rclone` (5 second limit) for doctor's note about a newer rclone.

**Exit codes** (`exec::ExitKind`): 0 success; 1 uncategorised (per-file failures, so partial when files moved or failed); 2 usage error, reported as an app bug; 3 and 4 source or destination not found; 5 and 6 retryable or less serious, partial; 7 fatal; 8 and 10 transfer or duration limit; 9 nothing transferred, treated as success; death by signal or 130 is cancelled. bisync abort messages are classified by substring in `bisync::classify` into needs-resync, too-many-deletes, locked, filters-changed, empty side and names too long.

## The event pipeline

```
rclone stderr (one JSON object per line)
   │  exec::run_streaming: reader task, line by line
   ├─► RunLog: every raw line appended to logs/<job>/<dest-id>/<timestamp>.jsonl
   ▼
events::decode ─► Event (Stats, Transferred, Skipped, Deleted, RemovedDir, Error,
   │                     Plan, Changes, Phase, Conflict, NoConflict, Queued,
   │                     Applying, Fatal, Notice, Unparsed)
   ▼
exec::Progress (Event | Stopping | Killing) ─► callback in sync::oneway / sync::twoway
   ▼
sync::summary (Accumulator for one-way, TwoWayTally for two-way): counts, bytes, errors, outcome
   ▼
Reporter ─► TuiReporter (ui::progress) | PlainReporter (ui::plain) | JsonReporter (jsonout::run)
```

- The run log's first line is OneFolder's own JSON record of the command line that ran (`{"onefolder":"command","command":...,"time":...}`); OneFolder adds similar lines for refusals and notes. Lines are written as they arrive, so a crash mid-run still leaves a usable log.
- The decoder is lenient: unknown fields are ignored, a JSON line of an unknown shape becomes `Notice`, a non-JSON line becomes `Unparsed`, and nothing panics.
- rclone runs in its own process group, so Ctrl-C reaches only OneFolder. The first press sends rclone SIGINT and reports `Stopping`; a second press, or no exit within the grace period, kills it. The grace period is 10 seconds for `copy` and 45 seconds for `bisync`, which needs the time to save its listings and release its lock.
- The run screen redraws from events at most ten times a second and never polls rclone. `--json`, or stdout that is not a terminal, sends the same events to the plain or NDJSON reporter instead. The NDJSON event shapes are documented at the top of `src/jsonout/run.rs`: `start`, `step`, `phase`, `preview`, `moves`, `plan`, `resync`, `progress`, `stats`, `file`, `error`, `conflict`, `folder_prompt`, `stopping`, `problem`, `note`, `skipped`, `summary`, `refused` and `done`.
- **Whole lists at the question.** The preview and the plan show the first 10 paths of a list. `ui::prompt::preview_full_lists` and `plan_full_lists` hand the confirmation screen (`PreviewPrompt`) every shortened list whole, as `FullList` values (title, total size, sorted files, and the paths in NFC and lower case, made once for the `/` filter). `preview_screen` and `plan_screen` give the screen's lines, whose `and N more` line adds `l whole list`, together with those lists, so the key never reaches the screen without them; `preview_lines` and `plan_lines` stay as they were for plain output and `--dry-run`. `l` opens the view; it draws only the rows on screen (`full_lines` over a slice of `full_rows`, with the heading of the list in view pinned on top), so 100,000 paths draw as fast as ten, and the question's own scroll is kept untouched for the way back. Paths are measured in display columns (`ui::width`, and `ui::progress::fit` for shortening), so wide and decomposed names keep the size column in line.
- **Moved files (one-way).** `rclone copy --track-renames` does nothing (rename tracking is for `sync` only), so OneFolder finds moves itself in the preview step of `oneway::run_destination`. Three things run side by side: the dry run, the destination listing (with QuickXorHashes for OneDrive) and a listing of the source with the same excludes. The files only on the destination are then an exact set (destination paths not in the source, compared as `sync::moves::fold` makes them: NFC and case-folded, since every destination OneFolder supports, and rclone's matching on it, ignores case), and `Preview::destination_only` is its count. `sync::moves` decides the rest without touching rclone: `candidates` pairs each new file of more than 0 bytes with destination-only files of the same size; `oneway::find_moves` hashes just those (`hashsum` on the source, and on a local destination; the OneDrive listing's hashes otherwise); `pair` keeps only identical content, breaks ties by the longest common path suffix (the same name, then the same name in the same subfolder, so identical files of a renamed folder stay in their places) and then the same modification time (2 s), accepts a pair only when it is the best for both of its files, and never uses a destination file twice, so a tie leaves the file to be uploaded; each side of a bucket is indexed by every path suffix and times are parsed once, so 3,000 identical files pair in milliseconds; `group` turns pairs into a folder move when every file listed under the old folder moves to the same new folder with the same relative paths, there are at least two, nothing is listed under the new folder (paths folded as above), and no other folder move overlaps it (so folder moves can run side by side), and into single-file moves otherwise. `Preview::apply_moves` takes the moved files out of `new`; `Preview::choose_moves` puts them back when they are uploaded again (`MovesChoice::Upload`), so every count and line is drawn from the preview as it stands. While the hashes are read the live figures keep moving and say `checking N possible moves` (`Looking::checking_moves`). The answer comes from `--moves` or the default (`sync::reporter::MovesPolicy`, worked out by `cli::run::moves_policy`) before the preview is reported (`--moves ask` with no terminal to ask on is copy, decided `NotAsked`: an explicit ask never moves without a yes), or from `Reporter::confirm(Question::Moves)` after it; `Reporter::moves_decided` reports it (the `moves` NDJSON event). After the confirmation `run_moves` (step `Moving`) checks each folder move against an unfiltered listing of the old folder (the same relative paths, folded, as the moving files) and the absence of the new one (otherwise its files move one by one), makes the new parent folders of each phase one at a time (`rclone mkdir`, so parallel moves never race to create one folder, which OneDrive can refuse), runs the folder moves and then the single moves four at a time on a `JoinSet`, logs `move` before and `moved` or `move-failed` after each, and reports `moved N of M` through `Reporter::looking`. Each `moveto` runs in its own process group: the first Ctrl-C lets the moves under way finish and starts no more, a second aborts them. A failed move is a per-file error and the copy that follows uploads the file. The copy then finds the moved files unchanged (a local rename and a OneDrive server-side move both keep the modification time). `Summary::moved` carries the files and bytes moved (would move, for a dry run), and is present only when something moved; the JSON `choice` and `answer` both say `copy` for uploading again, as the flag does.
- **Live figures while looking.** A preview and a two-way plan can take many minutes on a large folder, with nothing transferred, so `Reporter::looking` carries a `sync::reporter::Looking` about once a second: the step, the seconds since it started, and from the dry run's stats lines (`--stats 1s`) the files checked, rclone's `listed` count (present on 1.75.2), and the files and bytes the dry run would copy so far. For one-way, the destination listing runs beside the dry run and counts the `lsjson` entries as they arrive (an `AtomicU64` the stats handler reads); when the dry run finishes first, a one-second timer keeps the figures moving and they say the destination is still being listed. The run screen shows them under the step name (no bar, since the totals are unknown), the plain reporter prints one line at most every 10 seconds (none for a preview under 10 seconds), and the NDJSON reporter sends a `progress` event at most once a second. The two-way plan and the merge's dry run show the same figures without a destination count.

## Screens and the interactive session

Every full-screen view (the menu, the jobs and accounts lists, the job form, the run screen with its confirmation, folder prompt and whole-list view) gets its terminal from `ui::screen()`, which returns a `Screen` guard that derefs to the ratatui terminal. A direct subcommand (`onefolder run documents`, `onefolder jobs edit x`) has no session, so the guard takes the terminal itself, as `ratatui::try_init` does, and restores it when it drops; its text goes to stdout as plain output, and the small prompts of `accounts add` and the job picker draw inline on the normal screen (`ui::accounts::pick`).

`onefolder` with no arguments opens a session (`ui::session::start`, held by `cli::menu`): raw mode and the alternate screen are taken once and kept in `ui::session` until the menu is left. Inside it:

- **One terminal.** `ui::screen()` lends each screen the session's terminal and takes it back when the guard drops, so moving between screens never leaves the alternate screen. The run screen's thread borrows it the same way.
- **Output on a page.** Command text goes through `ui::out` and `ui::err` (the cli handlers use them in place of `println!` and `eprintln!`); outside a session they write to stdout and stderr, inside it they add to the session's page. The page keeps the text exactly as the plain output prints it, painted; `theme::unpaint` reads the colour codes `theme::paint` writes back into tones, so the words and tones come from the same `*_lines` and `render_text` functions as the plain output. `ui::pause` (the old "Press any key" after a command) shows the page on the text screen (`session::TextScreen`). It sits two columns in, as the menu and the lists do, and `ui::terminal_width` gives the commands that width (the terminal's less two) while a session is open. It starts at the end, as the shell would, and refits long lines on every draw (`session::wrap_for_screen`, so a resize just redraws): a rule is cut to the width, words wrap with the continuation indented, and a path (a long word with a slash) carries on from where it is and continues from the first column, so it copies in one piece. It scrolls with `↑↓`, `j`/`k`, `pgup`/`pgdn` and `g`/`G`, and any other key goes back; keys typed before its first draw are thrown away, so an Enter pressed while a command worked cannot close it unseen. While Status and Doctor work, the session shows a `Checking…` frame (`session::busy`). The menu, jobs and accounts loops also show any page left over (a note, a refusal) before their screen comes back, and an error from a menu item is shown there instead of ending the session. Inside a session a run never falls back to plain output: if the run screen cannot start, the run reports the error instead.
- **Prompts under the page.** The inline picker (`pick`, such as "Run which job?" or the drive type) draws under the page on the session's terminal, with the same lines (`Picker::segments`) and keys; its answer line goes on the page. The line-based questions (`ask_label`, `confirm`) read from `session::LineInput` and write to `session::PageOutput`, so their wording and validation stay in one place. rclone's browser sign-in, which talks to the user itself, gets the terminal through `session::hand_over`: raw mode off, the alternate screen cleared and showing the page, rclone writing under it. Taking it back turns raw mode on, throws away keys typed meanwhile and makes the next screen redraw in full. A screen asking for the terminal while another has it is a bug and an error (`Busy`), and a terminal is only ever given back to an empty slot.
- **Transcript.** `TuiReporter::finish` also records each summary block, each skipped or problem line (such as a destination's `not run` line) and the multi-destination block in the session's transcript (`session::record`), in the order plain output prints them. When the session ends (q, Esc, Ctrl-C at any screen, an error, or a signal through the exit hook), `session::end` leaves the alternate screen and prints the transcript to stdout, followed by nothing else; with no runs it prints nothing.
- **Restoring once.** `ui::restore_terminal` is the one way back: it ends the session if one is open and otherwise leaves the raw mode and alternate screen of the screen that is up, guarded by a flag so it happens once. It is the exit hook a signal runs (registered by the session, or by a screen that took the terminal itself), and `ui::quit_on_ctrl_c` calls it before exiting with 130. The session and every screen that takes the terminal start the signal dispatcher (`exec::install_signal_handler`) at once, rather than at a run's first `InterruptGuard`, so SIGINT, SIGTERM or SIGHUP at the menu also restores the terminal before OneFolder exits. A panic is covered by ratatui's own hook, and the session guard restores again on the way out. The run screen's Ctrl-C handling is unchanged: inside a run an `InterruptGuard` is alive, so the first press stops rclone and a second kills it, and the session carries on to the summary.

## Folder completion and the folder browser

The job form's folder fields (`Field::path`, and the OneDrive folder, a text field marked `Folder::Remote`) complete with Tab while being edited and open the folder browser with `b` (Ctrl-B while editing). `ui::complete` holds the pure part: `split` cuts the typed value at its last `/` (an empty local field is `~/`), `matches` keeps the folder names starting with the typed segment (NFC and case-folded, hidden ones only after a dot, sorted), and `Cycle` steps through them, keeping the typed base (a `~` stays). A local field reads the one parent folder synchronously (`complete::local_folders`, a single `read_dir`). A OneDrive field asks its owner with `FormEvent::List`; `JobForm` runs `rclone lsjson --dirs-only --no-mimetype --no-modtime` on the runtime the CLI passed in (`Context::rclone`, `Handle::try_current`), keeps the names for the life of the form by `remote:folder`, and hands them back with `Form::set_listing`, which completes the waiting Tab if the text has not changed meanwhile. The form shows `listing…` until then and ignores further Tabs for the same folder; a Tab for a different folder replaces the wait and asks again (the older answer, when it comes, only fills the cache). A changed account clears the form's names, and an answer for another account is dropped. The hint lists the matches in sorted order with the current one in brackets (`2 of 3: Desktop, [Documents], Downloads`), starting later (`…,`) when needed so the current one stays in view. Completion works on the whole text, wherever the cursor is.

The job form's key loop polls for keys with a 100 ms timeout and calls `JobForm::tick` in between, which drains the completion answers and the browser's updates without waiting, so the screen redraws at most about ten times a second from background work and never blocks on it.

**The browser** (`ui::browse::Browser`) is pure state: the folder shown, its subfolders sorted by name, a `Size` per subfolder (`Measuring` with the running total, `Done`, `Failed`), the `/` filter, the `.` hidden toggle and the cursor. Keys with Ctrl or Alt are ignored, except Ctrl-H, read as Backspace (up a folder, or a letter off the filter). The bars are relative to the largest size among the rows shown (a filter rescales them; running totals count), with mounted drives scaled among themselves. Each folder opened gets a new generation number; a `Source` lists it and measures its subfolders and answers with `Update`s on an `mpsc` channel, each tagged with the generation, and the browser drops any update for a generation it has left, and a running total that arrives after the final size. Choosing gives the field's text: the tilde form on this Mac, the path relative to the drive root on OneDrive. The browser draws on the terminal the form already holds (from `ui::screen()`), in place of the form.

**Local sizes.** `LocalSource` lists each opened folder on a thread of its own (a slow drive or a folder of many files never holds up the keys), sending the subfolders, the files directly inside (count and bytes) and the free space (`volume::space`, `statvfs`). A subfolder on another device than the folder (`st_dev`; a mount point, such as each drive under `/Volumes`) is never walked: its used space from `statvfs` is sent at once as `Size::Volume` and shown as `used 1.2 TB` (`local::sort_out` makes the split, unit-tested with made-up device ids). It then queues one job per subfolder, visible ones first, for a pool of four walker threads started on first use. A walker sums the apparent size of every file under its folder with `std::fs`, follows no symbolic links, stays on the folder's file system (as `du -x`), and counts what it cannot read as partial (`+` on screen). It reports its running total at most every 100 ms and its final size once; final sizes go into a `SizeCache` kept for the life of the job form, keyed by path, which the next listing of the same folder answers from at once (`r` forgets the current folder's). **Cancellation:** every opened folder has its own `Arc<AtomicBool>`; opening the next folder sets the old flag and empties the queue, and dropping the source (leaving the browser) sets it and closes the queue so the walkers exit. The listing thread checks the flag before queueing, and a walker checks it between directories and every 512 entries, so even a walk of a home folder with hundreds of thousands of files stops within moments.

**OneDrive sizes.** `RemoteSource` spawns one task per opened folder on the runtime: `rclone lsjson --no-mimetype --no-modtime <remote>:<folder>` (folders and files, one level; without `--dirs-only`, so the same request gives the files line, and exit code 3 sends the browser up to the nearest folder that exists when it starts from a typed folder), then `rclone size --json <remote>:<folder>/<subfolder>` for each subfolder not in the cache, two at a time on a `JoinSet`. Opening the next folder or leaving the browser aborts the task; the `JoinSet` it owns aborts the size runs, and dropping their futures kills the rclone processes (`exec::run_capture` is `kill_on_drop`). `rclone about --json <remote>:` runs once per browser for the free space, which is left out if it fails.

**Children left behind on quitting.** Ctrl-C at the form or browser quits through `ui::quit_on_ctrl_c` and `exec::exit_now`, so the futures that would kill a listing or a size on drop never get dropped. `exec` therefore keeps a registry of the children started by `run_capture`, `run_capture_input` and `run_capture_lines` (process id added after spawn, removed once waited for or when the future is dropped), and the exit hook (`run_exit_hook`, which `exit_now` and the signal dispatcher both run) sends SIGKILL to every registered child before anything else. A transfer (`run_streaming`), rclone's sign-in (`run_attached`) and a move (`run_capture_detached`, which must not be cut in half) are not registered, and the exit hook only runs when no `InterruptGuard` is alive, so a run's own Ctrl-C handling (the first press stops rclone, a second aborts it) is unchanged.

| Purpose | Command | Built in |
| --- | --- | --- |
| OneDrive folder names for completion | `rclone lsjson --no-mimetype --no-modtime --color never --dirs-only <remote>:<folder>` | `rclone/listing.rs` `top_level` |
| A OneDrive folder in the browser | `rclone lsjson --no-mimetype --no-modtime --color never <remote>:<folder>` | `rclone/listing.rs` `top_level` |
| A OneDrive folder's size in the browser | `rclone size --json --color never <remote>:<folder>` | `rclone/listing.rs` `size` |
| Free space in the browser | `rclone about <remote>: --json` | `rclone/remotes.rs` `about` |

## State on disk

All machine-written files live under the data directory (`~/.local/share/onefolder`, or `ONEFOLDER_DATA_DIR`). The config directory (`~/.config/onefolder`, or `ONEFOLDER_CONFIG_DIR`) holds only `config.toml`.

| File | Contents |
| --- | --- |
| `config.toml` | `version`, `[[accounts]]` (label, remote, drive_type, added), `[[jobs]]` (name, source, excludes, `copy_hidden`, and the source's drive when it is on one) with `[[jobs.destinations]]` (id, type, account or path, mode, and for drives the volume identity fields). Hand-editable; `toml_edit` keeps comments and unknown keys, and a newer `version` is refused. |
| `state.json` | `runs` (last real run per `<job>/<dest-id>`), `last_dry_run` (kept apart so a dry run never hides the last real run), `holds` (folders on hold per destination), `bisync` (initialised, last_resync, the source and destination pair, needs_resync). |
| `state.lock` | Every writer of `state.json` holds an exclusive `flock` on it while it loads, changes and saves, because runs of different jobs may overlap. Writes go to a temp file and are renamed into place. |
| `runs/<job>.lock` | The job's run lock: created with `O_EXCL`, holding the PID and start time. `run` (including `--dry-run`), `jobs edit`, `jobs remove` and `jobs release` take it without waiting. It is stale when the PID is dead, is no longer OneFolder, or the lock predates the last boot. `status` and `doctor` report a live or stale lock. |
| `logs/<job>/<dest-id>/<timestamp>.jsonl` | One file per run, with an `.excludes` file beside it when the excludes went in a file. Never deleted automatically. `doctor --prune-logs` removes logs older than 90 days or beyond the newest 50 per destination, except the logs of the last run and last dry run, logs that record a conflict (bisync's conflict lines or OneFolder's `moveto` of a conflict copy) and logs of jobs or destinations no longer in the config; it also removes `.excludes` files whose log is gone. |
| `bisync/<job>/<dest-id>/` | bisync's working directory: `.lst` listings (including `.path1.lst`, which `sync::folders` reads) and bisync's own `.lck` lock file, a JSON document with the PID. A bisync lock is live only if its PID is alive, is rclone, and it was renewed after boot. Changing a destination's pair moves this directory aside so the next run starts with a merge. |

Renaming a job rewrites state keys and log paths first, then moves `logs/<job>` and `bisync/<job>`, then saves the config, so the job's data is never keyed by a name the config does not have. Removing a job drops its state entries and archives `bisync/<job>`; its logs are kept.

## Testing

`just test` runs `cargo test`; `just lint` runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`.

**Unit tests** live beside the code (and in `src/config/tests.rs` and `src/sync/twoway/tests.rs`). They need no rclone. The decoder tests read real rclone 1.75.2 output from `tests/fixtures/rclone/`; config tests read `tests/fixtures/config/` (a sample, an extras file, and one file per validation failure); the volume tests read recorded `diskutil` plists from `tests/fixtures/volume/`.

**Snapshots** use insta. Every snapshot, including those of rendered screens drawn into a ratatui `TestBackend` at 60, 80 and 120 columns, lives in `tests/snapshots/` (the ui tests set `snapshot_path` there). A changed snapshot is written as `*.snap.new` (ignored by git) for review with `cargo insta review`.

**`tests/cli.rs`** runs the built binary end to end with `assert_cmd`, pointing `ONEFOLDER_CONFIG_DIR` and `ONEFOLDER_DATA_DIR` at a temp directory and controlling `PATH` (for example, an empty `PATH` to check the missing-rclone message). No rclone is needed.

**`tests/accounts.rs`** runs `onefolder accounts` against the real rclone with a private `RCLONE_CONFIG` whose `onefolder-test` remote is an alias of a temp folder, so `rclone about` works without a Microsoft account. The sign-in flows run under a pseudo-terminal (`script`) with a fake `rclone` first on `PATH` that turns `config create ... onedrive` into an alias remote. These tests skip when rclone is not installed, except under CI or with `ONEFOLDER_INTEGRATION=1`, where a missing rclone fails them.

**`tests/integration/`** runs only with `ONEFOLDER_INTEGRATION=1` and rclone on `PATH` (`just test-integration`). Each test gets its own config and data directories, a private `RCLONE_CONFIG` with the `onefolder-test` alias remote standing in for OneDrive, a source folder and a plain local destination; files are seeded with `rclone copy` so modification times match. It covers one-way, two-way, the folder prompt with each answer and holds, the safety checks, multi-destination sequencing, the volume check, the run lock and `status`. Tests that attach real disk images with `hdiutil` also need `ONEFOLDER_DISK_TESTS=1`.

**CI** (`.github/workflows/ci.yml`, macOS runners) has three jobs. The first runs formatting, clippy, and `cargo test --bins --test cli`. The second installs rclone and just with Homebrew, then runs `cargo test --test accounts`, `just test-integration` and `just fixtures`. The third installs dist and runs `dist plan` and `dist generate --check`, so a release configuration that no longer matches the workflow fails before a tag is pushed.

### Re-recording fixtures

```sh
just fixtures
```

This runs `scripts/record-fixtures.sh`, which builds small trees in a temp directory, runs the rclone on `PATH` with OneFolder's flag sets (including a `onefolder-test` alias remote), and writes stderr to `tests/fixtures/rclone/*.jsonl`, plus `exit-codes.txt`. Temp paths become `/fixture`, rclone's random partial-file suffix and the dates in conflict names are fixed, so recordings are stable. It then runs the decoder, listing, exit-code and sync tests with `INSTA_UPDATE=no INSTA_FORCE_PASS=0`, so the snapshots are checked and never updated: if a new rclone release changes its log lines, this fails rather than a user's run. To accept a deliberate change, review the differences with `cargo insta review`. `RCLONE=/path/to/rclone scripts/record-fixtures.sh` records with a different rclone.

## Releases

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/), configured in `dist-workspace.toml`:

- targets `aarch64-apple-darwin` and `x86_64-apple-darwin`, built with the `dist` profile (release with thin LTO);
- installers: a shell installer and a Homebrew formula (the formula picks the build for the Mac's processor and `depends_on "rclone"`);
- the formula is published to the tap `boraoku/homebrew-onefolder`, so users run `brew install boraoku/onefolder/onefolder`;
- a custom publish job, `.github/workflows/publish-universal.yml`, downloads the two macOS archives from the release, joins them with `scripts/universal.sh` (`lipo`), checks both architectures are there and the binary runs, and uploads `onefolder-universal-apple-darwin.tar.xz` with its checksum. dist itself has no universal-binary option for executables.

The toolchain is pinned in `rust-toolchain.toml` (1.93.1), so local builds and CI use the same compiler.

To cut a release:

1. Update the version in `Cargo.toml`, then `just release-notes <version>`: it renames the CHANGELOG's `## Unreleased` section to `## <version> - <date>`, which dist uses as the release notes (`just release-check` reminds you while the section is still called Unreleased).
2. Run `just release-check`. It runs `dist plan` to show what the release will build and publish, checks the workflow is current, builds both targets with `dist build --artifacts=local`, joins them into the universal binary and runs it.
3. Commit, then tag and push the tag:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

Pushing a tag of the form `vX.Y.Z` starts `.github/workflows/release.yml`, which cargo-dist generates. It builds both targets, creates the GitHub release with the archives, checksums and installers, adds the universal binary, and pushes the updated formula to `boraoku/homebrew-onefolder`. Pushing to the tap needs a repository secret named `HOMEBREW_TAP_TOKEN`, a token with write access to the tap repository. Do not edit `release.yml` by hand; change `dist-workspace.toml` and regenerate it with `dist generate`.

**The owner name.** The repository owner `boraoku` appears in `Cargo.toml`, `dist-workspace.toml`, `README.md` and `docs/ARCHITECTURE.md`; after changing it, `dist generate` rewrites `.github/workflows/release.yml`, and `dist generate --check` in CI fails if that step is forgotten.

`onefolder version` prints the version and the git commit the binary was built from (recorded by `build.rs`), which is how to tell which build a bug report came from.
