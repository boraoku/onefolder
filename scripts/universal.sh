#!/bin/sh
# Joins the aarch64 and x86_64 macOS release archives that dist builds into
# one universal archive, onefolder-universal-apple-darwin.tar.xz, with its
# .sha256 file beside it. Used by `just release-check` and by the
# publish-universal job of the release workflow.
#
# Usage: scripts/universal.sh <folder holding the two .tar.xz archives>
set -eu

dir=${1:?usage: scripts/universal.sh <folder with the dist archives>}
name=onefolder-universal-apple-darwin
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    archive="$dir/onefolder-$target.tar.xz"
    [ -f "$archive" ] || { echo "missing $archive" >&2; exit 1; }
    tar -xJf "$archive" -C "$work"
done

mkdir -p "$work/$name"
lipo -create -output "$work/$name/onefolder" \
    "$work/onefolder-aarch64-apple-darwin/onefolder" \
    "$work/onefolder-x86_64-apple-darwin/onefolder"
# The other files (README, LICENSE, CHANGELOG) are the same in both.
for f in "$work"/onefolder-aarch64-apple-darwin/*; do
    [ "$(basename "$f")" = onefolder ] || cp "$f" "$work/$name/"
done

# Both architectures must be there, or the release stops here.
archs=$(lipo -archs "$work/$name/onefolder")
case "$archs" in
    *arm64*x86_64* | *x86_64*arm64*) ;;
    *) echo "the universal binary has only: $archs" >&2; exit 1 ;;
esac

tar -cJf "$dir/$name.tar.xz" -C "$work" "$name"
(cd "$dir" && shasum -a 256 "$name.tar.xz" > "$name.tar.xz.sha256")
rm -rf "$dir/$name"
cp -R "$work/$name" "$dir/$name"
echo "$dir/$name.tar.xz ($archs)"
