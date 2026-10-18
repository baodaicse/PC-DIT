#!/usr/bin/env bash
# v1.2 C13: every ISA build of gpu-vram-dit must build the same GEMM corpora, bit for bit: the
# input and the CPU oracle's expected answers (their SHA-256), for u32, FP32 and INT8. The builds
# this CPU can run are compared with the baseline one; the others are reported as not run.
#   tools/test_gpu_isa.sh [baseline-binary]
set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
TARGET=x86_64-unknown-linux-gnu
BASE=${1:-"$ROOT_DIR/target/release/gpu-vram-dit"}

build_of() {
    printf '%s\n' "$ROOT_DIR/target/isa-$1/$TARGET/release/gpu-vram-dit"
}

if [[ ! -x "$BASE" ]]; then
    cargo build --release -p gpu-cli --manifest-path "$ROOT_DIR/Cargo.toml"
fi
for level in v3 v4; do
    if [[ ! -x "$(build_of "$level")" ]]; then
        CARGO_TARGET_DIR="$ROOT_DIR/target/isa-$level" RUSTFLAGS="-C target-cpu=x86-64-$level" \
            cargo build --release -p gpu-cli --target "$TARGET" --manifest-path "$ROOT_DIR/Cargo.toml"
    fi
done

flags=$(grep -m1 '^flags' /proc/cpuinfo || true)
has() {
    local flag
    for flag in "$@"; do
        [[ " $flags " == *" $flag "* ]] || return 1
    done
}
runnable=()
has avx2 fma bmi1 bmi2 f16c movbe && runnable+=(v3)
has avx2 fma avx512f avx512bw avx512cd avx512dq avx512vl && runnable+=(v4)

# arith jobs inner: small shapes and the RTX 5080 overlap one (128 jobs x K 4096).
CASES=("u32 16 256" "fp32 16 256" "int8 16 256" "u32 8 1024" "fp32 8 1024" "int8 8 1024"
    "u32 128 4096" "fp32 128 4096" "int8 128 4096")

digest() {
    "$1" corpus-digest --arith "$2" --jobs "$3" --inner "$4" --seed 11 --threads 4
}

failed=0
printf '%-6s %5s %5s  %-10s %s\n' arith jobs K build "build seconds and digest match"
for case in "${CASES[@]}"; do
    read -r arith jobs inner <<<"$case"
    reference=$(digest "$BASE" "$arith" "$jobs" "$inner")
    line=$(python3 -I -c 'import json,sys; seconds=json.loads(sys.argv[1])["build_seconds"]; print(f"v1 {seconds:.3f} s")' "$reference")
    for level in "${runnable[@]}"; do
        other=$(digest "$(build_of "$level")" "$arith" "$jobs" "$inner")
        verdict=$(python3 -I -c '
import json, sys
a, b = json.loads(sys.argv[1]), json.loads(sys.argv[2])
same = a["input_sha256"] == b["input_sha256"] and a["expected_sha256"] == b["expected_sha256"]
level, seconds = b["cpu_isa"]["level"], b["build_seconds"]
word = "same" if same else "DIFFERENT"
print(f"{level} {seconds:.3f} s {word}")
sys.exit(0 if same and level == sys.argv[3] else 1)
' "$reference" "$other" "$level") || failed=1
        line="$line | $verdict"
    done
    printf '%-6s %5s %5s  %s\n' "$arith" "$jobs" "$inner" "$line"
done
for level in v3 v4; do
    [[ " ${runnable[*]} " == *" $level "* ]] || printf 'not run here: %s (this CPU lacks it)\n' "$level"
done
if [[ $failed -ne 0 ]]; then
    printf 'GPU ISA TEST FAILED: a build gave other corpora than the baseline one\n' >&2
    exit 1
fi
printf 'GPU ISA TEST PASS\n'
