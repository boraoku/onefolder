#!/usr/bin/env bash
# Records rclone's JSON log output for the event decoder's fixtures.
#
# Builds small trees in a temporary directory, runs the rclone on PATH with
# OneFolder's flag set (DESIGN.md, Driving rclone), and writes what rclone
# printed under tests/fixtures/rclone/. Temporary paths become /fixture so
# the files are stable; everything else is rclone's own output, unedited.
#
#   scripts/record-fixtures.sh            # or: just fixtures
#
# `just fixtures` then runs the decoder's snapshot tests without updating
# them, so a new rclone release that changes its log lines fails loudly.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
OUT=${1:-$ROOT/tests/fixtures/rclone}
RCLONE=${RCLONE:-rclone}
command -v "$RCLONE" >/dev/null || { echo "rclone is not on PATH" >&2; exit 2; }

# A short name: bisync names its working files after both paths, and the
# two together must fit in 255 bytes.
T=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/of.XXXXXX")" && pwd -P)
# The same path as bisync spells it in those file names.
T_NAME=$(printf '%s' "${T#/}" | tr '/' '_')
cleanup() { chmod -R u+rw "$T" 2>/dev/null || true; rm -rf "$T"; }
trap cleanup EXIT

export RCLONE_CONFIG="$T/rclone.conf"
printf '[onefolder-test]\ntype = alias\nremote = %s\n' "$T/remote" > "$RCLONE_CONFIG"

# The flag set from DESIGN.md: global flags and the job's excludes.
FLAGS=(--use-json-log --stats 1s --stats-log-level NOTICE -v --color never
       --modify-window 2s --retries 3 --low-level-retries 10)
# The preview pass gets one attempt (see rclone::copy_args).
DRY_FLAGS=(--use-json-log --stats 1s --stats-log-level NOTICE -v --color never
           --modify-window 2s --retries 1 --low-level-retries 10)
EXCLUDES=(--exclude .DS_Store --exclude 'node_modules/**' --exclude '*.tmp')

: > "$T/exit-codes.txt"

# Replaces the temporary directory with /fixture, rclone's random
# partial-file suffix with a fixed one, and the date in conflict names
# (bisync's {DateOnly}) with a fixed one, so recordings are stable.
sanitise() {
  sed -e "s|$T|/fixture|g" -e "s|$T_NAME|fixture|g" -e 's|\.[0-9a-f]\{8\}\.partial|.00000000.partial|g' \
      -e 's|conflict-[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}|conflict-2026-10-13|g'
}

# record NAME -- rclone args...: runs rclone, keeps stderr as NAME.jsonl.
record() {
  local name=$1; shift
  local code=0
  "$RCLONE" "$@" 2> "$T/$name.raw" >/dev/null || code=$?
  sanitise < "$T/$name.raw" > "$OUT/$name.jsonl"
  echo "$name $code" >> "$T/exit-codes.txt"
  echo "recorded $name (exit $code)"
}

# seed SRC DST: copies with rclone so modification times match.
seed() { "$RCLONE" copy "$1" "$2" "${EXCLUDES[@]}" 2>/dev/null; }

# A source with new, changed, unchanged, modtime-only and excluded files,
# and a destination that holds a file the source does not have.
tree() {
  local src=$1 dst=$2
  mkdir -p "$src/Projects/2026" "$src/node_modules/pkg" "$dst"
  echo "same"            > "$src/same.txt"
  echo "budget v2"       > "$src/Projects/2026/budget.xlsx"
  echo "touched"         > "$src/touched.txt"
  echo "junk"            > "$src/.DS_Store"
  echo "dep"             > "$src/node_modules/pkg/index.js"
  echo "scratch"         > "$src/draft.tmp"
  seed "$src" "$dst"
  echo "budget v1, longer" > "$dst/Projects/2026/budget.xlsx"
  touch -t 202001010000 "$dst/touched.txt"
  echo "only here"       > "$dst/only-on-destination.txt"
  echo "brief"           > "$src/Projects/2026/brief.docx"
  echo "hello"           > "$src/new.txt"
}

