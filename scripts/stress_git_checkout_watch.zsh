#!/usr/bin/env zsh
set -euo pipefail

# Stress test for watcher-driven incremental indexing under VCS operations.
#
# This simulates the most common "daily change" in large projects: switching branches/commits,
# which can touch thousands of files and trigger watcher event storms.
#
# Usage:
#   scripts/stress_git_checkout_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19784 TOKEN=testtoken DEBOUNCE_MS=200 ITER=10 REF_A=HEAD REF_B=HEAD~1

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19784}"
base_url="http://127.0.0.1:${port}"
token="${TOKEN:-testtoken}"
debounce_ms="${DEBOUNCE_MS:-200}"
iter="${ITER:-10}"
ref_a="${REF_A:-HEAD}"
ref_b="${REF_B:-HEAD~1}"

index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
trap "rm -rf ${index_dir} 2>/dev/null || true" EXIT

if [[ ! -d "${project_root}/.git" ]]; then
  echo "expected a git repo at ${project_root} (missing .git)" >&2
  exit 1
fi

echo "Verifying git working tree is clean..."
(
  cd "${project_root}"
  if [[ -n "$(git status --porcelain=v1)" ]]; then
    echo "git working tree is dirty; please commit/stash first" >&2
    git status --porcelain=v1 >&2
    exit 1
  fi
)

echo "Resolving refs..."
(
  cd "${project_root}"
  if ! git rev-parse --verify -q "${ref_a}" >/dev/null; then
    echo "invalid REF_A=${ref_a}" >&2
    exit 1
  fi
  if ! git rev-parse --verify -q "${ref_b}" >/dev/null; then
    echo "invalid REF_B=${ref_b}" >&2
    exit 1
  fi
)

echo "Building release binaries..."
cargo build -q -p unity-asset-search-daemon -p unity-asset-search-cli --release

echo "Starting daemon..."
target/release/unity-asset-search-daemon \
  --project-root "${project_root}" \
  --index-dir "${index_dir}" \
  --listen "127.0.0.1:${port}" \
  --token "${token}" \
  --no-auto-reindex \
  --watch \
  --watch-debounce-ms "${debounce_ms}" \
  2>"${index_dir}/daemon.log" &
pid=$!
trap "kill ${pid} 2>/dev/null || true; rm -rf ${index_dir} 2>/dev/null || true" EXIT

echo "Waiting for daemon to become ready..."
for i in {1..200}; do
  if target/release/unity-asset-search-cli --base-url "${base_url}" health >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

wait_idle() {
  local label="$1"
  for i in {1..80}; do
    local json
    json="$(target/release/unity-asset-search-cli --base-url "${base_url}" status)"
    local ok
    ok="$(echo "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get(\"indexing\") is False))')"
    if [[ "${ok}" == "1" ]]; then
      echo "${label}: idle"
      echo "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(json.dumps({\"indexed_docs\": st.get(\"indexed_docs\"), \"updated_docs\": st.get(\"updated_docs\"), \"removed_docs\": st.get(\"removed_docs\"), \"last_index_duration_ms\": st.get(\"last_index_duration_ms\"), \"last_scan_ms\": st.get(\"last_scan_ms\")}, ensure_ascii=False))'
      return 0
    fi
    sleep 0.5
  done

  echo "timeout waiting for idle after ${label}" >&2
  target/release/unity-asset-search-cli --base-url "${base_url}" status >&2 || true
  if [[ -f "${index_dir}/daemon.log" ]]; then
    echo "daemon log (tail):" >&2
    tail -120 "${index_dir}/daemon.log" >&2 || true
  fi
  return 1
}

baseline_for_ref() {
  local ref="$1"
  echo "Checkout ${ref}..."
  (
    cd "${project_root}"
    git checkout -q "${ref}"
  )

  echo "Full reindex for baseline (${ref})..."
  target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null
  wait_idle "baseline-${ref}"

  target/release/unity-asset-search-cli --base-url "${base_url}" status \
    | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get("indexed_docs") or 0))'
}

echo "Computing baselines (this may take a while on large projects)..."
baseline_a="$(baseline_for_ref "${ref_a}")"
baseline_b="$(baseline_for_ref "${ref_b}")"
echo "baseline_a(${ref_a})=${baseline_a}"
echo "baseline_b(${ref_b})=${baseline_b}"

echo "Switching between refs for ${iter} iterations (watcher-driven incremental)..."
for i in $(seq 1 "${iter}"); do
  echo "iter ${i}/${iter}: checkout ${ref_a}"
  (
    cd "${project_root}"
    git checkout -q "${ref_a}"
  )
  wait_idle "checkout-${ref_a}"
  cur_a="$(target/release/unity-asset-search-cli --base-url "${base_url}" status | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get(\"indexed_docs\") or 0))')"
  if [[ "${cur_a}" -ne "${baseline_a}" ]]; then
    echo "indexed_docs mismatch after checkout ${ref_a}: got ${cur_a}, expected ${baseline_a}" >&2
    exit 1
  fi

  echo "iter ${i}/${iter}: checkout ${ref_b}"
  (
    cd "${project_root}"
    git checkout -q "${ref_b}"
  )
  wait_idle "checkout-${ref_b}"
  cur_b="$(target/release/unity-asset-search-cli --base-url "${base_url}" status | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get(\"indexed_docs\") or 0))')"
  if [[ "${cur_b}" -ne "${baseline_b}" ]]; then
    echo "indexed_docs mismatch after checkout ${ref_b}: got ${cur_b}, expected ${baseline_b}" >&2
    exit 1
  fi
done

echo "Restoring ${ref_a}..."
(
  cd "${project_root}"
  git checkout -q "${ref_a}"
)

echo "Done."

