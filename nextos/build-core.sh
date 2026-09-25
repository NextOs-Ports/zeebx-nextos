#!/bin/bash
# Compila o core Libretro do Zeebx para o NextOS (Amlogic Mali-450, aarch64) com o toolchain e o
# sysroot do NextOS atual. Cross-build do PC x86_64: o `dynarmic` (C++20/CMake/Ninja) e o
# `r36s_compat.c` (cc) recebem o compilador do toolchain por variável de ambiente.
set -euo pipefail
NEXTOS_ROOT=${NEXTOS_ROOT:-/mnt/ARQUIVOS/NextOS-Elite-Edition}
TC=${NEXTOS_TOOLCHAIN:-$(find -H "$NEXTOS_ROOT" -maxdepth 2 -type d \
  -path '*/build.NextOS-Retro-Elite-Edition-Amlogic-old.aarch64-*/toolchain' -print |
  while read -r d; do [ -x "$d/bin/aarch64-libreelec-linux-gnu-gcc" ] && echo "$d"; done | sort -V | tail -1)}
TRIPLO=aarch64-libreelec-linux-gnu
SR=$TC/$TRIPLO/sysroot
[ -x "$TC/bin/$TRIPLO-gcc" ] || { echo "toolchain não encontrado: $TC" >&2; exit 1; }
cd "$(dirname "$0")/.."

T=aarch64-unknown-linux-gnu
TU=aarch64_unknown_linux_gnu
export CC_$TU="$TC/bin/$TRIPLO-gcc"
export CXX_$TU="$TC/bin/$TRIPLO-g++"
export AR_$TU="$TC/bin/$TRIPLO-ar"
export CFLAGS_$TU="--sysroot=$SR"
export CXXFLAGS_$TU="--sysroot=$SR"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$TC/bin/$TRIPLO-gcc"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$SR -C target-cpu=cortex-a53"
export CMAKE_GENERATOR=Ninja
export CARGO_PROFILE_RELEASE_DEBUG=0
TCFILE=$PWD/nextos/toolchain.cmake
cat > "$TCFILE" <<CM
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER $TC/bin/$TRIPLO-gcc)
set(CMAKE_CXX_COMPILER $TC/bin/$TRIPLO-g++)
set(CMAKE_SYSROOT $SR)
set(CMAKE_FIND_ROOT_PATH $SR)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
CM
export CMAKE_TOOLCHAIN_FILE_$TU="$TCFILE"

cargo build --release --locked --target $T -p zeebx-libretro "$@"
SO=target/$T/release/libzeebx_libretro.so
file "$SO" | cut -d, -f1-3
"$TC/bin/$TRIPLO-readelf" -d "$SO" | grep -E "NEEDED|RPATH|RUNPATH" || true
"$TC/bin/$TRIPLO-readelf" -V "$SO" | grep -oE 'GLIBC_[0-9.]+|GLIBCXX_[0-9.]+' | sort -Vu | tail -2
mkdir -p nextos/out && cp "$SO" nextos/out/zeebx_libretro.so && cp frontends/libretro/zeebx_libretro.info nextos/out/
sha256sum nextos/out/zeebx_libretro.so