# 1. Preview: copy --dry-run with new, changed and modtime-only files.
tree "$T/a/src" "$T/a/dst"
record copy-dry-run copy "$T/a/src" "$T/a/dst" "${DRY_FLAGS[@]}" "${EXCLUDES[@]}" --dry-run

# 2. The destination listing the preview classifies against.
"$RCLONE" lsjson -R --files-only "${EXCLUDES[@]}" "$T/a/dst" 2>/dev/null | sanitise > "$OUT/lsjson.json"
echo "recorded lsjson"

# 3. The real copy of the same tree (local to local: server-side copies).
record copy-real copy "$T/a/src" "$T/a/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 4. Nothing left to do.
record copy-nothing copy "$T/a/src" "$T/a/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 5. The same tree with server-side copy disabled, as an upload to OneDrive
#    looks: "Copied (new)" and "Copied (replaced existing)".
tree "$T/b/src" "$T/b/dst"
RCLONE_DISABLE=copy record copy-upload copy "$T/b/src" "$T/b/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 6. Excludes: only kept.txt goes across.
mkdir -p "$T/c/src/node_modules/x" "$T/c/dst"
echo k > "$T/c/src/kept.txt"; echo j > "$T/c/src/.DS_Store"
echo n > "$T/c/src/node_modules/x/a.js"; echo t > "$T/c/src/notes.tmp"
record copy-exclude copy "$T/c/src" "$T/c/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 7. A per-file error: an unreadable source file.
mkdir -p "$T/d/src" "$T/d/dst"
echo ok > "$T/d/src/fine.txt"; echo secret > "$T/d/src/locked.txt"; chmod 000 "$T/d/src/locked.txt"
record copy-file-error copy "$T/d/src" "$T/d/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"
chmod 644 "$T/d/src/locked.txt"

# 8. A destination whose parent folders do not exist yet.
record copy-missing-parent copy "$T/d/src" "$T/d/missing/parent/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 9. A source that does not exist.
record copy-missing-source copy "$T/d/nowhere" "$T/d/dst" "${FLAGS[@]}" "${EXCLUDES[@]}"

# 9b. A usage error (an unknown flag). Only the exit code is kept: the
#     output is rclone's help text.
code=0; "$RCLONE" copy --no-such-flag a b >/dev/null 2>&1 || code=$?
echo "copy-usage $code" >> "$T/exit-codes.txt"
echo "recorded copy-usage (exit $code)"

# 10. The alias remote that stands in for OneDrive, with --fast-list.
mkdir -p "$T/remote"
record copy-alias copy "$T/a/src" onefolder-test:Backups/Documents "${FLAGS[@]}" "${EXCLUDES[@]}" --fast-list
"$RCLONE" lsjson -R --files-only --fast-list "${EXCLUDES[@]}" onefolder-test:Backups/Documents 2>/dev/null | sanitise > "$OUT/lsjson-alias.json"
echo "recorded lsjson-alias"
"$RCLONE" size --json "$T/a/dst" 2>/dev/null > "$OUT/size.json"
echo "recorded size"

# 11. Ctrl-C mid-transfer: a large sparse file, a bandwidth limit and no
#     server-side copy, so the copy is still running when SIGINT arrives.
mkdir -p "$T/e/src" "$T/e/dst"
echo small > "$T/e/src/a.txt"
if command -v mkfile >/dev/null; then mkfile -n 64m "$T/e/src/big.bin"; else truncate -s 64M "$T/e/src/big.bin"; fi
RCLONE_DISABLE=copy RCLONE_BWLIMIT=4M "$RCLONE" copy "$T/e/src" "$T/e/dst" "${FLAGS[@]}" "${EXCLUDES[@]}" \
  2> "$T/copy-sigint.raw" >/dev/null &
pid=$!
# Wait for at least two stats lines, so the fixture has an in-flight file.
for _ in $(seq 1 100); do
  [ "$(grep -c '"stats"' "$T/copy-sigint.raw" 2>/dev/null || true)" -ge 2 ] && break
  sleep 0.1
done
kill -INT "$pid"
code=0; wait "$pid" || code=$?
sanitise < "$T/copy-sigint.raw" > "$OUT/copy-sigint.jsonl"
echo "copy-sigint $code" >> "$T/exit-codes.txt"
echo "recorded copy-sigint (exit $code)"

