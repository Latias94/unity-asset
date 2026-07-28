#!/usr/bin/env zsh
set -euo pipefail

# Stress watcher-driven indexing while alternating between two Git revisions.
#
# Usage:
#   scripts/stress_git_checkout_watch.zsh repo-ref/BoatAttack
#
# Environment overrides:
#   PORT=19784 DEBOUNCE_MS=200 ITER=10 REF_A=HEAD REF_B=HEAD~1

set +x 2>/dev/null || true
unsetopt xtrace 2>/dev/null || true

project_root="${1:-repo-ref/BoatAttack}"
port="${PORT:-19784}"
base_url="http://127.0.0.1:${port}"
debounce_ms="${DEBOUNCE_MS:-200}"
iter="${ITER:-10}"
ref_a="${REF_A:-HEAD}"
ref_b="${REF_B:-HEAD~1}"

case "${iter}" in
  ''|0|0[0-9]*|*[!0-9]*)
    echo "ITER must be a positive decimal integer without leading zeros" >&2
    exit 1
    ;;
esac
case "${debounce_ms}" in
  ''|0[0-9]*|*[!0-9]*)
    echo "DEBOUNCE_MS must be a non-negative decimal integer without leading zeros" >&2
    exit 1
    ;;
esac

if [[ ! -d "${project_root}/.git" ]]; then
  echo "expected a git repo at ${project_root} (missing .git)" >&2
  exit 1
fi

echo "Verifying git working tree is clean..."
if [[ -n "$(git -C "${project_root}" status --porcelain=v1)" ]]; then
  echo "git working tree is dirty; commit or stash it before running this stress test" >&2
  git -C "${project_root}" status --porcelain=v1 >&2
  exit 1
fi

echo "Resolving refs..."
git -C "${project_root}" rev-parse --verify -q "${ref_a}" >/dev/null || {
  echo "invalid REF_A=${ref_a}" >&2
  exit 1
}
git -C "${project_root}" rev-parse --verify -q "${ref_b}" >/dev/null || {
  echo "invalid REF_B=${ref_b}" >&2
  exit 1
}
ref_a_commit="$(git -C "${project_root}" rev-parse "${ref_a}^{commit}")"
ref_b_commit="$(git -C "${project_root}" rev-parse "${ref_b}^{commit}")"
if [[ "${ref_a_commit}" == "${ref_b_commit}" ]]; then
  echo "REF_A and REF_B resolve to the same commit" >&2
  exit 1
fi

original_ref="$(git -C "${project_root}" symbolic-ref --quiet --short HEAD || git -C "${project_root}" rev-parse HEAD)"
index_dir="$(mktemp -d -t unity-asset-search-index.XXXXXX)"
metrics_dir="${index_dir}/metrics"
mkdir -p "${metrics_dir}"
daemon_pid=""

