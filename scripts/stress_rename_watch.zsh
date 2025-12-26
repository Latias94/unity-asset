#!/usr/bin/env zsh
set -euo pipefail

# Stress test for watcher-driven incremental indexing on rename/move operations.
#
# Usage:
#   scripts/stress_rename_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19783 TOKEN=testtoken FILES=200 DEBOUNCE_MS=200

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19783}"
base_url="http://127.0.0.1:${port}"
token="${TOKEN:-testtoken}"
files="${FILES:-200}"
debounce_ms="${DEBOUNCE_MS:-200}"

dir_a="${project_root}/Assets/zz_unity_asset_search_rename_a"
dir_b="${project_root}/Assets/zz_unity_asset_search_rename_b"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
trap "rm -rf ${index_dir} 2>/dev/null || true" EXIT

rm -rf "${dir_a}" "${dir_b}"

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

mkdir -p "${dir_a}"

write_fixture() {
  local i="$1"
  local guid
  guid="$(printf '%032x' "${i}")"
  local asset="${dir_a}/obj_${i}.prefab"
  local meta="${asset}.meta"

  cat > "${asset}" <<EOF
%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: RenameObj${i}
EOF

  cat > "${meta}" <<EOF
fileFormatVersion: 2
guid: ${guid}
EOF
}

wait_idle() {
  local label="$1"
  for i in {1..40}; do
    local json
    json="$(target/release/unity-asset-search-cli --base-url "${base_url}" status)"
    local ok
    ok="$(echo "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get(\"indexing\") is False))')"
    if [[ "${ok}" == "1" ]]; then
      echo "${label}: idle"
      echo "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(json.dumps({\"updated_docs\": st.get(\"updated_docs\"), \"removed_docs\": st.get(\"removed_docs\"), \"last_index_duration_ms\": st.get(\"last_index_duration_ms\"), \"last_scan_ms\": st.get(\"last_scan_ms\")}, ensure_ascii=False))'
      return 0
    fi
    sleep 1
  done

  echo "timeout waiting for idle after ${label}" >&2
  target/release/unity-asset-search-cli --base-url "${base_url}" status >&2 || true
  if [[ -f "${index_dir}/daemon.log" ]]; then
    echo "daemon log (tail):" >&2
    tail -80 "${index_dir}/daemon.log" >&2 || true
  fi
  return 1
}

count_hits() {
  local query="$1"
  target/release/unity-asset-search-cli --base-url "${base_url}" search "${query}" --limit 5000 \
    | python3 -c 'import json,sys; r=json.load(sys.stdin); print(int(r.get("total_hits") or 0))'
}

echo "Creating ${files} YAML assets in dir A..."
for i in $(seq 1 "${files}"); do
  write_fixture "${i}"
done

echo "Full reindex (baseline)..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null

echo "Verify A is searchable..."
a_hits="$(count_hits "in:Assets/zz_unity_asset_search_rename_a")"
if [[ "${a_hits}" -lt "${files}" ]]; then
  echo "expected >=${files} hits in A, got ${a_hits}" >&2
  exit 1
fi

echo "Rename dir A -> dir B..."
rm -rf "${dir_b}"
mv "${dir_a}" "${dir_b}"
wait_idle "rename"

echo "Verify old prefix is gone and new prefix is present..."
old_hits="$(count_hits "in:Assets/zz_unity_asset_search_rename_a")"
new_hits="$(count_hits "in:Assets/zz_unity_asset_search_rename_b")"
echo "old_hits=${old_hits} new_hits=${new_hits}"

if [[ "${old_hits}" -ne 0 ]]; then
  echo "expected 0 hits for old prefix, got ${old_hits}" >&2
  exit 1
fi
if [[ "${new_hits}" -lt "${files}" ]]; then
  echo "expected >=${files} hits for new prefix, got ${new_hits}" >&2
  exit 1
fi

echo "Done."

