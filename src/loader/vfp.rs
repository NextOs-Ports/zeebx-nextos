//! Troca o ponto flutuante por software do compilador ARM (RVCT) por instruções VFP.
//!
//! **Por que existe.** Os jogos do Zeebo foram compilados sem VFP: cada soma ou multiplicação de
//! `float` é uma chamada a uma rotina de 20 a 60 instruções inteiras da biblioteca do RVCT. Medido
//! no Need for Speed Carbon, com o interpretador contando instruções por PC: essas rotinas eram
//! **mais da metade** de tudo que o jogo executava (multiplicação 27,5%, soma 17,8%). O Dynarmic
//! traduz VFP para o ponto flutuante nativo do host, então a mesma conta sai em uma instrução.
//!
//! A biblioteca é a mesma, byte a byte a menos dos deslocamentos de desvio, em 21 dos 62 jogos do
//! acervo (Quake, Quake 2, FIFA 09, NFS, Galaxy on Fire, a série Zeebo Sports...). Cada rotina é
//! achada pela assinatura das primeiras instruções, com o campo de deslocamento dos desvios
//! mascarado, e só é trocada quando aparece **uma** vez.
//!
//! **A troca ocupa uma palavra**: um `b` para o trampolim, que mora perto do módulo (a menos de
//! 32 MB, o alcance de um desvio ARM). O resto da rotina fica intacto, porque outras rotinas da
//! biblioteca saltam para o meio dela (a soma entra no corpo da subtração e vice-versa).
//!
//! **E as chamadas vão direto ao trampolim.** Todo `bl`/`b` do módulo cujo destino é a entrada
//! trocada passa a apontar para o trampolim. No Dynarmic um desvio com destino fixo vira salto
//! ligado entre blocos; a versão anterior entrava por `ldr pc, [pc, #-4]`, que o tradutor termina
//! com `FastDispatchHint`, que o backend arm64 ainda não implementa: cada conta voltava ao
//! despachante do Dynarmic (busca na tabela de blocos). O NFS faz ~33 mil contas de `float` por
//! quadro. Uma chamada que não dá para reescrever (Thumb, ponteiro de função) cai na entrada e
//! segue pelo `b`, com um bloco a mais. Medido no .30, quadro 1800, com as rotinas novas abaixo e
//! as comparações: NFS 24,6 → 26,1 fps, Quake 26,0 → 28,8, Galaxy on Fire 37,5 → 39,1; Tênis
//! 40,3 → 40,5 (no ruído).
//!
//! Diferenças de resultado: a biblioteca trata subnormais como zero, e o VFP com o FPSCR padrão
//! não. O resto (arredondamento ao par mais próximo, saturação na conversão para inteiro) é o
//! mesmo do IEEE que a biblioteca implementa.

use std::collections::HashSet;

/// Onde ficam os trampolins: uma região só de leitura e execução, fora de tudo que o jogo usa e a
/// menos de 32 MB do módulo principal (que começa em 0xf000 e tem no máximo uns 4 MB com a folga
/// da `.bss`), para caber num `b`/`bl`.
pub const VFP_BASE: u32 = 0x0100_0000;

/// Onde os trampolins moravam quando a entrada era trocada por `ldr pc, [pc, #-4]` + endereço.
/// O save state grava o módulo inteiro, com a troca antiga dentro: um estado salvo por aquela
/// versão salta para cá. [`acelera`] devolve os corpos no leiaute antigo para mapear aqui.
pub const VFP_BASE_ANTIGO: u32 = 0x3300_0000;

/// Alcance de `b`/`bl` ARM: deslocamento de 24 bits com sinal, em palavras.
const ALCANCE: i64 = 1 << 25;

/// `bx lr`.
const BX_LR: u32 = 0xe12f_ff1e;
/// `ldr pc, [pc, #-4]`: salta para a palavra seguinte, qualquer que seja a distância.
const LDR_PC: u32 = 0xe51f_f004;
const VMOV_S0_R0: u32 = 0xee00_0a10;
const VMOV_S1_R1: u32 = 0xee00_1a90;
const VMOV_R0_S0: u32 = 0xee10_0a10;

/// Uma rotina reconhecida: nome, assinatura (desvios mascarados) e o corpo VFP que a substitui.
struct Rotina {
    nome: &'static str,
    assinatura: &'static [u32],
    corpo: &'static [u32],
}

