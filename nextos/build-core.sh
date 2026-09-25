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

# Vários worktrees (agentes) compartilham um diretório de build: o disco não comporta um por
# worktree. A trava serializa compilar + copiar, senão um agente copiaria o .so do outro.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/home/felipe/zeebx-nextos/target}
# O build de perfil muda as flags de compilação: em diretório próprio, senão invalidaria o outro.
[ -n "${ZEEBX_PERFIL:-}" ] && export CARGO_TARGET_DIR=/home/felipe/zeebx-nextos/target-perfil
if [ -z "${ZEEBX_TRAVA_BUILD:-}" ]; then
  export ZEEBX_TRAVA_BUILD=1
  exec flock /home/felipe/zeebx-nextos/.build.lock "$0" "$@"
fi

T=aarch64-unknown-linux-gnu
TU=aarch64_unknown_linux_gnu
export CC_$TU="$TC/bin/$TRIPLO-gcc"
export CXX_$TU="$TC/bin/$TRIPLO-g++"
export AR_$TU="$TC/bin/$TRIPLO-ar"
export CFLAGS_$TU="--sysroot=$SR"
export CXXFLAGS_$TU="--sysroot=$SR"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$TC/bin/$TRIPLO-gcc"
EXTRA_RUSTFLAGS=""
# ZEEBX_PERFIL=1: build de medição com ponteiros de quadro, para o amostrador subir a pilha.
[ -n "${ZEEBX_PERFIL:-}" ] && EXTRA_RUSTFLAGS="-C force-frame-pointers=yes"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$SR -C target-cpu=cortex-a53 $EXTRA_RUSTFLAGS"
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

# **O diretório compartilhado não separa worktrees.** O hash do crate não inclui o caminho do
# worktree e o dep-info guarda caminhos relativos: se outro agente compilou depois da última
# edição daqui, o cargo acha o rlib fresco e liga o código DELE neste core. Tocar as fontes
# deste worktree (dentro da trava) força a recompilação com elas. Aviso da frente api, 25/09.
find src frontends/libretro/src frontends/libretro/build.rs build.rs -type f -exec touch {} + 2>/dev/null || true
MARCA=$(mktemp); trap 'rm -f "$MARCA"' EXIT

# O dynarmic com os remendos de nextos/dynarmic/ (ver prepara.sh). ZEEBX_DYNARMIC_ORIGINAL=1 usa o
# crate como veio, para comparar.
LIGA_DYN=()
if [ -z "${ZEEBX_DYNARMIC_ORIGINAL:-}" ]; then
  DYN=$(./nextos/dynarmic/prepara.sh)
  LIGA_DYN=(--config "target.$T.dynarmic.rustc-link-search=[\"native=$DYN\"]"
            --config "target.$T.dynarmic.rustc-link-lib=[\"static=wrapper\",\"static=dynarmic\",\"static=fmt\",\"static=mcl\",\"stdc++\"]")
  echo "dynarmic remendado: $DYN"
fi

# ZEEBX_TESTA_AARCH64=1: em vez do core, roda os testes da biblioteca compilados para aarch64
# sob o qemu-aarch64 (o backend arm64 do dynarmic, o remendado, só existe lá; no PC os testes
# usam o backend x64). Argumentos extras viram filtro dos testes.
if [ -n "${ZEEBX_TESTA_AARCH64:-}" ]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="qemu-aarch64 -L $SR -E LD_LIBRARY_PATH=/usr/lib:/lib:$TC/$TRIPLO/lib64"
  exec cargo test --release --locked --target $T -p zeebx --lib --no-default-features --features gl,soundfont "${LIGA_DYN[@]}" -- "$@"
fi

cargo build --release --locked --target $T -p zeebx-libretro "${LIGA_DYN[@]}" "$@"
SO=$CARGO_TARGET_DIR/$T/release/libzeebx_libretro.so
[ "$SO" -nt "$MARCA" ] || { echo "ERRO: o .so não foi refeito por este build; nada copiado" >&2; exit 1; }
file "$SO" | cut -d, -f1-3
"$TC/bin/$TRIPLO-readelf" -d "$SO" | grep -E "NEEDED|RPATH|RUNPATH" || true
"$TC/bin/$TRIPLO-readelf" -V "$SO" | grep -oE 'GLIBC_[0-9.]+|GLIBCXX_[0-9.]+' | sort -Vu | tail -2
SAIDA=nextos/out/zeebx_libretro.so
[ -n "${ZEEBX_PERFIL:-}" ] && SAIDA=nextos/out/zeebx_libretro-perfil.so
mkdir -p nextos/out && cp "$SO" "$SAIDA" && cp frontends/libretro/zeebx_libretro.info nextos/out/
sha256sum "$SAIDA"
