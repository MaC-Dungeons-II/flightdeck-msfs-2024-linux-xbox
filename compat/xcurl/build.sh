#!/bin/bash
# Build XCurl.dll (macOS/Wine; see xcurl.c). Needs clang with the
# x86_64-pc-windows-msvc target and lld-link (llvm-mingw or Rust's gcc-ld).
#   CLANG=/usr/bin/clang LLD_LINK=/path/to/lld-link ./build.sh   ->  build/XCurl.dll
set -euo pipefail
cd "$(dirname "$0")"
CLANG=${CLANG:-clang}
LLD_LINK=${LLD_LINK:-lld-link}
mkdir -p build
"$LLD_LINK" /lib /machine:x64 /def:kernel32.def /out:build/kernel32.lib >/dev/null
"$CLANG" --target=x86_64-pc-windows-msvc -O2 -fno-stack-protector -fno-builtin -Wall -Wno-unused-function \
    -c xcurl.c -o build/xcurl.obj
"$LLD_LINK" /dll /brepro /nodefaultlib /entry:DllMain /machine:x64 /def:xcurl.def /out:build/XCurl.dll \
    build/xcurl.obj build/kernel32.lib
rm -f build/xcurl.obj build/kernel32.lib build/kernel32.exp build/XCurl.lib build/XCurl.exp
echo "built $(pwd)/build/XCurl.dll"