const ROTINAS: [Rotina; 9] = [
    Rotina {
        nome: "fadd",
        assinatura: &[
            0xe1300001, 0x42211102, 0x4a000000, 0xe0502001, 0x30400002, 0x30811002, 0xe1a02ba0,
            0xe3a0c4ff, 0xe11c0081, 0x113c0c02, 0xe0423ba1, 0x0a000000,
        ],
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xee30_0a20, VMOV_R0_S0, BX_LR], // vadd.f32 s0, s0, s1
    },
    Rotina {
        nome: "fsub",
        assinatura: &[
            0xe1300001, 0x42211102, 0x4a000000, 0xe0502001, 0x32222102, 0x30400002, 0x30811002,
            0xe1a02ba0, 0xe3a0c4ff, 0xe11c0081, 0x113c0c02, 0xe0423ba1,
        ],
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xee30_0a60, VMOV_R0_S0, BX_LR], // vsub.f32 s0, s0, s1
    },
    Rotina {
        nome: "fmul",
        assinatura: &[
            0xe3a0c8ff, 0xe01c23a0, 0x101c33a1, 0x1132000c, 0x1133000c, 0x0a000000, 0xe1300001,
            0xe3a0c102, 0x43822c01, 0xe18c0400, 0xe18c1401, 0xe0822003, 0xe083c190, 0xe2422502,
            0xe35c0000, 0x13833001,
        ],
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xee20_0a20, VMOV_R0_S0, BX_LR], // vmul.f32 s0, s0, s1
    },
    Rotina {
        nome: "fdiv",
        assinatura: &[
            0xe3a0c8ff, 0xe01c23a0, 0x101c33a1, 0x1132000c, 0x1133000c, 0x0a000000, 0xe1300001,
            0xe380c502, 0xe3810502, 0x43822c01, 0xe3cc14ff, 0xe3c004ff, 0xe92d4000, 0xe28fcf4d,
            0xe75ce8a0, 0xe1510000,
        ],
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xee80_0a20, VMOV_R0_S0, BX_LR], // vdiv.f32 s0, s0, s1
    },
    Rotina {
        nome: "fflt",
        assinatura: &[
            0xe2102102, 0x12600000, 0xe3822101, 0xe16f1f10, 0xe1b00110, 0xe0422b81, 0xe282253e,
            0x012fff1e, 0xe1b01c80, 0xe0a20440, 0x13b0c301, 0x23c00001,
        ],
        // vcvt.f32.s32 s0, s0
        corpo: &[VMOV_S0_R0, 0xeeb8_0ac0, VMOV_R0_S0, BX_LR],
    },
    Rotina {
        nome: "ffix",
        assinatura: &[
            0xe1b02bc0, 0xe1a03400, 0x13833102, 0x4a000000, 0xe272209e, 0x9a000000, 0xe1a00233,
            0xe1a0f00e, 0xe35004cf, 0x8a000000, 0xe3a00000, 0xe21220ff,
        ],
        // vcvt.s32.f32 s0, s0 (truncando, como a biblioteca)
        corpo: &[VMOV_S0_R0, 0xeebd_0ac0, VMOV_R0_S0, BX_LR],
    },
    // As três de baixo entraram depois; ficam no fim para não mudar o leiaute antigo dos
    // trampolins (ver `VFP_BASE_ANTIGO`).
    Rotina {
        // `float` para `unsigned`: negativo dá 0 e acima de 2^32 satura, truncando — igual ao
        // `vcvt.u32.f32` (NaN: a biblioteca chama o tratador de exceção; o VFP dá 0).
        // NFS: 60 mil chamadas por segundo de jogo, 17 jogos a têm.
        nome: "ffixu",
        assinatura: &[
            0xe1b02bc0, 0xe1a03400, 0x13833102, 0x4a000000, 0xe272209e, 0x3a000000, 0xe1a00233,
            0xe1a0f00e, 0xe1a01080, 0xe351047f, 0x2a000000, 0xe3a00000,
        ],
        corpo: &[VMOV_S0_R0, 0xeebc_0ac0, VMOV_R0_S0, BX_LR], // vcvt.u32.f32 s0, s0
    },
    Rotina {
        // Subtração invertida, `r1 - r0`: troca o sinal de `r0` e entra na soma ou na
        // subtração. Galaxy on Fire: 2,7% das instruções do jogo no menu; 20 jogos a têm.
        nome: "frsb",
        assinatura: &[0xe2200102, 0xe1300001, 0x5a000000, 0xe2211102, 0xea000000],
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xee30_0ac0, VMOV_R0_S0, BX_LR], // vsub.f32 s0, s1, s0
    },
    Rotina {
        // Raiz quadrada bit a bit: 24 voltas de laço, ~180 instruções por chamada. Negativo dá o
        // NaN padrão (0x7fc00000), como o `vsqrt`; ±0 volta ele mesmo. 17 jogos a têm.
        nome: "fsqrt",
        assinatura: &[
            0xe1a01ba0, 0xe21120ff, 0x135200ff, 0x0a000000, 0xe3100102, 0x1a000000, 0xe1a01ba0,
            0xe1a00400, 0xe3800102, 0xe281107d, 0xe3a02101, 0xe1b010a1,
        ],
        corpo: &[VMOV_S0_R0, 0xeeb1_0ac0, VMOV_R0_S0, BX_LR], // vsqrt.f32 s0, s0
    },
];

