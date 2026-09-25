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
//! **A troca ocupa duas palavras** na entrada: `ldr pc, [pc, #-4]` e o endereço do trampolim. O
//! resto da rotina fica intacto, porque outras rotinas da biblioteca saltam para o meio dela (a
//! soma entra no corpo da subtração e vice-versa). Por isso uma entrada só é trocada se nenhum
//! desvio do módulo — ARM ou Thumb — aponta para a segunda palavra dela.
//!
//! Diferenças de resultado: a biblioteca trata subnormais como zero, e o VFP com o FPSCR padrão
//! não. O resto (arredondamento ao par mais próximo, saturação na conversão para inteiro) é o
//! mesmo do IEEE que a biblioteca implementa.

use std::collections::HashSet;

/// Onde ficam os trampolins: uma região só de leitura e execução, fora de tudo que o jogo usa.
pub const VFP_BASE: u32 = 0x3300_0000;

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

const ROTINAS: [Rotina; 6] = [
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
];

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

/// Troca as rotinas achadas em `modulo` (mapeado no guest em `base`). Devolve os bytes da região
/// de trampolins, para mapear em [`VFP_BASE`], e o que foi trocado. Sem nada achado, os dois vêm
/// vazios e o módulo fica como estava.
pub fn acelera(modulo: &mut [u8], base: u32) -> (Vec<u8>, Vec<Troca>) {
    let alvos = destinos(modulo, base);
    let mut trampolins: Vec<u32> = Vec::new();
    let mut trocas = Vec::new();
    let mut troca = |modulo: &mut [u8], nome: &'static str, off: usize, corpo: &[u32]| {
        let entrada = base.wrapping_add(off as u32);
        if alvos.contains(&entrada.wrapping_add(4)) {
            return false;
        }
        let destino = VFP_BASE + (trampolins.len() * 4) as u32;
        trampolins.extend_from_slice(corpo);
        modulo[off..off + 4].copy_from_slice(&LDR_PC.to_le_bytes());
        modulo[off + 4..off + 8].copy_from_slice(&destino.to_le_bytes());
        trocas.push(Troca { nome, entrada });
        true
    };
    for rotina in &ROTINAS {
        let achados = procura(modulo, rotina.assinatura);
        let [off] = achados[..] else { continue };
        if !troca(modulo, rotina.nome, off, rotina.corpo) || rotina.nome != "fflt" {
            continue;
        }
        // A sem sinal, logo depois da com sinal.
        let u = off + FFLTU_DESLOCAMENTO as usize;
        if u + 8 <= modulo.len() && palavra(modulo, u) == FFLTU_MOV {
            let w = palavra(modulo, u + 4);
            let desloc = (((w & 0x00ff_ffff) << 8) as i32 >> 6) as u32;
            let alvo = base
                .wrapping_add(u as u32 + 4)
                .wrapping_add(8)
                .wrapping_add(desloc);
            if w & 0x0f00_0000 == 0x0a00_0000 && alvo == base.wrapping_add(off as u32 + 12) {
                troca(modulo, "ffltu", u, &FFLTU_CORPO);
            }
        }
    }
    let bytes = trampolins.iter().flat_map(|w| w.to_le_bytes()).collect();
    (bytes, trocas)
}

/// As assinaturas, na ordem de [`ROTINAS`], para testes de outros módulos.
#[cfg(test)]
pub fn assinaturas_para_teste() -> Vec<Vec<u32>> {
    ROTINAS.iter().map(|r| r.assinatura.to_vec()).collect()
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

    #[test]
    fn acha_e_troca_as_rotinas_uma_vez_cada() {
        let mut m = modulo_com(&[(ROTINAS[0].assinatura, 0x100), (ROTINAS[2].assinatura, 0x200)], 0x400);
        let (tramp, trocas) = acelera(&mut m, 0x1_0000);
        assert_eq!(
            trocas,
            vec![Troca { nome: "fadd", entrada: 0x1_0100 }, Troca { nome: "fmul", entrada: 0x1_0200 }]
        );
        assert_eq!(palavra(&m, 0x100), LDR_PC);
        assert_eq!(palavra(&m, 0x104), VFP_BASE);
        assert_eq!(palavra(&m, 0x204), VFP_BASE + 20);
        assert_eq!(tramp.len(), 40);
    }

    #[test]
    fn nao_troca_quando_alguem_salta_para_a_segunda_palavra() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x200)], 0x400);
        // `b` em 0x000 para 0x204: deslocamento (0x204 - 8) / 4.
        let b: u32 = 0xea00_0000 | ((0x204 - 8) / 4);
        m[0..4].copy_from_slice(&b.to_le_bytes());
        let (_, trocas) = acelera(&mut m, 0x1_0000);
        assert!(trocas.is_empty());
        assert_eq!(palavra(&m, 0x200), ROTINAS[2].assinatura[0]);
    }

    #[test]
    fn duas_copias_nao_trocam_nenhuma() {
        let mut m = modulo_com(&[(ROTINAS[2].assinatura, 0x100), (ROTINAS[2].assinatura, 0x300)], 0x500);
        let (_, trocas) = acelera(&mut m, 0x1_0000);
        assert!(trocas.is_empty());
    }
}
