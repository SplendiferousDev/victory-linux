# Wine patches

Patches to Wine itself, for running the title under Proton on Linux. They are
optional; `scripts/linux/build-wine.sh` applies every `*.patch` here, in file
name order, builds `wineserver` and `ntdll.so`, and puts both into a private
copy of your Proton's `files/` tree, which `run.sh` then uses. See
`docs/linux.md`. Patches 0002 and later change only the client library
(`dlls/ntdll/unix/`) and apply with or without 0001.

## 0001-server-index-memory-views.patch

Keeps a process's memory views in a red-black tree keyed by base address, in
addition to the existing list (which still gives the load order that the main
executable view and debug events need).

Why: the runtime emulates the console's memory with one mapped view per 64 KiB
page and holds about 140,000 of them in the world. Stock wineserver walks the
whole view list on every map (overlap check), every unmap and every address
query, so each call costs time proportional to the number of views and
wineserver becomes the bottleneck of the whole process.

Measured (micro-benchmark with 160,000 views, RTX-class desktop):

| Operation | Stock | Patched |
|---|---|---|
| map a view | 764 us | 11.8 us |
| unmap and remap | 2854 us | 22 us |
| address query | 900 us | 6 us |

In the title: the first world frame arrives after 77 s instead of 204-253 s,
and world frames take about 0.8 s instead of 8.7-13.5 s.

Only the server is changed, so the patched `wineserver` alone can be started
beside an unmodified Proton (`build-wine.sh --server-only`). The protocol
version must match the Proton build; the build script checks this.

Status: not upstream yet. Wine's own tests (ntdll virtual, info and file;
kernel32 virtual, loader and process; kernelbase process and memory) give the
same failure lists with and without it; the failures are environmental.

## 0002 to 0005: cheaper section maps in ntdll

Each map or unmap of a section view costs the process a server round trip
under its virtual-memory mutex; with a view per 64 KiB page the process spends
its time there. These four patches remove work from that path:

- 0002: requests made while the virtual-memory mutex is held use
  `server_call_unlocked()`; the mutex already blocks the signals that
  `wine_server_call()` would mask again (2 fewer `rt_sigprocmask` per request).
- 0003: when a section view replaces a placeholder, only the page protection
  bytes are set; the `mmap` and `mprotect` of the caller set the host
  protection (anonymous placeholder replacement is unchanged, so write watch
  keeps its ordering).
- 0004: no `mprotect` after mapping a section view when every page already
  has the protection the `mmap` used (read-write, no forced exec protection).
- 0005: the `get_mapping_info` reply of non-image sections is cached in a
  64-entry table keyed by handle, dropped on `NtClose` and on
  `NtDuplicateObject` with `DUPLICATE_CLOSE_SOURCE`. A section's size, flags
  and granted access never change, and the server still checks the range on
  `map_view`.

Measured with 150,000 live views, patched server for all rows (medians of 3):

| ntdll | map | unmap and remap | 1 thread | 4 threads | 12 threads |
|---|---|---|---|---|---|
| Proton stock | 11.9 us | 21.9 us | 41,000 cycles/s | 48,000 | 44,600 |
| 0002 to 0005 | 6.7 us | 15.4 us | 58,300 cycles/s | 56,100 | 54,400 |

Per unmap-and-remap cycle the process makes 12 system calls instead of 20
(no `mprotect`, fewer `rt_sigprocmask`). Wine's ntdll tests give the same
failure lists with and without the series.

The client library is not found through an environment variable: Wine loads
`ntdll.so` from the directory of the `wine` binary it runs. Hence the private
copy of Proton's `files/` tree. It is valid only for the Proton build it was
copied from, and `run.sh` refuses it after Proton is updated.

## Licence

Wine is LGPL-2.1-or-later and so are these patches, not GPL-3.0 like the rest
of the repository. They are included as patch files only; nothing here links
with the runtime.
