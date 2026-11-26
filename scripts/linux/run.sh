#!/usr/bin/env bash
# Starts the title under Proton's wine. See docs/linux.md.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() {
  cat <<USAGE
Usage: run.sh --game <folder> --exe <file name> [options]

  --game <folder>      your unpacked game folder
  --exe <file name>    the title's executable inside that folder
  --console <profile>  series-x (default) or series-s
  --diag               use the diagnostics build (target/.../diag/victory.exe)
  --proton <dir>       Proton build (default: saved by setup.sh, or VICTORY_PROTON)
  --prefix <dir>       Wine prefix (default: saved by setup.sh)
  --config <file>      victory.toml (default: $vl_config_home/victory.toml)
  --name <log name>    log file name (default: a timestamp)
  --wineserver <path>  wineserver to run for the prefix (default: the patched one
                       built by build-wineserver.sh, if installed, in
                       $vl_data_home/wineserver-patched)
  --stock-wineserver   use Proton's own wineserver (slow once the title holds
                       tens of thousands of memory views)
  --measure <file>     sample once a second into <file> while it runs: memory,
                       CPU and I/O pressure, MemAvailable, swap, and the
                       resident size of the title's processes (measure.sh)

Memory guard (needs systemd; the title uses more than 10 GB, and a limit
stops it from taking the whole desktop down when memory runs out):
  --scope              run in a transient systemd user scope
  --memory-high <size> throttle above this, e.g. 13G
  --memory-max <size>  kill above this, e.g. 14G
  --swap-max <size>    cap on swap use, e.g. 24G

Every VICTORY_* and VKD3D_* variable in your environment is passed on.
Logs go to $vl_state_home/logs.
USAGE
}

game= exe= console=series-x diag=0 proton=${VICTORY_PROTON:-} prefix=
wserver= stock=0 measure= config= name=$(date +%Y%m%d-%H%M%S) scope=0 mhigh= mmax= swapmax=

while (($#)); do
  case $1 in
    --game) game=${2:?--game needs a folder}; shift 2 ;;
    --exe) exe=${2:?--exe needs a file name}; shift 2 ;;
    --console) console=${2:?}; shift 2 ;;
    --diag) diag=1; shift ;;
    --proton) proton=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    --config) config=${2:?}; shift 2 ;;
    --name) name=${2:?}; shift 2 ;;
    --wineserver) wserver=${2:?--wineserver needs a path}; shift 2 ;;
    --stock-wineserver) stock=1; shift ;;
    --measure) measure=${2:?--measure needs a file}; shift 2 ;;
    --scope) scope=1; shift ;;
    --memory-high) mhigh=${2:?}; shift 2 ;;
    --memory-max) mmax=${2:?}; shift 2 ;;
    --swap-max) swapmax=${2:?}; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

[[ -n $game && -n $exe ]] || { usage >&2; die "--game and --exe are required"; }
[[ -d $game ]] || die "game folder not found: $game"
[[ -f $game/$exe ]] || die "no file named $exe in $game"
case $console in series-x | series-s) ;; *) die "--console must be series-x or series-s" ;; esac
[[ $name =~ ^[A-Za-z0-9._-]+$ ]] || die "--name may only hold letters, digits, . _ -"
if [[ -z $mhigh$mmax$swapmax ]]; then
  ((scope == 0)) || die "--scope needs at least one of --memory-high, --memory-max, --swap-max"
else
  ((scope)) || die "memory limits only apply with --scope"
fi

if ((stock)); then
  [[ -z $wserver ]] || die "--wineserver and --stock-wineserver exclude each other"
elif [[ -z $wserver && -x $vl_data_home/wineserver-patched ]]; then
  wserver=$vl_data_home/wineserver-patched
fi
if [[ -n $wserver ]]; then
  [[ -f $wserver && -x $wserver ]] || die "wineserver not found or not executable: $wserver"
  wserver=$(realpath -- "$wserver")
fi

# A config file given by hand has its launcher.env beside it.
if [[ -n $config && -z ${VICTORY_LINUX_CONFIG_DIR:-} ]]; then
  VICTORY_LINUX_CONFIG_DIR=$(dirname -- "$(realpath -- "$config")")
fi
load_saved
[[ -n $proton ]] || proton=${PROTON:-}
[[ -n $prefix ]] || prefix=${PREFIX:-}
need_proton "$proton"
[[ -f $prefix/system.reg ]] || die "no Wine prefix at '${prefix}'; run scripts/linux/setup.sh first"
proton=$(realpath -- "$proton")
prefix=$(realpath -- "$prefix")
wine=$(wine_of "$proton")
config=${config:-${VICTORY_CONFIG_FILE:-${VICTORY_LINUX_CONFIG_DIR:-$vl_config_home}/victory.toml}}
if [[ -z ${VICTORY_CONFIG:-} ]]; then
  [[ -f $config ]] || die "no config at $config; run setup.sh with --game"
  config=$(realpath -- "$config")
  win_config=$(winpath "$config")
else
  win_config=$VICTORY_CONFIG
fi

exe_dir=release
((diag == 0)) || exe_dir=diag
victory=$VL_ROOT/target/$VL_TARGET/$exe_dir/victory.exe
if [[ ! -f $victory ]]; then
  if ((diag)); then
    die "$victory missing; build it: cargo xwin build --profile diag -p victory --features diagnostics --target $VL_TARGET"
  fi
  die "$victory missing; run setup.sh or: cargo xwin build --release -p victory --target $VL_TARGET"
fi

