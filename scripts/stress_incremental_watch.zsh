#!/usr/bin/env zsh
set -euo pipefail

# Stress test for incremental indexing via watcher.
#
# Goal: ensure "daily changes" stay incremental (no full reindex) and finish quickly.
#
# Usage:
#   scripts/stress_incremental_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19782 TOKEN=testtoken FILES=1000 DEBOUNCE_MS=200

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19782}"
base_url="http://127.0.0.1:${port}"
token="${TOKEN:-testtoken}"
files="${FILES:-1000}"
debounce_ms="${DEBOUNCE_MS:-200}"

work_dir="${project_root}/Assets/zz_unity_asset_search_stress"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
trap "rm -rf ${index_dir} 2>/dev/null || true" EXIT

rm -rf "${work_dir}"

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

echo "Full reindex (baseline)..."
target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null

status_json="$(target/release/unity-asset-search-cli --base-url "${base_url}" status)"
echo "${status_json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print("Baseline:", json.dumps({"indexed_docs": st.get("indexed_docs"), "indexed_ref_sources": st.get("indexed_ref_sources"), "indexed_scripts": st.get("indexed_scripts"), "last_index_duration_ms": st.get("last_index_duration_ms"), "last_scan_ms": st.get("last_scan_ms")}, ensure_ascii=False))'
baseline_indexed_docs="$(echo "${status_json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(int(st.get("indexed_docs") or 0))')"

mkdir -p "${work_dir}"

write_fixture() {
  local i="$1"
  local guid
  guid="$(printf '%032x' "${i}")"
  local asset="${work_dir}/obj_${i}.prefab"
  local meta="${asset}.meta"

  cat > "${asset}" <<EOF
%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: StressObj${i}
EOF

  cat > "${meta}" <<EOF
fileFormatVersion: 2
guid: ${guid}
EOF
}

wait_idle() {
  local label="$1"
  local expect_indexed_docs="${2:-}"
  local require_metrics="${3:-1}"
  for i in {1..40}; do
    local json
    json="$(target/release/unity-asset-search-cli --base-url "${base_url}" status)"
    local match
    match="$(echo "${json}" | python3 -c "import json,sys; st=json.load(sys.stdin); ok=True; \
d=st.get('indexed_docs'); indexing=st.get('indexing'); ls=st.get('last_scan_ms'); u=st.get('updated_docs'); r=st.get('removed_docs'); \
ed='${expect_indexed_docs}'; rm='${require_metrics}'; \
ok = ok and (indexing is False); \
ok = ok and (ed=='' or str(d)==ed); \
ok = ok and (rm!='1' or (ls is not None and (u is not None or r is not None))); \
print(int(ok))")"
    if [[ "${match}" == "1" ]]; then
      echo "${label}: idle"
      echo "${json}" | python3 -c 'import json,sys; st=json.load(sys.stdin); print(json.dumps({"updated_docs": st.get("updated_docs"), "removed_docs": st.get("removed_docs"), "last_index_duration_ms": st.get("last_index_duration_ms"), "last_scan_ms": st.get("last_scan_ms")}, ensure_ascii=False))'
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

echo "Phase 1: create ${files} YAML assets quickly..."
for i in $(seq 1 "${files}"); do
  write_fixture "${i}"
done
wait_idle "create" "$(( baseline_indexed_docs + files ))" 1

echo "Phase 2: touch ${files} assets (mtime-only changes)..."
touch "${work_dir}"/*.prefab
wait_idle "touch" "$(( baseline_indexed_docs + files ))" 1

echo "Phase 3: remove directory (directory deletion semantics)..."
rm -rf "${work_dir}"
wait_idle "delete-dir" "${baseline_indexed_docs}" 1

echo "Done."
