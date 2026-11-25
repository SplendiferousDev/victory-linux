#!/usr/bin/env bash
# Cross-builds the workspace tests and runs every test executable under the
# prefix's wine. See docs/linux.md.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() {
  cat <<USAGE
Usage: test.sh [options] [-- <test harness arguments>]

  --proton <dir>   Proton build (default: saved by setup.sh, or VICTORY_PROTON)
  --prefix <dir>   Wine prefix (default: saved by setup.sh)
  --only <text>    run only executables whose name contains <text>
  -h, --help       this text

Prints one row per test executable. Exits 0 if the only failures are the ones
listed in EXPECTED below (differences between Proton and Windows that are known
and harmless), 1 otherwise.
USAGE
}

# Known failures under Proton. Format: <test name substring>|<reason>.
EXPECTED=(
  'stub_pipeline_is_created|vkd3d-proton does not check the DXIL container hash, so a container with a wrong or empty hash is accepted where a Windows driver refuses it'
  'a_rejected_blob_gives_an_empty_library|vkd3d-proton gives no usable pipeline library, so the library handling of the runtime cannot be exercised (its own disk cache keeps the pipelines)'
  'the_system_takes_the_subscription|no driver event watch (drvwatch): Wine has no Windows event log to subscribe to'
  'loads_binds_and_calls_probe|spawns cargo, which does not run inside the prefix'
  'repository_passes_policy|runs git, which does not run inside the prefix; run "cargo xtask policy" on the host instead'
)

proton=${VICTORY_PROTON:-} prefix= only=
while (($#)); do
  case $1 in
    --proton) proton=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    --only) only=${2:?}; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    --) shift; break ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done
harness_args=("$@")

export PATH="$HOME/.cargo/bin:$PATH"
load_saved
[[ -n $proton ]] || proton=${PROTON:-}
[[ -n $prefix ]] || prefix=${PREFIX:-}
need_proton "$proton"
[[ -f $prefix/system.reg ]] || die "no Wine prefix at '${prefix}'; run scripts/linux/setup.sh first"
prefix=$(realpath -- "$prefix")
wine=$(wine_of "$(realpath -- "$proton")")

cache=$vl_cache_home/vkd3d-cache
refuse_inside_repo "vkd3d shader cache" "$cache"
mkdir -p "$cache" "$vl_state_home/logs"
export_wine_env "$prefix"
export VKD3D_SHADER_CACHE_PATH
VKD3D_SHADER_CACHE_PATH=$(winpath "$cache")

cd "$VL_ROOT"
list=$(mktemp)
trap 'rm -f "$list"' EXIT
info "Building the tests"
nice cargo xwin test --workspace --release --target "$VL_TARGET" --no-run \
  --message-format=json 2>/dev/null |
  grep '"test":true' | grep -o '"executable":"[^"]*"' | sed 's/"executable":"\(.*\)"/\1/' |
  sort -u >"$list"
[[ -s $list ]] || die "no test executables found"

logdir=$vl_state_home/logs
stamp=$(date +%Y%m%d-%H%M%S)
unexpected=0
rows=()
notes=()

while IFS= read -r exe; do
  base=$(basename "$exe" .exe)
  [[ $base =~ ^(.*)-[0-9a-f]{16}$ ]] && base=${BASH_REMATCH[1]}
  [[ -z $only || $base == *"$only"* ]] || continue
  log=$logdir/test-$stamp-$base.log
  info "Running $base"
  rc=0
  "$wine" "$exe" "${harness_args[@]}" >"$log" 2>&1 || rc=$?
  ok=$(grep -c '^test .* \.\.\. ok$' "$log" || true)
  ign=$(grep -c '^test .* \.\.\. ignored' "$log" || true)
  mapfile -t failed < <(sed -n 's/^test \(.*\) \.\.\. FAILED$/\1/p' "$log")
  status=pass
  expected_n=0
  for t in "${failed[@]}"; do
    hit=
    for e in "${EXPECTED[@]}"; do
      if [[ $t == *"${e%%|*}"* ]]; then hit=$e; break; fi
    done
    if [[ -n $hit ]]; then
      expected_n=$((expected_n + 1))
      notes+=("expected: $base: $t - ${hit#*|}")
    else
      unexpected=$((unexpected + 1))
      notes+=("FAILED:   $base: $t")
    fi
  done
  if ((rc != 0 && ${#failed[@]} == 0)); then
    # No failing test was reported: the executable itself died.
    status=CRASH
    unexpected=$((unexpected + 1))
    notes+=("CRASH:    $base exited with status $rc; see $log")
  elif ((${#failed[@]} > expected_n)); then
    status=FAIL
  elif ((${#failed[@]} > 0)); then
    status=expected
  fi
  rows+=("$(printf '%-34s %-9s ok=%-5s failed=%-3s ignored=%s' "$base" "$status" "$ok" "${#failed[@]}" "$ign")")
done <"$list"

echo
printf '%s\n' "${rows[@]}"
echo
[[ ${#notes[@]} -eq 0 ]] || printf '%s\n' "${notes[@]}"
echo "logs: $logdir/test-$stamp-*.log"
if ((unexpected)); then
  echo "RESULT: $unexpected unexpected failure(s)"
  exit 1
fi
echo "RESULT: ok (only the known Proton differences failed, if any)"
