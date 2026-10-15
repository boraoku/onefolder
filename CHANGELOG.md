# Changelog

All notable changes to OneFolder are listed here. Versions follow semantic
versioning; the release workflow publishes a version when its tag (such as
`v0.1.0`) is pushed. Before tagging, `just release-notes <version>` renames
the Unreleased section to `## <version> - <date>`, which dist uses as the
release notes.

## 0.1.0 - 2026-10-15

The first release, built in seven milestones, and the changes after them.

### After milestone 7

- `onefolder` with no arguments now stays full screen for the whole
  session. Before, every switch between the menu, the job picker, the
  run screen and the summaries left the full screen and came back, so
  your shell's earlier commands flashed into view and the picker and the
  summaries were drawn on the normal screen between them. Now the menu
  takes the screen once and gives it back once, when you quit. The job
  picker, the account questions and pickers, and rclone's sign-in are
  drawn inside it; a run's summary blocks, Status, Doctor and any other
  output appear on a screen of their own, which `↑`/`↓`, `j`/`k`,
  `pgup`/`pgdn` and `g`/`G` scroll and any other key closes, in place of
  "Press any key to go back to the menu." When you quit (`q`, Esc or
  Ctrl-C), the summary block of every run of the session is printed to
  the terminal, with any destination that did not run, so the results
  and their `Log:` paths stay in your scrollback. While Status or Doctor
  works, the screen says `Checking…`. Subcommands such as `onefolder run documents` behave as
  before.
- An exclude such as `_0RAG/*` now leaves out the whole `_0RAG` folder.
  rclone's `*` never crosses a folder boundary, so before, only the
  files directly in the folder were left out and everything in its
  subfolders was still copied. OneFolder now hands rclone `folder/**`
  for `folder/*`, and for a name or path with no wildcard, such as
  `_0RAG` or `Archive/2019`, both the pattern (a file of that name) and
  the pattern with `/**` (the folder and everything in it); before, such
  a pattern matched only a file and left a folder of that name in.
  Patterns with wildcards, such as `*.tmp` or `build*`, keep rclone's
  meaning, and spaces at either end of a pattern are ignored. The job
  keeps the patterns as you typed them; with `--json`,
  `effective_excludes` lists the widened ones.
- One-way runs notice files and folders moved or renamed on your Mac. A
  new file whose exact content is already on the destination under a
  name the Mac no longer has (same size, then the same QuickXorHash,
  which OneDrive lists at no cost and which OneFolder reads on a drive
  only for the few files that could match) is shown in a Moved row and a
  Moved files list, a renamed folder as one line. The run asks `Move them
  on OneDrive instead of uploading them again? [Y/n]`: yes moves them on
  the destination before the copy (a folder in one step where nothing
  else is in it), no uploads them again as before. A file moved and
  changed is uploaded; where the content could match more than one file
  and the time and name do not settle it, nothing is guessed. `--moves
  ask|move|copy` answers in advance (`move` with `--yes` or without a
  terminal); `--dry-run` shows what would move. The summary says
  `Moved: N files, size (on OneDrive, no upload)`; the Deleted count
  stays 0. With `--json`: `moves` in `preview`, a `moves` event, step
  `moving` with its `progress`, and `moved` in the summary. Each move is
  logged before and after; a failed move is listed as an error and the
  file is uploaded instead. Names compare ignoring case, as on every
  destination OneFolder supports, so a rename that only changes case is
  not a move; identical files of a renamed folder stay with their
  folder; `--moves ask` without a terminal uploads again rather than
  moving; new folders for moves are made one at a time first.
- The confirmation screen of a run can show the whole list. Where the
  preview or the two-way plan shortens a list to its first 10 files, the
  `and N more` line says `l whole list`; `l` opens every shortened list
  in full, with its count and size, to scroll (`↑↓`, `pgup`/`pgdn`,
  `g`/`G`) and filter (`/`, which also finds names macOS stores with
  decomposed accents). The heading of the list in view stays at the top.
  `Esc`, `q` or `l` goes back to the question as it was (a kept filter is
  cleared by the first `Esc`); `y`, `n` and Enter do nothing while the
  list is open.
- Paths with wide characters (Chinese, Japanese, Korean) or decomposed
  accents no longer push the size column out of line in the preview and
  on the transfer screen: paths are measured and shortened in display
  columns. Plain output, `--dry-run` and `--json` are unchanged.
- System files are never synced: `.DS_Store`, `._*`, `.localized`,
  `Thumbs.db`, `ehthumbs.db`, `ehthumbs_vista.db`, `desktop.ini` (with
  either case of first letter), and the
  folders `.Spotlight-V100`, `.Trashes`, `.fseventsd`, `.TemporaryItems`,
  `.DocumentRevisions-V100`, `$RECYCLE.BIN` and `System Volume
  Information` are left out of every job, one-way and two-way.
- Hidden files and folders (names starting with a dot) are now left out
  by default. A job copies them when "Copy hidden files" is Yes in the job
  form, `--copy-hidden yes` is given to `jobs add` or `jobs edit`, or
  `copy_hidden = true` is in its `config.toml` entry. Existing jobs now
  skip hidden files unless `copy_hidden = true` is set. Files that were
  synced before stay where they are on both sides; a two-way destination
  needs no merge. On a two-way destination, hidden files synced before
  stop syncing and drift apart (an edit on one side stays there). Turning
  `copy_hidden` on later brings them back into the sync: a hidden file
  deleted on one side meanwhile comes back from the other, and one edited
  meanwhile can become a conflict, with the destination's copy kept as a
  `.conflict` copy (such as `.hidden.conflict-2026-10-13-1`).
