#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Build only the staged compatibility components; no install or game launch.
set -euo pipefail
stage=$(realpath -- "${1:-$(dirname -- "$0")/../build/compat}")
jobs=${BUILD_JOBS:-$(getconf _NPROCESSORS_ONLN)}
test -f "$stage/source-manifest.json"
mkdir -p "$stage/wine-build" "$stage/artifacts/bin" \
    "$stage/artifacts/runtime" "$stage/artifacts/builtin/x86_64-windows" \
    "$stage/artifacts/builtin/x86_64-unix"
if [[ ! -f "$stage/wine-build/Makefile" ]]; then
    (cd "$stage/wine-build" && ../wine-src/configure --enable-win64 \
        --without-x --without-wayland --without-freetype --without-alsa \
        --without-pulse --without-dbus --without-gstreamer --disable-tests)
fi
make -C "$stage/wine-build" -j"$jobs" \
    dlls/xgameruntime/x86_64-windows/xodus_store_test.dll
cp -- "$stage/wine-build/dlls/xgameruntime/x86_64-windows/xodus_store_test.dll" \
    "$stage/artifacts/builtin/x86_64-windows/"
# Remove compiler debug paths without changing the exported runtime interface.
x86_64-w64-mingw32-strip --strip-debug \
    "$stage/artifacts/builtin/x86_64-windows/xodus_store_test.dll"
gcc -shared -fPIC -O2 -D__WINESRC__ -DWINE_NO_TRACE_MSGS -DWINE_NO_DEBUG_MSGS \
    -fshort-wchar -Wl,-z,defs -pthread -I"$stage/wine-src/include" \
    "$stage/wine-src/dlls/xgameruntime/GDKComponent/Xodus/Unix/socket.c" \
    -o "$stage/artifacts/builtin/x86_64-unix/xodus_store_test.so"
for header in xasync xgameerr xgameruntimetypes xtaskqueue; do
    cp -- "$stage/wine-src/include/$header.h" "$stage/runtime/include/"
done
for header in xasyncprovider xgameruntimefeature xnetworking xsystem xuser; do
    "$stage/wine-build/tools/widl/widl" --win64 -D__WINESRC__ -h \
        -I "$stage/wine-src/include" -o "$stage/runtime/include/$header.h" \
        "$stage/wine-src/include/$header.idl"
done
"$stage/wine-build/tools/widl/widl" --win64 -D__WINESRC__ -h \
    -I "$stage/wine-src/include" -o "$stage/runtime/include/xstore.h" \
    "$stage/runtime/idl/xstore.idl"
(cd "$stage/runtime" && bash build.sh)
cp -- "$stage/runtime/xgameruntime.dll" "$stage/artifacts/runtime/"
# Uses the same generated headers and pinned Wine ABI; never authenticates.
bash "$stage/runtime/tools/build-connected-storage.sh" "$stage"
export CARGO_TARGET_DIR="$stage/cargo-target"
# Rust embeds panic/source locations even in release builds. Keep personal
# checkout and Cargo-cache paths out of distributable binaries. Cargo's encoded
# form preserves an installation path containing spaces.
flightdeck_cargo_sources=${CARGO_HOME:-"$HOME/.cargo"}
flightdeck_separator=$'\x1f'
flightdeck_flags=
if [[ -n ${CARGO_ENCODED_RUSTFLAGS:-} ]]; then
    flightdeck_flags=$CARGO_ENCODED_RUSTFLAGS$flightdeck_separator
else
    set -f
    for flightdeck_flag in ${RUSTFLAGS:-}; do
        flightdeck_flags+=$flightdeck_flag$flightdeck_separator
    done
    set +f
fi
flightdeck_flags+="--remap-path-prefix=$stage=/usr/src/flightdeck$flightdeck_separator"
flightdeck_flags+="--remap-path-prefix=$flightdeck_cargo_sources=/usr/src/cargo"
export CARGO_ENCODED_RUSTFLAGS=$flightdeck_flags
# CLI and service deliberately share default Linux Secret Service credentials.
cargo build --locked --release --manifest-path "$stage/xodus-src/Cargo.toml" \
    -p xodus-cli -p xodus-service --bin xodus-cli --bin xodus-service
cp -- "$CARGO_TARGET_DIR/release/xodus-cli" "$CARGO_TARGET_DIR/release/xodus-service" \
    "$stage/artifacts/bin/"
# The manifest binds the built files to the staged sources they came from.
flightdeck_digest() { sha256sum -- "$1" | cut -d' ' -f1; }
flightdeck_manifest=$stage/artifacts-manifest.new
flightdeck_count=0
{
    printf '{\n  "format": 1,\n  "source_manifest_sha256": "%s",\n' \
        "$(flightdeck_digest "$stage/source-manifest.json")"
    printf '  "credential_backend": "Linux Secret Service (D-Bus)",\n'
    printf '  "features": [\n    "connected-storage-read-v1",\n    "connected-storage-sync-v1"\n  ],\n'
    printf '  "cli_features": [\n    "streaming-resume-files-v1",\n    "package-info-json-v1",\n'
    printf '    "streaming-integrity-index-v1",\n    "streaming-progress-v1"\n  ],\n  "files": {'
    while IFS= read -r -d '' flightdeck_file; do
        flightdeck_name=${flightdeck_file#"$stage/artifacts/"}
        # Names are written into JSON as they are.
        [[ $flightdeck_name =~ ^[A-Za-z0-9._/-]+$ ]] || {
            echo "Unexpected artifact name: $flightdeck_name" >&2
            exit 1
        }
        if ((flightdeck_count > 0)); then printf ','; fi
        printf '\n    "%s": "%s"' "$flightdeck_name" "$(flightdeck_digest "$flightdeck_file")"
        flightdeck_count=$((flightdeck_count + 1))
    done < <(find "$stage/artifacts" -type f ! -name manifest.json -print0 | LC_ALL=C sort -z)
    printf '\n  }\n}\n'
} > "$flightdeck_manifest"
mv -- "$flightdeck_manifest" "$stage/artifacts/manifest.json"
echo "Built $flightdeck_count artifacts; no installation performed."
