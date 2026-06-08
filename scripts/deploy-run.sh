#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/deploy-run.sh [options]

Build and deploy codex-proxy-rs into the local unpacked run directory.

Options:
  --target DIR       Deployment directory (default: ../run-v0.0.1-linux-x86_64)
  --restart          Restart the process running from the target directory
  --skip-tests       Skip cargo test before deploying
  --no-frontend      Skip npm frontend build and frontend/dist deployment
  -h, --help         Show this help

The script preserves config.yaml, auths*, data, and other runtime files. Existing
binary and frontend/dist are moved under TARGET/backups/<timestamp>/.
USAGE
}

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="$ROOT_DIR/../run-v0.0.1-linux-x86_64"
RESTART=0
RUN_TESTS=1
BUILD_FRONTEND=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      [[ $# -ge 2 ]] || { echo "missing value for --target" >&2; exit 2; }
      TARGET_DIR="$2"
      shift 2
      ;;
    --restart)
      RESTART=1
      shift
      ;;
    --skip-tests)
      RUN_TESTS=0
      shift
      ;;
    --no-frontend)
      BUILD_FRONTEND=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"
BIN_SRC="$ROOT_DIR/target/release/codex-proxy-rs"
BIN_DST="$TARGET_DIR/codex-proxy-rs"
CONFIG_DST="$TARGET_DIR/config.yaml"
RUN_SH="$TARGET_DIR/run.sh"
TS="$(date +%Y%m%d-%H%M%S)"
BACKUP_DIR="$TARGET_DIR/backups/$TS"

if [[ ! -f "$CONFIG_DST" ]]; then
  echo "target config.yaml not found: $CONFIG_DST" >&2
  exit 1
fi
if [[ ! -x "$RUN_SH" ]]; then
  echo "target run.sh not found or not executable: $RUN_SH" >&2
  exit 1
fi

cd "$ROOT_DIR"

if [[ "$RUN_TESTS" -eq 1 ]]; then
  cargo test --locked
fi

if [[ "$BUILD_FRONTEND" -eq 1 ]]; then
  if [[ ! -d frontend/node_modules ]]; then
    (cd frontend && npm ci)
  fi
  (cd frontend && npm run build)
fi

cargo build --release --locked

mkdir -p "$BACKUP_DIR"
if [[ -f "$BIN_DST" ]]; then
  cp -a "$BIN_DST" "$BACKUP_DIR/codex-proxy-rs"
fi

TMP_BIN="$TARGET_DIR/.codex-proxy-rs.$TS.tmp"
install -m 0755 "$BIN_SRC" "$TMP_BIN"
mv "$TMP_BIN" "$BIN_DST"

if [[ "$BUILD_FRONTEND" -eq 1 && -d "$ROOT_DIR/frontend/dist" ]]; then
  mkdir -p "$TARGET_DIR/frontend"
  if [[ -d "$TARGET_DIR/frontend/dist" ]]; then
    mv "$TARGET_DIR/frontend/dist" "$BACKUP_DIR/frontend-dist"
  fi
  mkdir -p "$TARGET_DIR/frontend/dist"
  cp -a "$ROOT_DIR/frontend/dist/." "$TARGET_DIR/frontend/dist/"
fi

find_target_pids() {
  local pid cwd
  for pid in $(pgrep -x codex-proxy-rs 2>/dev/null || true); do
    cwd="$(readlink "/proc/$pid/cwd" 2>/dev/null || true)"
    if [[ "$cwd" == "$TARGET_DIR" ]]; then
      printf '%s\n' "$pid"
    fi
  done
}

wait_for_health() {
  local url="http://127.0.0.1:18080/health"
  if ! command -v curl >/dev/null 2>&1; then
    sleep 1
    return 0
  fi
  for _ in $(seq 1 40); do
    if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

if [[ "$RESTART" -eq 1 ]]; then
  mapfile -t pids < <(find_target_pids)
  if [[ "${#pids[@]}" -gt 0 ]]; then
    echo "stopping codex-proxy-rs: ${pids[*]}"
    kill -TERM "${pids[@]}"
    for _ in $(seq 1 40); do
      mapfile -t remaining < <(find_target_pids)
      [[ "${#remaining[@]}" -eq 0 ]] && break
      sleep 0.25
    done
    mapfile -t remaining < <(find_target_pids)
    if [[ "${#remaining[@]}" -gt 0 ]]; then
      echo "process did not stop after TERM: ${remaining[*]}" >&2
      exit 1
    fi
  fi

  rm -f "$TARGET_DIR/codex-proxy.pid"
  (
    cd "$TARGET_DIR"
    nohup ./codex-proxy-rs --config ./config.yaml > codex-proxy.log 2>&1 &
  )

  if ! wait_for_health; then
    echo "codex-proxy-rs did not pass health check; recent log output:" >&2
    tail -n 40 "$TARGET_DIR/codex-proxy.log" >&2 || true
    exit 1
  fi
  mapfile -t started < <(find_target_pids)
  if [[ "${#started[@]}" -eq 0 ]]; then
    echo "codex-proxy-rs did not start; recent log output:" >&2
    tail -n 40 "$TARGET_DIR/codex-proxy.log" >&2 || true
    exit 1
  fi
  printf '%s\n' "${started[0]}" > "$TARGET_DIR/codex-proxy.pid"
  echo "started codex-proxy-rs with pid ${started[0]}"
else
  echo "deployed files only; run with --restart to restart the target process"
fi

echo "backup: $BACKUP_DIR"
