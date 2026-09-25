//! Medição do áudio que o core entrega ao frontend, ligada por variável de ambiente.
//!
//! O áudio não se ouve numa bancada automática: aqui ele vira número. `ZEEBX_AUDIO_MEDE=1` põe no
//! log, a cada 120 quadros, quanto som foi entregue contra o tempo real que passou, e o que o
//! frontend disse do buffer dele. `ZEEBX_AUDIO_DUMP=<base>` grava, além disso, `<base>.pcm` (s16le
//! estéreo a 44,1 kHz, exatamente os bytes aceitos pelo frontend) e `<base>.csv` (uma linha por
//! `retro_run`), para analisar no PC.
//!
//! Desligado, custa uma leitura de `OnceLock` por quadro.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Ocupação do buffer do frontend, em porcentagem, como o último aviso trouxe.
pub(crate) static OCUPACAO: AtomicU32 = AtomicU32::new(u32::MAX);
/// O último aviso do frontend dizia que o buffer está para secar.
pub(crate) static SECANDO: AtomicBool = AtomicBool::new(false);

/// Salto entre amostras vizinhas acima do qual contamos um estalo: um quarto da escala cheia.
///
/// Música e efeitos do Zeebo são de 8 a 22 kHz reamostrados para 44,1: o maior passo natural
/// medido nos WAV do RE4 gerados no PC fica bem abaixo disso, então um salto assim é degrau.
const SALTO_ESTALO: i32 = i16::MAX as i32 / 4;

struct Janela {
    quadros: u32,
    virtual_ms: u64,
    devidas: u64,
    oferecidas: u64,
    aceitas: u64,
    descartadas: u64,
    estalos: u64,
    secando: u32,
    ocupacao_min: u32,
    ocupacao_soma: u64,
    ocupacao_n: u32,
    inicio: Instant,
}

impl Janela {
    fn nova() -> Self {
        Self {
            quadros: 0,
            virtual_ms: 0,
            devidas: 0,
            oferecidas: 0,
            aceitas: 0,
            descartadas: 0,
            estalos: 0,
            secando: 0,
            ocupacao_min: u32::MAX,
            ocupacao_soma: 0,
            ocupacao_n: 0,
            inicio: Instant::now(),
        }
    }
}

struct Medidor {
    pcm: Option<BufWriter<File>>,
    csv: Option<BufWriter<File>>,
    inicio: Instant,
    janela: Janela,
    ultima: [i16; 2],
    /// Totais desde o começo, para o relatório final.
    total_aceitas: u64,
    total_devidas: u64,
    total_estalos: u64,
}

fn medidor(jogo: Option<&std::path::Path>) -> Option<&'static Mutex<Medidor>> {
    static M: OnceLock<Option<Mutex<Medidor>>> = OnceLock::new();
    M.get_or_init(|| {
        // Terminado em `/`, é um diretório: o nome vem do jogo, e uma fila de vários jogos na
        // mesma medição não sobrescreve um arquivo com o outro.
        let dump = std::env::var("ZEEBX_AUDIO_DUMP").ok().filter(|s| !s.is_empty()).map(|d| {
            match (d.ends_with('/'), jogo.and_then(|j| j.file_name())) {
                (true, Some(nome)) => format!("{d}audio_{}", nome.to_string_lossy()),
                _ => d,
            }
        });
        let mede = std::env::var("ZEEBX_AUDIO_MEDE").is_ok_and(|v| v != "0");
        if dump.is_none() && !mede {
            return None;
        }
        let abre = |ext: &str| {
            dump.as_ref()
                .and_then(|base| File::create(format!("{base}.{ext}")).ok())
                .map(|f| BufWriter::with_capacity(1 << 16, f))
        };
        let mut csv = abre("csv");
        if let Some(c) = csv.as_mut() {
            let _ = writeln!(
                c,
                "real_us,virtual_ms,devidas,oferecidas,aceitas,pendente,descartadas,ocupacao,secando"
            );
        }
        Some(Mutex::new(Medidor {
            pcm: abre("pcm"),
            csv,
            inicio: Instant::now(),
            janela: Janela::nova(),
            ultima: [0; 2],
            total_aceitas: 0,
            total_devidas: 0,
            total_estalos: 0,
        }))
    })
    .as_ref()
}

/// Se a medição está ligada: quem chama pede o aviso de buffer ao frontend.
pub(crate) fn ligado(jogo: &std::path::Path) -> bool {
    medidor(Some(jogo)).is_some()
}

