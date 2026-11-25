# Running on Linux (Proton)

The runtime is a Windows program. On Linux it runs under Wine, using the
Wine that ships with a Proton build, with vkd3d-proton as the Direct3D 12
implementation. This is work in progress: what runs under Windows runs here
up to the same stage, with the differences listed below. The scripts in
`scripts/linux/` set everything up outside the repository.

## Requirements

- A GPU and driver with Vulkan 1.3 and what vkd3d-proton needs for Direct3D 12
  feature level 12.2.
- A Proton build that contains `files/bin/wine`. Tested with a recent
  Proton CachyOS build. GE-Proton has newer vkd3d-proton builds. Valve's
  Proton is best kept for reports upstream.
- rustup. The toolchain in `rust-toolchain.toml` and the
  `x86_64-pc-windows-msvc` target are installed by rustup. A system-wide Rust
  ahead of rustup in `PATH` ignores the pin, so the scripts put
  `~/.cargo/bin` first.
- [cargo-xwin](https://github.com/rust-cross/cargo-xwin)
  (`cargo install cargo-xwin`), which cross-compiles for MSVC on Linux.
- `winetricks` (and `cabextract`).
- 16 GB of RAM plus swap, or more. See [Memory](#memory).
- Your own unpacked game folder. The repository contains no game files.

### vkd3d-proton version

vkd3d-proton 3.1 translates three of the title's compute shaders to invalid
SPIR-V (its dxil-spirv emits "Function end cannot be called in blocks" and
loops without a structured exit). Some GPU drivers abort the whole process on
such a shader; the title then dies during the first load, usually with exit
status 3 or SIGKILL, always at the same shader.

Use a vkd3d-proton with dxil-spirv `7c04863` or newer. That is vkd3d-proton
master from 2026-11-24 (commit `ec20719`) on, or a release after 3.1. A running build prints a line `vkd3d-proton - build: <hash>` in the log
(set `VKD3D_DEBUG=info` if it is missing); master of 2026-11-24 is `ec20719`.

To build it (needs `meson`, `ninja`, `glslang` and the `mingw-w64` toolchain):

```sh
git clone --recursive https://github.com/HansKristian-Work/vkd3d-proton
cd vkd3d-proton
meson setup --cross-file build-win64.txt --buildtype release \
    --prefix "$PWD/out" -Denable_tests=false build64
ninja -C build64 install
# the DLLs are in out/bin/ (or out/x64/, depending on the version)
```

Give the directory that holds `d3d12.dll` and `d3d12core.dll` to
`setup.sh --vkd3d`.

## Setup

```sh
scripts/linux/setup.sh --proton <proton dir> --game <your game folder> \
    [--prefix <dir>] [--vkd3d <dir with d3d12.dll and d3d12core.dll>]
```

`--proton` can be replaced by the environment variable `VICTORY_PROTON`.
`setup.sh --help` lists every option. The script:

1. checks the tools (`setup.sh --check` does only this);
2. creates a Wine prefix (default `~/.local/share/victory-linux/prefix`) and,
   when the Proton build keeps vkd3d-proton and DXVK in separate folders,
   copies them in;
3. installs Microsoft's `d3dcompiler_47` with winetricks. Wine's built-in one
   compiles HLSL with vkd3d-shader, which fails on several of the runtime's own
   helper compute shaders, among them the indirect-argument clamp;
4. optionally installs the vkd3d-proton from `--vkd3d` into the prefix. The
   prefix's own `d3d12.dll` and `d3d12core.dll` are copied once to
   `<prefix>/vkd3d-backup`, and are never overwritten there;
5. builds `target/x86_64-pc-windows-msvc/release/victory.exe`
   (`cargo xwin build --release -p victory --target x86_64-pc-windows-msvc`;
   the `--target` is required, because without it every `cfg(windows)` block is
   compiled out);
6. writes `victory.toml` in `~/.config/victory-linux/` with the game, cache and
   data folders as `Z:\` paths, and remembers the Proton build and the prefix
   in `launcher.env` beside it.

It can be run again; finished steps are skipped. Running a Proton game through
Steam or umu on the same prefix may copy Proton's own vkd3d-proton back over
yours: run `setup.sh --vkd3d` again if so.

Everything the scripts write is outside the repository, and they refuse paths
inside it.

## Running

```sh
scripts/linux/run.sh --game <your game folder> --exe <file name of the title> \
    [--console series-x|series-s] [--diag]
```

- The log goes to `~/.local/state/victory-linux/logs/` and to the terminal.
- `--diag` uses the diagnostics build, made with
  `cargo xwin build --profile diag -p victory --features diagnostics --target x86_64-pc-windows-msvc`.
  It enables `VICTORY_CAPTURE` and related variables.
- Wine needs `WINEDLLOVERRIDES` to use the prefix's d3d12 and compiler DLLs:
  the script sets `d3d12,d3d12core,dxgi,d3d11,d3d10core,d3d9,d3dcompiler_47=n`,
  `WINEPREFIX`, `VKD3D_SHADER_CACHE_PATH` (outside the repository, or
  vkd3d-proton writes its cache into the working directory), `VICTORY_CONFIG`
  and `VICTORY_NO_WATCHDOG=1`. The title has a watchdog that crashes it on
  purpose after about 30 seconds without progress of its main thread, which a
  slow load can reach.
- Every `VICTORY_*` and `VKD3D_*` variable in your environment is passed on, so
  `VICTORY_LOG=debug scripts/linux/run.sh ...` works.
- It prints the vkd3d-proton version found in the prefix. The version number
  does not tell a 3.1 release from a later build of the same number; the
  `vkd3d-proton - build:` line in the log does.
- Run the title from this launcher rather than from a shell inside an agent
  or terminal service that has a memory limit; see below.

Under Wine the window needs focus for the keyboard, which acts as a pad when
no pad is connected (see the README for the keys).

### Memory

The title needs more than 10 GB of memory, and about 22 GB was measured on
another machine. On a 16 GB machine, give the system swap (zram, a swap file)
and think about a guard: when the system runs out of memory, the desktop
session can go down with the game.

`--scope` runs the title in a transient systemd user scope with limits:

```sh
scripts/linux/run.sh --game ... --exe ... --scope \
    --memory-high 13G --memory-max 14G --swap-max 24G
```

`--memory-high` makes the kernel reclaim and throttle above that size,
`--memory-max` kills the process group, and `--swap-max` caps swap use. Choose
values that leave room for the desktop. A process started inside a limited
service (an editor, an agent runner, a terminal slice) is under that service's
limit, not the scope's, unless the scope is made from a different slice; check
`systemctl --user status` if the title dies early.

### Scripted pad

For unattended runs, `VICTORY_PAD_FILE` names a file that is read as a list of
pad steps; see `crates/victory-hle/src/modules/gameinput/padscript.rs` for the
full syntax. Set it as a Windows path:

```sh
export VICTORY_PAD_FILE='Z:\home\<you>\.local\state\victory-linux\pad.txt'
scripts/linux/run.sh ...
# in another shell, while it runs:
echo A >> ~/.local/state/victory-linux/pad.txt        # tap A
echo 'LSTICK=0,1 @2000' >> ~/.local/state/victory-linux/pad.txt
```

The file is taken (renamed away) as it is read; progress is written to
`<path>.status` as `done=<n> queued=<n> playing=<step>`. Steps: buttons
`A B X Y MENU VIEW UP DOWN LEFT RIGHT LB RB LS RS`, `LSTICK=x,y`, `RSTICK=x,y`,
`LT=v`, `RT=v`, `WAIT @ms`, `CLEAR`, `@ms` for a hold time.

## Differences from Windows

- **No pipeline library.** vkd3d-proton does not give the runtime a pipeline
  library (the log says "pipelines are not kept"). vkd3d-proton's own shader
  cache under `VKD3D_SHADER_CACHE_PATH` keeps the compiled pipelines, so later
  runs are faster.
- **No DXIL hash check.** A Windows driver refuses a DXIL container with a
  wrong hash; vkd3d-proton accepts it.
- **No driver event watch.** The Windows event log that the runtime reads for
  driver resets does not exist under Wine.
- **Descriptor heaps are capped at 1,000,000 views** by vkd3d-proton. The title
  asks for 1,245,184. What this costs is not measured yet.
- **Wave64 compute is refused on NVIDIA.** Some compute pipelines need 64-lane
  waves, which NVIDIA GPUs do not offer. AMD RDNA GPUs do.
- **The Series S profile: the upscaler is replaced by a plain stretch.** The
  title renders below 1080p and has its FidelityFX upscaler write the 1080p
  image; the upscaler's passes are not translated, so that image stayed black.
  On Series S the runtime takes the title's `ffxDispatch` import and records a
  bilinear stretch of the colour input instead (no temporal upscaling or
  sharpening). The log says `upscale dispatch: a stretch ... was recorded` once,
  or `upscale dispatch NOT stretched` with the reason.
  `VICTORY_UPSCALE_STRETCH=0` turns it off. The title also presents its
  interface as a second 4K plane; on Series S it is drawn over the game image
  in the window (`VICTORY_SECOND_PLANE=0` turns that off). Not yet confirmed
  with the game; if the window stays black, use `--console series-x`.

## Performance

- Loading and the world are slow under stock Wine mostly in wineserver: it
  walks the list of the process's mapped views (an overlap check on every map,
  then a lookup by base address), which is linear in the number of views, and
  the title holds about 140,000 of them in the world. The wineserver patch
  below removes this; use it.
- With the patch, Linux is in the same range as the Windows forks at the same
  stage: menus at 30 fps, and in the world about 1-1.7 fps (frames of about
  0.8 s at roughly 3,000 draws) on an RTX 3060 Ti with 8 GB. The world is not
  playable yet on any fork.
- Memory is the next wall. With 16 GB of RAM and an 8 GB card the driver runs
  out of system memory in the world after a few minutes (the process is killed
  with `NV_ERR_NO_MEMORY` in the kernel log). Plan on more RAM, or a card with
  more memory; swap only delays it.
- Swap helps with loading peaks. zram first and a swap file behind it work well.

## Faster memory mapping: the wineserver patch

The runtime keeps one mapped 64 KiB view per page of the title's memory, about
140,000 in the world. Stock wineserver searches a process's views linearly on
every map, unmap and address query. With 160,000 views, in a micro-benchmark:
map 764 us to 11.8 us, unmap and remap 2854 us to 22 us, address query 900 us
to 6 us. In the title, the first world frame arrives after 77 s instead of
204-253 s, and world frames take about 0.8 s instead of 8.7-13.5 s.

`patches/wine/0001-server-index-memory-views.patch` indexes the views by base
address in the server. It is optional, but strongly recommended. It touches only
the server, so Proton's other files stay as they are. It is LGPL-2.1-or-later
like Wine and is not upstream yet (`patches/wine/README.md`).

Build it from the Wine source your Proton build was made from (the protocol
must match exactly; the script checks this by running `wine cmd /c echo ok`
against the new server on a temporary prefix):

```sh
scripts/linux/build-wineserver.sh --proton <proton dir> \
    --wine-src <git url or local clone> --wine-ref <commit>
```

The commit is the one the Proton build was made from; `--help` explains how to
find it. Only `wineserver` is built (about a minute). It is installed to
`~/.local/share/victory-linux/wineserver-patched`.

`run.sh` uses that file if it exists, starts it for the prefix before wine
(inside the same `--scope`, if any; wine would otherwise use the server next to
it) and prints which server it runs. `--wineserver <path>` picks another,
`--stock-wineserver` turns it off.

## Troubleshooting

- **The process is killed during the first load (exit status 3, 137 or
  SIGKILL).** Either the memory limit of the scope, or of the service you started
  it from, was reached (`journalctl --user` shows the kill), or vkd3d-proton is
  older than described above. Check the version first.
- **No D3D12 device.** Running wine directly skips Proton's DLL overrides;
  use `run.sh`, which sets them. Check that `d3d12core.dll` is in the prefix's
  `drive_c/windows/system32` and that the Vulkan driver works (`vulkaninfo`).
- **Shader compile errors from `D3DCompile`.** The native `d3dcompiler_47` is
  missing from the prefix, or `d3dcompiler_47=n` is not in `WINEDLLOVERRIDES`.
- **A black window with `--console series-s`.** Look for `upscale` in the
  log (see above); use `series-x` meanwhile.
- **A `vkd3d-proton.cache` file appears in the working directory.**
  `VKD3D_SHADER_CACHE_PATH` was not set.
- **The title exits after about 30 seconds of a slow load.** The watchdog is on;
  `VICTORY_NO_WATCHDOG=1` (set by `run.sh`) turns it off.

## Tests under Wine

```sh
scripts/linux/test.sh [--only <text>] [-- <test harness arguments>]
```

builds the workspace tests with `cargo xwin test --workspace --release
--no-run --target x86_64-pc-windows-msvc`, runs each test executable with the
prefix's wine and the environment above, and prints one row per executable.
Logs go to `~/.local/state/victory-linux/logs/`. The tests need no game data.
The script exits 0 when the only failures are known differences:

| Test | Reason |
|------|--------|
| `emit::tests::stub_pipeline_is_created` | vkd3d-proton does not check the DXIL hash |
| `pipeline::library::tests::a_rejected_blob_gives_an_empty_library` | no usable pipeline library |
| `drvwatch::tests::the_system_takes_the_subscription` | no Windows event log under Wine |
| `loads_binds_and_calls_probe` | runs cargo, which is not in the prefix |
| `policy::tests::repository_passes_policy` | runs git, which is not in the prefix; run `cargo xtask policy` on the host |

The tests that need a GPU run on the card through vkd3d-proton; the memory-model
tests (placeholders, `MapViewOfFile3`, guard pages, write watch) pass under Wine.