/// Comparações de `float` que devolvem o resultado nas flags, como um `cmp` (convenção do RVCT:
/// quem chama testa C e Z — `blo`, `bls`, `beq`...). O `vcmp` + `vmrs APSR_nzcv` dá as mesmas
/// flags: menor N=1 C=0, igual Z=1 C=1, maior C=1, NaN C=1 V=1 (a biblioteca chama o tratador de
/// exceção). Com operandos de sinais trocados a biblioteca deixa V diferente do VFP, mas só C e Z
/// fazem parte do contrato. Não mexem em r0–r3 nem no VFP além de s0/s1.
///
/// **Podem aparecer duas vezes**: a versão que sinaliza NaN silencioso e a que não sinaliza são
/// idênticas até o literal do código de exceção, e para o VFP são a mesma conta. Retornam com
/// `mov pc, lr`, que o Dynarmic termina voltando ao despachante (nem pilha de retorno): o Quake
/// faz 69 mil por segundo de jogo.
const COMPARACOES: [Rotina; 2] = [
    Rotina {
        nome: "fcmp",
        assinatura: &[
            0xe190c001, 0x4a000000, 0xe37c0502, 0x535c0502, 0x4a000000, 0xe1500001, 0xe1a0f00e,
            0x7a000000, 0xe3700502, 0x53710502, 0x4a000000, 0xe1500001, 0xe1a0f00e, 0xe1510001,
            0xe1a0f00e, 0xe37c0502, 0x5a000000, 0xe35c0502, 0x5a000000, 0xe1510000, 0xe1a0f00e,
        ],
        // vcmp.f32 s0, s1 ; vmrs APSR_nzcv, fpscr
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xeeb4_0a60, 0xeef1_fa10, BX_LR],
    },
    Rotina {
        // A invertida: as flags de `cmp(r1, r0)`.
        nome: "fcmpr",
        assinatura: &[
            0xe191c000, 0x4a000000, 0xe37c0502, 0x535c0502, 0x4a000000, 0xe1510000, 0xe1a0f00e,
            0x7a000000, 0xe3710502, 0x53700502, 0x4a000000, 0xe1510000, 0xe1a0f00e, 0xe1510001,
            0xe1a0f00e, 0xe37c0502, 0x5a000000, 0xe35c0502, 0x5a000000, 0xe1500001, 0xe1a0f00e,
        ],
        // vcmp.f32 s1, s0 ; vmrs APSR_nzcv, fpscr
        corpo: &[VMOV_S0_R0, VMOV_S1_R1, 0xeef4_0a40, 0xeef1_fa10, BX_LR],
    },
];

/// Quantas das [`ROTINAS`] existiam no leiaute antigo dos trampolins (`VFP_BASE_ANTIGO`).
const ROTINAS_ANTIGAS: usize = 6;

/// A conversão sem sinal mora logo depois da com sinal: `mov r2, #0x40000000` e um desvio de
/// volta para o corpo dela, 13 palavras depois da entrada da `fflt`.
const FFLTU_DESLOCAMENTO: u32 = 13 * 4;
const FFLTU_MOV: u32 = 0xe3a0_2101;
const FFLTU_CORPO: [u32; 4] = [VMOV_S0_R0, 0xeeb8_0a40, VMOV_R0_S0, BX_LR]; // vcvt.f32.u32

/// Uma troca feita, para o relatório.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Troca {
    pub nome: &'static str,
    pub entrada: u32,
    /// Quantos `bl`/`b` do módulo passaram a apontar direto para o trampolim.
    pub chamadas: u32,
}

/// Os trampolins: os corpos no leiaute novo (em [`VFP_BASE`]) e no antigo (em
/// [`VFP_BASE_ANTIGO`], só para estados salvos pela versão do `ldr pc`).
#[derive(Debug, Default)]
pub struct Trampolins {
    pub novos: Vec<u8>,
    pub antigos: Vec<u8>,
    /// Chamadas embutidas em trampolins de outras (ver `estende`), para o relatório.
    pub embutidas: u32,
}

