#!/bin/bash
# Smoke test of the installed RPM. Run by Packit through tmt.
set -euxo pipefail

# tmt runs tests from a copy of the repository; TMT_TREE is its root.
tree="${TMT_TREE:-$(dirname "$0")/../../..}"
root="$tree/tests/fixtures/roots/shared"

# The packaged binary, with the version and the commit filled in.
test "$(command -v bree)" = /usr/bin/bree
version="$(bree --version)"
echo "$version"
if grep -Eq '@|unknown|unfilled' <<<"$version"; then
    echo "bree --version shows a placeholder or no commit" >&2
    exit 1
fi
grep -Eq '[0-9a-f]{12}' <<<"$version"

# Render a profile of the fixture root, then check the render is current.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/home"
run() { env -i HOME="$tmp/home" USER=alice PATH="$PATH" bree --root "$root" -p work "$@"; }

run render --out "$tmp/out"
test -f "$tmp/out/.claude/CLAUDE.md"
test -f "$tmp/out/.claude/.bree-manifest.json"
run check --out "$tmp/out"

# A change to a rendered file is reported with exit status 1.
echo extra >> "$tmp/out/.claude/CLAUDE.md"
rc=0
run check --out "$tmp/out" || rc=$?
test "$rc" = 1
