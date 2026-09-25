#!/usr/bin/env python3
"""Simboliza as amostras de /tmp/zeebx-prof.txt contra o .so local (nm -C)."""
import bisect, collections, subprocess, sys
so, amostras = sys.argv[1], sys.argv[2]
syms = []
for linha in subprocess.run(["nm", "-C", "--defined-only", so], capture_output=True, text=True).stdout.splitlines():
    partes = linha.split(" ", 2)
    if len(partes) == 3 and partes[1].lower() in "tw" and not partes[2].startswith("$"):
        syms.append((int(partes[0], 16), partes[2]))
syms.sort()
ends = [a for a, _ in syms]
mod = collections.Counter(); fn = collections.Counter(); total = 0
for linha in open(amostras):
    nome, off = linha.split()
    total += 1
    mod[nome.rsplit("/", 1)[-1]] += 1
    if nome.endswith("zeebx_libretro.so"):
        i = bisect.bisect_right(ends, int(off, 16)) - 1
        s = syms[i][1] if i >= 0 else "?"
        for corta in ("::h", "{{closure}}"):
            pass
        fn[s[:150]] += 1
print(f"total {total}")
for m, c in mod.most_common(8):
    print(f"{100*c/total:5.1f}%  {m}")
print("--- funções do core")
for f, c in fn.most_common(int(sys.argv[3]) if len(sys.argv) > 3 else 40):
    print(f"{100*c/total:5.1f}%  {f}")
