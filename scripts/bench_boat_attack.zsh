#!/usr/bin/env zsh
set -euo pipefail

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19781}"
base_url="http://127.0.0.1:${port}"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
daemon_pid=""

cleanup() {
  if [[ -n "${daemon_pid}" ]]; then
    kill "${daemon_pid}" 2>/dev/null || true
  fi
  rm -rf -- "${index_dir}"
}
trap cleanup EXIT

echo "Building release binaries..."
cargo build -q -p unity-asset-search-daemon -p unity-asset-search-cli --release

echo "Starting daemon..."
target/release/unity-asset-search-daemon \
  --project-root "${project_root}" \
  --index-dir "${index_dir}" \
  --listen "127.0.0.1:${port}" \
  --no-startup-reindex \
  --reconcile-interval-ms 0 &
daemon_pid=$!

echo "Waiting for daemon to become ready..."
ready=0
for _ in {1..100}; do
  if target/release/unity-asset-search-cli --base-url "${base_url}" health >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.05
done
if [[ "${ready}" -ne 1 ]]; then
  echo "daemon did not become ready" >&2
  exit 1
fi

token_file="${index_dir}/daemon.token"
if [[ ! -r "${token_file}" ]]; then
  echo "daemon token is not readable: ${token_file}" >&2
  exit 1
fi
token="$(tr -d '\r\n' < "${token_file}")"

echo "Full reindex..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full
target/release/unity-asset-search-cli --base-url "${base_url}" status

echo "Bench..."
target/release/unity-asset-search-cli --base-url "${base_url}" bench --repeat 3 --warmup 1 --limit 20

echo "Done."
