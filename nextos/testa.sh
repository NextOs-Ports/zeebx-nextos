#!/bin/bash
# Testes da biblioteca no host, no diretório de build compartilhado e sob a mesma trava dos builds
# (vários agentes, disco apertado). Uso: nextos/testa.sh [filtro]
# A falha de `save_state_recria_texturas_e_a_segunda_unidade` já existe no upstream.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=/home/felipe/zeebx-nextos/target
# Toca as fontes: o diretório compartilhado não separa worktrees (ver build-core.sh).
exec flock /home/felipe/zeebx-nextos/.build.lock sh -c 'find src -type f -exec touch {} + ; exec cargo test --locked -p zeebx --lib "$@"' sh "$@"
