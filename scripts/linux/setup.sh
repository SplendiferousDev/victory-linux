#!/usr/bin/env bash
# One-time setup for running the project under Proton on Linux.
# See docs/linux.md. Safe to run again: finished steps are skipped.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() {
  cat <<USAGE
Usage: setup.sh [options]

Options:
  --proton <dir>    Proton build whose files/bin/wine is used (env VICTORY_PROTON)
  --prefix <dir>    Wine prefix to create or reuse
                    (default: $vl_data_home/prefix)
  --game <dir>      your unpacked game folder; written to victory.toml
  --cache-dir <dir> derived data (default: $vl_data_home/cache)
  --data-dir <dir>  saves and settings (default: $vl_data_home/data)
  --config-dir <dir> where victory.toml and launcher.env go
                    (default: $vl_config_home)
  --vkd3d <dir>     install d3d12.dll and d3d12core.dll from this directory
                    into the prefix (originals are backed up once)
  --skip-build      do not build the release binary
  --check           only check the tools; change nothing
  -h, --help        this text

Steps: check tools, create the prefix, install native d3dcompiler_47,
optionally install a newer vkd3d-proton, build victory.exe, write victory.toml.
USAGE
}

proton=${VICTORY_PROTON:-}
prefix=
game=
cache_dir=$vl_data_home/cache
data_dir=$vl_data_home/data
config_dir=$vl_config_home
vkd3d=
build=1
check_only=0

while (($#)); do
  case $1 in
    --proton) proton=${2:?--proton needs a directory}; shift 2 ;;
    --prefix) prefix=${2:?--prefix needs a directory}; shift 2 ;;
    --game) game=${2:?--game needs a directory}; shift 2 ;;
    --cache-dir) cache_dir=${2:?}; shift 2 ;;
    --data-dir) data_dir=${2:?}; shift 2 ;;
    --config-dir) config_dir=${2:?}; shift 2 ;;
    --vkd3d) vkd3d=${2:?--vkd3d needs a directory}; shift 2 ;;
    --skip-build) build=0; shift ;;
    --check) check_only=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

export PATH="$HOME/.cargo/bin:$PATH"
missing=0
miss() { warn "$*"; missing=1; }

# ---- 1. tools --------------------------------------------------------------
info "Checking tools"
command -v rustup >/dev/null || miss "rustup not found (install from https://rustup.rs; a system Rust ignores rust-toolchain.toml)"
if command -v rustup >/dev/null; then
  channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$VL_ROOT/rust-toolchain.toml")
  if ! rustup toolchain list | grep -q "^$channel"; then
    if ((check_only)); then miss "toolchain $channel not installed (rustup toolchain install $channel)"; fi
  fi
  if ! rustup target list --toolchain "$channel" --installed 2>/dev/null | grep -qx "$VL_TARGET"; then
    if ((check_only)); then miss "target $VL_TARGET missing (rustup target add $VL_TARGET --toolchain $channel)"; fi
  fi
fi
command -v cargo-xwin >/dev/null || miss "cargo-xwin not found (cargo install cargo-xwin)"
command -v winetricks >/dev/null || miss "winetricks not found (needed for d3dcompiler_47)"
command -v cabextract >/dev/null || warn "cabextract not found; winetricks usually needs it"
command -v vulkaninfo >/dev/null || warn "vulkaninfo not found; a working Vulkan driver is needed (vulkan-tools)"

VICTORY_LINUX_CONFIG_DIR=$config_dir
load_saved
[[ -n $proton ]] || proton=${PROTON:-}
[[ -n $prefix ]] || prefix=${PREFIX:-$vl_data_home/prefix}
if [[ -z $proton ]]; then
  miss "no Proton build: pass --proton <dir> or set VICTORY_PROTON"
elif [[ ! -x $(wine_of "$proton") ]]; then
  miss "no executable files/bin/wine in $proton"
fi
((missing == 0)) || die "fix the problems above and run again"
proton=$(realpath -- "$proton")
wine=$(wine_of "$proton")
info "Wine: $("$wine" --version 2>/dev/null || echo unknown)"
if ((check_only)); then info "Tools are in place."; exit 0; fi

prefix=$(realpath -m -- "$prefix")
config_dir=$(realpath -m -- "$config_dir")
cache_dir=$(realpath -m -- "$cache_dir")
data_dir=$(realpath -m -- "$data_dir")
for pair in "prefix:$prefix" "config dir:$config_dir" "cache dir:$cache_dir" "data dir:$data_dir"; do
  refuse_inside_repo "${pair%%:*}" "${pair#*:}"
done

# Toolchain from rust-toolchain.toml is installed by rustup on first use.
(cd "$VL_ROOT" && rustup target add "$VL_TARGET" >/dev/null 2>&1 || true)

# ---- 2. prefix -------------------------------------------------------------
export_wine_env "$prefix"
if [[ -f $prefix/system.reg ]]; then
  info "Prefix exists: $prefix"