# ---------------------------------------------------------------- bisync
#
# The flag set from DESIGN.md (Two-way sync) as rclone::bisync::args builds
# it: Path1 is the local source, Path2 the destination.

# bisync_flags WORKDIR: the flags of every bisync call.
bisync_flags() {
  BFLAGS=(--workdir "$1" --conflict-resolve path1 --conflict-loser num
          --conflict-suffix 'race-conflict-{DateOnly}-' --suffix-keep-extension
          --create-empty-src-dirs --resilient --recover --max-delete 50
          --compare size,modtime "${EXCLUDES[@]}"
          --exclude '*.conflict-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]-*')
}

# The conflict copy name OneFolder gives the destination's copy of a file.
DATE=$(date +%Y-%m-%d)

# bisync NAME DIR [extra flags...]: records `rclone bisync DIR/src DIR/dst`.
bisync() {
  local name=$1 dir=$2; shift 2
  bisync_flags "$dir/wd"
  record "$name" bisync "$dir/src" "$dir/dst" "${FLAGS[@]}" "${BFLAGS[@]}" "$@"
}

# The same, for a planning pass (one attempt, as rclone::bisync::args).
bisync_plan() {
  local name=$1 dir=$2; shift 2
  bisync_flags "$dir/wd"
  record "$name" bisync "$dir/src" "$dir/dst" "${DRY_FLAGS[@]}" "${BFLAGS[@]}" "$@"
}

# Files on both sides of a pair, sorted, relative to each side.
trees() {
  local dir=$1
  echo "# Path1 (local) after the run"; (cd "$dir/src" && find . -type f | LC_ALL=C sort)
  echo "# Path2 (destination) after the run"; (cd "$dir/dst" && find . -type f | LC_ALL=C sort)
}

# 12. First run: a merge. The destination has a file of its own and a
#     different copy of one local file; Path1 wins (no conflict copy is kept
#     by --resync, which is why OneFolder renames it first).
B=$T/ba
mkdir -p "$B/src/sub/deep" "$B/src/keep" "$B/src/Notes" "$B/dst" "$B/wd"
for i in 1 2 3; do echo "file $i" > "$B/src/sub/f$i.txt"; done
echo deep > "$B/src/sub/deep/d.txt"
for i in 1 2 3 4 5 6; do echo "keep $i" > "$B/src/keep/k$i.txt"; done
echo m > "$B/src/mod.txt"; echo d > "$B/src/del.txt"
echo r > "$B/src/rdel.txt"; echo rm > "$B/src/rmod.txt"
echo "plan v1" > "$B/src/Notes/plan.md"
echo "only here" > "$B/dst/only-on-destination.txt"
echo "a different copy" > "$B/dst/mod.txt"
bisync_plan bisync-resync-dry-run "$B" --resync --resync-mode path1 --dry-run
bisync bisync-resync "$B" --resync --resync-mode path1
sleep 1

# 13. The plan: new, changed and deleted files on both sides, a whole local
#     folder deleted, and a conflict (Notes/plan.md changed on both sides).
echo newlocal > "$B/src/newlocal.txt"; echo "mod changed" > "$B/src/mod.txt"
rm "$B/src/del.txt"; rm -r "$B/src/sub"
echo newremote > "$B/dst/newremote.txt"; echo "rmod changed" > "$B/dst/rmod.txt"
rm "$B/dst/rdel.txt"
echo "plan v2, local" > "$B/src/Notes/plan.md"
echo "plan v2, on the destination, longer" > "$B/dst/Notes/plan.md"
bisync_plan bisync-plan "$B" --dry-run --force

# 14. The real run as OneFolder makes it: first the destination's copy of
#     the conflicted file is set aside under a dated name checked free on
#     both sides; bisync then sees it changed here and gone there, and
#     uploads the local copy. The dated copy is excluded, so it stays on the
#     destination only.
"$RCLONE" moveto "$B/dst/Notes/plan.md" "$B/dst/Notes/plan.conflict-$DATE-1.md" 2>/dev/null
bisync bisync-sync "$B"
trees "$B" | sanitise > "$OUT/bisync-sync-trees.txt"
echo "recorded bisync-sync-trees"

