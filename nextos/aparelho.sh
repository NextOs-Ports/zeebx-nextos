#!/bin/bash
# Mede um core no .30 em FILA: só um RetroArch roda por vez no aparelho, e vários agentes
# disputam o mesmo. Roda NO PC.
#
# Uso: nextos/aparelho.sh FRENTE CORE.so QUADROS jogo1.7z [jogo2 ...]
#   FRENTE  nome curto do agente/frente (vira o nome do core e o prefixo dos logs no aparelho)
# Saída: uma linha por jogo (fps, temperatura, resfriadores). Logs e screenshots ficam no
# aparelho em /storage/roms/zeebo/.log/<FRENTE>_<jogo>.{log,png}; para trazer:
#   scp root@192.168.31.30:/storage/roms/zeebo/.log/<FRENTE>_<jogo>.png .
# Variáveis de ambiente ZEEBX_* passadas em EXTRA_ENV (ex.: EXTRA_ENV="ZEEBX_PROF=1").
#
# Regras do aparelho (inegociáveis): nunca emuelecRunEmu.sh, nunca systemctl start/unmask
# emustation, nunca mexer em /sys/class/display nem free_scale, nunca ler /dev/fb0, nunca
# `paste` no aparelho, nunca apagar por glob fora de /storage/roms/zeebo/.log.
set -euo pipefail
FRENTE=$1; SO=$2; Q=$3; shift 3
[ -f "$SO" ] || { echo "core não existe: $SO" >&2; exit 1; }
DEST=/storage/cores/zeebx_${FRENTE}_libretro.so
exec 9>/home/felipe/zeebx-nextos/.aparelho.lock
echo "[$FRENTE] esperando o aparelho..." >&2
flock 9
echo "[$FRENTE] aparelho livre" >&2
scp -q "$SO" "root@192.168.31.30:$DEST"
scp -q "$(dirname "$0")/mede.sh" root@192.168.31.30:/storage/roms/zeebo/.log/mede.sh
# Segunda trava, NO APARELHO: matar este script no PC libera a trava daqui, mas o RetroArch
# remoto continua vivo; com a trava de lá, a próxima medida espera ele terminar (o mede.sh tem
# timeout por jogo). Aviso da frente guest, 25/09.
ssh root@192.168.31.30 "cd /storage/roms/zeebo/.log; flock /storage/roms/zeebo/.log/aparelho.lock env ${EXTRA_ENV:-} CORE=$DEST TAG=$FRENTE sh mede.sh $Q $*"
