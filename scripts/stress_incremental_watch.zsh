#!/usr/bin/env zsh
set -euo pipefail

# Stress watcher-driven indexing across create, metadata-only touch, and directory deletion.
#
# Usage:
#   scripts/stress_incremental_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19782 FILES=1000 DEBOUNCE_MS=200

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19782}"
base_url="http://127.0.0.1:${port}"
files="${FILES:-1000}"
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

work_dir="${project_root}/Assets/zz_unity_asset_search_stress"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
daemon_pid=""

cleanup() {
  if [[ -n "${daemon_pid}" ]]; then
    kill "${daemon_pid}" 2>/dev/null || true
  fi
  rm -rf -- "${work_dir}" "${index_dir}"
}
trap cleanup EXIT

rm -rf -- "${work_dir}"

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
  python3 -c 'import json,sys; st=json.load(sys.stdin); active=(st.get("generation") or {}).get("active") or {}; print(active.get("generation") or "")'
}

last_build_marker() {
  python3 -c 'import json,sys; value=json.load(sys.stdin).get("last_build_unix_ms"); print("" if value is None else value)'
}

indexed_assets() {
  python3 -c 'import json,sys; print(int(json.load(sys.stdin).get("indexed_assets") or 0))'
}

print_status_summary() {
  python3 -c 'import json,sys; st=json.load(sys.stdin); active=(st.get("generation") or {}).get("active") or {}; print(json.dumps({"generation": active.get("generation"), "revision": active.get("actual_revision"), "stale": active.get("stale"), "indexed_assets": st.get("indexed_assets"), "indexed_search_documents": st.get("indexed_search_documents"), "indexed_reference_facts": st.get("indexed_reference_facts"), "last_build_duration_ms": st.get("last_build_duration_ms"), "last_build_unix_ms": st.get("last_build_unix_ms")}, ensure_ascii=False))'
}

wait_for_build() {
  local label="$1"
  local before_generation="$2"
  local before_build="$3"
  local require_generation="$4"
  local expected_assets="$5"

  for _ in {1..80}; do
    local json
    json="$(status_json)"
    local match
    match="$(print -r -- "${json}" | python3 -c 'import json,sys
st=json.load(sys.stdin)
before_generation,before_build,require_generation,expected_assets=sys.argv[1:]
active=(st.get("generation") or {}).get("active") or {}
generation=active.get("generation") or ""
build=st.get("last_build_unix_ms")
fresh=(active.get("stale") is False and active.get("actual_revision") == active.get("desired_revision"))
ok=(st.get("indexing") is False and fresh and build is not None and str(build) != before_build)
ok=ok and (require_generation != "1" or generation != before_generation)
ok=ok and (expected_assets == "" or str(st.get("indexed_assets")) == expected_assets)
print(int(ok))' "${before_generation}" "${before_build}" "${require_generation}" "${expected_assets}")"
    if [[ "${match}" == "1" ]]; then
      echo "${label}: generation barrier satisfied"
      print -r -- "${json}" | print_status_summary
      return 0
    fi
    sleep 0.5
  done

  echo "timeout waiting for watcher build after ${label}" >&2
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

write_fixture() {
  local fixture_index="$1"
  local guid
  guid="$(printf '%032x' "${fixture_index}")"
  local asset="${work_dir}/obj_${fixture_index}.prefab"
  local meta="${asset}.meta"

  cat > "${asset}" <<EOF
%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: StressObj${fixture_index}
EOF

  cat > "${meta}" <<EOF
fileFormatVersion: 2
guid: ${guid}
EOF
}

echo "Full reindex (baseline)..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null

baseline_status="$(status_json)"
print -r -- "${baseline_status}" | print_status_summary
baseline_assets="$(print -r -- "${baseline_status}" | indexed_assets)"

echo "Phase 1: create ${files} YAML assets quickly..."
before_generation="$(print -r -- "${baseline_status}" | active_generation)"
before_build="$(print -r -- "${baseline_status}" | last_build_marker)"
mkdir -p "${work_dir}"
for fixture_index in $(seq 1 "${files}"); do
  write_fixture "${fixture_index}"
done
wait_for_build "create" "${before_generation}" "${before_build}" 1 "$(( baseline_assets + files ))"
if [[ "$(count_hits StressObj)" -lt "${files}" ]]; then
  echo "created fixtures are missing from search results" >&2
  exit 1
fi

echo "Phase 2: touch ${files} assets without changing bytes..."
before_status="$(status_json)"
before_generation="$(print -r -- "${before_status}" | active_generation)"
before_build="$(print -r -- "${before_status}" | last_build_marker)"
touch "${work_dir}"/*.prefab
wait_for_build "touch" "${before_generation}" "${before_build}" 0 "$(( baseline_assets + files ))"
if [[ "$(count_hits StressObj)" -lt "${files}" ]]; then
  echo "metadata-only touch changed search results" >&2
  exit 1
fi

echo "Phase 3: remove directory..."
before_status="$(status_json)"
before_generation="$(print -r -- "${before_status}" | active_generation)"
before_build="$(print -r -- "${before_status}" | last_build_marker)"
rm -rf -- "${work_dir}"
wait_for_build "delete-dir" "${before_generation}" "${before_build}" 1 "${baseline_assets}"
if [[ "$(count_hits StressObj)" -ne 0 ]]; then
  echo "deleted fixtures remain searchable" >&2
  exit 1
fi

echo "Done."