/// `b` (sempre) de `pc` para `alvo`, se couber no alcance.
fn desvio(pc: u32, alvo: u32, cond_e_tipo: u32) -> Option<u32> {
    let delta = i64::from(alvo) - (i64::from(pc) + 8);
    (delta & 3 == 0 && (-ALCANCE..ALCANCE).contains(&delta))
        .then(|| cond_e_tipo | ((delta >> 2) as u32 & 0x00ff_ffff))
}

/// B/BL com condição: o deslocamento de 24 bits não entra na assinatura.
fn mascara(w: u32) -> u32 {
    if w & 0x0e00_0000 == 0x0a00_0000 && w >> 28 != 0xf {
        w & 0xff00_0000
    } else {
        w
    }
}

fn palavra(bytes: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
}

/// Todos os destinos de desvio que dá para calcular: B/BL/BLX ARM e BL/BLX Thumb.
fn destinos(bytes: &[u8], base: u32) -> HashSet<u32> {
    let mut alvos = HashSet::new();
    for i in (0..bytes.len().saturating_sub(3)).step_by(4) {
        let w = palavra(bytes, i);
        let pc = base.wrapping_add(i as u32);
        let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32; // ×4 com sinal
        if w & 0x0e00_0000 == 0x0a00_0000 {
            let extra = if w >> 28 == 0xf { (w >> 23) & 2 } else { 0 }; // BLX imm: bit H
            alvos.insert(pc.wrapping_add(8).wrapping_add(desloc).wrapping_add(extra));
        }
    }
    // Thumb: BL/BLX em duas metades, 11110 + 11111/11101.
    for i in (0..bytes.len().saturating_sub(3)).step_by(2) {
        let a = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let b = u16::from_le_bytes([bytes[i + 2], bytes[i + 3]]);
        if a & 0xf800 == 0xf000 && (b & 0xf800 == 0xf800 || b & 0xf800 == 0xe800) {
            let alto = ((u32::from(a & 0x7ff) << 21) as i32 >> 9) as u32;
            let baixo = u32::from(b & 0x7ff) << 1;
            let pc = base.wrapping_add(i as u32).wrapping_add(4);
            let alvo = pc.wrapping_add(alto).wrapping_add(baixo);
            alvos.insert(if b & 0xf800 == 0xe800 { alvo & !3 } else { alvo });
        }
    }
    alvos
}

/// Onde a assinatura aparece, em deslocamentos de byte (alinhados a 4).
fn procura(bytes: &[u8], assinatura: &[u32]) -> Vec<usize> {
    let n = assinatura.len() * 4;
    (0..bytes.len().saturating_sub(n))
        .step_by(4)
        .filter(|&i| {
            mascara(palavra(bytes, i)) == assinatura[0]
                && assinatura
                    .iter()
                    .enumerate()
                    .all(|(k, &w)| mascara(palavra(bytes, i + 4 * k)) == w)
        })
        .collect()
}

