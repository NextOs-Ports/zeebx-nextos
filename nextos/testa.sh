#!/bin/bash
# Testes da biblioteca no host, no diretório de build compartilhado e sob a mesma trava dos builds
# (vários agentes, disco apertado). Uso: nextos/testa.sh [filtro]
# A falha de `save_state_recria_texturas_e_a_segunda_unidade` já existe no upstream.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=/home/felipe/zeebx-nextos/target
exec flock /home/felipe/zeebx-nextos/.build.lock cargo test --locked -p zeebx --lib "$@"