# 15. A run after it: nothing to do.
bisync bisync-after-conflict "$B"

# 15b. Evidence for why OneFolder names conflict copies itself: bisync's
#      own handling with an excluded dated suffix (the first design) next to
#      the earlier copy from 14. bisync cannot see that copy, numbers from
#      -1 again, deletes it and renames over it, then copies the new loser
#      to the local side as well.
sleep 1
echo "plan v3, local, longer" > "$B/src/Notes/plan.md"
echo "plan v3, on the destination, longer still" > "$B/dst/Notes/plan.md"
record bisync-own-conflict bisync "$B/src" "$B/dst" "${FLAGS[@]}" --workdir "$B/wd" \
  --conflict-resolve path1 --conflict-loser num --conflict-suffix 'conflict-{DateOnly}-' \
  --suffix-keep-extension --create-empty-src-dirs --resilient --recover --max-delete 50 \
  --compare size,modtime "${EXCLUDES[@]}" --exclude '*.conflict-*'
trees "$B" | sanitise > "$OUT/bisync-own-conflict-trees.txt"
echo "recorded bisync-own-conflict-trees"

# 15c. bisync's own handling with OneFolder's fallback suffix (used only
#      when a file changes between the plan and the run): the name is not
#      excluded, so the copy syncs to both sides like any file.
sleep 1
echo "plan v4, local, longer again" > "$B/src/Notes/plan.md"
echo "plan v4, on the destination, longer than ever" > "$B/dst/Notes/plan.md"
bisync bisync-race-conflict "$B"
trees "$B" | sanitise > "$OUT/bisync-race-conflict-trees.txt"
echo "recorded bisync-race-conflict-trees"

# 16. A deleted local folder, with nested folders and names that need
#     quoting; the prior listing it is checked against.
C=$T/bc
mkdir -p "$C/src/Old projects/2019/drafts" "$C/src/Old projects/2018" "$C/src/Photos" "$C/dst" "$C/wd"
echo a > "$C/src/Old projects/2019/a b.txt"; echo c > "$C/src/Old projects/2019/c.txt"
echo d > "$C/src/Old projects/2019/drafts/d.txt"; echo e > "$C/src/Old projects/2018/e.txt"
for i in 1 2 3; do echo "photo $i" > "$C/src/Photos/p$i.jpg"; done
echo q > "$C/src/say \"hi\".txt"; echo b > "$C/src/back\\slash.txt"
for i in 1 2 3 4 5 6 7 8; do echo "keep $i" > "$C/src/keep$i.txt"; done
bisync bisync-folder-resync "$C" --resync --resync-mode path1
sanitise < "$(ls "$C"/wd/*.path1.lst)" > "$OUT/bisync.path1.lst"
echo "recorded bisync.path1.lst"
rm -r "$C/src/Old projects/2019"; rm "$C/src/Photos/p1.jpg" "$C/src/Photos/p2.jpg"
echo moved > "$C/src/elsewhere.txt"
bisync_plan bisync-folder-plan "$C" --dry-run --force

