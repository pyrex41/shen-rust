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

cat > /tmp/cl-in.shen <<EOF
(cd "$PWD/kernel/tests")
(load "runme.shen")
(cl.exit)
EOF

# Time one shell command string, echoing its `real` seconds. The command's own
# stdout+stderr are silenced *inside* an inner `sh -c`, so only /usr/bin/time's
# report reaches the outer stderr -> the pipe -> awk. (Redirecting the program's
# stderr at the outer level would also swallow time's own output, which writes
# to fd 2 — that was the bug this structure avoids.)
timeit() {
  { /usr/bin/time -p sh -c "$1 >/dev/null 2>&1"; } 2>&1 | awk '/^real/{print $2}'
}

mn() { printf '%s\n' "$@" | sort -n | head -1; }

echo "== 4-way kernel-tests, min-of-$N (interleaved) =="
declare -a R C LJ L
for i in $(seq 1 "$N"); do
  R[$i]=$(timeit "'$RUST_BIN' --kernel-tests")
  C[$i]=$(timeit "'$CL_BIN' < /tmp/cl-in.shen")
  if command -v luajit >/dev/null; then
    LJ[$i]=$(timeit "cd '$LUA_DIR' && luajit run-kernel-tests.lua")
  fi
  if command -v lua >/dev/null; then
    L[$i]=$(timeit "cd '$LUA_DIR' && lua run-kernel-tests.lua")
  fi
  printf "round %d: rust=%s cl=%s luajit=%s lua=%s\n" \
    "$i" "${R[$i]:-NA}" "${C[$i]:-NA}" "${LJ[$i]:-NA}" "${L[$i]:-NA}"
done

rmin=$(mn "${R[@]}"); cmin=$(mn "${C[@]}")
echo "---"
printf "shen-cl   (SBCL)   : %ss   1.00x (ref)\n" "$cmin"
awk -v r="$rmin" -v c="$cmin" 'BEGIN{printf "shen-rust (release): %ss   %.2fx\n", r, r/c}'
if [ -n "${LJ[1]:-}" ]; then ljmin=$(mn "${LJ[@]}"); awk -v x="$ljmin" -v c="$cmin" 'BEGIN{printf "shen-lua (luajit)  : %ss   %.2fx\n", x, x/c}'; fi
if [ -n "${L[1]:-}" ];  then lmin=$(mn "${L[@]}");  awk -v x="$lmin"  -v c="$cmin" 'BEGIN{printf "shen-lua (PUC lua) : %ss   %.2fx\n", x, x/c}'; fi
