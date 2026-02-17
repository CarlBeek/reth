#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../../.." && pwd)"
cd "$REPO_ROOT"

if command -v nproc >/dev/null 2>&1; then
    TOTAL_CPUS="$(nproc)"
else
    TOTAL_CPUS="$(sysctl -n hw.ncpu)"
fi

if [[ -r /proc/meminfo ]]; then
    TOTAL_RAM_KB="$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
    TOTAL_RAM_GB="$((TOTAL_RAM_KB / 1024 / 1024))"
else
    TOTAL_RAM_GB=128
fi

RESERVED_CPU_CORES="${RETH_RESERVED_CPU_CORES:-2}"
if (( RESERVED_CPU_CORES >= TOTAL_CPUS )); then
    RESERVED_CPU_CORES=1
fi
WORKER_CPUS="$((TOTAL_CPUS - RESERVED_CPU_CORES))"

DEFAULT_CACHE_MB="$((TOTAL_RAM_GB * 1024 / 4))"
if (( DEFAULT_CACHE_MB < 4096 )); then
    DEFAULT_CACHE_MB=4096
fi
if (( DEFAULT_CACHE_MB > 65536 )); then
    DEFAULT_CACHE_MB=65536
fi

ENGINE_CROSS_BLOCK_CACHE_MB="${RETH_ENGINE_CROSS_BLOCK_CACHE_MB:-$DEFAULT_CACHE_MB}"
ENGINE_PREWARMING_THREADS="${RETH_ENGINE_PREWARMING_THREADS:-$WORKER_CPUS}"
ENGINE_STORAGE_WORKERS="${RETH_ENGINE_STORAGE_WORKERS:-$((WORKER_CPUS / 2))}"
ENGINE_ACCOUNT_WORKERS="${RETH_ENGINE_ACCOUNT_WORKERS:-$ENGINE_STORAGE_WORKERS}"
RPC_CACHE_CONCURRENT_DB_REQUESTS="${RETH_RPC_MAX_CONCURRENT_DB_REQUESTS:-$WORKER_CPUS}"

if [[ "${RETH_NATIVE_BUILD:-1}" == "1" ]]; then
    export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
fi

if [[ ! -x target/maxperf/reth-research ]]; then
    echo "[xeon-perf] building reth-research-bin with --profile maxperf"
    cargo build --profile maxperf -p reth-research-bin
fi

echo "[xeon-perf] cpus=$TOTAL_CPUS reserved=$RESERVED_CPU_CORES worker_cpus=$WORKER_CPUS ram_gb=$TOTAL_RAM_GB"
echo "[xeon-perf] cross_block_cache_mb=$ENGINE_CROSS_BLOCK_CACHE_MB prewarming_threads=$ENGINE_PREWARMING_THREADS"
echo "[xeon-perf] storage_workers=$ENGINE_STORAGE_WORKERS account_workers=$ENGINE_ACCOUNT_WORKERS"

exec ./target/maxperf/reth-research node \
    --engine.reserved-cpu-cores "$RESERVED_CPU_CORES" \
    --engine.cross-block-cache-size "$ENGINE_CROSS_BLOCK_CACHE_MB" \
    --engine.prewarming-threads "$ENGINE_PREWARMING_THREADS" \
    --engine.storage-worker-count "$ENGINE_STORAGE_WORKERS" \
    --engine.account-worker-count "$ENGINE_ACCOUNT_WORKERS" \
    --rpc-cache.max-concurrent-db-requests "$RPC_CACHE_CONCURRENT_DB_REQUESTS" \
    "$@"