/// Troca as rotinas achadas em `modulo` (mapeado no guest em `base`). Devolve os trampolins, para
/// mapear em [`VFP_BASE`] (e [`VFP_BASE_ANTIGO`]), e o que foi trocado. Sem nada achado, tudo vem
/// vazio e o módulo fica como estava.
pub fn acelera(modulo: &mut [u8], base: u32) -> (Trampolins, Vec<Troca>) {
    let alvos = destinos(modulo, base);
    let mut novos: Vec<u32> = Vec::new();
    let mut antigos: Vec<u32> = Vec::new();
    let mut trocas: Vec<Troca> = Vec::new();
    // (entrada, trampolim, corpo) de cada troca feita, para redirecionar as chamadas no fim.
    let mut redireciona: Vec<(u32, u32, &'static [u32])> = Vec::new();
    // `antiga`: a versão do `ldr pc` também trocaria esta (ela exigia que ninguém saltasse para a
    // segunda palavra da entrada). Só serve para montar o leiaute antigo, byte a byte igual.
    let mut troca = |modulo: &mut [u8], nome: &'static str, off: usize, corpo: &'static [u32], antiga: bool| {
        let entrada = base.wrapping_add(off as u32);
        let destino = VFP_BASE + (novos.len() * 4) as u32;
        let segunda_livre = !alvos.contains(&entrada.wrapping_add(4));
        match desvio(entrada, destino, 0xea00_0000) {
            Some(b) => modulo[off..off + 4].copy_from_slice(&b.to_le_bytes()),
            // Longe demais para um `b` (módulo mapeado longe da região): a troca antiga, de duas
            // palavras, que exige a segunda livre.
            None if segunda_livre => {
                modulo[off..off + 4].copy_from_slice(&LDR_PC.to_le_bytes());
                modulo[off + 4..off + 8].copy_from_slice(&destino.to_le_bytes());
            }
            None => return false,
        }
        novos.extend_from_slice(corpo);
        let antiga = antiga && segunda_livre;
        if antiga {
            antigos.extend_from_slice(corpo);
        }
        redireciona.push((entrada, destino, corpo));
        trocas.push(Troca { nome, entrada, chamadas: 0 });
        antiga
    };
    for (i, rotina) in ROTINAS.iter().enumerate() {
        let achados = procura(modulo, rotina.assinatura);
        let [off] = achados[..] else { continue };
        // A sem sinal mora logo depois da com sinal; lida antes de trocar qualquer palavra.
        let u = off + FFLTU_DESLOCAMENTO as usize;
        let ffltu = rotina.nome == "fflt" && u + 8 <= modulo.len() && palavra(modulo, u) == FFLTU_MOV && {
            let w = palavra(modulo, u + 4);
            let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
            let alvo = base.wrapping_add(u as u32 + 4).wrapping_add(8).wrapping_add(desloc);
            w & 0x0f00_0000 == 0x0a00_0000 && alvo == base.wrapping_add(off as u32 + 12)
        };
        let antiga = troca(modulo, rotina.nome, off, rotina.corpo, i < ROTINAS_ANTIGAS);
        if ffltu {
            // A versão antiga só olhava a `ffltu` quando a `fflt` tinha sido trocada.
            troca(modulo, "ffltu", u, &FFLTU_CORPO, antiga);
        }
    }
    for rotina in &COMPARACOES {
        let achados = procura(modulo, rotina.assinatura);
        if (1..=2).contains(&achados.len()) {
            for off in achados {
                troca(modulo, rotina.nome, off, rotina.corpo, false);
            }
        }
    }
    // As chamadas: todo `b`/`bl` ARM (condicional ou não) com destino numa entrada trocada. Um
    // literal que por acaso tivesse a forma exata de um `bl` para uma dessas entradas seria
    // reescrito também; a chance é de 1 em 2^24 por palavra com esse primeiro byte, e o mesmo
    // risco já é aceito na procura de destinos acima.
    //
    // **Cada `bl` ganha um trampolim só dele**, que volta com `b` fixo para a instrução seguinte
    // em vez de `bx lr`. No backend arm64 do Dynarmic o `bx lr` desempilha a pilha de retorno e
    // salta por registrador — um único salto indireto para todos os chamadores, que o A53 erra
    // quase sempre — e o `bl` empilha. Com `b` de ida e de volta os dois viram salto ligado. O
    // `lr` deixa de ser escrito: depois de um `bl` ele é lixo para o compilador (a chamada o
    // destruiu), então ninguém o lê esperando o endereço de volta. Um `b` (salto de cauda)
    // continua no trampolim comum, que volta pelo `lr` de quem chamou.
    let mapa: rustc_hash::FxHashMap<u32, (u32, &'static [u32], usize)> =
        redireciona.iter().enumerate().map(|(k, &(e, d, c))| (e, (d, c, k))).collect();
    // O código como estava antes de reescrever as chamadas, para copiar os trechos.
    let original = modulo.to_vec();
    let mut embutidas_total = 0u32;
    for i in (0..modulo.len().saturating_sub(3)).step_by(4) {
        let w = palavra(modulo, i);
        if w & 0x0e00_0000 != 0x0a00_0000 || w >> 28 == 0xf {
            continue;
        }
        let pc = base.wrapping_add(i as u32);
        let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
        let Some(&(comum, corpo, k)) = mapa.get(&pc.wrapping_add(8).wrapping_add(desloc)) else {
            continue;
        };
        let proprio = VFP_BASE + (novos.len() * 4) as u32;
        let so_dele = w & 0x0100_0000 != 0 && corpo.last() == Some(&BX_LR);
        let novo = match so_dele.then(|| desvio(pc, proprio, (w & 0xf000_0000) | 0x0a00_0000)).flatten() {
            Some(ida) => {
                // A conta, e o trecho reto que vem depois da chamada copiado junto (ver
                // `estende`), para o Dynarmic traduzir tudo num bloco só.
                let mut seq = corpo[..corpo.len() - 1].to_vec();
                let (seguinte, embutidas) = if w >> 28 == 0xe {
                    estende(&original, base, i + 4, &mapa, &mut seq)
                } else {
                    (pc + 4, 0) // chamada condicional: só a conta
                };
                let b_volta = desvio(proprio + (seq.len() * 4) as u32, seguinte, 0xea00_0000);
                match b_volta {
                    Some(b) => {
                        seq.push(b);
                        novos.extend_from_slice(&seq);
                        embutidas_total += embutidas;
                        Some(ida)
                    }
                    None => desvio(pc, comum, w & 0xff00_0000),
                }
            }
            None => desvio(pc, comum, w & 0xff00_0000),
        };
        if let Some(novo) = novo {
            modulo[i..i + 4].copy_from_slice(&novo.to_le_bytes());
            trocas[k].chamadas += 1;
        }
    }
    let bytes = |v: &[u32]| v.iter().flat_map(|w| w.to_le_bytes()).collect();
    (Trampolins { novos: bytes(&novos), antigos: bytes(&antigos), embutidas: embutidas_total }, trocas)
}

/// Quantas instruções do jogo, no máximo, um trampolim copia depois da chamada.
const TRECHO_MAXIMO: usize = 48;

/// Copia para `seq` o trecho reto que segue a chamada em `off`, até a primeira instrução que não
/// pode mudar de endereço, e embute no lugar as chamadas (`bl` incondicional) a outras rotinas
/// trocadas. Devolve o endereço da primeira instrução não copiada (para onde o trampolim volta)
/// e quantas chamadas foram embutidas.
///
/// **Por quê.** Cada conta de `float` por trampolim custa dois blocos a mais no Dynarmic — o do
/// trampolim e o que recomeça depois da chamada —, e em cada um o JIT recarrega do estado os
/// registradores do guest que usa e grava de volta os que sujou. O laço de partículas do Quake faz
/// ~50 contas em linha reta por partícula (`fsub`, `fmul`, `fadd`, `ffix` intercalados com
/// `ldr`/`str`): copiado, vira um bloco só, e os valores ficam nos registradores do host.
///
/// O original fica intacto: a cópia só é alcançada pelo `bl` reescrito. Por isso a cópia não
/// pode ler o PC (literal, `add rX, pc`), desviar, nem escrever o PC; ao achar uma dessas, pára
/// ali e volta ao original. O `lr` não é escrito pelas chamadas embutidas: depois de um `bl` o
/// compilador o trata como destruído.
fn estende(
    original: &[u8],
    base: u32,
    mut off: usize,
    mapa: &rustc_hash::FxHashMap<u32, (u32, &'static [u32], usize)>,
    seq: &mut Vec<u32>,
) -> (u32, u32) {
    let mut embutidas = 0;
    for _ in 0..TRECHO_MAXIMO {
        if off + 4 > original.len() {
            break;
        }
        let w = palavra(original, off);
        let pc = base.wrapping_add(off as u32);
        if w >> 24 == 0xeb {
            let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
            match mapa.get(&pc.wrapping_add(8).wrapping_add(desloc)) {
                Some(&(_, corpo, _)) if corpo.last() == Some(&BX_LR) => {
                    seq.extend_from_slice(&corpo[..corpo.len() - 1]);
                    embutidas += 1;
                }
                _ => break,
            }
        } else if copiavel(w) {
            seq.push(w);
        } else {
            break;
        }
        off += 4;
    }
    (base.wrapping_add(off as u32), embutidas)
}

/// Uma instrução ARM que faz a mesma coisa em qualquer endereço: sem ler nem escrever o PC, sem
/// desviar, sem coprocessador nem chamada de sistema. Conservadora: um campo de registrador com
/// valor 15 recusa a instrução mesmo quando, naquela codificação, o campo é parte de um imediato.
fn copiavel(w: u32) -> bool {
    let cond = w >> 28;
    if cond == 0xf {
        return false;
    }
    let pc_em = |bit: u32| (w >> bit) & 0xf == 0xf;
    match (w >> 25) & 7 {
        // Processamento de dados, multiplicação, transferências de meia palavra/dupla e o grupo
        // "misc" (MRS/MSR/BX/BLX/CLZ...), que fica de fora inteiro.
        0b000 | 0b001 => {
            let misc = w & 0x0190_0000 == 0x0100_0000 && (w >> 25) & 1 == 0 && w & 0x90 != 0x90;
            let msr_imediato = w & 0x0fb0_0000 == 0x0320_0000;
            !misc && !msr_imediato && !pc_em(16) && !pc_em(12) && !pc_em(0) && !(w & 0x0200_0010 == 0x10 && pc_em(8))
        }
        // As extensões do ARMv6 (uxth, sxtb, ...): Rn = 15 quer dizer "sem somar", não PC.
        0b011 if w & 0x0f80_03f0 == 0x0680_0070 => !pc_em(12) && !pc_em(0),
        // LDR/STR/LDRB/STRB e, com o bit 4, o resto das de mídia do ARMv6.
        0b010 | 0b011 => !pc_em(16) && !pc_em(12) && !((w >> 25) & 1 == 1 && pc_em(0)),
        // LDM/STM sem o PC na lista nem como base.
        0b100 => !pc_em(16) && w & 0x8000 == 0,
        _ => false,
    }
}

/// As assinaturas, na ordem de [`ROTINAS`] e depois [`COMPARACOES`], para testes de outros
/// módulos.
#[cfg(test)]
pub fn assinaturas_para_teste() -> Vec<Vec<u32>> {
    ROTINAS.iter().chain(&COMPARACOES).map(|r| r.assinatura.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modulo_com(rotinas: &[(&[u32], usize)], tamanho: usize) -> Vec<u8> {
        let mut m = vec![0u8; tamanho];
        for (assinatura, off) in rotinas {
            for (k, w) in assinatura.iter().enumerate() {
                m[off + 4 * k..off + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
            }
        }
        m
    }

    /// Destino de um `b`/`bl` em `off` de um módulo mapeado em `base`.
    fn destino_em(m: &[u8], base: u32, off: usize) -> u32 {
        let w = palavra(m, off);
        let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
        base.wrapping_add(off as u32).wrapping_add(8).wrapping_add(desloc)
    }

    #[test]
    fn acha_e_troca_as_rotinas_uma_vez_cada() {
        let mut m = modulo_com(&[(ROTINAS[0].assinatura, 0x100), (ROTINAS[2].assinatura, 0x200)], 0x400);
        let (tramp, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(
            trocas,
            vec![
                Troca { nome: "fadd", entrada: 0x1_0100, chamadas: 0 },
                Troca { nome: "fmul", entrada: 0x1_0200, chamadas: 0 }
            ]
        );
        // Uma palavra só: `b` para o trampolim; a segunda fica como estava.
        assert_eq!(palavra(&m, 0x100) & 0xff00_0000, 0xea00_0000);
        assert_eq!(destino_em(&m, 0x1_0000, 0x100), VFP_BASE);
        assert_eq!(destino_em(&m, 0x1_0000, 0x200), VFP_BASE + 20);
        assert_eq!(palavra(&m, 0x104), ROTINAS[0].assinatura[1]);
        assert_eq!(tramp.novos.len(), 40);
        assert_eq!(tramp.antigos, tramp.novos);
    }

    #[test]
    fn chamadas_apontam_direto_para_o_trampolim() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x200)], 0x400);
        // `bl` em 0x000 e `bne` em 0x010 para 0x200; `bl` em 0x020 para outro lugar.
        let bl = |de: u32, para: u32, cond: u32| cond | (((para - de - 8) / 4) & 0x00ff_ffff);
        m[0..4].copy_from_slice(&bl(0, 0x200, 0xeb00_0000).to_le_bytes());
        m[0x10..0x14].copy_from_slice(&bl(0x10, 0x200, 0x1a00_0000).to_le_bytes());
        m[0x20..0x24].copy_from_slice(&bl(0x20, 0x300, 0xeb00_0000).to_le_bytes());
        m[4..8].copy_from_slice(&BX_LR.to_le_bytes()); // não copiável: o trecho pára aqui
        let (tramp, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(trocas[0].chamadas, 2);
        // O `bl` vira `b` para um trampolim só dele, depois do comum (5 palavras)...
        assert_eq!(palavra(&m, 0) >> 24, 0xea, "bl vira b");
        assert_eq!(destino_em(&m, 0x1_0000, 0), VFP_BASE + 20);
        // ...que faz a conta e volta com `b` para a instrução seguinte à chamada.
        let t = &tramp.novos[20..];
        assert_eq!(t.len(), 20);
        assert_eq!(palavra(t, 8), ROTINAS[2].corpo[2], "a conta");
        assert_eq!(destino_em(t, VFP_BASE + 20, 16), 0x1_0004, "volta depois do bl");
        // O `bne` é salto, não chamada: vai ao trampolim comum, que volta pelo `lr`.
        assert_eq!(destino_em(&m, 0x1_0000, 0x10), VFP_BASE);
        assert_eq!(palavra(&m, 0x10) >> 24, 0x1a, "continua bne");
        assert_eq!(destino_em(&m, 0x1_0000, 0x20), 0x1_0300, "a outra chamada fica");
    }

    #[test]
    fn trecho_reto_depois_da_chamada_vai_junto_e_embute_a_proxima_conta() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x200)], 0x400);
        let bl = |de: u32, para: u32| 0xeb00_0000 | (((para - de - 8) / 4) & 0x00ff_ffff);
        let codigo = [
            bl(0, 0x200),
            0xe594_1000, // ldr r1, [r4]
            bl(8, 0x200),
            0xe585_0000, // str r0, [r5]
            0xe59f_0008, // ldr r0, [pc, #8]: lê o PC, a cópia pára aqui
        ];
        for (k, w) in codigo.iter().enumerate() {
            m[4 * k..4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
        let (tramp, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(tramp.embutidas, 1);
        assert_eq!(trocas[0].chamadas, 2, "as duas chamadas reescritas");
        // O trampolim da primeira: conta, ldr, conta, str, e volta ao `ldr r0, [pc]`.
        let t = &tramp.novos[20..];
        let fmul = &ROTINAS[2].corpo[..4];
        let esperado: Vec<u32> =
            fmul.iter().chain(&[0xe594_1000]).chain(fmul).chain(&[0xe585_0000]).copied().collect();
        for (k, w) in esperado.iter().enumerate() {
            assert_eq!(palavra(t, 4 * k), *w, "palavra {k}");
        }
        assert_eq!(destino_em(t, VFP_BASE + 20, 4 * esperado.len()), 0x1_0010);
    }

    #[test]
    fn copiavel_recusa_o_que_depende_do_endereco() {
        for (w, ok, o_que) in [
            (0xe28f_0004, false, "add r0, pc, #4"),
            (0xe1a0_f00e, false, "mov pc, lr"),
            (0xe59f_0000, false, "ldr r0, [pc]"),
            (0xe92d_4010, true, "push {r4, lr}"),
            (0xe8bd_8010, false, "pop {r4, pc}"),
            (0xe6ff_2072, true, "uxth r2, r2"),
            (0xe12f_ff1e, false, "bx lr"),
            (0xe000_0291, true, "mul r0, r1, r2"),
            (0xe350_0000, true, "cmp r0, #0"),
            (0x13a0_1001, true, "movne r1, #1"),
            (0xe328_f000, false, "msr cpsr_f, #0"),
            (0xef00_0000, false, "svc 0"),
            (0xee30_0a20, false, "vadd.f32"),
            (0xea00_0000, false, "b"),
            (0xe1d0_00b2, true, "ldrh r0, [r0, #2]"),
            (0xe10f_0000, false, "mrs r0, cpsr"),
        ] {
            assert_eq!(copiavel(w), ok, "{o_que}");
        }
    }

    #[test]
    fn quem_salta_para_a_segunda_palavra_nao_impede_a_troca_mas_sai_do_leiaute_antigo() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x200)], 0x400);
        // `b` em 0x000 para 0x204: deslocamento (0x204 - 8) / 4.
        let b: u32 = 0xea00_0000 | ((0x204 - 8) / 4);
        m[0..4].copy_from_slice(&b.to_le_bytes());
        let (tramp, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(trocas.len(), 1);
        assert_eq!(palavra(&m, 0x204), ROTINAS[2].assinatura[1], "a segunda palavra é código vivo");
        assert_eq!(destino_em(&m, 0x1_0000, 0), 0x1_0204, "o salto para o meio fica");
        assert!(tramp.antigos.is_empty(), "a versão do `ldr pc` não trocava esta");
    }

    #[test]
    fn longe_demais_para_um_b_usa_a_troca_de_duas_palavras() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x200)], 0x400);
        let base = 0x0800_0000; // onde moram os módulos de extensão: 112 MB da região
        let (_, trocas) = acelera(&mut m, base);
        assert_eq!(trocas.len(), 1);
        assert_eq!(palavra(&m, 0x200), LDR_PC);
        assert_eq!(palavra(&m, 0x204), VFP_BASE);
    }

    #[test]
    fn comparacao_em_duas_copias_troca_as_duas() {
        let mut m = modulo_com(&[(COMPARACOES[0].assinatura, 0x100), (COMPARACOES[0].assinatura, 0x300)], 0x500);
        let (_, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(trocas.len(), 2);
        assert!(trocas.iter().all(|t| t.nome == "fcmp"));
    }

    #[test]
    fn duas_copias_nao_trocam_nenhuma() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x100), (ROTINAS[2].assinatura, 0x300)], 0x500);
        let (_, trocas) = acelera(&mut m, 0x1_0000);
        assert!(trocas.is_empty());
    }
}
