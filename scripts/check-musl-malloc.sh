#!/bin/sh
# Fail unless malloc, free, calloc and realloc of a static musl paneld
# binary are musl's (mallocng), not Zig's libc (heap.SmpAllocator) or
# any other strong definition that replaced musl's weak malloc.
#
# usage: scripts/check-musl-malloc.sh <unstripped paneld binary>...
#
# The binary must keep its symbols: build it with
# CARGO_PROFILE_RELEASE_STRIP=none (scripts/build-static.sh --check
# does this). See docs/deploy.md, "Allocator check".
#
# What musl 1.2 (mallocng) looks like once linked:
#   malloc   weak alias of default_malloc (same address)
#   free     global, branches straight to __libc_free
#   realloc  global, branches straight to __libc_realloc
#   calloc   global, calls malloc (= default_malloc)
#   __libc_free, __libc_realloc, __libc_malloc_impl, __malloc_alloc_meta defined
# Each of the four names must be defined exactly once.
set -eu

[ $# -ge 1 ] || { echo "usage: $0 <unstripped paneld binary>..." >&2; exit 2; }
NM=${NM:-nm}
command -v "$NM" >/dev/null 2>&1 || NM=llvm-nm
OBJDUMP=${OBJDUMP:-llvm-objdump}
command -v "$OBJDUMP" >/dev/null 2>&1 || {
    echo "$0: needs llvm-objdump (set OBJDUMP=...)" >&2; exit 2; }

status=0
for bin in "$@"; do
    fail() { echo "FAIL $bin: $*" >&2; status=1; }
    syms=$("$NM" "$bin" 2>/dev/null) || { fail "cannot read symbols"; continue; }
    if [ -z "$syms" ]; then
        fail "stripped; rebuild with CARGO_PROFILE_RELEASE_STRIP=none"; continue
    fi
    # "<addr> <type>" of every definition of a name, one per line.
    defs() { printf '%s\n' "$syms" | awk -v n="$1" '$3 == n && $2 != "U" && $2 != "w" { print $1, $2 }'; }
    # The functions the first call/branch in a symbol's code goes to.
    calls() {
        "$OBJDUMP" -d --no-show-raw-insn --disassemble-symbols="$1" "$bin" 2>/dev/null \
            | sed -n 's/.*\(b\|bl\|call\|callq\|jmp\|jmpq\)[[:space:]]\{1,\}0x[0-9a-f]* <\([^>+]*\)>.*/\2/p'
    }
    before=$status

    zig=$(printf '%s\n' "$syms" | grep -E ' (heap\.SmpAllocator|c\.malloc\.)' || true)
    [ -z "$zig" ] || fail "Zig libc malloc linked in: $(printf '%s' "$zig" | head -1)"

    for s in __malloc_alloc_meta __libc_malloc_impl __libc_free __libc_realloc default_malloc; do
        [ "$(defs $s | wc -l)" -ge 1 ] || fail "musl mallocng symbol $s missing"
    done
    for s in malloc free calloc realloc; do
        [ "$(defs $s | wc -l)" -eq 1 ] || fail "$s defined $(defs $s | wc -l) times, want 1"
    done
    if [ $status = $before ]; then
        m=$(defs malloc); dm=$(defs default_malloc | head -1)
        [ "${m#* }" = W ] || fail "malloc is not musl's weak alias (type ${m#* }): something overrides it"
        [ "${m%% *}" = "${dm%% *}" ] || fail "malloc (${m%% *}) is not default_malloc (${dm%% *})"
        for s in free realloc calloc; do
            t=$(defs $s); [ "${t#* }" = T ] || fail "$s is not a global definition (type ${t#* })"
        done
        calls free | head -1 | grep -qx __libc_free || fail "free does not go to __libc_free"
        calls realloc | head -1 | grep -qx __libc_realloc || fail "realloc does not go to __libc_realloc"
        calls calloc | grep -qxE 'malloc|default_malloc|__libc_malloc' || fail "calloc does not call musl's malloc"
    fi
    [ $status != $before ] || echo "ok   $bin: malloc/free/calloc/realloc are musl mallocng"
done
exit $status
