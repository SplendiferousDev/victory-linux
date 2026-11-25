#!/usr/bin/env bash
# Builds ONLY wineserver from a Wine source tree with patches/wine applied, and
# installs it for run.sh. See patches/wine/README.md and docs/linux.md.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() {
  cat <<USAGE
Usage: build-wineserver.sh --proton <dir> --wine-src <git url or dir> --wine-ref <commit> [options]

  --proton <dir>     the Proton build the server must match
  --wine-src <src>   git URL or local clone of the Wine tree Proton was built from
  --wine-ref <ref>   commit of that tree which matches the Proton build
  --work <dir>       build directory (default: $vl_cache_home/wineserver-build)
  --install <file>   install path (default: $vl_data_home/wineserver-patched)
  --jobs <n>         make jobs (default: all cores)
  --skip-check       skip the start-up check against Proton's wine

Finding the source commit for a Proton build: Proton ships its Wine as a git
submodule (the 'wine' directory of the Proton repository). The Proton
repository's tag or release for your build names a commit of that submodule;
'git ls-tree <proton tag> wine' prints it. Forks (such as proton-cachyos) keep
their own Wine repository and say which commit a package was built from in the
package sources (the PKGBUILD names the commit or tag). The server protocol
version must match the client libraries exactly, which is why the commit has
to be the right one; the check at the end of this script catches a mismatch.
If the tree keeps generated protocol files out of git, they are regenerated;
if Proton's build committed regenerated files, check out that commit.

Needs: git, make, a C compiler and autoconf (autoreconf). Only the server is
built (no mingw, no X, no graphics).
USAGE
}

proton= src= ref= work=$vl_cache_home/wineserver-build
install_to=$vl_data_home/wineserver-patched jobs=$(nproc) verify=1

while (($#)); do
  case $1 in
    --proton) proton=${2:?}; shift 2 ;;
    --wine-src) src=${2:?}; shift 2 ;;
    --wine-ref) ref=${2:?}; shift 2 ;;
    --work) work=${2:?}; shift 2 ;;
    --install) install_to=${2:?}; shift 2 ;;
    --jobs) jobs=${2:?}; shift 2 ;;
    --skip-check) verify=0; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

[[ -n $src && -n $ref ]] || { usage >&2; die "--wine-src and --wine-ref are required"; }
[[ -n $proton ]] || proton=${VICTORY_PROTON:-}
need_proton "$proton"
proton=$(realpath -- "$proton")
for tool in git make autoreconf; do
  command -v "$tool" >/dev/null || die "$tool not found"
done
command -v "${CC:-cc}" >/dev/null || die "no C compiler (set CC)"
[[ $jobs =~ ^[0-9]+$ ]] || die "--jobs must be a number"

patches=("$VL_ROOT"/patches/wine/*.patch)
[[ -f ${patches[0]} ]] || die "no patches in $VL_ROOT/patches/wine"

mkdir -p "$work"
work=$(realpath -- "$work")
refuse_inside_repo "build directory" "$work"
refuse_inside_repo "install path" "$install_to"
tree=$work/wine build=$work/build

info "checking out $ref"
if [[ ! -d $tree/.git ]]; then
  # A local directory is cloned; its working tree is never touched.
  git clone --quiet --no-checkout "$src" "$tree"
fi
git -C "$tree" fetch --quiet "$src" "$ref" 2>/dev/null || true
git -C "$tree" checkout --quiet --force --detach "$ref" ||
  die "cannot check out $ref from $src"
git -C "$tree" clean -qfdx

info "applying patches"
for p in "${patches[@]}"; do
  git -C "$tree" apply --check "$p" || die "patch does not apply to $ref: $p"
  git -C "$tree" apply "$p"
  echo "  $(basename "$p")"
done

info "generating build files"
(
  cd "$tree"
  ./tools/make_requests
  # Generated headers (include/wine/vulkan.h) that configure's dependency scan wants.
  if [[ -f dlls/winevulkan/make_vulkan ]]; then
    (cd dlls/winevulkan && python3 ./make_vulkan -x vk.xml -X video.xml >/dev/null)
  elif [[ -x tools/make_vulkan ]]; then
    ./tools/make_vulkan
  fi
  [[ ! -x tools/make_specfiles ]] || ./tools/make_specfiles
  autoreconf -if >/dev/null 2>&1
)

info "configuring (server only)"
rm -rf "$build"
mkdir -p "$build"
(
  cd "$build"
  "$tree/configure" --enable-win64 --without-mingw --without-x \
    --without-freetype --without-gstreamer --without-wayland --without-vulkan \
    --without-opengl --without-pulse --without-alsa --without-cups \
    --without-sane --without-gphoto --without-dbus --without-krb5 \
    --without-netapi --without-pcap --without-usb --without-v4l2 \
    --without-ffmpeg --without-capi --without-oss --without-udev \
    --without-unwind --without-gnutls --without-sdl --without-fontconfig \
    >configure.out 2>&1 || { tail -20 configure.out >&2; die "configure failed ($build/configure.out)"; }
)

info "building server/wineserver with $jobs jobs"
make -C "$build" -j"$jobs" server/wineserver >"$build/make.out" 2>&1 ||
  { tail -20 "$build/make.out" >&2; die "build failed ($build/make.out)"; }
server=$build/server/wineserver
[[ -x $server ]] || die "no server binary was produced"

if ((verify)); then
  info "checking that Proton's wine can talk to it"
  tmp=$(mktemp -d)
  trap 'WINEPREFIX=$tmp/pfx "$server" -k >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/pfx"
  export WINEPREFIX=$tmp/pfx WINEDEBUG=-all WINEDLLOVERRIDES='mscoree,mshtml='
  "$server" -p10
  out=$("$(wine_of "$proton")" cmd /c echo ok 2>&1 | tr -d '\r') || true
  if [[ $out != *ok* ]]; then
    printf '%s\n' "$out" | tail -5 >&2
    die "Proton's wine does not work with this server (protocol mismatch? wrong --wine-ref)"
  fi
  "$server" -k || true
fi

mkdir -p "$(dirname "$install_to")"
install -m755 "$server" "$install_to"
info "installed $install_to"