/// Um `retro_run`: `lote` é o que foi oferecido ao frontend e `aceitos` quantos quadros ele
/// aceitou; `devidas` são os quadros que o relógio virtual pediu desta vez.
pub(crate) fn registra(
    virtual_ms: u64,
    devidas: usize,
    lote: &[i16],
    aceitos: usize,
    pendente: usize,
    descartadas: usize,
) -> Option<String> {
    let m = medidor(None)?;
    let mut m = m.lock().ok()?;
    let ocupacao = OCUPACAO.load(Relaxed);
    let secando = SECANDO.load(Relaxed);
    let aceitas = &lote[..(aceitos * 2).min(lote.len())];
    // Estalos: salto grande entre vizinhas, contando a costura com o lote anterior.
    let mut estalos = 0u64;
    let mut ant = m.ultima;
    for q in aceitas.chunks_exact(2) {
        for c in 0..2 {
            if (i32::from(q[c]) - i32::from(ant[c])).abs() > SALTO_ESTALO {
                estalos += 1;
            }
        }
        ant = [q[0], q[1]];
    }
    m.ultima = ant;
    if let Some(p) = m.pcm.as_mut() {
        let bytes: Vec<u8> = aceitas.iter().flat_map(|s| s.to_le_bytes()).collect();
        let _ = p.write_all(&bytes);
    }
    let real_us = m.inicio.elapsed().as_micros();
    if let Some(c) = m.csv.as_mut() {
        let _ = writeln!(
            c,
            "{real_us},{virtual_ms},{devidas},{},{aceitos},{pendente},{descartadas},{},{}",
            lote.len() / 2,
            if ocupacao == u32::MAX { -1 } else { ocupacao as i64 },
            u8::from(secando)
        );
    }
    m.total_aceitas += aceitos as u64;
    m.total_devidas += devidas as u64;
    m.total_estalos += estalos;
    let j = &mut m.janela;
    j.quadros += 1;
    j.virtual_ms += virtual_ms;
    j.devidas += devidas as u64;
    j.oferecidas += (lote.len() / 2) as u64;
    j.aceitas += aceitos as u64;
    j.descartadas += descartadas as u64;
    j.estalos += estalos;
    j.secando += u32::from(secando);
    if ocupacao != u32::MAX {
        j.ocupacao_min = j.ocupacao_min.min(ocupacao);
        j.ocupacao_soma += u64::from(ocupacao);
        j.ocupacao_n += 1;
    }
    if j.quadros < 120 {
        return None;
    }
    let real = j.inicio.elapsed().as_secs_f64();
    let texto = format!(
        "Zeebx AUDIO: {:.1} qps | virtual/real {:.2} | entregue {:.0} Hz reais ({:.0}% do tempo) | devidas {:.0}/s | descartadas {} | estalos {} | buffer min {}% med {:.0}% | secando {}/{} | pendente {}",
        f64::from(j.quadros) / real,
        j.virtual_ms as f64 / 1000.0 / real,
        j.aceitas as f64 / real,
        j.aceitas as f64 / real / 441.0,
        j.devidas as f64 / real,
        j.descartadas,
        j.estalos,
        if j.ocupacao_min == u32::MAX { -1 } else { j.ocupacao_min as i64 },
        if j.ocupacao_n == 0 { -1.0 } else { j.ocupacao_soma as f64 / f64::from(j.ocupacao_n) },
        j.secando,
        j.quadros,
        pendente,
    );
    let texto = {
        use zeebx::audio::conta as c;
        format!(
            "{texto} | fluxo: recebidos {} com som {} faltas {} descartes {} | vozes: {} novas, {} cortadas",
            c::FLUXO_RECEBIDOS.swap(0, Relaxed),
            c::FLUXO_COM_SOM.swap(0, Relaxed),
            c::FLUXO_FALTAS.swap(0, Relaxed),
            c::FLUXO_DESCARTES.swap(0, Relaxed),
            c::VOZES_INICIADAS.swap(0, Relaxed),
            c::VOZES_CORTADAS.swap(0, Relaxed),
        )
    };
    m.janela = Janela::nova();
    // O RetroArch pode sair sem `retro_deinit` (timeout, sinal): a cada janela o disco fica em dia.
    if let Some(p) = m.pcm.as_mut() {
        let _ = p.flush();
    }
    if let Some(c) = m.csv.as_mut() {
        let _ = c.flush();
    }
    Some(texto)
}

/// Fecha os arquivos e devolve o resumo da sessão inteira.
pub(crate) fn encerra() -> Option<String> {
    let m = medidor(None)?;
    let mut m = m.lock().ok()?;
    if let Some(p) = m.pcm.as_mut() {
        let _ = p.flush();
    }
    if let Some(c) = m.csv.as_mut() {
        let _ = c.flush();
    }
    let real = m.inicio.elapsed().as_secs_f64();
    Some(format!(
        "Zeebx AUDIO total: {:.1} s reais, {:.1} s de som aceito, {:.1} s devidos, {} estalos",
        real,
        m.total_aceitas as f64 / 44_100.0,
        m.total_devidas as f64 / 44_100.0,
        m.total_estalos
    ))
}
