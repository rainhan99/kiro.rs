#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
test_root="$(mktemp -d)"
fixture_root="$test_root/aligned"
trap 'rm -rf "$test_root"' EXIT

mkdir -p "$fixture_root/admin-ui" "$fixture_root/desktop/src-tauri"
cp "$repo_root/Cargo.toml" "$fixture_root/Cargo.toml"
cp "$repo_root/Cargo.lock" "$fixture_root/Cargo.lock"
cp "$repo_root/admin-ui/package.json" "$fixture_root/admin-ui/package.json"
cp "$repo_root/desktop/src-tauri/Cargo.toml" "$fixture_root/desktop/src-tauri/Cargo.toml"
cp "$repo_root/desktop/src-tauri/tauri.conf.json" "$fixture_root/desktop/src-tauri/tauri.conf.json"

current="$(sed -n 's/^version = "\(.*\)"/\1/p' "$fixture_root/Cargo.toml" | head -n1)"
next="$(bash "$repo_root/scripts/next-version.sh" patch "$current")"
bash "$repo_root/scripts/sync-release-version.sh" "$current" "$next" "$fixture_root"

assert_contains() {
  local path="$1"
  local expected="$2"
  python3 - "$path" "$expected" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
expected = sys.argv[2]
if expected not in path.read_text(encoding="utf-8"):
    raise SystemExit(f"expected {path} to contain: {expected}")
PY
}

assert_contains "$fixture_root/Cargo.toml" "version = \"$next\""
assert_contains "$fixture_root/admin-ui/package.json" "  \"version\": \"$next\","
assert_contains "$fixture_root/desktop/src-tauri/Cargo.toml" "version = \"$next\""
assert_contains "$fixture_root/desktop/src-tauri/tauri.conf.json" "  \"version\": \"$next\","
assert_contains "$fixture_root/Cargo.lock" "name = \"kiro-rs\"
version = \"$next\""
assert_contains "$fixture_root/Cargo.lock" "name = \"kiro-rs-desktop\"
version = \"$next\""

# Every release entry path (automatic bump, explicit version, or tag) can use
# the same non-mutating validation after it resolves the requested version.
bash "$repo_root/scripts/sync-release-version.sh" --check "$next" "$fixture_root"

# A stale component must fail both validation and the next update without
# leaving an already-validated file partially changed.
mismatch_root="$test_root/mismatch"
cp -R "$fixture_root" "$mismatch_root"
python3 - "$mismatch_root/desktop/src-tauri/tauri.conf.json" "$next" "$current" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
path.write_text(text.replace(f'"version": "{sys.argv[2]}"', f'"version": "{sys.argv[3]}"', 1), encoding="utf-8")
PY

if bash "$repo_root/scripts/sync-release-version.sh" --check "$next" "$mismatch_root" \
    >"$test_root/mismatch-check.log" 2>&1; then
  echo "expected version validation to reject a stale Tauri version" >&2
  exit 1
fi

snapshot_root="$test_root/snapshot"
cp -R "$mismatch_root" "$snapshot_root"
next_after="$(bash "$repo_root/scripts/next-version.sh" patch "$next")"
if bash "$repo_root/scripts/sync-release-version.sh" "$next" "$next_after" "$mismatch_root" \
    >"$test_root/mismatch-update.log" 2>&1; then
  echo "expected version update to reject a stale Tauri version" >&2
  exit 1
fi
diff -ru "$snapshot_root" "$mismatch_root"

# Duplicate package records must not satisfy an "exactly once" check.
duplicate_root="$test_root/duplicate"
cp -R "$fixture_root" "$duplicate_root"
python3 - "$duplicate_root/Cargo.lock" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
match = re.search(r'\[\[package\]\]\nname = "kiro-rs"\n.*?(?=\n\[\[package\]\]|\Z)', text, re.DOTALL)
if match is None:
    raise SystemExit("kiro-rs lockfile package not found in fixture")
path.write_text(text + "\n" + match.group(0) + "\n", encoding="utf-8")
PY

if bash "$repo_root/scripts/sync-release-version.sh" --check "$next" "$duplicate_root" \
    >"$test_root/duplicate.log" 2>&1; then
  echo "expected version validation to reject duplicate kiro-rs lockfile packages" >&2
  exit 1
fi

echo "sync-release-version test: update, validation, and fail-closed checks passed"
