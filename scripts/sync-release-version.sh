#!/usr/bin/env bash
# Keep every shipped component on the same release version.
set -euo pipefail

usage() {
  echo "usage: $0 <current-version> <next-version> [repository-root]" >&2
  echo "       $0 --check <expected-version> [repository-root]" >&2
  exit 1
}

case "${1:-}" in
  --check)
    [ "$#" -ge 2 ] && [ "$#" -le 3 ] || usage
    mode="check"
    current="$2"
    next="$2"
    repository_root="${3:-.}"
    ;;
  "") usage ;;
  *)
    [ "$#" -ge 2 ] && [ "$#" -le 3 ] || usage
    mode="update"
    current="$1"
    next="$2"
    repository_root="${3:-.}"
    ;;
esac

python3 - "$mode" "$current" "$next" "$repository_root" <<'PY'
from pathlib import Path
import re
import sys

mode, current, next_version, root_arg = sys.argv[1:]
root = Path(root_arg).resolve()

records = (
    (
        root / "Cargo.toml",
        r'(?m)\A(?P<prefix>\[package\]\n(?:(?!^\[).*\n)*?version = ")'
        r'(?P<version>[^"\n]+)(?P<suffix>"[^\n]*$)',
        "root Cargo package",
    ),
    (
        root / "admin-ui/package.json",
        r'(?m)^(?P<prefix>  "version": ")(?P<version>[^"\n]+)(?P<suffix>",\s*)$',
        "admin UI package",
    ),
    (
        root / "desktop/src-tauri/Cargo.toml",
        r'(?m)\A(?P<prefix>\[package\]\n(?:(?!^\[).*\n)*?version = ")'
        r'(?P<version>[^"\n]+)(?P<suffix>"[^\n]*$)',
        "desktop Cargo package",
    ),
    (
        root / "desktop/src-tauri/tauri.conf.json",
        r'(?m)^(?P<prefix>  "version": ")(?P<version>[^"\n]+)(?P<suffix>",\s*)$',
        "Tauri bundle",
    ),
    (
        root / "Cargo.lock",
        r'(?P<prefix>\[\[package\]\]\nname = "kiro-rs"\nversion = ")'
        r'(?P<version>[^"\n]+)(?P<suffix>")',
        "kiro-rs lockfile package",
    ),
    (
        root / "Cargo.lock",
        r'(?P<prefix>\[\[package\]\]\nname = "kiro-rs-desktop"\nversion = ")'
        r'(?P<version>[^"\n]+)(?P<suffix>")',
        "kiro-rs-desktop lockfile package",
    ),
)

updated_files: dict[Path, str] = {}
for path, pattern, label in records:
    text = updated_files.get(path)
    if text is None:
        text = path.read_text(encoding="utf-8")

    matches = list(re.finditer(pattern, text))
    if len(matches) != 1:
        raise SystemExit(
            f"{path}: expected exactly one {label} version record, found {len(matches)}"
        )

    match = matches[0]
    actual = match.group("version")
    if actual != current:
        action = "validation" if mode == "check" else "update"
        raise SystemExit(
            f"{path}: {label} version is {actual}, expected {current}; refusing {action}"
        )

    if mode == "update":
        replacement = f'{match.group("prefix")}{next_version}{match.group("suffix")}'
        text = f"{text[:match.start()]}{replacement}{text[match.end():]}"
    updated_files[path] = text

if mode == "update":
    for path, text in updated_files.items():
        path.write_text(text, encoding="utf-8")
PY
