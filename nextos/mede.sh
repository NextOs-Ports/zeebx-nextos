#!/bin/sh
# Mede jogos no aparelho com temperatura controlada. Roda NO APARELHO.
# Uso: [CORE=/storage/cores/x.so] [TAG=nome] mede.sh QUADROS jogo1 jogo2 ...   (jogos em /storage/roms/zeebo)
# Antes de cada jogo espera a CPU cair abaixo de 76 °C (teto de 3 min; o .30 parado fica em 74 °C), e registra a temperatura
# e o estado dos resfriadores no começo e no fim: acima de 85 °C o Amlogic desliga núcleo e
# derruba a GPU, e a medida deixa de valer.
Q=$1; shift
L=/storage/roms/zeebo/.log
CORE=${CORE:-/tmp/cores/zeebx_libretro.so}
TAG=${TAG:-m}
estado() { printf "%s°C resfr=%s" "$(($(cat /sys/class/thermal/thermal_zone0/temp)/1000))" "$(cat /sys/class/thermal/cooling_device*/cur_state | tr '\n' ',')"; }
for jogo in "$@"; do
  espera=0
  while [ "$(cat /sys/class/thermal/thermal_zone0/temp)" -gt 76000 ] && [ $espera -lt 180 ]; do sleep 5; espera=$((espera+5)); done
  antes=$(estado)
  ZEEBX_MEDE=1 timeout -s TERM 300 retroarch -v -L $CORE --max-frames=$Q \
    --max-frames-ss --max-frames-ss-path=$L/${TAG}_$jogo.png /storage/roms/zeebo/$jogo > $L/${TAG}_$jogo.log 2>&1
  st=$?
  fps=$(grep -o "Zeebx MEDE: [0-9.]* fps" $L/${TAG}_$jogo.log | tail -1 | grep -o "[0-9.]* fps")
  echo "$jogo | $fps | status $st | antes $antes | depois $(estado) | esperou ${espera}s"
done
