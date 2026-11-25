# Wine patches

Patches to Wine itself, for running the title under Proton on Linux. They are
optional; `scripts/linux/build-wineserver.sh` applies them and builds a
replacement `wineserver`. See `docs/linux.md`.

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

Only the server is changed, not the client libraries, so the patched
`wineserver` can be started beside an unmodified Proton. The protocol version
must match the Proton build; the build script checks this.

Licence: Wine is LGPL-2.1-or-later and so is this patch, not GPL-3.0 like the
rest of the repository. It is included as a patch file only; it does not link
with the runtime.

Status: not upstream yet. Wine's own tests have not been run against it.
