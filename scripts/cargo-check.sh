#!/usr/bin/env bash
# Repo-wide Rust type check used by the `cargo-check` Turborepo task.
#
# Discovers every valid Cargo workspace in the repository, checks each one with
# `cargo check`, and keeps incremental artifacts in a fingerprinted target
# directory so repeated turbo runs reuse compilation results. Structured
# key=value logs go to stderr and a machine-readable summary is written to
# .turbo/cargo-check/last-run.json.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage: scripts/cargo-check.sh [options] [-- <cargo check args>]

Options:
  -h, --help              Show this help.
  -w, --workspace PATH    Check only PATH (repeatable). Paths are relative to
                          the repository root unless absolute.
      --no-cache          Use a disposable target directory instead of the
                          fingerprinted incremental cache.
      --fail-fast         Stop after the first failing workspace.
  -v, --verbose           Echo the exact cargo invocation.
  --                      Remaining arguments are forwarded to `cargo check`.

Environment:
  CARGO                   Cargo binary to use (default: cargo).
  CARGO_CHECK_CACHE_DIR   Cache root (default: .turbo/cargo-check).

Exit codes:
  0 all workspaces checked successfully
  1 at least one workspace failed to check
  2 invalid usage or missing toolchain
EOF
}

log() { printf '[cargo-check] %s\n' "$*" >&2; }
die() {
  log "$1"
  exit "${2:-2}"
}

# Node.js on Windows passes C:\ paths to bash; translate them when cygpath is
# available so explicit --workspace arguments work on every platform.
to_posix_path() {
  local path="$1"
  if command -v cygpath >/dev/null 2>&1 && [[ "$path" =~ ^[A-Za-z]:[\\/] ]]; then
    cygpath -u "$path"
  else
    printf '%s' "$path"
  fi
}

WORKSPACES=()
NO_CACHE=0
FAIL_FAST=0
VERBOSE=0
FORWARDED=()
CARGO_BIN="${CARGO:-cargo}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    -w|--workspace)
      [[ $# -ge 2 ]] || die 'error=invalid_usage detail=--workspace_requires_path' 2
      WORKSPACES+=("$2")
      shift 2
      ;;
    --no-cache)
      NO_CACHE=1
      shift
      ;;
    --fail-fast)
      FAIL_FAST=1
      shift
      ;;
    -v|--verbose)
      VERBOSE=1
      shift
      ;;
    --)
      shift
      FORWARDED+=("$@")
      break
      ;;
    *)
      die "error=invalid_usage detail=unknown_argument:$1" 2
      ;;
  esac
done

command -v "$CARGO_BIN" >/dev/null 2>&1 || die "error=cargo_not_found detail=$CARGO_BIN" 2
command -v rustc >/dev/null 2>&1 || die 'error=rustc_not_found detail=rustc' 2

CACHE_DIR="${CARGO_CHECK_CACHE_DIR:-$REPO_ROOT/.turbo/cargo-check}"
mkdir -p "$CACHE_DIR"

discover_workspaces() {
  local manifest workspace
  while IFS= read -r manifest; do
    grep -q '^\[workspace\]' "$manifest" || continue
    workspace="$(dirname -- "$manifest")"
    # The repository has a legacy root workspace whose members moved into
    # stellar-lend/. Ignore manifests cargo itself cannot load; an explicitly
    # requested workspace is still checked and fails loudly.
    if ! "$CARGO_BIN" metadata --no-deps --format-version 1 --manifest-path "$manifest" >/dev/null 2>&1; then
      log "event=skip_workspace workspace=${workspace#$REPO_ROOT/} reason=metadata_unavailable"
      continue
    fi
    printf '%s\n' "$workspace"
  done < <(find "$REPO_ROOT" -name Cargo.toml \
    -not -path '*/target/*' \
    -not -path '*/.git/*' \
    -not -path '*/.turbo/*' | LC_ALL=C sort)
}

