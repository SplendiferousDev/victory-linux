#!/usr/bin/env bash
# Builds wineserver and ntdll.so from a Wine source tree with every
# patches/wine/*.patch applied (in file name order), and puts both into a
# private copy of the Proton "files" tree that run.sh then uses. With
# --server-only only the server is built and installed. See
# patches/wine/README.md and docs/linux.md.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck source=lib.sh
source "$here/lib.sh"

usage() {
  cat <<USAGE
Usage: build-wine.sh --proton <dir> --wine-src <git url or dir> --wine-ref <commit> [options]

  --proton <dir>     the Proton build the server must match
  --wine-src <src>   git URL or local clone of the Wine tree Proton was built from
  --wine-ref <ref>   commit of that tree which matches the Proton build
  --work <dir>       build directory (default: $vl_cache_home/wine-build)
  --tree <dir>       where the private Proton tree goes (default: $vl_cache_home/proton-patched)
  --install <file>   also install the server alone here (default: $vl_data_home/wineserver-patched)
  --server-only      build and install only wineserver; no ntdll.so, no private tree
  --jobs <n>         make jobs (default: 4 or the core count, whichever is less)
  --skip-check       skip the start-up check of the result

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

Wine loads ntdll.so only from the directory of the wine binary it runs, so
the patched ntdll.so (patches 0002 and later) needs a private copy of the
Proton files/ tree (about 1.6 GB, reflinked where the file system can) with
ntdll.so and bin/wineserver replaced. The tree is only valid for the Proton
build it was copied from; run.sh refuses it after Proton was updated, and you
run this script again.

Needs: git, make, a C compiler, autoconf (autoreconf) and libunwind headers
(ntdll is configured --with-unwind, as Proton does). Only the server and
ntdll are built (no mingw, no X, no graphics).
USAGE
}

proton= src= ref= work=$vl_cache_home/wine-build tree_dir=$vl_cache_home/proton-patched
install_to=$vl_data_home/wineserver-patched server_only=0 verify=1
jobs=$(nproc)
((jobs <= 4)) || jobs=4

while (($#)); do
  case $1 in
    --proton) proton=${2:?}; shift 2 ;;
    --wine-src) src=${2:?}; shift 2 ;;
    --wine-ref) ref=${2:?}; shift 2 ;;
    --work) work=${2:?}; shift 2 ;;
    --install) install_to=${2:?}; shift 2 ;;
    --tree) tree_dir=${2:?}; shift 2 ;;
    --server-only) server_only=1; shift ;;
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
refuse_inside_repo "private tree" "$tree_dir"
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

info "configuring"
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
    --with-unwind --without-gnutls --without-sdl --without-fontconfig \
    >configure.out 2>&1 || { tail -20 configure.out >&2; die "configure failed ($build/configure.out)"; }
)

targets=(server/wineserver)
((server_only)) || targets+=(dlls/ntdll/ntdll.so)
info "building ${targets[*]} with $jobs jobs"
make -C "$build" -j"$jobs" "${targets[@]}" >"$build/make.out" 2>&1 ||
  { tail -20 "$build/make.out" >&2; die "build failed ($build/make.out)"; }
server=$build/server/wineserver
ntdll=$build/dlls/ntdll/ntdll.so
[[ -x $server ]] || die "no server binary was produced"
is_elf "$server" || die "$server is not an ELF file"
if ((!server_only)); then
  [[ -f $ntdll ]] || die "no ntdll.so was produced"
  is_elf "$ntdll" || die "$ntdll is not an ELF file"
fi

tmp=$(mktemp -d)
cleanup() {
  [[ -z ${wineserver_to_stop:-} ]] ||
    WINEPREFIX=$tmp/pfx "$wineserver_to_stop" -k >/dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT

if ((server_only)); then
  if ((verify)); then
    info "checking that Proton's wine can talk to the server"
    mkdir -p "$tmp/pfx"
    export WINEPREFIX=$tmp/pfx WINEDEBUG=-all WINEDLLOVERRIDES='mscoree,mshtml='
    wineserver_to_stop=$server
    "$server" -p10
    out=$("$(wine_of "$proton")" cmd /c echo ok 2>&1 | tr -d '\r') || true
    if [[ $out != *ok* ]]; then
      printf '%s\n' "$out" | tail -5 >&2
      die "Proton's wine does not work with this server (protocol mismatch? wrong --wine-ref)"
    fi
  fi
  mkdir -p "$(dirname "$install_to")"
  install -m755 "$server" "$install_to"
  info "installed $install_to"
  exit 0
fi

# The private Proton tree. Built beside the final place, then moved into it, so
# an interrupted run never leaves a half tree that run.sh would use.
info "copying $proton/files to $tree_dir (about 1.6 GB)"
mkdir -p "$tree_dir"
tree_dir=$(realpath -- "$tree_dir")
new=$tree_dir/files.new
rm -rf "$new"
cp -a --reflink=auto "$proton/files" "$new"
chmod -R u+w "$new"
unix=$new/lib/wine/x86_64-unix
[[ -f $unix/ntdll.so && -f $new/bin/wineserver ]] ||
  die "Proton's files/ tree has no lib/wine/x86_64-unix/ntdll.so and bin/wineserver"
# Remove first: the copy may share data with the original through a reflink or
# a hard link, and install would otherwise write through it.
rm -f "$unix/ntdll.so" "$new/bin/wineserver"
install -m755 "$ntdll" "$unix/ntdll.so"
install -m755 "$server" "$new/bin/wineserver"
is_elf "$unix/ntdll.so" && is_elf "$new/bin/wineserver" || die "replaced files are not ELF"

if ((verify)); then
  info "checking the new tree (throwaway prefix)"
  ver=$("$new/bin/wine" --version 2>&1) || die "wine --version fails in the new tree: $ver"
  echo "  $ver"
  mkdir -p "$tmp/pfx"
  export WINEPREFIX=$tmp/pfx WINEDEBUG=-all WINEDLLOVERRIDES='mscoree,mshtml='
  wineserver_to_stop=$new/bin/wineserver
  out=$("$new/bin/wine" cmd /c echo ok 2>&1 | tr -d '\r') || true
  if [[ $out != *ok* ]]; then
    printf '%s\n' "$out" | tail -5 >&2
    die "the new tree's wine cannot run a program"
  fi
  "$new/bin/wineserver" -k || true
  wineserver_to_stop=
fi

# Stamp: the tree is valid only for the Proton build it was copied from.
patch_hash=$(cat "${patches[@]}" | sha256sum | cut -d' ' -f1)
{
  printf 'proton=%s\n' "$proton"
  printf 'fingerprint=%s\n' "$(proton_fingerprint "$proton")"
  printf 'patches=%s\n' "$patch_hash"
  printf 'wine_ref=%s\n' "$(git -C "$tree" rev-parse HEAD)"
} >"$new/../stamp.new"

rm -rf "$tree_dir/files"
mv "$new" "$tree_dir/files"
mv "$tree_dir/stamp.new" "$tree_dir/stamp"
mkdir -p "$(dirname "$install_to")"
install -m755 "$server" "$install_to"

info "done"
cat <<DONE
Private Proton tree: $tree_dir/files
  (ntdll.so and bin/wineserver patched: $(printf '%s ' "${patches[@]##*/}" | sed 's/ $//'))
run.sh uses it automatically; --stock-wine uses Proton's own files instead.
Rebuild after Proton is updated: run this script again.
The server alone is also in $install_to (for --wineserver).
DONE
