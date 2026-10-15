#!/bin/sh
# Demonstrates OneFolder refusing to sync to an unplugged drive, on a real
# external volume: an APFS disk image.
#
#   scripts/demo-unplugged-drive.sh            # works in a fresh temp folder
#   DEMO_DIR=/some/empty/folder scripts/demo-unplugged-drive.sh
#
# What it does:
#   1. creates a 20 MB APFS disk image and attaches it (at /Volumes/<name>);
#   2. adds a job with a destination on it (the drive's identity is
#      recorded) and runs it once;
#   3. detaches the image ("unplugs the drive") and runs again: refused;
#   4. attaches it at a folder of its own, adds a second job there, runs,
#      detaches, and runs again: the mount point is now an empty folder on
#      the startup disk, the dangerous case, and the run is refused with
#      the design's message;
#   5. attaches the image somewhere else and runs: OneFolder says where the
#      drive is and how to point the job at it;
#   6. attaches it read-only at the right place and runs: refused, since a
#      destination must be writable;
#   7. detaches everything and removes the temp folder.
#
# Uses its own config, state and rclone config (ONEFOLDER_CONFIG_DIR,
# ONEFOLDER_DATA_DIR, RCLONE_CONFIG), so your own setup is never touched.
# Needs macOS (hdiutil, diskutil), rclone on PATH, and a built binary
# (cargo build), or ONEFOLDER=/path/to/onefolder.

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ONEFOLDER=${ONEFOLDER:-$ROOT/target/debug/onefolder}
if [ ! -x "$ONEFOLDER" ]; then
    echo "Build OneFolder first (cargo build), or set ONEFOLDER." >&2
    exit 2
fi
command -v rclone >/dev/null 2>&1 || { echo "rclone is not on PATH." >&2; exit 2; }
command -v hdiutil >/dev/null 2>&1 || { echo "This demo needs macOS (hdiutil)." >&2; exit 2; }

if [ -n "${DEMO_DIR:-}" ]; then
    WORK=$DEMO_DIR
    mkdir -p "$WORK"
    OWN_WORK=0
else
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/onefolder-demo.XXXXXX")
    OWN_WORK=1
fi
WORK=$(cd "$WORK" && pwd -P)

VOLNAME="OFDemo$$"
IMAGE="$WORK/drive.dmg"
DEVICE=""

export ONEFOLDER_CONFIG_DIR="$WORK/config"
export ONEFOLDER_DATA_DIR="$WORK/data"
export RCLONE_CONFIG="$WORK/rclone.conf"
export NO_COLOR=1
: >"$RCLONE_CONFIG"

detach() {
    if [ -n "$DEVICE" ]; then
        hdiutil detach "$DEVICE" -quiet || hdiutil detach "$DEVICE" -force -quiet
        DEVICE=""
    fi
}

cleanup() {
    detach
    if [ "$OWN_WORK" = 1 ]; then
        rm -rf "$WORK"
    else
        rm -rf "$WORK/drive.dmg" "$WORK/config" "$WORK/data" "$WORK/src" \
            "$WORK/Backup" "$WORK/Elsewhere" "$WORK/attach.err" "$RCLONE_CONFIG"
    fi
}
# POSIX sh carries on after an INT or TERM handler returns, so those exit
# explicitly; EXIT covers every other way out.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# attach [mount point [-readonly]]: attaches the image and sets DEVICE.
attach() {
    # hdiutil warns that -nobrowse is deprecated; the warning is noise here.
    if [ $# -ge 1 ]; then
        out=$(hdiutil attach "$IMAGE" -nobrowse -mountpoint "$1" ${2:-} 2>"$WORK/attach.err")
    else
        out=$(hdiutil attach "$IMAGE" -nobrowse 2>"$WORK/attach.err")
    fi || { cat "$WORK/attach.err" >&2; exit 1; }
    DEVICE=$(printf '%s\n' "$out" | awk '/^\/dev\// {print $1; exit}')
}

step() {
    printf '\n$ %s\n' "$*"
}

# show <args...>: prints the command line, runs OneFolder, prints its exit code.
show() {
    step "onefolder $*"
    "$ONEFOLDER" "$@" 2>&1
    echo "[exit $?]"
}

echo "== Creating a 20 MB APFS disk image, volume \"$VOLNAME\""
hdiutil create -size 20m -fs APFS -volname "$VOLNAME" "$IMAGE" -quiet || exit 1
attach
echo "Attached $DEVICE at /Volumes/$VOLNAME"

mkdir -p "$WORK/src"
echo "hello" >"$WORK/src/hello.txt"
echo "notes" >"$WORK/src/notes.md"
mkdir -p "/Volumes/$VOLNAME/Documents"

echo
echo "== 1. A job with a destination on the drive"
show jobs add demo --source "$WORK/src" --local "/Volumes/$VOLNAME/Documents"
show run demo --yes

echo
echo "== 2. Unplug the drive (hdiutil detach) and run again"
detach
step "ls /Volumes/$VOLNAME"
ls "/Volumes/$VOLNAME" 2>&1
show run demo --yes
show status demo

echo
echo "== 3. The dangerous case: the mount point is left behind as an empty folder"
mkdir -p "$WORK/Backup"
attach "$WORK/Backup"
echo "Attached $DEVICE at $WORK/Backup"
mkdir -p "$WORK/Backup/Documents"
show jobs add demo2 --source "$WORK/src" --local "$WORK/Backup/Documents"
show run demo2 --yes
detach
step "ls -A $WORK/Backup   (now an empty folder on the startup disk)"
ls -A "$WORK/Backup"
show run demo2 --yes
show doctor
step "ls -A $WORK/Backup   (still empty: nothing was written)"
ls -A "$WORK/Backup"

echo
echo "== 4. The drive plugged in again, but mounted somewhere else"
mkdir -p "$WORK/Elsewhere"
attach "$WORK/Elsewhere"
echo "Attached $DEVICE at $WORK/Elsewhere"
show run demo2 --yes --json
detach

echo
echo "== 5. The right drive at the right place, but mounted read-only"
attach "$WORK/Backup" -readonly
echo "Attached $DEVICE read-only at $WORK/Backup"
show run demo2 --yes
show status demo2
detach

echo
echo "== Cleaning up"
trap - EXIT INT TERM
cleanup
step "hdiutil info | grep -c $IMAGE"
hdiutil info | grep -c "$IMAGE"
echo "Done."
