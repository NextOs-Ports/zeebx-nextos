#!/bin/sh
# Mede jogos no aparelho com temperatura controlada. Roda NO APARELHO.
# Uso: mede.sh QUADROS jogo1 jogo2 ...   (jogos em /storage/roms/zeebo)
# Antes de cada jogo espera a CPU cair abaixo de 72 °C (teto de 10 min), e registra a temperatura
# e o estado dos resfriadores no começo e no fim: acima de 85 °C o Amlogic desliga núcleo e
# derruba a GPU, e a medida deixa de valer.
Q=$1; shift
L=/storage/roms/zeebo/.log
estado() { printf "%s°C resfr=%s" "$(($(cat /sys/class/thermal/thermal_zone0/temp)/1000))" "$(cat /sys/class/thermal/cooling_device*/cur_state | tr '\n' ',')"; }
for jogo in "$@"; do
  espera=0
  while [ "$(cat /sys/class/thermal/thermal_zone0/temp)" -gt 72000 ] && [ $espera -lt 600 ]; do sleep 5; espera=$((espera+5)); done
  antes=$(estado)
  ZEEBX_MEDE=1 timeout -s TERM 300 retroarch -v -L /tmp/cores/zeebx_libretro.so --max-frames=$Q \
    --max-frames-ss --max-frames-ss-path=$L/m_$jogo.png /storage/roms/zeebo/$jogo > $L/m_$jogo.log 2>&1
  st=$?
  fps=$(grep -o "Zeebx MEDE: [0-9.]* fps" $L/m_$jogo.log | tail -1 | grep -o "[0-9.]* fps")
  echo "$jogo | $fps | status $st | antes $antes | depois $(estado) | esperou ${espera}s"
done