unixpath() { # Windows Z: path -> Unix path (other drives are left alone)
  local p=$1
  [[ $p == [Zz]:* ]] && p=${p:2}
  printf '%s' "${p//\\//}"
}

# Where the run keeps things. None of it may land in the repository.
run_dir=$vl_state_home/run
log_dir=$vl_state_home/logs
vkd3d_cache=$vl_cache_home/vkd3d-cache
refuse_inside_repo "state directory" "$vl_state_home"
if [[ -n ${VKD3D_SHADER_CACHE_PATH:-} ]]; then
  if [[ $VKD3D_SHADER_CACHE_PATH == [Zz]:* || $VKD3D_SHADER_CACHE_PATH == /* ]]; then
    refuse_inside_repo "VKD3D_SHADER_CACHE_PATH" "$(unixpath "$VKD3D_SHADER_CACHE_PATH")"
  fi
else
  refuse_inside_repo "vkd3d shader cache" "$vkd3d_cache"
fi
refuse_inside_repo "wine prefix" "$prefix"

# Cache and data directories named by the environment or the config file.
check_win_dir() { # label value
  local v=$2
  [[ -n $v ]] || return 0
  [[ $v == [Zz]:* || $v == /* ]] || return 0
  refuse_inside_repo "$1" "$(unixpath "$v")"
}
check_win_dir "VICTORY_CACHE_DIR" "${VICTORY_CACHE_DIR:-}"
check_win_dir "VICTORY_DATA_DIR" "${VICTORY_DATA_DIR:-}"
if [[ -f $config ]]; then
  for key in cache_dir data_dir game_dir; do
    v=$(sed -n "s/^[[:space:]]*$key[[:space:]]*=[[:space:]]*'\\(.*\\)'.*/\\1/p" "$config" | head -1)
    check_win_dir "$key in $config" "$v"
  done
fi

preflight_host
if [[ -n $measure ]]; then
  refuse_inside_repo "measure file" "$measure"
  mkdir -p "$(dirname -- "$measure")"
fi

mkdir -p "$run_dir" "$log_dir" "$vkd3d_cache"
log=$log_dir/$name.log

export_wine_env "$prefix"
if [[ -z ${VKD3D_SHADER_CACHE_PATH:-} ]]; then
  export VKD3D_SHADER_CACHE_PATH
  VKD3D_SHADER_CACHE_PATH=$(winpath "$vkd3d_cache")
fi
export VICTORY_CONFIG=$win_config
export VICTORY_NO_WATCHDOG=1
export VICTORY_LOG=${VICTORY_LOG:-info}
# Proton's own defaults; launching wine directly would otherwise leave
# vkd3d-proton's logging on.
export VKD3D_DEBUG=${VKD3D_DEBUG:-none}
export VKD3D_SHADER_DEBUG=${VKD3D_SHADER_DEBUG:-none}
# NVIDIA's driver keeps compiled pipelines in a disk cache of 256 MiB unless
# told otherwise. The title's pipelines need several times that: once the cache
# is full, older entries are thrown out and every pipeline, even one loaded from
# the pipeline library, is compiled again (1 to 7 s each instead of
# milliseconds).
export __GL_SHADER_DISK_CACHE_SIZE=${__GL_SHADER_DISK_CACHE_SIZE:-12000000000}
export __GL_SHADER_DISK_CACHE_SKIP_CLEANUP=${__GL_SHADER_DISK_CACHE_SKIP_CLEANUP:-1}
# Any other VICTORY_* or VKD3D_* variable is already in the environment and is
# inherited as it is.

b=$(vkd3d_build_of "$prefix")
echo "prefix:   $prefix"
echo "vkd3d:    ${b:-unknown version} (the log's 'vkd3d-proton - build:' line confirms the build)"
echo "profile:  $console ($exe_dir build)"
echo "log:      $log"
if [[ -n $wserver ]]; then
  echo "server:   $wserver (patched)"
else
  echo "server:   Proton's stock wineserver (slow in the world; see docs/linux.md)"
fi

cmd=("$wine" "$victory" run --execute --console "$console" "$(winpath "$game")\\$exe")
if [[ -n $wserver ]]; then
  # Wine's ntdll ignores WINESERVER and uses the server next to wine unless one
  # already runs for the prefix, so start ours first, in the same scope and
  # process group as wine. -p10 makes it exit 10 s after the last client.
  # A server left over from an earlier run would be reused, so stop it first.
  cmd=(bash -c '"$1" -k 2>/dev/null; sleep 1; "$2" -p10 && shift 2 && exec "$@"' _ \
    "$proton/files/bin/wineserver" "$wserver" "${cmd[@]}")
fi
if ((scope)); then
  command -v systemd-run >/dev/null || die "systemd-run not found; run without --scope"
  props=()
  [[ -z $mhigh ]] || props+=(-p "MemoryHigh=$mhigh")
  [[ -z $mmax ]] || props+=(-p "MemoryMax=$mmax")
  [[ -z $swapmax ]] || props+=(-p "MemorySwapMax=$swapmax")
  cmd=(systemd-run --user --scope --collect --quiet "${props[@]}" -- "${cmd[@]}")
fi

cd "$run_dir"
if [[ -n $measure ]]; then
  "$here/measure.sh" "$measure" $$ &
  sampler=$!
  trap 'kill "$sampler" 2>/dev/null || true' EXIT
  echo "measure:  $measure"
fi
set +e
"${cmd[@]}" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
set -e
echo "exit status $rc (log: $log)"
exit "$rc"
