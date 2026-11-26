#!/usr/bin/env bash
# Generic once-a-second sampler for a run: system pressure (PSI), MemAvailable,
# swap, and the resident size of a process tree plus any wineserver of the
# user. Writes tab-separated values to a file until the root process ends.
# Used by run.sh --measure; can also be run by hand:
#   measure.sh <file> <root pid>
set -u

out=${1:?usage: measure.sh <file> <root pid>}
root=${2:?usage: measure.sh <file> <root pid>}
page_kb=$(($(getconf PAGESIZE) / 1024))

printf 'time\tmem_some\tmem_full\tcpu_some\tio_some\tio_full\tmemavail_kb\tswapfree_kb\ttree_procs\ttree_rss_kb\n' >"$out"
while kill -0 "$root" 2>/dev/null; do
  # One awk per sample: reads the pressure files, meminfo and every
  # process's stat and statm; follows parent links from the root.
  awk -v root="$root" -v pk="$page_kb" -v uid="$UID" -v now="$(date +%s)" '
    FILENAME ~ /^\/proc\/pressure\// {
      split(FILENAME, p, "/"); r = p[4]
      for (i = 2; i <= NF; i++) if ($i ~ /^avg10=/) v[r, $1] = substr($i, 7)
      next
    }
    FILENAME == "/proc/meminfo" { if ($1 == "MemAvailable:") ma = $2; if ($1 == "SwapFree:") sf = $2; next }
    FILENAME ~ /\/stat$/ {
      line = $0; q = match(line, /\) [A-Za-z] /)
      if (!q) next
      split(FILENAME, p, "/"); pid = p[3]
      comm = substr(line, index(line, "(") + 1, q - index(line, "(") - 1)
      split(substr(line, q + 4), f, " "); par[pid] = f[1]; name[pid] = comm
      next
    }
    FILENAME ~ /\/statm$/ { split(FILENAME, p, "/"); rss[p[3]] = $2 * pk; next }
    END {
      inset[root] = 1
      do { ch = 0
        for (x in par) if (!(x in inset) && (par[x] in inset)) { inset[x] = 1; ch = 1 }
      } while (ch)
      for (x in name) if (name[x] ~ /^wineserver/ && !(x in inset)) inset[x] = 1
      for (x in inset) if (x in rss) { n++; tot += rss[x] }
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%d\t%d\n", now, v["memory","some"], v["memory","full"], v["cpu","some"], v["io","some"], v["io","full"], ma, sf, n, tot
    }' /proc/pressure/memory /proc/pressure/cpu /proc/pressure/io /proc/meminfo \
    /proc/[0-9]*/stat /proc/[0-9]*/statm 2>/dev/null >>"$out"
  sleep 1
done
