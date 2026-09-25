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

## Medidas no Mali-450 (192.168.31.30), quadro 1800, CPU abaixo de 77 °C

`nextos/mede.sh` espera a CPU esfriar e registra os resfriadores: o .30 parado fica em 74 °C, e
durante o jogo a GPU já roda estrangulada. O ruído entre medidas é de uns 5%.

| Jogo | Upstream (software) | Agora | O que mais pesou |
|---|---|---|---|
| Resident Evil 4 (abertura 3D) | 27 fps (trava 1,5 s por música) | 42 fps | GLES2, MIDI em segundo plano |
| Caveman Ninja | 4,8 fps | 30,4 fps | GLES2, sem leitura de volta |
| Need for Speed Carbon | — | 24,7 fps | JIT por linha, constantes, VFP |
| Quake | — | 26,1 fps | superfície escalada, VFP |
| Galaxy on Fire | — | 37–39 fps | JIT, constantes |
| FIFA 09 (menu) | — | 22,5 fps | modo cópia nas superfícies |
| Zeebo Sports Tênis | — | 40,5 fps | VFP |
| Bejeweled (PopCap) | — | 58,1 fps | |
| Pac-Mania | preto | 8,4 fps | 2D aparecendo; sincronização sem varredura |

Os jogos 3D estão limitados pela CPU (A53 a 1,0 GHz). O Pac-Mania faz 2.800 chamadas de desenho
por quadro, e cada uma sai e volta do JIT: é o próximo alvo.

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
| `ZEEBX_SEM_VFP=1` | Não troca o float por software do RVCT por VFP |
| `ZEEBX_PERFIL=1` (build) | `build-core.sh` gera `zeebx_libretro-perfil.so` com ponteiros de quadro |

Para achar as funções quentes **do jogo**: `cargo build --release -p zeebx-classical-standalone
--features zeebx/perfil-guest` e `ZEEBX_PC_HIST=h.txt ./target/release/zeebx run jogo.7z
--frames=1800 --sem-rede` (sem janela, no interpretador).

## Pendências conhecidas

- Cores trocadas em jogos da PopCap: vem do upstream (igual no rasterizador por software).
- Controles não foram testados com o pad no aparelho.
- A falha do teste `save_state_recria_texturas_e_a_segunda_unidade` já existe no upstream.
