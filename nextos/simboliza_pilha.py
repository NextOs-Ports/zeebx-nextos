#!/usr/bin/env python3
"""Perfil com pilha de /tmp/zeebx-prof.txt (build ZEEBX_PERFIL=1): tempo próprio e inclusivo.

Uso: simboliza_pilha.py core.so amostras.txt [N] [filtro]
Cada linha: "módulo off;módulo off;..." (PC primeiro, depois retornos). Endereço de retorno
é simbolizado em off-1. Função só conta uma vez por amostra no inclusivo.
"""
import bisect, collections, subprocess, sys

def tabela(so, dinamica=False):
    args = ["nm", "-C", "--defined-only"] + (["-D"] if dinamica else []) + [so]
    syms = []
    for l in subprocess.run(args, capture_output=True, text=True).stdout.splitlines():
        p = l.split(" ", 2)
        if len(p) == 3 and p[1].lower() in "tw" and not p[2].startswith("$"):
            syms.append((int(p[0], 16), p[2]))
    syms.sort()
    return syms, [a for a, _ in syms]

so, arq = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 40
filtro = sys.argv[4] if len(sys.argv) > 4 else None
tabs = {"zeebx_libretro.so": tabela(so)}
for extra, nome in [("nextos/libc.so.6", "libc.so.6"), ("nextos/libMali.so", "libGLESv2.so")]:
    try:
        tabs[nome] = tabela(extra, True)
    except Exception:
        pass

def nome_de(mod, off, retorno):
    base = mod.rsplit("/", 1)[-1]
    if base == "JIT":
        return "[JIT]"
    t = tabs.get(base)
    if not t:
        return f"[{base}]"
    s, e = t
    i = bisect.bisect_right(e, off - (1 if retorno else 0)) - 1
    nome = s[i][1] if i >= 0 else "?"
    return (nome[:110] + ("" if base == "zeebx_libretro.so" else f" [{base}]"))

proprio = collections.Counter(); incl = collections.Counter(); total = 0
for linha in open(arq):
    partes = [p for p in linha.strip().split(";") if p]
    if not partes:
        continue
    pilha = []
    for k, p in enumerate(partes):
        mod, off = p.rsplit(" ", 1)
        pilha.append(nome_de(mod, int(off, 16), k > 0))
    if filtro and not any(filtro in f for f in pilha):
        continue
    total += 1
    proprio[pilha[0]] += 1
    for f in set(pilha):
        incl[f] += 1
print(f"amostras {total}")
print("--- inclusivo")
for f, c in incl.most_common(n):
    print(f"{100*c/total:5.1f}%  {f}")
print("--- próprio")
for f, c in proprio.most_common(n // 2):
    print(f"{100*c/total:5.1f}%  {f}")
