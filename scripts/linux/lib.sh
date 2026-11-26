# Shared helpers for the Linux launcher scripts. Sourced, not run.
# shellcheck shell=bash

VL_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
VL_TARGET=x86_64-pc-windows-msvc

vl_data_home=${XDG_DATA_HOME:-$HOME/.local/share}/victory-linux
vl_config_home=${XDG_CONFIG_HOME:-$HOME/.config}/victory-linux
vl_state_home=${XDG_STATE_HOME:-$HOME/.local/state}/victory-linux
vl_cache_home=${XDG_CACHE_HOME:-$HOME/.cache}/victory-linux

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
warn() { printf 'warning: %s\n' "$*" >&2; }
info() { printf '==> %s\n' "$*"; }

# Unix path -> Windows path on Wine's Z: drive (the whole Unix tree).
winpath() {
  local p
  p=$(realpath -m -- "$1")
  printf 'Z:%s' "${p//\//\\}"
}

# True if $1 is the repository or below it.
inside_repo() {
  local p
  p=$(realpath -m -- "$1")
  [[ $p == "$VL_ROOT" || $p == "$VL_ROOT"/* ]]
}

refuse_inside_repo() { # label path
  if inside_repo "$2"; then
    die "$1 ($2) is inside the repository; choose a directory outside it"
  fi
}

# Saved by setup.sh: PROTON and PREFIX (shell syntax).
load_saved() {
  local f=${VICTORY_LINUX_CONFIG_DIR:-$vl_config_home}/launcher.env
  if [[ -f $f ]]; then
    # shellcheck disable=SC1090
    source "$f"
  fi
}

wine_of() { printf '%s/files/bin/wine' "$1"; }

need_proton() { # dir
  [[ -n $1 ]] || die "no Proton build given (--proton <dir> or VICTORY_PROTON)"
  [[ -x $(wine_of "$1") ]] || die "no executable files/bin/wine in Proton directory: $1"
}

# Prints the first existing directory among the arguments.
first_dir() {
  local d
  for d in "$@"; do
    if [[ -d $d ]]; then printf '%s' "$d"; return 0; fi
  done
  return 1
}

# Environment for running a Windows program under the prefix with a Proton wine.
export_wine_env() { # prefix
  export WINEPREFIX=$1
  export WINEDEBUG=${WINEDEBUG:--all}
  export WINEDLLOVERRIDES='d3d12,d3d12core,dxgi,d3d11,d3d10core,d3d9,d3dcompiler_47=n'
}

# Best-effort: the version string of the vkd3d-proton in the prefix (for
# example 3.1.0, or v3.0b-171-gabcdef0 for a build from git). Prints nothing
# if it cannot tell.
vkd3d_build_of() { # prefix
  local dll=$1/drive_c/windows/system32/d3d12core.dll v
  [[ -f $dll ]] || return 0
  v=$(LC_ALL=C grep -a -o -P '(?<=\x00)v?\d+\.\d+[\w.]*(-\d+-g[0-9a-f]+)?(?=\x00)' "$dll" 2>/dev/null |
    awk '/-g[0-9a-f]+$/ {print; found=1; exit} !first {first=$0} END {if (!found) print first}') || true
  printf '%s' "$v"
}

# Warns about host settings the title is known to need. Never fails.
preflight_host() {
  local mmc
  mmc=$(cat /proc/sys/vm/max_map_count 2>/dev/null || true)
  if [[ $mmc =~ ^[0-9]+$ ]] && ((mmc < 1048576)); then
    warn "vm.max_map_count is $mmc; the layer keeps about 150,000 mapped views (2-3 kernel mappings each) and can run out. Raise it:"
    warn "  sudo sysctl -w vm.max_map_count=1048576   (persist: echo 'vm.max_map_count = 1048576' | sudo tee /etc/sysctl.d/99-victory.conf)"
  fi
  if [[ ! -e /dev/ntsync ]]; then
    warn "/dev/ntsync is missing (needs kernel 6.14+ with the ntsync module: sudo modprobe ntsync); Wine falls back to slower synchronisation"
  fi
}