# 17. Safety aborts on a pair of ten files.
D=$T/bd
mkdir -p "$D/src" "$D/dst" "$D/wd" "$D/aside"
for i in 1 2 3 4 5 6 7 8 9 10; do echo "f$i" > "$D/src/f$i.txt"; done
bisync bisync-safety-resync "$D" --resync --resync-mode path1
rm "$D/src/f1.txt" "$D/src/f2.txt" "$D/src/f3.txt" "$D/src/f4.txt" "$D/src/f5.txt" "$D/src/f6.txt"
bisync bisync-too-many-deletes "$D"
bisync bisync-too-many-deletes-force "$D" --force
# Listings gone, backups too (a damaged working directory). With only the
# listings gone, --recover reverts to the backups (.lst-old) by itself.
mkdir -p "$D/aside-wd"
mv "$D"/wd/*.lst* "$D/aside-wd/"
bisync bisync-missing-listings "$D"
mv "$D"/aside-wd/*.lst* "$D/wd/"
# A filters file: bisync wants a resync the first time it sees one (this
# is why OneFolder never passes one).
printf -- '- /x/**\n' > "$D/filters.txt"
bisync bisync-filters-file "$D" --filters-file "$D/filters.txt"
# An empty destination (an unmounted drive's empty mount point).
mv "$D"/dst/* "$D/aside/"
bisync bisync-empty-side "$D"
mv "$D"/aside/* "$D/dst/"
# A destination folder that does not exist.
bisync_flags "$D/wd2"
record bisync-missing-destination bisync "$D/src" "$D/nowhere" "${FLAGS[@]}" "${BFLAGS[@]}" --resync --resync-mode path1

# 18. Every file changed on one side.
G=$T/bg
mkdir -p "$G/src" "$G/dst" "$G/wd"
for i in 1 2 3; do echo "v1 $i" > "$G/src/g$i.txt"; done
bisync bisync-all-changed-resync "$G" --resync --resync-mode path1
sleep 1
for i in 1 2 3; do echo "v2, longer $i" > "$G/src/g$i.txt"; done
bisync bisync-all-changed "$G"

# 19. A lock left by a run that was killed, and a run that meets it.
#     (Bandwidth limit and no server-side copy, so it is still running.)
if command -v mkfile >/dev/null; then mkfile -n 48m "$D/src/big.bin"; else truncate -s 48M "$D/src/big.bin"; fi
bisync_flags "$D/wd"
RCLONE_DISABLE=copy RCLONE_BWLIMIT=2M "$RCLONE" bisync "$D/src" "$D/dst" "${FLAGS[@]}" "${BFLAGS[@]}" \
  2> "$T/bisync-killed.raw" >/dev/null &
pid=$!
for _ in $(seq 1 100); do
  [ "$(grep -c '"bytes":[1-9]' "$T/bisync-killed.raw" 2>/dev/null || true)" -ge 1 ] && break
  sleep 0.1
done
kill -9 "$pid"; wait "$pid" 2>/dev/null || true
sanitise < "$(ls "$D"/wd/*.lck)" > "$OUT/bisync.lck"
echo "recorded bisync.lck"
bisync bisync-locked "$D"
rm "$D"/wd/*.lck "$D"/dst/*.partial
# Finish that run unrecorded, so the next one has a single file in flight.
bisync_flags "$D/wd"
"$RCLONE" bisync "$D/src" "$D/dst" "${FLAGS[@]}" "${BFLAGS[@]}" >/dev/null 2>&1 || true

# 20. Ctrl-C (SIGINT) mid-transfer: bisync wraps up gracefully for 30 s,
#     then cancels the copy and saves its listings; the next run (always
#     with --recover) carries on.
if command -v mkfile >/dev/null; then mkfile -n 64m "$D/src/big2.bin"; else truncate -s 64M "$D/src/big2.bin"; fi
RCLONE_DISABLE=copy RCLONE_BWLIMIT=1M "$RCLONE" bisync "$D/src" "$D/dst" "${FLAGS[@]}" "${BFLAGS[@]}" \
  2> "$T/bisync-sigint.raw" >/dev/null &
pid=$!
for _ in $(seq 1 100); do
  [ "$(grep -c '"bytes":[1-9]' "$T/bisync-sigint.raw" 2>/dev/null || true)" -ge 2 ] && break
  sleep 0.1
done
kill -INT "$pid"
code=0; wait "$pid" || code=$?
sanitise < "$T/bisync-sigint.raw" > "$OUT/bisync-sigint.jsonl"
echo "bisync-sigint $code" >> "$T/exit-codes.txt"
echo "recorded bisync-sigint (exit $code)"
bisync bisync-recover "$D"

# 21. Paths so long that bisync's working file names pass 255 bytes.
L=$T/bl/$(printf 'long-folder-name-%.0s' 1 2 3 4 5 6 7)
mkdir -p "$L/src" "$L/dst" "$L/wd"
echo x > "$L/src/x.txt"
bisync bisync-name-too-long "$L" --resync --resync-mode path1

{
  echo "# rclone exit code of each recorded run, from $("$RCLONE" version | head -1)"
  cat "$T/exit-codes.txt"
} > "$OUT/exit-codes.txt"
echo "done: $OUT"
