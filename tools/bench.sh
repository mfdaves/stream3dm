#!/usr/bin/env bash
# Compares the two object decoders (our reader, cadmpeg) natively and in WebAssembly.
#
#   tools/bench.sh [file.3dm ...]
#
# Needs release builds: cargo build --release -p stream3dm-cli, the small WebAssembly module
# (cargo build --release -p stream3dm-wasm --target wasm32-unknown-unknown) for ours, and the full
# one (the same with --features full --target-dir target/full) for cadmpeg; use -j 1 on small
# machines.
# Prints a Markdown table: median time of 5 runs (native) or 3 (WebAssembly, warm: after one
# run that compiles the code), throughput, and peak memory (native heap; WebAssembly linear
# memory).
set -euo pipefail
cd "$(dirname "$0")/.."
STREAM3DM=target/release/stream3dm
WASM=target/wasm32-unknown-unknown/release/stream3dm_wasm.wasm
WASM_FULL=target/full/wasm32-unknown-unknown/release/stream3dm_wasm.wasm

median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

native() { # file flag -> "seconds peak_mb"
  local times=() peak=0
  for _ in 1 2 3 4 5; do
    line=$("$STREAM3DM" inspect "$1" --stats $2 2>&1 >/dev/null | grep '^stats:')
    times+=("$(sed 's/.* in \([0-9.]*\) s.*/\1/' <<<"$line")")
    peak=$(sed 's/.*peak heap \([0-9.]*\) MB.*/\1/' <<<"$line")
  done
  echo "$(printf '%s\n' "${times[@]}" | median) $peak"
}

wasm() { # file flag -> "seconds memory_mb"
  local times=() memory=0 module=$WASM
  [ -n "$2" ] && module=$WASM_FULL
  for _ in 1 2 3; do
    line=$(node tools/wasm/inspect.mjs "$module" "$1" $2 --warm 2>&1 >/dev/null | grep '^wasm stats:')
    times+=("$(sed 's/.* in \([0-9.]*\) s.*/\1/' <<<"$line")")
    memory=$(sed 's/.*linear memory \([0-9.]*\) MB.*/\1/' <<<"$line")
  done
  echo "$(printf '%s\n' "${times[@]}" | median) $memory"
}

echo "| File | MB | Native cadmpeg | Native ours | Speed-up | WASM cadmpeg | WASM ours | Speed-up |"
echo "|---|---:|---|---|---:|---|---|---:|"
for f in "$@"; do
  mb=$(awk -v b="$(stat -c %s "$f")" 'BEGIN {printf "%.1f", b / 1e6}')
  read -r nc nc_peak < <(native "$f" --cadmpeg)
  read -r no no_peak < <(native "$f" "")
  read -r wc wc_mem < <(wasm "$f" --cadmpeg)
  read -r wo wo_mem < <(wasm "$f" "")
  cell() { awk -v t="$1" -v m="$2" -v s="$mb" 'BEGIN {printf "%.3f s, %.0f MB/s, %.1f MB", t, (t > 0 ? s / t : 0), m}'; }
  ratio() { awk -v a="$1" -v b="$2" 'BEGIN {if (b > 0) printf "%.1f×", a / b; else printf "–"}'; }
  echo "| $(basename "$f") | $mb | $(cell "$nc" "$nc_peak") | $(cell "$no" "$no_peak") | $(ratio "$nc" "$no") | $(cell "$wc" "$wc_mem") | $(cell "$wo" "$wo_mem") | $(ratio "$wc" "$wo") |"
done
