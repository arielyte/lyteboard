#!/usr/bin/env bash
set -euo pipefail

ndk=$1
min_sdk=$2
bindings_dir=$3
jni_dir=$4
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
target_dir="$repo/target"
target=aarch64-linux-android
abi=arm64-v8a
library="$target_dir/$target/release/liblyteboard_android.so"
linker="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/$target$min_sdk-clang"

if [[ ! -x "$linker" ]]; then
    printf 'Missing Android NDK linker: %s\nSee apps/android/README.md.\n' "$linker" >&2
    exit 1
fi

cd "$repo"
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$linker" \
    cargo build --locked --target-dir "$target_dir" -p lyteboard-android --lib --release --target "$target"

mkdir -p "$bindings_dir"
cargo run --locked --target-dir "$target_dir" -p lyteboard-android --features bindgen \
    --bin lyteboard-uniffi-bindgen -- generate "$library" \
    --config apps/android/rust/uniffi.toml --language kotlin --out-dir "$bindings_dir" --no-format
install -Dm644 "$library" "$jni_dir/$abi/${library##*/}"
