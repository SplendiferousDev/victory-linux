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
  --preset <name>      a named set of VICTORY_* switches (or VICTORY_PRESET):
                         none        change nothing (default)
                         tested-8gb  what the project tests with on a 16 GB PC
                                     and an 8 GB NVIDIA card (series-s unless
                                     --console is given; holds the title at its
                                     start screen until pipelines are made).
                                     Variables you set yourself win.
                       The applied switches are printed before the start.
  --print-env          check everything, print the environment and the command,
                       and stop: nothing is started, no directory is made
  --diag               use the diagnostics build (target/.../diag/victory.exe)
  --proton <dir>       Proton build (default: saved by setup.sh, or VICTORY_PROTON)
  --prefix <dir>       Wine prefix (default: saved by setup.sh)
  --config <file>      victory.toml (default: $vl_config_home/victory.toml)
  --name <log name>    log file name (default: a timestamp)
  --stock-wine         use Proton's own files. Default: the private patched
                       tree built by build-wine.sh (in $vl_patched_tree), if
                       it exists; it is refused when Proton was updated since
  --wineserver <path>  wineserver to run for the prefix (default: the one in the
                       patched tree; or, without a tree, the server built by
                       build-wine.sh --server-only in
                       $vl_data_home/wineserver-patched)
  --stock-wineserver   use Proton's own wineserver, also with the patched tree (slow once the title holds
                       tens of thousands of memory views)
  --measure <file>     sample once a second into <file> while it runs: memory,
                       CPU and I/O pressure, MemAvailable, swap, and the
                       resident size of the title's processes (measure.sh)
  --pool-dir <dir>     experimental: keep the title's physical page pool in a
                       temporary file in <dir> instead of shared memory (sets
                       VICTORY_XMEM_POOL_DIR). Creates <dir>, marks it no
                       copy-on-write on btrfs (chattr +C) and removes pool files
                       a killed run left there. Needs free disk space of about
                       the pool's size (6-7 GB). Off unless given; see
                       docs/linux.md
  --no-fault-watch     do not follow the kernel's log for faults of the card.
                       By default fault-watch.sh appends each NVIDIA Xid and
                       amdgpu reset/timeout/page-fault line to <log>.faults
                       and the layer reads it (VICTORY_HOST_FAULT_FILE), so
                       the paths that switch off at the driver's first error
                       do so under Wine too. Skipped when the journal cannot
                       be read or VICTORY_HOST_FAULT_FILE is already set

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

game= exe= console= diag=0 proton=${VICTORY_PROTON:-} prefix=
wserver= stock=0 stockwine=0 measure= config= name=$(date +%Y%m%d-%H%M%S) scope=0 mhigh= mmax= swapmax=
pool_dir= fault_watch=1 preset=${VICTORY_PRESET:-none} print_env=0

while (($#)); do
  case $1 in
    --game) game=${2:?--game needs a folder}; shift 2 ;;
    --exe) exe=${2:?--exe needs a file name}; shift 2 ;;
    --console) console=${2:?}; shift 2 ;;
    --diag) diag=1; shift ;;
    --preset) preset=${2:?--preset needs a name}; shift 2 ;;
    --print-env) print_env=1; shift ;;
    --proton) proton=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    --config) config=${2:?}; shift 2 ;;
    --name) name=${2:?}; shift 2 ;;
    --wineserver) wserver=${2:?--wineserver needs a path}; shift 2 ;;
    --stock-wineserver) stock=1; shift ;;
    --stock-wine) stockwine=1; stock=1; shift ;;
    --measure) measure=${2:?--measure needs a file}; shift 2 ;;
    --pool-dir) pool_dir=${2:?--pool-dir needs a directory}; shift 2 ;;
    --no-fault-watch) fault_watch=0; shift ;;
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
apply_preset "$preset"
[[ -n $console ]] || console=$(preset_console "$preset")
[[ -n $console ]] || console=series-x
case $console in series-x | series-s) ;; *) die "--console must be series-x or series-s" ;; esac
[[ $name =~ ^[A-Za-z0-9._-]+$ ]] || die "--name may only hold letters, digits, . _ -"
if [[ -z $mhigh$mmax$swapmax ]]; then
  ((scope == 0)) || die "--scope needs at least one of --memory-high, --memory-max, --swap-max"
else
  ((scope)) || die "memory limits only apply with --scope"
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

# The patched Proton tree (ntdll.so and wineserver), if there is one.
patched_tree=
tree_note=
if ((stockwine)); then
  [[ -z $wserver ]] || die "--wineserver and --stock-wine exclude each other"