else
  info "Creating prefix: $prefix"
  mkdir -p "$prefix"
  "$wine" wineboot --init >/dev/null 2>&1 || true
  # The registry is written when the prefix's wineserver exits.
  "$proton/files/bin/wineserver" -w 2>/dev/null || true
  [[ -f $prefix/system.reg ]] || die "wineboot did not create a prefix at $prefix"
fi
sys32=$prefix/drive_c/windows/system32

# Some Proton builds keep vkd3d-proton and DXVK outside Wine's own DLL
# directory; copy them in the way Proton does. Where they are not found
# separately, the builtin copies the prefix already holds are used.
copy_dlls() { # label dir dll...
  local label=$1 dir=$2 d
  shift 2
  for d in "$@"; do
    [[ -f $dir/$d ]] && cp -f "$dir/$d" "$sys32/$d"
  done
  info "Installed $label DLLs from ${dir#"$proton"/}"
}
if [[ ! -f $prefix/.victory-proton-dlls ]]; then
  if d=$(first_dir "$proton/files/lib/wine/vkd3d-proton/x86_64-windows" \
      "$proton/files/lib64/wine/vkd3d-proton" "$proton/files/lib/vkd3d-proton/x86_64-windows"); then
    copy_dlls vkd3d-proton "$d" d3d12.dll d3d12core.dll
  fi
  if d=$(first_dir "$proton/files/lib/wine/dxvk/x86_64-windows" "$proton/files/lib64/wine/dxvk"); then
    copy_dlls DXVK "$d" dxgi.dll d3d11.dll d3d10core.dll d3d9.dll
  fi
  : >"$prefix/.victory-proton-dlls"
fi

# ---- 3. native d3dcompiler_47 ---------------------------------------------
# Wine's built-in d3dcompiler_47 compiles HLSL with vkd3d-shader, which rejects
# several of the runtime's own helper compute shaders (D3DCompile, cs_5_1),
# among them the indirect-argument clamp. Microsoft's DLL compiles them, so it
# is installed into the prefix and selected with WINEDLLOVERRIDES.
if [[ -f $prefix/.victory-d3dcompiler_47 ]]; then
  info "Native d3dcompiler_47 already installed"
else
  info "Installing native d3dcompiler_47 with winetricks"
  WINE=$wine WINETRICKS_SUPER_QUIET=1 winetricks -q d3dcompiler_47
  : >"$prefix/.victory-d3dcompiler_47"
fi

# ---- 4. newer vkd3d-proton (optional) -------------------------------------
if [[ -n $vkd3d ]]; then
  for f in d3d12.dll d3d12core.dll; do
    [[ -f $vkd3d/$f ]] || die "$vkd3d/$f not found"
  done
  backup=$prefix/vkd3d-backup
  if [[ ! -d $backup ]]; then
    info "Backing up the prefix's d3d12 DLLs to $backup"
    mkdir -p "$backup"
    cp -p "$sys32/d3d12.dll" "$sys32/d3d12core.dll" "$backup/"
  fi
  info "Installing vkd3d-proton from $vkd3d"
  cp -f "$vkd3d/d3d12.dll" "$vkd3d/d3d12core.dll" "$sys32/"
fi
b=$(vkd3d_build_of "$prefix")
[[ -z $b ]] || info "vkd3d-proton in the prefix: $b"

# ---- 5. build --------------------------------------------------------------
exe=$VL_ROOT/target/$VL_TARGET/release/victory.exe
if ((build)); then
  info "Building victory.exe (release)"
  (cd "$VL_ROOT" && nice cargo xwin build --release -p victory --target "$VL_TARGET")
fi
[[ -f $exe ]] || warn "$exe not built yet"

# ---- 6. config -------------------------------------------------------------
mkdir -p "$config_dir" "$cache_dir" "$data_dir"
toml=$config_dir/victory.toml
if [[ -z $game ]]; then
  [[ -f $toml ]] || warn "no --game given and no $toml yet; pass --game <your game folder> to write it"
else
  game=$(realpath -- "$game")
  [[ -d $game ]] || die "game folder not found: $game"
  refuse_inside_repo "game folder" "$game"
  tmp=$(mktemp)
  cat >"$tmp" <<TOML
# Written by scripts/linux/setup.sh. Paths are Windows paths on Wine's Z: drive.
[paths]
game_dir = '$(winpath "$game")'
cache_dir = '$(winpath "$cache_dir")'
data_dir = '$(winpath "$data_dir")'
test_titles = []
TOML
  if [[ -f $toml ]] && cmp -s "$tmp" "$toml"; then
    info "Config unchanged: $toml"; rm -f "$tmp"
  else
    mv "$tmp" "$toml"; info "Wrote $toml"
  fi
fi
{
  printf 'PROTON=%q\n' "$proton"
  printf 'PREFIX=%q\n' "$prefix"
} >"$config_dir/launcher.env"

info "Done. Run the title with: scripts/linux/run.sh --game <folder> --exe <file name>"
