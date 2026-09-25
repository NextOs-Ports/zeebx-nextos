#!/bin/bash
# Histograma de PC do JOGO no interpretador, sem janela, no PC. Uso:
#   nextos/perfil-guest.sh jogo.7z QUADROS saida.txt [pasta-de-trabalho]
# A pasta de trabalho recebe os .bmp que o `zeebx run` grava; fica fora do repositório.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD
export CARGO_TARGET_DIR=/home/felipe/zeebx-nextos/target
flock /home/felipe/zeebx-nextos/.build.lock cargo build --release --locked -p zeebx-classical-standalone --features zeebx/perfil-guest >&2
BIN=$CARGO_TARGET_DIR/release/zeebx
cp "$BIN" "${4:-/tmp}/zeebx-perfil-guest.$$"
cd "${4:-/tmp}"
ZEEBX_PC_HIST="$3" timeout 1800 "./zeebx-perfil-guest.$$" run "$1" --frames="$2" --sem-rede > "$3.run.log" 2>&1 || true
rm -f "./zeebx-perfil-guest.$$"
echo "histograma: $3 ($(wc -l < "$3") PCs)"
