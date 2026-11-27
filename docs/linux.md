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
- `VKD3D_DEBUG=none` and `VKD3D_SHADER_DEBUG=none` are set unless you set them
  (they are Proton's own defaults, which launching wine directly would miss).
  With `VKD3D_DEBUG=none` the `vkd3d-proton - build:` line is not logged; set
  `VKD3D_DEBUG=info` to see it.
- Before starting, the script warns when `vm.max_map_count` is below 1048576
  (the layer keeps about 150,000 mapped views with 2-3 kernel mappings each, so
  the common default of 65530 is too low; the message gives the `sysctl` line)
  and when `/dev/ntsync` is missing.
- Synchronisation: Wine uses ntsync automatically when `/dev/ntsync` exists
  (kernel 6.14 or newer with the `ntsync` module loaded). Do not set
  `PROTON_NO_NTSYNC`, `WINEESYNC` or `WINEFSYNC`. A log line `ntsync: up and
  running.` (printed by Wine when it is in use) confirms it.
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

### Pipeline caches

Pipelines are kept in three places: the runtime's pipeline library and early
list (`pipelines/` beside the shader cache), vkd3d-proton's cache
(`VKD3D_SHADER_CACHE_PATH`) and, on NVIDIA, the driver's own disk cache
(`~/.cache/nvidia/GLCache`). The driver's cache holds 256 MiB unless
`__GL_SHADER_DISK_CACHE_SIZE` says more, and the title's pipelines need several
times that; when it is full every pipeline is compiled again, library or not.
The launcher sets 12 GB (`__GL_SHADER_DISK_CACHE_SIZE`) and turns the driver's
clean-up off (`__GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1`); set them yourself when
starting wine another way, including for `victory warm` and
`victory precompile-graphics`.

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

### Ambient light (stopgap)

The title's indirect diffuse light comes from ray-traced passes that do not run
yet, so the world is dark where the sun does not reach. The runtime can fill
that light with a constant, shaded by occlusion from the scene's depth, right
after the compute program that resolves it. It is off until you name that
program; nothing of any title is built in:

```sh
# 1. find candidates: each compute program that writes a 32-bit-integer image
#    and reads a depth image of the same size is logged once
VICTORY_AMBIENT_SCAN=1 scripts/linux/run.sh ...
#    ("ambient light: a candidate program", with its hash and views)
# 2. name it (start of its hash, or name:<text in its container>)
VICTORY_AMBIENT_SHADER=<hash> scripts/linux/run.sh ...
```

The log says `ambient light: on for these programs` at the first dispatch, then
`ambient light engaged` (with the views used) or `no ambient light after this
program: <why>`. More settings, all optional: `VICTORY_AMBIENT=<level>` (0.6 by
default) or `off` (turns it off), `VICTORY_AMBIENT_LIGHT=<root>:<offset>` and
`VICTORY_AMBIENT_DEPTH=<root>:<offset>` to pin the views when the log says the
choice is ambiguous, `VICTORY_AMBIENT_TINT=r,g,b`, `VICTORY_AMBIENT_SHADE=s,r,n,t`
(see `crates/victory-gpu/src/ambient.rs`).

## Differences from Windows

- **No pipeline library.** vkd3d-proton does not give the runtime a pipeline
  library (the log says "pipelines are not kept"). vkd3d-proton's own shader
  cache under `VKD3D_SHADER_CACHE_PATH` keeps the compiled pipelines, so later
  runs are faster.
- **No DXIL hash check.** A Windows driver refuses a DXIL container with a
  wrong hash; vkd3d-proton accepts it.
- **No driver event watch.** The Windows event log that the runtime reads for
  driver resets does not exist under Wine; the kernel's log stands in for it
  (see "The card's faults from the kernel's log").
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
- The title waits for fences with AMD's `monitorx` / `mwaitx`, which Intel CPUs
  lack. Each one is an exception that the runtime emulates, and under Wine an
  exception costs far more than on Windows (a signal, then the exception
  dispatch): a long wait takes about 250,000 exceptions a second.
  `VICTORY_MWAITX=wait` (off by default, see below) makes an emulated
  `mwaitx` block on the monitored address (`RtlWaitOnAddress`, a futex under
  Wine) for up to 1 ms once a wait has lasted 200 us; the runtime's fence and
  bottom-of-pipe writes wake it. `VICTORY_MWAITX=spin` (the default) spins as
  before; `VICTORY_MWAITX=alt` does 20 s of each in turn for measuring. `VICTORY_MWAITX_SPIN_US` and `VICTORY_MWAITX_TIMEOUT_US` tune the
  two phases. The log's "console CPU instructions, exceptions a second" line
  counts the blocks, their ends and the wakes.

## Safety defaults

A GPU fault (an NVIDIA `Xid 13` or `Xid 31` MMU fault in the kernel log) can
do more than end the run: once it hung the kernel driver, froze the desktop
and left the game process unkillable until a reboot. Until these faults are
understood, the features that were on when they became frequent are opt-in:

| Switch | Default | To turn it on |
|---|---|---|
| merge of the title's bytes on the card | off | `VICTORY_MERGE_ON_CARD=1` (`alt`: 20 s on, 20 s off) |
| blocking `mwaitx` emulation | off (spins) | `VICTORY_MWAITX=wait` |
| plain (swappable) CPU sides of card copies | off, also with `VICTORY_TIGHT_MEMORY=1` | `VICTORY_CARD_TWIN_PLAIN=1` |

The idle sweep (which lets go of host copies nothing uses) can no longer be
switched off by mistake: `VICTORY_SWEEP=0` alone logs a warning and is
ignored; it takes effect only together with `VICTORY_SWEEP_OFF_CONFIRM=1`.

Also avoid `VKD3D_CONFIG=fault` in unattended runs: it makes vkd3d-proton
wait for the device to be lost after a fault, which was part of the same
hang. After every run, check the kernel log:
`journalctl -k --since "-10 min" | grep Xid`.

### The card's faults from the kernel's log

On Windows the layer subscribes to the display driver's entries in the
system's event log, and the first one switches off the paths under suspicion
for the rest of the run (today: the indirect draw windows of
`VICTORY_INDIRECT_WINDOW=1`). Under Wine the driver reports to the Linux
kernel instead, so `run.sh` starts `scripts/linux/fault-watch.sh` beside the
title. It follows `journalctl -k -f` (no root, but the user must be allowed
to read the system journal, e.g. as a member of `wheel`, `adm` or
`systemd-journal`) and appends each NVIDIA `NVRM: Xid` line and each amdgpu
ring timeout, `GPU reset begin!` and page fault line to `<log>.faults` next
to the log. The layer gets that file as a Windows path in
`VICTORY_HOST_FAULT_FILE`, looks at its size every 250 ms, takes the lines of
one fault (same vendor and kind within 2 s) as one event, logs a `WARN` line
per event and treats it as a driver error: the same counter and switch as the
event log. Lines the file held when the layer started, or stamped before it,
are ignored. Faults of any process on the card count, as on Windows.

NVIDIA's kernel driver can also fail inside its own records of the card's
address space with no Xid: `intermapRegisterDmaMapping: Failed to insert new
mapping node`, then assertions in `gpu_vaspace.c`. Its error path leaves those
records inconsistent; once the process went on and later spun inside the
driver with its lock held, which hung the desktop until a reboot. On these
lines the layer ends the process at once (`TerminateProcess`, no exit
handlers), so the kernel frees what it held before anything else is asked of
the driver. `VICTORY_DRIVER_BROKEN_EXIT=0` keeps running instead.

The watcher costs nothing measurable (one idle `journalctl`). `--no-fault-watch`
turns it off; with `VICTORY_HOST_FAULT_FILE` already set, `run.sh` starts no
watcher and the layer reads the given file (run `fault-watch.sh <file>`
yourself). Without journal access `run.sh` warns and runs without it.

## Measuring a run

`run.sh --measure <file>` samples once a second while the title runs and
writes tab-separated values: memory, CPU and I/O pressure (PSI `avg10`),
`MemAvailable`, free swap, and the number of processes and total resident size
of the launcher's process tree (including any wineserver). The file must be
outside the repository. `scripts/linux/measure.sh <file> <pid>` does the same
for any process. Read the pressure columns together with the frame lines of
the log: a world frame that takes seconds while `mem_full` is high is waiting
for memory, not for the GPU.

## Tuning for 16 GB machines

Verified on a machine with 16 GB of RAM, an 8 GB card and an NVIDIA driver:

- Swap (zram plus a swap file) helps loading peaks, and a `--scope` with
  `--memory-high`/`--memory-max` keeps the desktop alive when memory runs out.
- The patched wineserver removes the largest CPU cost; use it.
- In the world the machine ran out of both video memory and system memory
  (the driver then failed system-memory allocations and the process was
  killed). Nothing we tried tunes this away; more RAM and a larger card are
  needed.
- On a machine short of cores, the pipelines started early (the ones earlier
  runs drew with) can take minutes to load and compile. If the world loads
  meanwhile, its pipelines wait behind them and the picture stays black.
  `VICTORY_PIPELINE_HOLD=1` (off by default; `run.sh` does not set it) holds
  the title's input until they are made: pads and keyboard stay connected
  but report no buttons, sticks or keys, so the title keeps running at its
  first screen that waits for a button. Scripted pad steps are queued and
  played after the hold. The hold ends when the queue is empty (log line
  `title's input released`), or after `VICTORY_PIPELINE_HOLD_SECS` (default
  300). Every 10 s the log says how many are left (`holding the title's
  input: N pipelines left`), and the frame-rate box shows `N PIPELINES LEFT`
  below the rate.

Experimental: `VICTORY_TIGHT_MEMORY=1` changes defaults for a machine short of
memory. The layer's host copies of title memory are buffers in system memory
that the driver pins, so they never go to swap and the title's own memory is
swapped in their place. The mode cuts those copies (the windows outside
buffers) on a 1 MiB grid instead of 4 MiB, puts them into the card's memory
through GPU upload heaps while the card has room (this needs resizable BAR), and
keeps the spare pools small. Any of its switches that you set yourself keeps
your value (`VICTORY_WINDOW_KB`, `VICTORY_WINDOW_UPLOAD`,
`VICTORY_CARD_WINDOWS_MB`, `VICTORY_STAGING_POOL_MB`, `VICTORY_SNAPSHOT_POOL_MB`,
`VICTORY_TEXTURE_READ_KEEP_MB`). Every build logs a line `memory of host
copies ...` every 10 s showing what the copies hold and where.

Experimental: the page pool on a disk file. The title's physical pages are
one section of about 6 GB, of which about 4 GB gets written. Under Wine that
section is shared memory (a memfd, `memfd:wine-mapping` in
`/proc/<pid>/smaps`). When memory runs short its cold pages go to swap, and
with zram the compressed store itself takes RAM (several GB at the kills
measured on a 16 GB machine). `VICTORY_XMEM_POOL_DIR=<Windows path of a
directory>` (off unless set) puts the section on a temporary file in that
directory instead. The kernel then evicts its pages by writing them back to
the file, and clean pages cost no RAM. `run.sh --pool-dir <dir>` prepares
the directory and sets the switch:

```sh
scripts/linux/run.sh --game ... --exe ... --pool-dir ~/.cache/victory-linux/pool
```

- The file holds title data. Put the directory in your own data or cache
  directory, never in the repository (`run.sh` refuses that) and not in a
  RAM file system such as `/tmp` on many distributions (that saves nothing).
  The file is opened with delete-on-close and goes when the title exits. A
  run killed together with its wineserver leaves it behind; `--pool-dir`
  removes such leftovers (`victory-pool-*.tmp`) before the next start.
- It needs free disk space of about the pool's size: the file is sparse, but
  a page the title writes must find room on the disk, or the title crashes.
  The runtime makes the file only with 1 GiB more than the pool free.
- On btrfs, mark the directory no copy-on-write while it is empty
  (`chattr +C <dir>`; `--pool-dir` does it). Otherwise each written-back page
  is copied to a new place, the file fragments, and with data checksums a
  title write to a page being written back waits for that write.
- Disk writes: Linux writes dirty pages of a shared file back on its own
  after `vm.dirty_expire_centisecs` (30 s by default), not only under memory
  pressure. A load writes roughly the pool's touched size (about 4 GB); in
  play, about what the title changes in each 30 s period. Measure with the
  disk's write counters (`/proc/diskstats`, or `iostat`) if SSD wear matters.
- Throttling: a process that dirties pages faster than the disk takes them
  is held in the kernel once dirty pages pass `vm.dirty_ratio` (or
  `vm.dirty_bytes`; under a cgroup memory limit, a share of that limit). The
  title's threads then stall. Watch the `io` pressure columns of
  `--measure`. Lower `vm.dirty_background_bytes` (for example 256 MB) to make
  writeback start earlier and in smaller pieces.
- Wine maps a section on a file the same way as shared memory (one shared
  read-write `mmap` of the section's file descriptor per view). Placeholders,
  views at several addresses, remapping and page guards behave the same.
  The runtime's own changes are small: a file has no reserved state, so the
  section is created committed and views are never committed one by one.
- `VICTORY_INPLACE` and this switch exclude each other. NVIDIA's driver
  refuses to import file-backed pages as GPU host memory. With both set,
  the pool stays shared memory and the log says so; `run.sh --pool-dir`
  refuses to start.
- The log line `physical page pool` names the backing in use and its size.
  To judge it, compare against a run without it: the `memfd:wine-mapping`
  swap in smaps (it should drop to about 0), the zram store (`zramctl`,
  `mm_stat`), major faults of the process, disk writes, and frame times.

Not tested yet (do not rely on it): `vm.swappiness`, `vm.max_map_count` effects
beyond avoiding the limit, transparent huge pages, `zswap` instead of zram,
`MemoryHigh` values other than the ones above, an AMD card (different memory
behaviour).

## Faster memory mapping: the Wine patches

The runtime keeps one mapped 64 KiB view per page of the title's memory, about
140,000 in the world. Stock wineserver searches a process's views linearly on
every map, unmap and address query. With 160,000 views, in a micro-benchmark:
map 764 us to 11.8 us, unmap and remap 2854 us to 22 us, address query 900 us
to 6 us. In the title, the first world frame arrives after 77 s instead of
204-253 s, and world frames take about 0.8 s instead of 8.7-13.5 s.

`patches/wine/` holds two series, optional but recommended; both are
LGPL-2.1-or-later like Wine and not upstream yet (`patches/wine/README.md`):

- 0001 indexes the views by base address in the server (the big win above).
- 0002 to 0005 make section maps cheaper in ntdll: fewer signal-mask system
  calls and no redundant `mprotect`, and a cache of section info. In a
  150,000-view micro-benchmark, map 11.9 to 6.7 us, unmap and remap 21.9 to
  15.4 us, single-thread throughput 41,000 to 58,300 cycles/s.

Build them from the Wine source your Proton build was made from (the protocol
must match exactly):

```sh
scripts/linux/build-wine.sh --proton <proton dir> \
    --wine-src <git url or local clone> --wine-ref <commit>
```

The commit is the one the Proton build was made from; `--help` explains how to
find it. The script applies every `patches/wine/*.patch` in order, builds
`wineserver` and `ntdll.so` (about a minute, at most 4 jobs), and copies
Proton's `files/` tree (about 1.6 GB; reflinked on btrfs and xfs) to
`~/.cache/victory-linux/proton-patched`, replacing `lib/wine/x86_64-unix/ntdll.so`
and `bin/wineserver`. A copy is needed because Wine loads `ntdll.so` only from
next to the `wine` binary it runs. It then checks that `wine --version` and a
trivial program run from the new tree on a throwaway prefix, and writes a
stamp. The server alone is also installed to
`~/.local/share/victory-linux/wineserver-patched`.
`build-wine.sh --server-only` (also what the old `build-wineserver.sh` does)
builds only the server, as before.

`run.sh` uses the patched tree automatically when it exists and prints which
wine and server it runs. The tree is valid only for the Proton build it was
copied from; the stamp records that build's version and file sizes and times,
and `run.sh` stops with a message when Proton has been updated. Run
`build-wine.sh` again then (or `--stock-wine`). `run.sh` warns when
`patches/wine` has changed since the tree was built. Options:

- `--stock-wine`: Proton's own files, no patches.
- `--stock-wineserver`: Proton's own server, even with the patched tree (the
  patched ntdll still runs).
- `--wineserver <path>`: another server, started before wine. Without a tree,
  `run.sh` falls back to the server-only install above.

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
