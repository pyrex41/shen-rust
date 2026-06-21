#!/bin/bash
# 4-way cross-port benchmark on the upstream Shen kernel test suite:
# shen-rust (release), shen-cl (SBCL image), shen-lua under LuaJIT, and
# shen-lua under PUC Lua. Reports min-of-N wall-clock, interleaved to share
# thermal state. The machine has run-to-run variance, so trust the ratio and
# the ordering, not any single absolute (and run it on an UNLOADED machine —
# a background build or video call dominates the signal).
#
# Note on fairness: shen-rust (AOT kernel baked into the binary) and shen-cl
# (saved Lisp image) boot in ~0; shen-lua loads its kernel from a cache
# (~0.04s LuaJIT / ~0.39s PUC) then *executes* the suite. The kernel-load
# fraction is small for all ports, so total wall-clock is a fair proxy for
# execution speed here — but see run-kernel-tests.lua's printed
# load_kernel/initialise split if you want execution-only.
set -euo pipefail
cd "$(dirname "$0")/.."

N="${1:-5}"
RUST_BIN="target/release/shen-rust"
CL_BIN="../shen-cl/bin/sbcl/shen"
LUA_DIR="../shen-lua"

run_rust() { ( "$RUST_BIN" --kernel-tests ) ; }
run_cl()   { "$CL_BIN" < /tmp/cl-in.shen ; }
run_lua()  { ( cd "$LUA_DIR" && "$1" run-kernel-tests.lua ) ; }

cat > /tmp/cl-in.shen <<EOF
(cd "$PWD/kernel/tests")
(load "runme.shen")
(cl.exit)
EOF

bench() { # name cmd...
  local name="$1"; shift
  local min=99999
  for i in $(seq 1 "$N"); do
    local t
    t=$( { /usr/bin/time -p "$@" >/dev/null 2>/dev/null; } 2>&1 | awk '/^real/{print $2}' ) || t=99999
    awk -v a="$t" -v m="$min" 'BEGIN{exit !(a<m)}' && min=$t
  done
  printf "%-22s min=%ss\n" "$name" "$min"
  echo "$min"
}

echo "== 4-way kernel-tests, min-of-$N (interleaved) =="
# Interleave: one round-robin pass per iteration keeps thermal state shared.
declare -a R C LJ L
for i in $(seq 1 "$N"); do
  R[$i]=$(  { /usr/bin/time -p "$RUST_BIN" --kernel-tests >/dev/null 2>/dev/null; } 2>&1 | awk '/^real/{print $2}')
  C[$i]=$(  { /usr/bin/time -p bash -c "\"$CL_BIN\" < /tmp/cl-in.shen" >/dev/null 2>/dev/null; } 2>&1 | awk '/^real/{print $2}')
  if command -v luajit >/dev/null; then
    LJ[$i]=$( { /usr/bin/time -p bash -c "cd \"$LUA_DIR\" && luajit run-kernel-tests.lua" >/dev/null 2>/dev/null; } 2>&1 | awk '/^real/{print $2}')
  fi
  if command -v lua >/dev/null; then
    L[$i]=$(  { /usr/bin/time -p bash -c "cd \"$LUA_DIR\" && lua run-kernel-tests.lua" >/dev/null 2>/dev/null; } 2>&1 | awk '/^real/{print $2}')
  fi
  printf "round %d: rust=%s cl=%s luajit=%s lua=%s\n" "$i" "${R[$i]}" "${C[$i]}" "${LJ[$i]:-NA}" "${L[$i]:-NA}"
done

mn() { printf '%s\n' "$@" | sort -n | head -1; }
rmin=$(mn "${R[@]}"); cmin=$(mn "${C[@]}")
echo "---"
printf "shen-cl   (SBCL)   : %ss   1.00x (ref)\n" "$cmin"
awk -v r="$rmin" -v c="$cmin" 'BEGIN{printf "shen-rust (release): %ss   %.2fx\n", r, r/c}'
if [ -n "${LJ[1]:-}" ]; then ljmin=$(mn "${LJ[@]}"); awk -v x="$ljmin" -v c="$cmin" 'BEGIN{printf "shen-lua (luajit)  : %ss   %.2fx\n", x, x/c}'; fi
if [ -n "${L[1]:-}" ]; then lmin=$(mn "${L[@]}"); awk -v x="$lmin" -v c="$cmin" 'BEGIN{printf "shen-lua (PUC lua) : %ss   %.2fx\n", x, x/c}'; fi