elif [[ -f $vl_patched_tree/stamp && -x $vl_patched_tree/files/bin/wine ]]; then
  stamp_proton=$(sed -n 's/^proton=//p' "$vl_patched_tree/stamp")
  stamp_fp=$(sed -n 's/^fingerprint=//p' "$vl_patched_tree/stamp")
  stamp_patches=$(sed -n 's/^patches=//p' "$vl_patched_tree/stamp")
  if [[ $stamp_proton != "$proton" || $stamp_fp != "$(proton_fingerprint "$proton")" ]]; then
    die "the patched Proton tree in $vl_patched_tree was built from another Proton build (or one that has been updated since). Rebuild it with scripts/linux/build-wine.sh, or run with --stock-wine"
  fi
  cur_patches=$(cat "$VL_ROOT"/patches/wine/*.patch 2>/dev/null | sha256sum | cut -d' ' -f1)
  if [[ $stamp_patches != "$cur_patches" ]]; then
    warn "patches/wine has changed since the patched tree was built; run build-wine.sh to apply the new ones"
  fi
  patched_tree=$vl_patched_tree/files
  wine=$patched_tree/bin/wine
  [[ -z $wserver ]] || tree_note=" (the tree's own server is replaced by --wineserver)"
fi
if ((stock)); then
  [[ -z $wserver ]] || die "--wineserver and --stock-wineserver exclude each other"
  # With the patched tree, wine would start the tree's server: run Proton's own.
  if [[ -n $patched_tree && $stockwine == 0 ]]; then wserver=$proton/files/bin/wineserver; fi
elif [[ -z $wserver && -z $patched_tree && -x $vl_data_home/wineserver-patched ]]; then
  wserver=$vl_data_home/wineserver-patched
fi
if [[ -n $wserver ]]; then
  [[ -f $wserver && -x $wserver ]] || die "wineserver not found or not executable: $wserver"
  wserver=$(realpath -- "$wserver")
fi
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
    hint="build it: cargo xwin build --profile diag -p victory --features diagnostics --target $VL_TARGET"
  else
    hint="run setup.sh or: cargo xwin build --release -p victory --target $VL_TARGET"
  fi
  if ((print_env)); then
    warn "$victory is missing (a real start would stop here)"
  else
    die "$victory missing; $hint"
  fi
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
driver_cache=$vl_cache_home/driver-cache
refuse_inside_repo "state directory" "$vl_state_home"
refuse_inside_repo "driver shader cache" "${__GL_SHADER_DISK_CACHE_PATH:-$driver_cache}"
refuse_inside_repo "driver shader cache" "${MESA_SHADER_CACHE_DIR:-$driver_cache}"
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
  ((print_env)) || mkdir -p "$(dirname -- "$measure")"
fi

((print_env)) || mkdir -p "$run_dir" "$log_dir" "$vkd3d_cache" "$driver_cache"
log=$log_dir/$name.log

# The page pool on a file (VICTORY_XMEM_POOL_DIR). The file holds title data:
# it must not be in the repository. On btrfs, copy-on-write would fragment it
# and make writes to pages being written back wait; +C only takes effect for
# files made after it is set, so it is set on the (still empty) directory.
if [[ -n $pool_dir ]]; then
  [[ ${VICTORY_INPLACE:-0} == 0 ]] || die "--pool-dir and VICTORY_INPLACE exclude each other"
  refuse_inside_repo "pool directory" "$pool_dir"
  ((print_env)) || mkdir -p -- "$pool_dir"
  pool_dir=$(realpath -m -- "$pool_dir")
  # Files of a run that was killed (with its wineserver) before it could
  # delete them. Nothing else uses them.
  ((print_env)) || find "$pool_dir" -maxdepth 1 -type f -name 'victory-pool-*.tmp' -delete
  case $( ((print_env)) && echo none || stat -f -c %T -- "$pool_dir") in
    btrfs)
      if ! { command -v chattr >/dev/null && chattr +C -- "$pool_dir" 2>/dev/null; }; then
        warn "could not mark $pool_dir no copy-on-write (chattr +C); the pool file will be copy-on-write"
      fi
      ;;
    tmpfs | ramfs) warn "$pool_dir is in RAM ($(stat -f -c %T -- "$pool_dir")): a pool file there saves nothing" ;;
  esac
  export VICTORY_XMEM_POOL_DIR
  VICTORY_XMEM_POOL_DIR=$(winpath "$pool_dir")
fi

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
#
# The cache also gets a directory of its own. The size is a property of the
# cache, not of the program: in the default directory (~/.cache/nvidia/GLCache)
# any other program that uses the driver without the variable (a desktop
# program, a test binary, a tool started with plain wine) sets it back to the
# default and wipes the whole cache when it is over (the driver's README, "If
# the default cache is used and __GL_SHADER_DISK_CACHE_SIZE is set locally").
# The next start of the title then compiles every pipeline again, which looked
# like a cold cache after a crash. Proton does the same (a directory per game).
# Mesa (RADV) gets the same treatment: its cache is shared and trimmed to
# whatever size the program using it last asked for.
export __GL_SHADER_DISK_CACHE_PATH=${__GL_SHADER_DISK_CACHE_PATH:-$driver_cache}
export __GL_SHADER_DISK_CACHE_SIZE=${__GL_SHADER_DISK_CACHE_SIZE:-12000000000}
export __GL_SHADER_DISK_CACHE_SKIP_CLEANUP=${__GL_SHADER_DISK_CACHE_SKIP_CLEANUP:-1}
export MESA_SHADER_CACHE_DIR=${MESA_SHADER_CACHE_DIR:-$driver_cache}
export MESA_SHADER_CACHE_MAX_SIZE=${MESA_SHADER_CACHE_MAX_SIZE:-12G}
# Any other VICTORY_* or VKD3D_* variable is already in the environment and is
# inherited as it is.

b=$(vkd3d_build_of "$prefix")
echo "prefix:   $prefix"
echo "vkd3d:    ${b:-unknown version} (the log's 'vkd3d-proton - build:' line confirms the build)"
echo "drivers:  shader cache in $__GL_SHADER_DISK_CACHE_PATH (NVIDIA) / $MESA_SHADER_CACHE_DIR (Mesa)"
echo "profile:  $console ($exe_dir build)"
if [[ $preset == none ]]; then
  echo "preset:   none"
else
  echo "preset:   $preset (variables you set yourself are kept)"
  printf '            %s\n' "${PRESET_REPORT[@]}"
fi
echo "log:      $log"
[[ -z $pool_dir ]] || echo "pool:     a file in $pool_dir (VICTORY_XMEM_POOL_DIR)"
if [[ -n $patched_tree ]]; then
  echo "wine:     patched Proton tree $patched_tree (patched ntdll.so and wineserver)$tree_note"
fi
if [[ -n $wserver ]]; then
  echo "server:   $wserver$( ((stock)) || echo " (patched)")"
elif [[ -n $patched_tree ]]; then
  echo "server:   $patched_tree/bin/wineserver (patched; started by wine)"
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

if ((print_env)); then
  echo "scope:    $( ((scope)) && echo "systemd user scope (MemoryHigh=${mhigh:-} MemoryMax=${mmax:-} MemorySwapMax=${swapmax:-})" || echo none)"
  if ((fault_watch)) && [[ -z ${VICTORY_HOST_FAULT_FILE:-} ]]; then
    echo "faults:   $log_dir/$name.faults (fault-watch.sh would run beside the title)"
  fi
  echo "environment:"
  env | grep -E '^(WINE|VICTORY_|VKD3D_|__GL_|MESA_)' | LC_ALL=C sort | sed 's/^/  /'
  echo "command:"
  printf '  %q' "${cmd[@]}"
  echo
  echo "(--print-env: nothing was started)"
  exit 0
fi

cd "$run_dir"
sampler= watcher=
stop_helpers() {
  [[ -z $sampler ]] || kill "$sampler" 2>/dev/null || true
  # The watcher runs in a process group of its own (journalctl | grep | loop).
  [[ -z $watcher ]] || kill -- "-$watcher" 2>/dev/null || true
}
trap stop_helpers EXIT
if [[ -n $measure ]]; then
  "$here/measure.sh" "$measure" $$ &
  sampler=$!
  echo "measure:  $measure"
fi
if ((fault_watch)) && [[ -z ${VICTORY_HOST_FAULT_FILE:-} ]]; then
  if command -v journalctl >/dev/null && command -v setsid >/dev/null && journalctl -k -n 1 -q >/dev/null 2>&1; then
    faults=$log_dir/$name.faults
    : >"$faults"
    setsid "$here/fault-watch.sh" "$faults" &
    watcher=$!
    export VICTORY_HOST_FAULT_FILE
    VICTORY_HOST_FAULT_FILE=$(winpath "$faults")
    echo "faults:   $faults (the card's faults from the kernel's log)"
  else
    warn "the kernel's log cannot be read (journalctl -k): the card's faults are not watched"
  fi
fi
set +e
"${cmd[@]}" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
set -e
echo "exit status $rc (log: $log)"
exit "$rc"
