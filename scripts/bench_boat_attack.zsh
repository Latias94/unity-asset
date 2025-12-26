#!/usr/bin/env zsh
set -euo pipefail

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19781}"
base_url="http://127.0.0.1:${port}"
token="${TOKEN:-testtoken}"

echo "Building release binaries..."
cargo build -q -p unity-asset-search-daemon -p unity-asset-search-cli --release

echo "Starting daemon..."
target/release/unity-asset-search-daemon \
  --project-root "${project_root}" \
  --listen "127.0.0.1:${port}" \
  --token "${token}" \
  --no-auto-reindex \
  --watch &
pid=$!
trap "kill ${pid} 2>/dev/null || true" EXIT

echo "Waiting for daemon to become ready..."
for i in {1..100}; do
  if target/release/unity-asset-search-cli --base-url "${base_url}" health >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

echo "Full reindex..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full
target/release/unity-asset-search-cli --base-url "${base_url}" status

echo "Bench..."
target/release/unity-asset-search-cli --base-url "${base_url}" bench --repeat 3 --warmup 1 --limit 20

echo "Done."
