#!/bin/bash
# Monta a libdynarmic do core aarch64 com os remendos desta pasta (`*.patch`). Roda no PC,
# chamado pelo `build-core.sh`, com o CARGO_TARGET_DIR e o toolchain dele.
#
# Por que assim, e não um `[patch]` no Cargo.toml: o crate `dynarmic` 0.1.3 tem 76 MB (o C++
# com boost, zydis, catch...), grande demais para o repositório, e um `[patch]` por caminho muda
# o Cargo.lock e quebra o `--locked`. O crate declara `links = "dynarmic"`, e o Cargo deixa
# trocar a saída do build script por configuração (`target.<triplo>.dynarmic`): o `build-core.sh`
# passa as bibliotecas daqui e o resto do build não percebe.
#
# Por que só recompila o que o remendo toca: o disco do PC vive com menos de 2 GB livres, e um
# build inteiro do dynarmic são 300 MB de objetos. Aproveita o build original que o próprio Cargo
# já fez (o mesmo toolchain, os mesmos comandos, a mesma PCH), recompila só as unidades que
# incluem um arquivo remendado (a lista sai do `ninja -t deps`) e troca esses objetos numa cópia
# da `libdynarmic.a`. São 5 unidades e ~15 MB.
#
# Saída: imprime o diretório com libdynarmic.a, libfmt.a, libmcl.a e libwrapper.a.
set -euo pipefail
AQUI=$(cd "$(dirname "$0")" && pwd)
: "${CARGO_TARGET_DIR:?}"
REG=$(ls -d "$HOME"/.cargo/registry/src/*/dynarmic-0.1.3 | head -1)
HASH=$(cat "$AQUI"/*.patch "$0" | sha256sum | cut -c1-12)
OUT=$CARGO_TARGET_DIR/dynarmic-nextos/$HASH
if [ -f "$OUT/pronto" ]; then echo "$OUT"; exit 0; fi

# O build original: o do mesmo triplo neste CARGO_TARGET_DIR, com o toolchain de agora.
CXX=${CXX_aarch64_unknown_linux_gnu:?}
ORIG=""
for n in $(ls -dt "$CARGO_TARGET_DIR"/aarch64-unknown-linux-gnu/release/build/dynarmic-*/out/build/build.ninja 2>/dev/null); do
  if grep -qF "$CXX" "$(dirname "$n")/CMakeFiles/rules.ninja"; then ORIG=$(dirname "$n"); break; fi
done
if [ -z "$ORIG" ]; then
  echo "prepara.sh: falta o build original do dynarmic; rode uma vez ZEEBX_DYNARMIC_ORIGINAL=1 ./nextos/build-core.sh" >&2
  exit 1
fi
ORIG_OUT=$(dirname "$ORIG")

rm -rf "$OUT"; mkdir -p "$OUT/obj"
cp -a "$REG/dynarmic/src" "$OUT/src"
for p in "$AQUI"/*.patch; do patch -s -d "$OUT" -p1 < "$p"; done

# Arquivos remendados, e as unidades que dependem de algum deles.
mudados=$(cat "$AQUI"/*.patch | sed -n 's#^+++ b/src/\([^	]*\).*#\1#p')
alvos=$(cd "$ORIG" && ninja -t deps 2>/dev/null | awk -v m="$mudados" '
  BEGIN { n = split(m, v, "\n") }
  /^[^ ].*\.o: #deps/ { o = substr($1, 1, length($1) - 1); next }
  { for (i = 1; i <= n; i++) if (v[i] != "" && index($0, v[i])) print o }' | sort -u)
[ -n "$alvos" ] || { echo "prepara.sh: nenhuma unidade depende do remendo?" >&2; exit 1; }

cp "$ORIG/src/dynarmic/libdynarmic.a" "$OUT/libdynarmic.a"
objs=()
for o in $alvos; do
  cmd=$(cd "$ORIG" && ninja -t commands "$o" | tail -1)
  saida="$OUT/obj/$(basename "$o")"
  cmd=${cmd//"$REG/dynarmic/src/"/"$OUT/src/"}
  cmd=${cmd//"-MD -MT $o -MF $o.d"/}
  cmd=${cmd//"-o $o"/"-o $saida"}
  (cd "$ORIG" && eval "$cmd")
  objs+=("$saida")
done
AR=${AR_aarch64_unknown_linux_gnu:?}
"$AR" rs "$OUT/libdynarmic.a" "${objs[@]}"
cp "$ORIG_OUT"/lib/libfmt.a "$ORIG_OUT"/lib/libmcl.a "$ORIG_OUT"/libwrapper.a "$OUT/"
rm -rf "$OUT/src" "$OUT/obj"
touch "$OUT/pronto"
echo "$OUT"
