#!/bin/sh
# Demonstrates OneFolder refusing to sync to an unplugged drive on Linux,
# on a real file system: a 64 MB ext4 image with the label Backup, mounted
# with sudo where udisks would mount a USB drive, /media/$USER/Backup.
#
#   scripts/demo-linux-drive.sh            # works in a fresh temp folder
#   DEMO_DIR=/some/empty/folder scripts/demo-linux-drive.sh
#
# What it does (the scenario of scripts/demo-unplugged-drive.sh):
#   1. makes the image, mounts it at /media/$USER/Backup, adds a job with a
#      destination on it (the drive's UUID is recorded) and runs it once;
#   2. unmounts it ("unplugs the drive"): the mount point is left behind as
#      an empty folder on the system disk, the dangerous case, and the run
#      is refused with the design's message; status and doctor say so;
#   3. mounts it at /media/$USER/Backup1 (as udisks does when the name is
#      taken) and runs: OneFolder says where the drive is and how to point
#      the job at it;
#   4. mounts another drive with the same label at the right place: the
#      UUID gives it away, and the run is refused as the wrong drive;
#   5. mounts the right drive read-only at the right place: refused, since
#      a destination must be writable;
#   6. unmounts everything and removes the mount points and the temp folder.
#
# Uses its own config, state and rclone config (ONEFOLDER_CONFIG_DIR,
# ONEFOLDER_DATA_DIR, RCLONE_CONFIG), so your own setup is never touched.
# Needs Linux, sudo (for mount), mkfs.ext4 and util-linux, rclone on PATH,
# and a built binary (cargo build), or ONEFOLDER=/path/to/onefolder.

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ONEFOLDER=${ONEFOLDER:-$ROOT/target/debug/onefolder}
if [ ! -x "$ONEFOLDER" ]; then
    echo "Build OneFolder first (cargo build), or set ONEFOLDER." >&2
    exit 2
fi
command -v rclone >/dev/null 2>&1 || { echo "rclone is not on PATH." >&2; exit 2; }
[ "$(uname -s)" = Linux ] || { echo "This demo needs Linux." >&2; exit 2; }
command -v mkfs.ext4 >/dev/null 2>&1 || PATH=$PATH:/sbin:/usr/sbin
command -v mkfs.ext4 >/dev/null 2>&1 || { echo "This demo needs mkfs.ext4 (e2fsprogs)." >&2; exit 2; }
sudo -n true 2>/dev/null || sudo -v || { echo "This demo needs sudo to mount the image." >&2; exit 2; }

if [ -n "${DEMO_DIR:-}" ]; then
    WORK=$DEMO_DIR
    mkdir -p "$WORK"
    OWN_WORK=0
else
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/onefolder-demo.XXXXXX")
    OWN_WORK=1
fi
WORK=$(cd "$WORK" && pwd -P)

ME=$(id -un)
MEDIA="/media/$ME"
DRIVE="$MEDIA/Backup"
ELSEWHERE="$MEDIA/Backup1"
IMAGE="$WORK/drive.img"
IMPOSTOR="$WORK/impostor.img"
MADE_MEDIA=0
[ -d "$MEDIA" ] || MADE_MEDIA=1

export ONEFOLDER_CONFIG_DIR="$WORK/config"
export ONEFOLDER_DATA_DIR="$WORK/data"
export RCLONE_CONFIG="$WORK/rclone.conf"
export NO_COLOR=1
: >"$RCLONE_CONFIG"

if mountpoint -q "$DRIVE" 2>/dev/null || mountpoint -q "$ELSEWHERE" 2>/dev/null; then
    echo "Something is already mounted at $DRIVE or $ELSEWHERE; unmount it first." >&2
    exit 2
fi

# unmount <mount point>: unmounts if mounted (a loop mount frees its device).
unmount() {
    if mountpoint -q "$1" 2>/dev/null; then
        sudo umount "$1" || sudo umount -l "$1"
    fi
}

cleanup() {
    unmount "$DRIVE"
    unmount "$ELSEWHERE"
    sudo rmdir "$DRIVE" "$ELSEWHERE" 2>/dev/null
    [ "$MADE_MEDIA" = 1 ] && sudo rmdir "$MEDIA" 2>/dev/null
    if [ "$OWN_WORK" = 1 ]; then
        rm -rf "$WORK"
    else
        rm -rf "$IMAGE" "$IMPOSTOR" "$WORK/config" "$WORK/data" "$WORK/src" "$RCLONE_CONFIG"
    fi
}
# POSIX sh carries on after an INT or TERM handler returns, so those exit
# explicitly; EXIT covers every other way out.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# attach <image> <mount point> [ro]: mounts the image through a loop device.
attach() {
    sudo mkdir -p "$2" || exit 1
    sudo mount -o "loop${3:+,$3}" "$1" "$2" || exit 1
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

echo "== Making a 64 MB ext4 image, label \"Backup\""
truncate -s 64M "$IMAGE" && mkfs.ext4 -q -L Backup "$IMAGE" || exit 1
truncate -s 64M "$IMPOSTOR" && mkfs.ext4 -q -L Backup "$IMPOSTOR" || exit 1
attach "$IMAGE" "$DRIVE"
sudo chown "$ME:" "$DRIVE"
mkdir -p "$DRIVE/Documents"
step "findmnt $DRIVE"
findmnt "$DRIVE"

mkdir -p "$WORK/src"
echo "hello" >"$WORK/src/hello.txt"
echo "notes" >"$WORK/src/notes.md"

echo
echo "== 1. A job with a destination on the drive"
show jobs add demo --source "$WORK/src" --local "$DRIVE/Documents"
show run demo --yes
step "ls $DRIVE/Documents"
ls "$DRIVE/Documents"

echo
echo "== 2. Unplug the drive (umount): the mount point stays as an empty folder"
unmount "$DRIVE"
step "ls -A $DRIVE   (now an empty folder on the system disk)"
ls -A "$DRIVE"
show run demo --yes
show status demo
show doctor
step "ls -A $DRIVE   (still empty: nothing was written)"
ls -A "$DRIVE"

echo
echo "== 3. The drive plugged in again, but mounted somewhere else"
attach "$IMAGE" "$ELSEWHERE"
echo "Mounted at $ELSEWHERE"
show run demo --yes --json
unmount "$ELSEWHERE"

echo
echo "== 4. Another drive with the same label at the right place"
attach "$IMPOSTOR" "$DRIVE"
sudo chown "$ME:" "$DRIVE"
mkdir -p "$DRIVE/Documents"
show run demo --yes
step "ls -A $DRIVE/Documents   (still empty: nothing was written)"
ls -A "$DRIVE/Documents"
unmount "$DRIVE"

echo
echo "== 5. The right drive at the right place, but mounted read-only"
attach "$IMAGE" "$DRIVE" ro
echo "Mounted read-only at $DRIVE"
show run demo --yes
show status demo
unmount "$DRIVE"

echo
echo "== Cleaning up"
trap - EXIT INT TERM
cleanup
step "findmnt $DRIVE; findmnt $ELSEWHERE"
findmnt "$DRIVE" || findmnt "$ELSEWHERE" || echo "nothing mounted"
echo "Done."