- Choosing Delete in the folder-deletion prompt now removes the folder on
  the destination completely, including the hidden and system files
  inside it, after a clean run; a folder still holding a file the sync
  handles (such as a conflict copy) is left, and the run says so. The
  summary names the folders removed.
- Files that left the sync (newly excluded, or deleted on both sides) no
  longer count when the two-way plan decides whether more than half a side
  would be deleted, so they can never hide a real mass deletion, and no
  longer bring up the folder-deletion prompt for a folder the sync cannot
  touch. Restoring a folder brings back only what the sync would.
- The preview says what is left out: the job's own patterns, whether
  hidden files are copied, and the system files always excluded; `--json`
  previews and plans carry `copy_hidden` and every pattern in force.
- The preview and the two-way plan show live figures while they run: the
  time so far, files checked and listed, files to copy and their size, and
  how many destination files have been listed, so a long preview of a
  large folder no longer looks stuck. Without a terminal a line is printed
  at most every 10 seconds; `--json` sends a `progress` event at most once
  a second.
- bisync always gets its excludes as repeated `--exclude` flags, never a
  file, however many patterns a job has.
- The folder fields of the job form (the source, a local destination's
  folder and a OneDrive folder) complete with Tab while you type: Tab
  fills in the first folder whose name starts with what you typed
  (ignoring case; hidden folders only after a dot), more Tabs and
  Shift-Tab step through the others, and the line under the field lists
  them with the current one in brackets, such as `2 of 3: Desktop,
  [Documents], Downloads`. A OneDrive folder's names are
  listed once in the background (`listing…`). While a folder field is
  being edited, Tab completes instead of moving to the next field: Enter
  ends the edit and stays on the field, and `↑`/`↓` move on.
- A folder browser: `b` on a folder field (Ctrl-B while typing) opens
  `Choose a folder`, a list of the folders in a folder with a bar and the
  size of each, measured in the background and counting up while you
  look, after Mole's disk browser. Enter opens a folder, `←` or `h` goes
  up, Space chooses the highlighted folder, `c` the folder you are in,
  `.` shows hidden folders, `/` filters, `~` and `v` go home and to
  `/Volumes`, `r` measures again, Esc goes back. A drive mounted inside
  the folder shows the space used on it instead of being walked, so `v`
  never reads every disk in full. It works for OneDrive too (`rclone
  lsjson` and `rclone size`, two at a time), and shows the drive's free
  space.
- Quitting with Ctrl-C now also ends the rclone commands OneFolder
  started in the background (a listing, a folder size, an account
  check), which before could carry on after OneFolder had gone. A run's
  own rclone is handled as before: the first Ctrl-C stops it, a second
  aborts it.

### Milestone 1: scaffold

- One binary, `onefolder`, with every subcommand: `accounts`, `jobs`, `run`,
  `status`, `doctor`, `completion` and `version`; `--json` and `--dry-run`
  on every command that can change something.
- The config at `~/.config/onefolder/config.toml`, validated on load, with
  comments and unknown keys kept when OneFolder saves it.
- rclone found on PATH and checked against the minimum version, 1.66.

### Milestone 2: accounts

- `accounts add` signs in through rclone's own browser flow; `list`,
  `remove`, `test` and `reconnect`, each with `--json`.

### Milestone 3: jobs

- Jobs with an ordered list of OneDrive and local destinations, each with
  its own mode, added and edited in a form or with flags.
- External drives recorded by volume identity, so an unplugged drive is
  never taken for an empty folder.

### Milestone 4: one-way runs

- A preview of new and changed files, a confirmation, then `rclone copy`
  with a progress screen and a summary block; nothing is ever deleted on
  the destination.
- Per-run logs and a state file with the last run of each destination.

### Milestone 5: two-way runs

- `rclone bisync` with a first-run merge, the plan, the folder-deletion
  prompt (delete, keep and stop syncing, or restore), holds and
  `jobs release`.
- Conflicts: the local copy wins and the destination's copy is kept there as
  `name.conflict-<date>-<n>.ext`.

### Milestone 6: several destinations and volumes

- Destinations run in order, each with its own summary, and a closing
  summary for the job; the drive is checked before every run.
- `status`, and a per-job run lock so two runs of a job never overlap.

### Milestone 7: polish and release

- The main menu, with a status line for each item, `?` for the keys and
  `/` to filter the jobs and accounts lists; Ctrl-C quits from any screen
  but the run screen, where it stops the run.
- `doctor --prune-logs` removes old run logs when asked (older than 90
  days, or beyond the newest 50 of a destination), keeping the last run's
  log, logs that record a conflict, and logs of removed jobs; logs are
  never deleted automatically.
- `status` and `doctor` show a run in progress or a stale run lock.
- `accounts test` exits 2 when every account fails; `accounts remove`
  needs `--yes` without a terminal and with `--json`; a sign-in whose
  check times out keeps the account with a warning; `accounts add` asks
  about delta listing.
- Drive checks give up after 10 seconds on a hanging network drive (2
  seconds for the lists' quick look), and follow a symlinked destination
  to the drive it leads to.
- The job form, and `jobs add` and `jobs edit` with flags, warn when the
  paths are too long for bisync's file names.
- Release builds with cargo-dist: macOS binaries for Apple silicon and
  Intel, a universal binary, a shell installer and a Homebrew formula that
  installs rclone too.
