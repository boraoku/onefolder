# OneFolder tasks. Run `just` to list them.

default:
    @just --list

# Build the debug binary
build:
    cargo build

# Unit, snapshot and CLI tests (no rclone needed)
test:
    cargo test

# Integration tests against the rclone on PATH
test-integration:
    ONEFOLDER_INTEGRATION=1 cargo test --test integration

# Formatting and clippy, as CI runs them
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Re-record the rclone JSON log fixtures with the rclone on PATH, then check
# the decoder's snapshots without updating them: fails if rclone's output changed
fixtures:
    scripts/record-fixtures.sh
    INSTA_UPDATE=no INSTA_FORCE_PASS=0 cargo test --bin onefolder -- rclone::events rclone::listing rclone::exec::tests::exit_codes_follow_rclones_documentation sync::

# Checks a release before tagging: dist's plan, the release workflow is
# current, then both macOS targets built with dist and joined into the
# universal binary, which must hold both architectures and run
release-check:
    @grep -q '^## Unreleased' CHANGELOG.md && echo "Note: CHANGELOG.md still says '## Unreleased'. Before tagging, run: just release-notes <version> (dist uses that section as the release notes)" || true
    dist plan
    dist generate --check
    dist build --artifacts=local
    scripts/universal.sh target/distrib
    file target/distrib/onefolder-universal-apple-darwin/onefolder
    lipo -archs target/distrib/onefolder-universal-apple-darwin/onefolder
    target/distrib/onefolder-universal-apple-darwin/onefolder version

# Names the CHANGELOG's Unreleased section for a release, such as
# `just release-notes 0.1.0`, which makes it `## 0.1.0 - 2026-10-13`; dist
# then uses that section as the GitHub release's notes. Commit it before
# tagging v<version>.
release-notes version:
    @grep -q '^## Unreleased' CHANGELOG.md || (echo "CHANGELOG.md has no '## Unreleased' section" >&2; exit 1)
    sed -i '' "s/^## Unreleased$/## {{version}} - $(date +%Y-%m-%d)/" CHANGELOG.md
    @grep -n '^## {{version}} - ' CHANGELOG.md
