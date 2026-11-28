#!/usr/bin/env bash
# Host-side tests of the launcher's presets. Starts nothing: run.sh is only
# called with --print-env, against a fake Proton directory and prefix.
# Usage: scripts/linux/test-launcher.sh
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
run=$here/run.sh
fails=0
checks=0

ok() { checks=$((checks + 1)); }
bad() { checks=$((checks + 1)); fails=$((fails + 1)); printf 'FAIL: %s\n' "$*" >&2; }
expect_in() { # label text pattern
  if grep -Fq -- "$3" <<<"$2"; then ok; else bad "$1: no line '$3'"; fi
}
expect_not_in() { # label text pattern
  if grep -Fq -- "$3" <<<"$2"; then bad "$1: unexpected '$3'"; else ok; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/proton/files/bin" "$tmp/prefix" "$tmp/game" "$tmp/xdg"
printf '#!/bin/sh\nexit 0\n' >"$tmp/proton/files/bin/wine"
chmod +x "$tmp/proton/files/bin/wine"
: >"$tmp/prefix/system.reg"
: >"$tmp/game/title.exe"
: >"$tmp/victory.toml"

launch() { # extra run.sh arguments; env comes from the caller
  env -i PATH="$PATH" HOME="$tmp/xdg" XDG_DATA_HOME="$tmp/xdg/d" XDG_CONFIG_HOME="$tmp/xdg/c" \
    XDG_STATE_HOME="$tmp/xdg/s" XDG_CACHE_HOME="$tmp/xdg/k" "${EXTRA_ENV[@]}" \
    "$run" --game "$tmp/game" --exe title.exe --proton "$tmp/proton" --prefix "$tmp/prefix" \
    --config "$tmp/victory.toml" --print-env "$@" 2>&1
}

EXTRA_ENV=(X=1)

# 1. No preset: nothing of the preset's is set, profile stays series-x.
out=$(launch)
expect_in "default" "$out" "preset:   none"
expect_in "default" "$out" "profile:  series-x"
expect_not_in "default" "$out" "VICTORY_TIGHT_MEMORY"
expect_in "default" "$out" "(--print-env: nothing was started)"

# 2. The tested preset sets its switches and the tested profile.
out=$(launch --preset tested-8gb)
expect_in "tested-8gb" "$out" "profile:  series-s"
for kv in VICTORY_TIGHT_MEMORY=1 VICTORY_VRAM_HEADROOM_MB=768 VICTORY_CARD_BUFFERS_MB=1280 \
  VICTORY_WINDOW_KB=256 VICTORY_WINDOW_TAILS=1 VICTORY_TEXTURE_BUDGET_EVICT=1 VICTORY_PIPELINE_HOLD=1; do
  expect_in "tested-8gb" "$out" "  $kv"
done

# 3. What the user set wins, in the environment and for the console.
EXTRA_ENV=(VICTORY_CARD_BUFFERS_MB=900 VICTORY_ASYNC_PRESENT=0)
out=$(launch --preset tested-8gb --console series-x)
expect_in "precedence" "$out" "VICTORY_CARD_BUFFERS_MB=900 (yours; preset: 1280)"
expect_in "precedence" "$out" "VICTORY_ASYNC_PRESENT=0 (yours; preset: 1)"
expect_in "precedence" "$out" "profile:  series-x"
expect_not_in "precedence" "$out" "VICTORY_CARD_BUFFERS_MB=1280"

# 4. VICTORY_PRESET works, and --preset beats it.
EXTRA_ENV=(VICTORY_PRESET=tested-8gb)
expect_in "env preset" "$(launch)" "preset:   tested-8gb"
expect_in "flag over env" "$(launch --preset none)" "preset:   none"

# 5. An unknown preset is refused.
EXTRA_ENV=(X=1)
if out=$(launch --preset nope); then bad "unknown preset accepted"; else ok; fi
expect_in "unknown" "$out" "unknown preset 'nope'"

# 6. Nothing was created by the dry run.
if [[ -e $tmp/xdg/s || -e $tmp/xdg/k ]]; then bad "--print-env made directories"; else ok; fi

printf '%d checks, %d failed\n' "$checks" "$fails"
((fails == 0))
