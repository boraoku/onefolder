# Changelog

All notable changes to OneFolder are listed here. Versions follow semantic
versioning; the release workflow publishes a version when its tag (such as
`v0.1.0`) is pushed. Before tagging, `just release-notes <version>` renames
the Unreleased section to `## <version> - <date>`, which dist uses as the
release notes.

## Unreleased

### Several jobs, one after another

- **Run a job** (and `onefolder run` on its own) ends its list with
  `Several jobs, one after another…` when there are two jobs or more. It
  runs all of them, or the ones you tick on a selection screen (Space
  ticks, `a` all, `n` none, Enter starts); the ticks are remembered for
  next time, and each job shows what the Jobs list would warn about, such
  as a drive not mounted.
- `onefolder run documents records` runs those jobs and `onefolder run
  --all` runs every job, one after another in config order. `run --all
  --yes` is the form for launchd, systemd and Task Scheduler, and the
  README's schedule examples use it.
- Each job runs exactly as it would alone. A job that fails its checks is
  recorded and the next one starts; a job whose last run is still going
  is skipped (`another run in progress`); only Ctrl-C stops the batch, and
  the jobs not yet started are recorded as `not run, the batch was
  stopped`.
- Every screen of a batch has a line at the top saying which job of how
  many is running and how the others ended: `Several jobs · job 2 of 3 ·
  documents ✓ · records · photos next`. Without a terminal, `Job 2 of 3:
  records` is printed before each job.
- The preview asks `Continue? [Y/n/a]`: `a` is yes to this and every
  remaining preview and plan confirmation of the batch. The question
  about moved files and the folder prompt still ask.
- The batch ends with a closing summary: a line per job, and under it a
  line per destination with its outcome and reason. The exit code follows
  the usual rule over every destination, and is 2 when no job ran at all
  (each skipped because another run held it, or because it has no
  destinations).
- With `--json`, every event of a batch carries `batch: {index, of}`, a
  job that does not run sends `job-skipped`, and a final `batch-done`
  lists every job with its destinations and the batch's exit code. A
  single job's output is unchanged.
- `--dest` belongs to one job and is refused with several; a job name that
  does not exist refuses the whole batch before anything runs. A name
  given twice runs once, as a single job; `--all` is a batch even with one
  job. `--resync` and `--folder-deletes` apply to every job of a batch.
- A closing summary no longer repeats `not confirmed` after a destination
  that was not confirmed.

## 0.2.1 - 2026-10-26

### Run screen

- The live figures of a preview or a two-way plan no longer show
  `listed`: rclone counts every entry on both sides there, folders too,
  so it read as a second, larger count of the same files. The `--json`
  `progress` event still carries `listed`.
- The copy and sync steps show their full totals from the start, taken
  from the preview or the plan: `Comparing · 2,726 of 33,388 checked`,
  `Transferring · 8 of 20 files` and the size under the bar no longer grow
  as rclone walks the folder. If rclone finds more than the preview did,
  its own figures are shown, so a count never reads `9 of 8`. The ETA
  follows the full size. Plain output shows the same totals; the `--json`
  `stats` event keeps rclone's own figures.
- File counts on the copy screen and in the plain stats line have
  thousands separators, like the rest of OneFolder.

## 0.2.0 - 2026-10-16

### Linux and Windows (WSL)

- OneFolder runs on Linux (x86_64 and ARM64, glibc 2.35 or newer) and on
  Windows through WSL (WSL 2, and WSL 1), as well as on macOS. Releases
  include Linux builds: the shell installer works on Linux and WSL, and
  `brew install boraoku/apps/onefolder` works with Homebrew on Linux.
  Without Homebrew, install rclone from https://rclone.org/install/;
  `doctor` and the rclone-missing message say so on Linux, and name
  `brew upgrade rclone` only when Homebrew is there.
- External drives are checked on Linux as on a Mac. Any file system
  mounted on its own (at `/media/<you>/<label>`, `/run/media`, `/mnt`,
  `/data`, `/srv/backup` or anywhere else) is recorded at setup by its
  file system UUID (or partition UUID, or device and label) from `lsblk`,
  and checked before every run: an unplugged drive, the wrong drive, a
  drive mounted somewhere else and a read-only drive (including one whose
  file system went read-only after an error) are refused with the same
  messages as on a Mac. Only the system disk is left unchecked: `/`, the
  standard system mount points such as `/home`, `/var` and `/boot`, and
  memory file systems such as `/tmp`; setup names the mount a folder is
  on. A folder under `/media`, `/run/media` or `/mnt` that is not on a
  mounted drive is refused at setup. A btrfs subvolume or a bind mount of
  a drive's folder is recorded as that folder, so another one mounted in
  its place is the wrong drive, and a file system mounted over the drive
  later counts as the drive being gone. Network shares and file systems
  with no block device get the weaker device and name check.
- Under WSL, a Windows drive (`/mnt/d` and so on) is identified by its
  Windows volume GUID and label, which OneFolder asks Windows for through
  PowerShell (a drive check may take up to 30 seconds there).
  `/mnt/c`, the Windows system drive, counts as the system disk, so a
  destination under `/mnt/c/Users/...` is allowed and not checked. A
  drive plugged in after WSL started is not mounted by WSL, and the "not
  mounted" message says how (`sudo mount -t drvfs E: /mnt/e`, with the
  letter Windows has the drive at now). A drive mounted at another folder
  is reported as mounted somewhere else. With WSL's Windows interop off,
  the check says so and how to turn it on.
- Moved files and case: on Linux, where most file systems tell
  `Report.pdf` and `report.pdf` apart, a rename that only changes case is
  now a move between two such folders (ext4, btrfs, xfs and the like),
  offered like any other move. Where either side ignores case (OneDrive,
  every folder on a Mac, FAT, exFAT, NTFS and HFS+ drives, network shares,
  Windows drives under WSL, any other file system, and a casefolded ext4
  or f2fs folder), it is still the same file.
- Messages say "this computer" on Linux and WSL where a Mac says "this
  Mac", and two-way counts say `2 here` for `2 to Mac`. On a Mac every
  message is unchanged.
- The browser's `v` key opens the drives folder of the system:
  `/Volumes` on a Mac, `/media/<you>` (or `/run/media/<you>`, `/media`,
  `/mnt`) on Linux, `/mnt` under WSL; the job form's example drive path
  follows it.
- The run lock and bisync's lock read the boot time and process names
  from `/proc` on Linux.
- `just fixtures` records rclone's output into a temporary folder and
  never rewrites the committed fixtures.

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
