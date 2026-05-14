#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CRATE_DIR=$(cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(cd -- "$CRATE_DIR/../.." && pwd)

REMOTE=${REMOTE:-dtietjen@ssh.tryeden.dev}
REMOTE_DIR=${REMOTE_DIR:-rblock-bench}
CLOUDFLARED=${CLOUDFLARED:-/opt/homebrew/bin/cloudflared}
BENCH=${BENCH:-rwlock_load}

DURATION_MS=${DURATION_MS:-500}
WARMUP_MS=${WARMUP_MS:-150}
THREADS=${THREADS:-1,2,4,8,16}
SHARDS=${SHARDS:-1,16,64}
MIXES=${MIXES:-100-0,95-5,80-20,50-50,20-80,0-100}
LATENCY_SAMPLE_RATE=${LATENCY_SAMPLE_RATE:-1024}

if [[ $# -gt 0 ]]; then
  BENCH_ARGS=("$@")
else
  BENCH_ARGS=(
    --duration-ms "$DURATION_MS"
    --warmup-ms "$WARMUP_MS"
    --threads "$THREADS"
    --shards "$SHARDS"
    --mixes "$MIXES"
    --latency-sample-rate "$LATENCY_SAMPLE_RATE"
  )
fi

SSH_CMD=(ssh)
RSYNC_RSH=ssh
if [[ "$REMOTE" == *@ssh.tryeden.dev && -x "$CLOUDFLARED" ]]; then
  SSH_CMD=(ssh -o "ProxyCommand=$CLOUDFLARED access ssh --hostname %h")
  RSYNC_RSH="ssh -o 'ProxyCommand=$CLOUDFLARED access ssh --hostname %h'"
fi

echo "remote: $REMOTE"
echo "remote_dir: $REMOTE_DIR"
echo "bench: $BENCH"
echo "bench args: ${BENCH_ARGS[*]}"
echo

"${SSH_CMD[@]}" "$REMOTE" "mkdir -p '$REMOTE_DIR/crates/rblock'"
rsync -az --delete --exclude target -e "$RSYNC_RSH" "$CRATE_DIR/" "$REMOTE:$REMOTE_DIR/crates/rblock/"

if [[ -f "$REPO_ROOT/Cargo.lock" ]]; then
  rsync -az -e "$RSYNC_RSH" "$REPO_ROOT/Cargo.lock" "$REMOTE:$REMOTE_DIR/Cargo.lock"
fi

"${SSH_CMD[@]}" "$REMOTE" "cat > '$REMOTE_DIR/Cargo.toml'" <<'TOML'
[workspace]
members = ["crates/rblock"]
resolver = "3"

[workspace.package]
version = "0.1.0"
edition = "2024"
authors = ["Devon Tietjen <devon@eden.dev>"]
homepage = "https://github.com/d-tietjen/rblock"
license = "Apache-2.0"
repository = "https://github.com/d-tietjen/rblock"
rust-version = "1.90"

[workspace.dependencies]
lock_api = "0.4"
parking_lot_core = "0.9"
parking_lot = "0.12"

[profile.release]
strip = true
lto = "fat"
codegen-units = 1
panic = "abort"

[profile.bench]
inherits = "release"
strip = false
codegen-units = 1
TOML

"${SSH_CMD[@]}" "$REMOTE" "cd '$REMOTE_DIR' && hostname && uname -a && if [ -f \"\$HOME/.cargo/env\" ]; then . \"\$HOME/.cargo/env\"; fi && rustc --version && cargo bench -p rblock --bench '$BENCH' -- ${BENCH_ARGS[*]}"