cleanup() {
  if [[ -n "${daemon_pid}" ]]; then
    kill "${daemon_pid}" 2>/dev/null || true
  fi
  git -C "${project_root}" checkout -q "${original_ref}" 2>/dev/null || true
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

active_revision() {
  python3 -c 'import json,sys; active=(json.load(sys.stdin).get("generation") or {}).get("active") or {}; print(active.get("actual_revision") or "")'
}

indexed_assets() {
  python3 -c 'import json,sys; print(int(json.load(sys.stdin).get("indexed_assets") or 0))'
}

wait_for_generation() {
  local label="$1"
  local before_generation="$2"
  local expected_revision="$3"
  local expected_assets="$4"
  local status_path="${metrics_dir}/${label}.status.json"

  for _ in {1..160}; do
    local json
    json="$(status_json)"
    local match
    match="$(print -r -- "${json}" | python3 -c 'import json,sys
st=json.load(sys.stdin)
before_generation,expected_revision,expected_assets=sys.argv[1:]
active=(st.get("generation") or {}).get("active") or {}
fresh=(active.get("stale") is False and active.get("actual_revision") == active.get("desired_revision"))
ok=(st.get("indexing") is False and fresh and active.get("generation") != before_generation)
ok=ok and active.get("actual_revision") == expected_revision
ok=ok and str(st.get("indexed_assets")) == expected_assets
print(int(ok))' "${before_generation}" "${expected_revision}" "${expected_assets}")"
    if [[ "${match}" == "1" ]]; then
      print -r -- "${json}" > "${status_path}"
      return 0
    fi
    sleep 0.5
  done

  echo "timeout waiting for watcher generation after ${label}" >&2
  status_json >&2 || true
  if [[ -f "${index_dir}/daemon.log" ]]; then
    echo "daemon log (tail):" >&2
    tail -120 "${index_dir}/daemon.log" >&2 || true
  fi
  return 1
}

extract_status_field() {
  local status_path="$1"
  local key="$2"
  python3 - "${status_path}" "${key}" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as status_file:
    status = json.load(status_file)
value = status.get(sys.argv[2])
if value is None:
    raise SystemExit(f"status field {sys.argv[2]!r} is missing")
if isinstance(value, bool) or not isinstance(value, int):
    raise SystemExit(f"status field {sys.argv[2]!r} is not an integer")
print(value)
PY
}

baseline_for_ref() {
  local ref="$1"
  echo "Checkout ${ref}..." >&2
  git -C "${project_root}" checkout -q "${ref}"

  echo "Full reindex for baseline (${ref})..." >&2
  target/release/unity-asset-search-cli --base-url "${base_url}" --token "${token}" reindex --full >/dev/null

  local json
  json="$(status_json)"
  local generation
  generation="$(print -r -- "${json}" | active_generation)"
  local revision
  revision="$(print -r -- "${json}" | active_revision)"
  local assets
  assets="$(print -r -- "${json}" | indexed_assets)"
  if [[ -z "${generation}" || -z "${revision}" ]]; then
    echo "baseline ${ref} has no active generation or revision" >&2
    return 1
  fi
  printf '%s\t%s\t%s\n' "${generation}" "${revision}" "${assets}"
}

echo "Computing baselines (this may take a while on large projects)..."
baseline_a_record="$(baseline_for_ref "${ref_a_commit}")"
baseline_b_record="$(baseline_for_ref "${ref_b_commit}")"
IFS=$'\t' read -r baseline_a_generation baseline_a_revision baseline_a_assets <<< "${baseline_a_record}"
IFS=$'\t' read -r baseline_b_generation baseline_b_revision baseline_b_assets <<< "${baseline_b_record}"

if [[ "${baseline_a_revision}" == "${baseline_b_revision}" ]]; then
  echo "REF_A and REF_B produce the same workspace revision; choose refs with different indexed content" >&2
  exit 1
fi
echo "baseline_a(${ref_a}): generation=${baseline_a_generation} revision=${baseline_a_revision} assets=${baseline_a_assets}"
echo "baseline_b(${ref_b}): generation=${baseline_b_generation} revision=${baseline_b_revision} assets=${baseline_b_assets}"

echo "Switching between refs for ${iter} iterations..."
durations_path="${metrics_dir}/durations_ms.txt"
: > "${durations_path}"
for iteration in $(seq 1 "${iter}"); do
  before_status="$(status_json)"
  before_generation="$(print -r -- "${before_status}" | active_generation)"
  echo "iter ${iteration}/${iter}: checkout ${ref_a}"
  git -C "${project_root}" checkout -q "${ref_a_commit}"
  wait_for_generation "checkout-${iteration}-A" "${before_generation}" "${baseline_a_revision}" "${baseline_a_assets}"
  extract_status_field "${metrics_dir}/checkout-${iteration}-A.status.json" "last_build_duration_ms" >> "${durations_path}"

  before_status="$(status_json)"
  before_generation="$(print -r -- "${before_status}" | active_generation)"
  echo "iter ${iteration}/${iter}: checkout ${ref_b}"
  git -C "${project_root}" checkout -q "${ref_b_commit}"
  wait_for_generation "checkout-${iteration}-B" "${before_generation}" "${baseline_b_revision}" "${baseline_b_assets}"
  extract_status_field "${metrics_dir}/checkout-${iteration}-B.status.json" "last_build_duration_ms" >> "${durations_path}"
done

echo "Summary (generation build duration):"
python3 - "${durations_path}" "$(( iter * 2 ))" <<'PY'
import json
import sys
from pathlib import Path

durations = [
    int(line)
    for line in Path(sys.argv[1]).read_text(encoding="utf-8").splitlines()
    if line.strip()
]
expected_samples = int(sys.argv[2])
if len(durations) != expected_samples:
    raise SystemExit(
        f"expected {expected_samples} generation build samples, found {len(durations)}"
    )

def percentile(values: list[int], fraction: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    index = round((len(ordered) - 1) * fraction)
    return ordered[max(0, min(len(ordered) - 1, index))]

print(
    json.dumps(
        {
            "operations": len(durations),
            "generation_build_ms": {
                "p50": percentile(durations, 0.50),
                "p95": percentile(durations, 0.95),
                "max": max(durations) if durations else 0,
            },
        },
        ensure_ascii=False,
        indent=2,
    )
)
PY

echo "Done."
