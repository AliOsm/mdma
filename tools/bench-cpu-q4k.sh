#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 MODEL.gguf [RESULTS.txt]" >&2
    exit 2
fi

model=$1
output=${2:-cpu-q4k-benchmark.txt}
prompt_tokens=${MDMA_BENCH_PROMPT_TOKENS:-128}
decode_steps=${MDMA_BENCH_DECODE_STEPS:-16}
repetitions=${MDMA_BENCH_REPETITIONS:-3}
threads=${RAYON_NUM_THREADS:-$(nproc)}
cpu_list=${MDMA_CPU_LIST:-0-$((threads - 1))}
binary=${MDMA_BENCH_BIN:-target/release/mdma-bench}
rustflags=${RUSTFLAGS:--C target-cpu=native}

[[ -f "$model" ]] || { echo "model not found: $model" >&2; exit 1; }

RUSTFLAGS="$rustflags" cargo build --release --bin mdma-bench

{
    printf '# mdma CPU Q4_K benchmark\n'
    printf 'timestamp_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'revision=%s\n' "$(git rev-parse HEAD)"
    printf 'model=%s\n' "$(realpath "$model")"
    printf 'model_sha256=%s\n' "$(sha256sum "$model" | awk '{print $1}')"
    printf 'rustc=%s\n' "$(rustc --version)"
    printf 'threads=%s\n' "$threads"
    printf 'cpu_list=%s\n' "$cpu_list"
    printf 'prompt_tokens=%s\n' "$prompt_tokens"
    printf 'decode_steps=%s\n' "$decode_steps"
    printf 'repetitions=%s\n' "$repetitions"
    printf 'rustflags=%s\n' "$rustflags"
    lscpu | awk -F: '/Model name|CPU\(s\)|Core\(s\) per socket|Thread\(s\) per core/ {gsub(/^[ \t]+/, "", $2); printf "lscpu_%s=%s\n", $1, $2}'
} >"$output"

run_mode() {
    local mode=$1
    local -a mode_env=(MDMA_BACKEND=cpu RAYON_NUM_THREADS="$threads")
    local -a runner=()
    if command -v taskset >/dev/null 2>&1; then
        runner=(taskset -c "$cpu_list")
    fi
    case "$mode" in
        exact)
            ;;
        q8)
            mode_env+=(MDMA_CPU_Q4K_VEC_DOT=1)
            ;;
        f32)
            mode_env+=(MDMA_CPU_Q4K_F32_DOT=1)
            ;;
        *)
            echo "unknown mode: $mode" >&2
            exit 2
            ;;
    esac
    printf '\nmode=%s\n' "$mode" | tee -a "$output"
    env -u MDMA_CPU_Q4K_VEC_DOT -u MDMA_CPU_Q4K_F32_DOT \
        "${mode_env[@]}" "${runner[@]}" "$binary" \
        "$model" "$prompt_tokens" "$decode_steps" "$repetitions" | tee -a "$output"
}

run_mode exact
run_mode q8
run_mode f32

printf '\nresults=%s\n' "$(realpath "$output")"
