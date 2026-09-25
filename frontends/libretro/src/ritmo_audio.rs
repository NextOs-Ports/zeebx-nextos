//! Quantas amostras entregar a cada `retro_run`: pelo **relógio real**, com o buffer do frontend
//! como régua.
//!
//! O frontend toca 44,1 kHz de tempo real, não importa quantos `retro_run` por segundo o core
//! aguenta. Antes o core entregava o que o relógio **virtual** andou, e medido no .30 (Mali-450,
//! A53 a 1,0 GHz) isso deixava o som em pedaços sempre que o jogo roda abaixo do console: o Need
//! for Speed Carbon a 24-26 `retro_run`/s andava 0,43 s virtuais por segundo e entregava só 42% do
//! som que o frontend consumia — 57% do tempo o buffer secava e o pulse tocava silêncio. O RE4 na
//! cena 3D (41 qps) ficava em 71%. E o salto do relógio virtual numa carga (2 s de uma vez no NFS)
//! virava um lote de 2 s que o `audio_sync` bloqueava de uma vez só.
//!
//! O mixer agora anda o tempo que passou de verdade: música e efeitos saem inteiros e na altura
//! certa, e o jogo lento fica lento só na imagem. A velocidade da lógica continua presa pelo freio
//! do `run_frame` (dorme até o real alcançar o virtual), e não pelo bloqueio do áudio.
//!
//! O relógio de parede tem ruído de quadro a quadro, e o frontend ainda reamostra com controle de
//! taxa: a ocupação que ele avisa (`SET_AUDIO_BUFFER_STATUS_CALLBACK`) corrige a produção em até
//! [`CORRECAO_MAXIMA`] para o buffer ficar perto de [`OCUPACAO_ALVO`], o mesmo meio que o controle
//! de taxa do RetroArch persegue.

use std::time::{Duration, Instant};

/// Taxa de saída do core, a mesma do `retro_get_system_av_info`.
const TAXA: f64 = 44_100.0;

/// Ocupação do buffer do frontend que se persegue, em porcentagem.
const OCUPACAO_ALVO: f64 = 50.0;

/// Quanto a produção pode passar ou faltar do tempo real para corrigir a ocupação.
///
/// 5% de 44,1 kHz são 2,2 mil amostras por segundo: tira um buffer de 264 ms (o que o pulse deu no
/// .30) de 13% a 50% em uns 2 s sem que um quadro isolado pese.
const CORRECAO_MAXIMA: f64 = 0.05;

/// Maior intervalo que vira som de uma vez.
///
/// Um `retro_run` que demora mais que isto (carga, menu do RetroArch aberto) já deixou o buffer
/// secar: o buraco aconteceu e não volta. Entregar o intervalo inteiro só enfileiraria atraso — o
/// `audio_sync` bloquearia o core até tocar tudo. 100 ms repõem o buffer sem travar.
const TETO: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub(crate) struct RitmoAudio {
    ultimo: Option<Instant>,
    /// A fração de amostra que sobrou do quadro anterior: truncar 16,7 ms × 44,1 a cada quadro
    /// perderia 0,6 amostra por quadro.
    resto: f64,
}

impl RitmoAudio {
    /// Recomeça a contagem: a próxima chamada entrega um quadro de 60 Hz.
    pub(crate) fn zera(&mut self) {
        *self = Self::default();
    }

    /// Quantos quadros estéreo entregar agora. `ocupacao` é a porcentagem do buffer do frontend
    /// no último aviso, quando ele avisa.
    pub(crate) fn quadros(&mut self, agora: Instant, ocupacao: Option<u32>) -> usize {
        let Some(antes) = self.ultimo.replace(agora) else {
            return (TAXA / 60.0) as usize;
        };
        let real = agora.saturating_duration_since(antes).min(TETO).as_secs_f64();
        let fator = match ocupacao {
            Some(o) if o <= 100 => {
                1.0 + ((OCUPACAO_ALVO - f64::from(o)) / 100.0 * 0.2)
                    .clamp(-CORRECAO_MAXIMA, CORRECAO_MAXIMA)
            }
            _ => 1.0,
        };
        let exato = real * TAXA * fator + self.resto;
        let n = exato.floor().max(0.0);
        self.resto = exato - n;
        n as usize
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn um_segundo_de_chamadas_lentas_da_um_segundo_de_som() {
        // 24 chamadas por segundo, o NFS no .30: o som tem de cobrir o segundo inteiro.
        let mut r = RitmoAudio::default();
        let t0 = Instant::now();
        let mut total = r.quadros(t0, None);
        for i in 1..=24u32 {
            total += r.quadros(t0 + Duration::from_secs(1) * i / 24, None);
        }
        // O `Duration` trunca em nanossegundos: no máximo uma amostra de diferença.
        assert!((735 + 44_099..=735 + 44_100).contains(&total), "{total}");
    }

    #[test]
    fn a_fracao_nao_se_perde() {
        let mut r = RitmoAudio::default();
        let t0 = Instant::now();
        r.quadros(t0, None);
        let mut total = 0;
        for i in 1..=600u32 {
            total += r.quadros(t0 + Duration::from_secs(10) * i / 600, None);
        }
        // 10 s a 60 Hz: 441 000 quadros, com no máximo um de arredondamento.
        assert!((440_999..=441_001).contains(&total), "{total}");
    }

    #[test]
    fn uma_trava_longa_nao_vira_segundos_de_som_de_uma_vez() {
        let mut r = RitmoAudio::default();
        let t0 = Instant::now();
        r.quadros(t0, None);
        assert_eq!(r.quadros(t0 + Duration::from_secs(2), None), 4410);
    }

    #[test]
    fn buffer_vazio_pede_mais_e_cheio_pede_menos() {
        let t0 = Instant::now();
        let passo = Duration::from_millis(100);
        let mut vazio = RitmoAudio::default();
        vazio.quadros(t0, Some(13));
        let mut cheio = RitmoAudio::default();
        cheio.quadros(t0, Some(95));
        let (v, c) = (vazio.quadros(t0 + passo, Some(13)), cheio.quadros(t0 + passo, Some(95)));
        assert!(v > 4410 && v <= 4631, "{v}");
        assert!(c < 4410 && c >= 4189, "{c}");
    }
}
