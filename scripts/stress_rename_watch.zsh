#!/usr/bin/env zsh
set -euo pipefail

# Stress watcher-driven indexing across a directory rename.
#
# Usage:
#   scripts/stress_rename_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19783 FILES=200 DEBOUNCE_MS=200

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19783}"
base_url="http://127.0.0.1:${port}"
files="${FILES:-200}"
debounce_ms="${DEBOUNCE_MS:-200}"

case "${files}" in
  ''|0|0[0-9]*|*[!0-9]*)
    echo "FILES must be a positive decimal integer without leading zeros" >&2
    exit 1
    ;;
esac
case "${debounce_ms}" in
  ''|0[0-9]*|*[!0-9]*)
    echo "DEBOUNCE_MS must be a non-negative decimal integer without leading zeros" >&2
    exit 1
    ;;
esac

dir_a="${project_root}/Assets/zz_unity_asset_search_rename_a"
dir_b="${project_root}/Assets/zz_unity_asset_search_rename_b"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
daemon_pid=""

cleanup() {
  if [[ -n "${daemon_pid}" ]]; then
    kill "${daemon_pid}" 2>/dev/null || true
  fi
  rm -rf -- "${dir_a}" "${dir_b}" "${index_dir}"
}
trap cleanup EXIT

rm -rf -- "${dir_a}" "${dir_b}"

echo "Building release binaries..."
cargo build -q -p unity-asset-search-daemon -p unity-asset-search-cli --release

echo "Starting daemon..."
target/release/unity-asset-search-daemon \
  --project-root "${project_root}" \
  --index-dir "${index_dir}" \
  --listen "127.0.0.1:${port}" \
  --no-startup-reindex \
  --watch \
  --watch-debounce-ms "${debounce_ms}" \
  --reconcile-interval-ms 0 \
  2>"${index_dir}/daemon.log" &
daemon_pid=$!

echo "Waiting for daemon to become ready..."
ready=0
for _ in {1..200}; do
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

status_json() {
  target/release/unity-asset-search-cli --base-url "${base_url}" status
}

active_generation() {
  python3 -c 'import json,sys; active=(json.load(sys.stdin).get("generation") or {}).get("active") or {}; print(active.get("generation") or "")'
}

indexed_assets() {
  python3 -c 'import json,sys; print(int(json.load(sys.stdin).get("indexed_assets") or 0))'
}

wait_for_generation() {
  local label="$1"
  local before_generation="$2"
  local expected_assets="$3"

  for _ in {1..80}; do
    local json
    json="$(status_json)"
    local match
    match="$(print -r -- "${json}" | python3 -c 'import json,sys
st=json.load(sys.stdin)
before_generation,expected_assets=sys.argv[1:]
active=(st.get("generation") or {}).get("active") or {}
fresh=(active.get("stale") is False and active.get("actual_revision") == active.get("desired_revision"))
ok=(st.get("indexing") is False and fresh and active.get("generation") != before_generation)
ok=ok and str(st.get("indexed_assets")) == expected_assets
print(int(ok))' "${before_generation}" "${expected_assets}")"
    if [[ "${match}" == "1" ]]; then
      echo "${label}: generation barrier satisfied"
      print -r -- "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); active=(st.get("generation") or {}).get("active") or {}; print(json.dumps({"generation": active.get("generation"), "revision": active.get("actual_revision"), "indexed_assets": st.get("indexed_assets"), "indexed_search_documents": st.get("indexed_search_documents"), "last_build_duration_ms": st.get("last_build_duration_ms")}, ensure_ascii=False))'
      return 0
    fi
    sleep 0.5
  done

  echo "timeout waiting for watcher generation after ${label}" >&2
  status_json >&2 || true
  if [[ -f "${index_dir}/daemon.log" ]]; then
    echo "daemon log (tail):" >&2
    tail -80 "${index_dir}/daemon.log" >&2 || true
  fi
  return 1
}

count_hits() {
  local query="$1"
  target/release/unity-asset-search-cli --base-url "${base_url}" search "${query}" --limit 5000 \
    | python3 -c 'import json,sys; result=json.load(sys.stdin); print(int((result.get("match_count") or {}).get("value") or 0))'
}

mkdir -p "${dir_a}"
for fixture_index in $(seq 1 "${files}"); do
  guid="$(printf '%032x' "${fixture_index}")"
  asset="${dir_a}/obj_${fixture_index}.prefab"
  cat > "${asset}" <<EOF
%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: RenameObj${fixture_index}
EOF
  cat > "${asset}.meta" <<EOF
fileFormatVersion: 2
guid: ${guid}
EOF
done

echo "Full reindex (baseline)..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null
baseline_status="$(status_json)"
baseline_generation="$(print -r -- "${baseline_status}" | active_generation)"
baseline_assets="$(print -r -- "${baseline_status}" | indexed_assets)"
if [[ -z "${baseline_generation}" ]]; then
  echo "baseline has no active generation" >&2
  exit 1
fi

echo "Verify A is searchable..."
a_hits="$(count_hits "in:Assets/zz_unity_asset_search_rename_a")"
if [[ "${a_hits}" -lt "${files}" ]]; then
  echo "expected >=${files} hits in A, got ${a_hits}" >&2
  exit 1
fi

echo "Rename dir A -> dir B..."
rm -rf -- "${dir_b}"
mv "${dir_a}" "${dir_b}"
wait_for_generation "rename" "${baseline_generation}" "${baseline_assets}"

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
