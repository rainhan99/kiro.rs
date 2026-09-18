#!/usr/bin/env bash
# 版本号自增（里程表式进位）。
#
# 格式 x.xx.xxx 是**位数预算**，不是补零显示：minor 上限 99（2 位），patch 上限
# 999（3 位）。补零不可行——Cargo 按 SemVer 校验，`0.09.001` 会直接报
# `invalid leading zero in minor version number` 并拒绝构建。
#
#   patch  每次发布 +1；从 999 再进位 → patch=0, minor+1
#   minor  大版本 +1，patch 归零；从 99 再进位 → minor=0, major+1
#   major  +1，minor 与 patch 归零
#
# 用法:
#   scripts/next-version.sh patch 0.9.0     -> 0.9.1
#   scripts/next-version.sh minor 0.9.7     -> 0.10.0
#   scripts/next-version.sh major 0.9.7     -> 1.0.0
#   scripts/next-version.sh --self-test
set -euo pipefail

readonly MAX_MINOR=99
readonly MAX_PATCH=999

die() {
  echo "next-version: $*" >&2
  exit 1
}

# 解析并校验一个版本号。拒绝前导零：它既不是合法 SemVer，也会让 Cargo 拒绝构建。
parse_version() {
  local version="$1"
  [[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
    || die "invalid version '$version'; expected MAJOR.MINOR.PATCH without leading zeros"
  MAJOR="${BASH_REMATCH[1]}"
  MINOR="${BASH_REMATCH[2]}"
  PATCH="${BASH_REMATCH[3]}"
  (( MINOR <= MAX_MINOR )) || die "minor $MINOR exceeds the ${MAX_MINOR} budget (x.xx.xxx)"
  (( PATCH <= MAX_PATCH )) || die "patch $PATCH exceeds the ${MAX_PATCH} budget (x.xx.xxx)"
}

# 进位到 minor：minor 本身溢出时再进位到 major。
carry_minor() {
  MINOR=$(( MINOR + 1 ))
  PATCH=0
  if (( MINOR > MAX_MINOR )); then
    MINOR=0
    MAJOR=$(( MAJOR + 1 ))
  fi
}

next_version() {
  local level="$1"
  parse_version "$2"
  case "$level" in
    patch)
      PATCH=$(( PATCH + 1 ))
      # patch 位满即向 minor 进位，minor 满再向 major 进位。
      if (( PATCH > MAX_PATCH )); then
        carry_minor
      fi
      ;;
    minor) carry_minor ;;
    major)
      MAJOR=$(( MAJOR + 1 ))
      MINOR=0
      PATCH=0
      ;;
    *) die "unknown level '$level'; expected patch, minor or major" ;;
  esac
  printf '%s.%s.%s\n' "$MAJOR" "$MINOR" "$PATCH"
}

self_test() {
  local failures=0
  check() { # check <level> <from> <expected>
    local got
    got="$(next_version "$1" "$2")"
    if [ "$got" != "$3" ]; then
      echo "FAIL: $1 $2 -> $got (expected $3)" >&2
      failures=$(( failures + 1 ))
    fi
  }
  check_rejects() {
    # 必须在子 shell 里调用：die 用的是 exit，直接调用会连整个自测脚本一起杀掉，
    # 而且前面通过的检查是静默的，表现为「毫无输出且退出码非零」，极难定位。
    if ( next_version "$1" "$2" ) >/dev/null 2>&1; then
      echo "FAIL: $1 $2 should have been rejected" >&2
      failures=$(( failures + 1 ))
    fi
  }

  # 常规自增
  check patch 0.9.0    0.9.1
  check patch 0.9.998  0.9.999
  check minor 0.9.7    0.10.0
  check major 0.9.7    1.0.0

  # patch 位满 → 进位到 minor，patch 归零
  check patch 0.9.999  0.10.0
  # minor 位满时 patch 再进位 → 一路进到 major
  check patch 0.99.999 1.0.0
  # minor 位满再自增 → major
  check minor 0.99.0   1.0.0
  check minor 3.99.500 4.0.0
  # major 自增清空低位
  check major 2.37.845 3.0.0
  # 边界值本身可用，不提前进位
  check patch 0.98.998 0.98.999
  check minor 0.98.0   0.99.0

  # 非法输入必须拒绝，而不是猜
  check_rejects patch 0.09.001   # 前导零：Cargo 会拒绝构建
  check_rejects patch 1.2        # 位数不足
  check_rejects patch 1.2.3.4    # 位数过多
  check_rejects patch v1.2.3     # 带前缀
  check_rejects patch 1.100.0    # minor 超出 xx 预算
  check_rejects patch 1.2.1000   # patch 超出 xxx 预算
  check_rejects bogus 1.2.3      # 未知级别

  if (( failures == 0 )); then
    echo "next-version self-test: all checks passed"
  else
    echo "next-version self-test: $failures check(s) failed" >&2
    return 1
  fi
}

main() {
  case "${1:-}" in
    --self-test) self_test ;;
    patch|minor|major)
      [ $# -eq 2 ] || die "usage: $0 <patch|minor|major> <current-version>"
      next_version "$1" "$2"
      ;;
    *) die "usage: $0 <patch|minor|major> <current-version> | --self-test" ;;
  esac
}

main "$@"