resolve_workspaces() {
  local supplied resolved manifest
  if [[ ${#WORKSPACES[@]} -gt 0 ]]; then
    for supplied in "${WORKSPACES[@]}"; do
      supplied="$(to_posix_path "$supplied")"
      if [[ "$supplied" = /* ]]; then
        resolved="$supplied"
      else
        resolved="$REPO_ROOT/$supplied"
      fi
      [[ -d "$resolved" ]] || die "error=workspace_not_found detail=$supplied" 2
      manifest="$resolved/Cargo.toml"
      [[ -f "$manifest" ]] || die "error=workspace_manifest_missing detail=$supplied" 2
      printf '%s\n' "$resolved"
    done
    return
  fi
  discover_workspaces
}

fingerprint_for() {
  local workspace="$1" digest manifest
  digest="$(
    {
      "$CARGO_BIN" --version 2>/dev/null || true
      rustc --version 2>/dev/null || true
      find "$workspace" -type f \
        \( -name Cargo.toml -o -name Cargo.lock -o -name rust-toolchain.toml \) \
        -not -path '*/target/*' -print | LC_ALL=C sort |
        while IFS= read -r manifest; do
          printf '\n--- %s\n' "${manifest#$REPO_ROOT/}"
          cat "$manifest"
        done
    } | cksum
  )"
  printf '%s' "${digest%% *}"
}

now_ms() {
  local value
  value="$(date +%s%3N 2>/dev/null)" || value="$(date +%s)000"
  printf '%s' "$value"
}

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

TMP_TARGET=""
cleanup() {
  if [[ -n "$TMP_TARGET" && -d "$TMP_TARGET" ]]; then
    rm -rf -- "$TMP_TARGET"
  fi
}
trap cleanup EXIT

mapfile -t CHECK_ROOTS < <(resolve_workspaces)
[[ ${#CHECK_ROOTS[@]} -gt 0 ]] || die 'error=no_workspaces detail=no_valid_cargo_workspace_found' 2

FAILED=0
SUMMARY_ENTRIES=()
for workspace in "${CHECK_ROOTS[@]}"; do
  relative="${workspace#$REPO_ROOT/}"
  [[ "$relative" = "$workspace" ]] && relative="."

  if [[ $NO_CACHE -eq 1 ]]; then
    TMP_TARGET="$(mktemp -d "$CACHE_DIR/no-cache.XXXXXX")"
    target="$TMP_TARGET"
    cache_state='disabled'
  else
    fingerprint="$(fingerprint_for "$workspace")"
    target="$CACHE_DIR/$fingerprint/target"
    mkdir -p "$target"
    if [[ -d "$target/debug/.fingerprint" ]]; then
      cache_state='hit'
    else
      cache_state='miss'
    fi
  fi

  started="$(now_ms)"
  log "event=check_start workspace=$relative cache=$cache_state target=$target"
  if [[ $VERBOSE -eq 1 ]]; then
    log "event=command workspace=$relative argv=$CARGO_BIN check --workspace --all-targets --manifest-path $workspace/Cargo.toml ${FORWARDED[*]:-}"
  fi

  set +e
  (
    export CARGO_TARGET_DIR="$target"
    export CARGO_INCREMENTAL=1
    "$CARGO_BIN" check --workspace --all-targets \
      --manifest-path "$workspace/Cargo.toml" "${FORWARDED[@]}"
  )
  code=$?
  set -e

  duration=$(( $(now_ms) - started ))
  if [[ $code -eq 0 ]]; then
    log "event=check_complete workspace=$relative status=success cache=$cache_state duration_ms=$duration"
    result='success'
  else
    log "event=check_complete workspace=$relative status=failure code=$code cache=$cache_state duration_ms=$duration"
    result='failure'
    FAILED=$((FAILED + 1))
  fi
  SUMMARY_ENTRIES+=("{\"workspace\":\"$(json_escape "$relative")\",\"status\":\"$result\",\"exit_code\":$code,\"cache\":\"$cache_state\",\"duration_ms\":$duration}")

  if [[ -n "$TMP_TARGET" ]]; then
    rm -rf -- "$TMP_TARGET"
    TMP_TARGET=""
  fi

  if [[ $FAILED -gt 0 && $FAIL_FAST -eq 1 ]]; then
    break
  fi
done

{
  printf '{"version":1,"generated_at":"%s","workspaces":[' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  for index in "${!SUMMARY_ENTRIES[@]}"; do
    [[ $index -gt 0 ]] && printf ','
    printf '%s' "${SUMMARY_ENTRIES[$index]}"
  done
  printf '],"failed":%d}\n' "$FAILED"
} > "$CACHE_DIR/last-run.json"

log "event=summary workspaces=${#CHECK_ROOTS[@]} failed=$FAILED report=$CACHE_DIR/last-run.json"
[[ $FAILED -eq 0 ]] || exit 1
