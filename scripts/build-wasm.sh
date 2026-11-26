#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

set -eo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
source env/env.sh
set -u
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=2
export RUST_TEST_THREADS=3
export SOURCE_DATE_EPOCH=0

root="$(pwd -P)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
remap_flags=(
    "--remap-path-prefix=$root=/rcmake"
    "--remap-path-prefix=$cargo_home=/cargo"
    "--remap-path-prefix=$rustup_home=/rustup"
)
if [[ -n "${HOME:-}" ]]; then
    remap_flags+=("--remap-path-prefix=$HOME=/home")
fi
export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }${remap_flags[*]}"

cargo build --locked -p rcmake-wasm --target wasm32-wasip1 --profile wasm-release --no-default-features
target_dir="${CARGO_TARGET_DIR:-target}"
binary="$target_dir/wasm32-wasip1/wasm-release/rcmake-wasm.wasm"
artifact=site/wasm/rcmake_wasm.wasm
if command -v wasm-opt >/dev/null 2>&1; then
    optimized="$target_dir/wasm32-wasip1/wasm-release/rcmake-wasm.opt.wasm"
    wasm-opt -Oz --strip-debug "$binary" -o "$optimized"
    binary="$optimized"
fi
raw_bytes=$(wc -c < "$binary")
gzip_bytes=$(gzip -n -9 -c "$binary" | wc -c)
printf 'WebAssembly size\n%u bytes raw\n%u bytes gzipped\n' "$raw_bytes" "$gzip_bytes"
if (( raw_bytes > 7 * 1024 * 1024 / 2 )); then
    printf '%s\n' 'The WebAssembly file exceeds 3.5 MiB. Use --browser to check instantiation in Chromium.'
fi

install -m 0644 "$binary" "$artifact"
node --experimental-default-type=module crates/rcmake-wasm/tests/wasm.mjs "$@"
