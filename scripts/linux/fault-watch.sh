#!/usr/bin/env bash
# Follows the kernel's log for faults of the graphics card and appends one
# line per kernel line about one to <file>: "<unix time> <message>". NVIDIA
# reports them as "NVRM: Xid (...): <code>, ..." lines, amdgpu as ring
# timeouts, "GPU reset begin!" and page faults. Only lines logged after the
# start are written. The layer reads the file (VICTORY_HOST_FAULT_FILE) and
# takes the lines of one fault as one event. See docs/linux.md.
#
# Needs no root, but the user must be allowed to read the system journal
# (on most distributions members of the wheel, adm or systemd-journal group
# are). Runs until killed.
set -euo pipefail

usage() { echo "usage: fault-watch.sh <file>" >&2; exit 2; }
(($# == 1)) || usage
out=$1

command -v journalctl >/dev/null || { echo "fault-watch: journalctl not found; the card's faults are not watched" >&2; exit 1; }
if ! journalctl -k -n 1 -q >/dev/null 2>&1; then
  echo "fault-watch: the kernel's log cannot be read (journalctl -k); the card's faults are not watched" >&2
  exit 1
fi

: >>"$out"
# Driver lines only; the layer tells faults from follow-ups.
pattern='NVRM: Xid|amdgpu.*(timeout|GPU reset begin|page fault)'
journalctl -k -f -n 0 -q -o short-unix |
  grep --line-buffered -E "$pattern" |
  while IFS= read -r line; do
    # "<time> <host> kernel: <message>" -> "<time> <message>", short.
    time=${line%% *}
    msg=${line#* * kernel: }
    printf '%s %.300s\n' "$time" "$msg" >>"$out"
  done
