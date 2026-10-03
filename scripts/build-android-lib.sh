#!/usr/bin/env bash
# Собирает linet-ffi под Android и генерирует Kotlin-биндинги.
set -euo pipefail
cd "$(dirname "$0")/.."
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/Android/Sdk/ndk/$(ls "$HOME/Android/Sdk/ndk" | sort -V | tail -1)}"
PROFILE="${PROFILE:-release}"
TARGETS="${TARGETS:-arm64-v8a}"

args=()
for t in $TARGETS; do args+=(-t "$t"); done
[[ "$PROFILE" == release ]] && rel=(--release) || rel=()
cargo ndk "${args[@]}" -P 29 -o android/app/src/main/jniLibs build "${rel[@]}" -p linet-ffi

# Release-сборка стрипается вместе с метаданными uniffi, поэтому биндинги
# генерируем из хостовой debug-сборки: интерфейс у них один и тот же.
cargo build -q -p linet-ffi --lib
lib=target/debug/liblinet_ffi.so
cargo run -q -p linet-ffi --features bindgen --bin uniffi-bindgen -- \
    generate --library "$lib" --language kotlin --no-format --out-dir android/app/src/main/java
