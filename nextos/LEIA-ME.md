# Zeebx no NextOS (Mali-450, OpenGL ES 2.0)

Fork do [Zeebx](https://github.com/ZeebxTeam/zeebx-emu) para o core Libretro rodar na placa dos
Amlogic com Mali-400/450, que só têm OpenGL ES 2.0. Branch: `mali450-gles2`.

## O que muda em relação ao upstream

| Mudança | Onde | Por quê |
|---|---|---|
| Detecta Mali Utgard e pede contexto GLES2 | `frontends/libretro/src/lib.rs` (`placa_so_gles2`) | O RetroArch aceita o pedido de GLES3 e depois não cria o EGL: o jogo nem abria |
| Perfil ES 2.0 no rasterizador de placa | `src/video/gpu.rs` | GLSL ES 1.00, sem VAO, sem MSAA/blit, depth+stencil em anexos separados |
| Shaders de fragmento por combinação de estado | `src/video/gpu.rs` (`Variante`) | Tira laços e uniformes inteiros do fragmento no Mali |
| Não devolve o estado de GL a cada desenho | `GpuState::devolve_o_contexto` | Cada troca de framebuffer num Mali por blocos custava ~5 ms por desenho |
| Não lê o quadro de volta com o framebuffer do frontend | `frame_rgb565`, `present_gl` | Lia o destino errado e esperava a placa: 41 ms por quadro |
| Retângulo real da superfície para o frontend | `retangulo_do_quadro_gl` | Quake e Galaxy on Fire apareciam num canto |
| JIT invalida por linha de 64 bytes | `src/cpu/dynarmic.rs` | Variável ao lado do código recompilava a página a cada quadro |
| JIT dobra constantes de páginas executadas | `is_readonly_memory` | Literal pools iam à callback: 16 mil por quadro no NFS |

## Medidas no Mali-450 (192.168.31.30), quadro 1800

| Jogo | Upstream (software) | Placa, primeira versão | Agora |
|---|---|---|---|
| Resident Evil 4 (título) | 41 fps | 6 fps | 60 fps |
| Caveman Ninja | 4,8 fps | 8,6 fps | 30,4 fps |
| Need for Speed Carbon | — | 16,1 fps | 21,6 fps |
| Quake | — | 19,2 fps | 23,6 fps |
| Galaxy on Fire | — | 8,6 fps | 36,9 fps |
| Bejeweled (PopCap) | — | 58,1 fps | 58,1 fps |

O "Placa, primeira versão" é o core só com o GLES2 ligado. Os jogos 3D pesados estão limitados
pela CPU (A53 a 1,0 GHz): quase metade do tempo é o código do jogo no JIT.

## Compilar

```sh
./nextos/build-core.sh        # usa o toolchain do NextOS Amlogic-old; sai em nextos/out/
```

A receita da ROM está em `NextOS-Elite-Edition`, `packages/sx05re/libretro/zeebx/package.mk`.

## Variáveis de ambiente de diagnóstico

| Variável | Efeito |
|---|---|
| `ZEEBX_GLES=2` / `3` | Força o perfil de contexto |
| `ZEEBX_MEDE=1` | Relatório a cada 120 quadros: fps, envio, leitura, vsync, JIT |
| `ZEEBX_MEDE=2` | O mesmo, com `glFinish` por desenho (custo real da placa) |
| `ZEEBX_PROF=1` | Amostrador de PC em `/tmp/zeebx-prof.txt`; `nextos/simboliza.py` simboliza |
| `ZEEBX_LEITURA=1` | Volta a ler o quadro de volta a cada swap |
| `ZEEBX_SEM_CONSTANTES=1` | Desliga a dobra de constantes do JIT |

## Pendências conhecidas

- Cores trocadas em jogos da PopCap: vem do upstream (igual no rasterizador por software).
- Controles não foram testados com o pad no aparelho.
- A falha do teste `save_state_recria_texturas_e_a_segunda_unidade` já existe no upstream.
