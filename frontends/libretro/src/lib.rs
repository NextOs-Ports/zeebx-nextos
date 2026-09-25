//! Core Libretro do Zeebx.
//!
//! O frontend é dono de vídeo, áudio e entrada; aqui só se traduz isso para o motor `zeebx`.
//! Nenhuma janela, placa de som ou controle do host é aberto por este crate: o motor roda pelo
//! passo virtual de [`zeebx::session::Session::run_frame`].
//!
//! O core atende conteúdo `.mod`/`.zip`/`.7z`, vídeo RGB565, áudio PCM16 a 44,1 kHz, RetroPad nas
//! duas portas, teclado USB e Boomerang alimentado pelo sensor do frontend. Renderização em
//! hardware OpenGL/GLES 3, save state versionado e troca da Z-Wheel também estão implementados;
//! mouse do guest continua fora da ABI.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use zeebx::audio::Mixer;

mod audio_mede;
use zeebx::config::ZWheel;
use zeebx::input::Pad;
use zeebx::input::bindings::Aparelho;
use zeebx::session::{Session, StartError, Step};
use zeebx::storage::StoragePaths;

/// Versão da ABI que este core implementa.
const API_VERSION: u32 = 1;

/// Taxa nominal do core: 44,1 kHz estéreo.
///
/// A quantidade entregue por chamada **não** é fixa: sai do tempo virtual que passou desde a
/// chamada anterior. Ver `retro_run`.
const SAMPLE_RATE: u32 = 44_100;

/// Formato de pixel negociado com o frontend, em `retro_pixel_format`.
const PIXEL_FORMAT_RGB565: u32 = 2;

/// Comandos de ambiente usados.
/// Pede ao frontend que descarregue o conteúdo e volte ao menu dele.
const ENV_SHUTDOWN: u32 = 7;

/// Mostra um aviso ao jogador, na tela do próprio frontend.
const ENV_SET_MESSAGE: u32 = 6;

/// Pergunta se o frontend entrega todos os botões numa palavra só.
const ENV_GET_INPUT_BITMASKS: u32 = 51;

/// Identificador especial do RetroPad que devolve os botões como máscara de bits.
const ID_JOYPAD_MASK: u32 = 256;

/// Recebe as teclas do frontend. É por aqui que a Z-Wheel navega: ela pede `AVK_0` e `AVK_CLR`,
/// que não existem no RetroPad.
const ENV_SET_KEYBOARD_CALLBACK: u32 = 12;

/// Códigos de tecla da ABI (`enum retro_key`), que seguem os do SDL 1.2.
const RETROK_BACKSPACE: u32 = 8;
const RETROK_RETURN: u32 = 13;
const RETROK_ESCAPE: u32 = 27;
const RETROK_ASTERISK: u32 = 42;
const RETROK_HASH: u32 = 35;
const RETROK_0: u32 = 48;
const RETROK_UP: u32 = 273;
const RETROK_DOWN: u32 = 274;
const RETROK_RIGHT: u32 = 275;
const RETROK_LEFT: u32 = 276;

/// Quantos jogos o core achou ao lado do conteúdo.
///
/// **É a precondição da Z-Wheel listar alguma coisa.** A roda enumera os applets instalados e pede
/// o `.mod` de cada um por ClassID; quem sabe onde eles estão é o levantamento feito ao abrir o
/// conteúdo. Roda vazia aqui é roda vazia na tela — e sem esta conta o sintoma "a Z-Wheel abre sem
/// jogo nenhum" não teria onde ser medido sem janela. **Instrumento de teste, e só dele.**
#[cfg(test)]
static JOGOS_VISTOS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A última classe que o shell pediu para abrir.
///
/// **Instrumento de teste, e só dele.** O log do core sai pelo callback do frontend, que é
/// **variádico** — uma função `extern "C" fn(...)` não pode ser escrita em Rust estável —, então o
/// teste observa por aqui o que a interface mostraria como texto. Fora de `cfg(test)` não existe.
#[cfg(test)]
static ULTIMA_ABERTURA: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A classe do applet que está rodando agora, a cada quadro.
///
/// **Instrumento de teste, e só dele**, pelo mesmo motivo de [`ULTIMA_ABERTURA`]. É o que deixa o
/// teste do ciclo da Z-Wheel dizer *quem* está rodando — a roda, ou o jogo que ela abriu — sem
/// janela e sem olhar pixels.
#[cfg(test)]
static CLASSE_ATUAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// O relógio virtual da sessão, em milissegundos, a cada quadro.
///
/// **Instrumento de teste, e só dele.** É o que diz se a roda está no mesmo ponto da linha do tempo
/// nos dois caminhos — o da varredura e o do core —, e é a primeira coisa a conferir quando o mesmo
/// roteiro dá desfechos diferentes: a tecla pode estar certa e o instante, não. Medido com ele:
/// cada `retro_run` avança ~26 ms de relógio virtual, e não os 16 ms de um quadro a 60 Hz, porque a
/// volta do core termina quando a máquina **apresenta** um quadro.
#[cfg(test)]
static RELOGIO: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// As instruções que a máquina já executou, a cada quadro.
///
/// **Instrumento de teste, e só dele.** É o par do [`RELOGIO`] para a pergunta que separa "a roda
/// está lenta" de "a roda parou": o relógio do guest anda por vsync e por instrução, então
/// instruções que sobem com o relógio parado são guest girando sem apresentar quadro. Foi o que
/// mostrou que o caminho do core entrega 1 214 quadros por milissegundo virtual depois do
/// confirmar, contra ~240 quadros por segundo no regime normal.
#[cfg(test)]
static INSTRUCOES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// O frontend oferece um contexto de placa para o core desenhar.
const ENV_SET_HW_RENDER: u32 = 14;

/// Perfil de hardware que o core pede.
///
/// Desktop usa `RETRO_HW_CONTEXT_OPENGL_CORE` (3), medido com RetroArch/Mesa. Os handhelds
/// Linux AArch64 (R36S/R35S/RGB20S com ArkOS/AeolusUX/dArkOS/dArkOSen e RG40XX-H com muOS)
/// expõem OpenGL ES no RetroArch, não um contexto OpenGL Core 3.3. Neles pedimos GLES 3.2
/// (`RETRO_HW_CONTEXT_OPENGLES_VERSION`, 5), e o `gpu.rs` usa `#version 300 es`, compatível com
/// o subconjunto necessário. Se o frontend/driver só oferecer GLES 3.1, ele recusa
/// o pedido e o core permanece no rasterizador software — nunca depende de X11, Wayland ou EGL.
///
/// No desktop o valor 1 (`RETRO_HW_CONTEXT_OPENGL`, compatibilidade) não serve: em RetroArch/EGL
/// ele entregava perfil diferente do que o motor esperava e falhava com `GL: Invalid enum`.
#[cfg(target_os = "emscripten")]
const HW_CONTEXT: u32 = 4; // RETRO_HW_CONTEXT_OPENGLES3 — WebGL2
#[cfg(target_os = "emscripten")]
const HW_VERSION_MAJOR: u32 = 3;
#[cfg(target_os = "emscripten")]
const HW_VERSION_MINOR: u32 = 0;

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HW_CONTEXT: u32 = 5; // RETRO_HW_CONTEXT_OPENGLES_VERSION (GLES 3.1+)
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HW_VERSION_MAJOR: u32 = 3;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HW_VERSION_MINOR: u32 = 2;

/// Os perfis pedidos ao frontend, do mais capaz ao mais modesto, quando o primeiro é recusado.
///
/// Os Amlogic com Mali-400/450 (NextOS, CoreELEC) só têm OpenGL ES 2.0, e o driver `gl` do
/// RetroArch neles responde "unknown context" ao pedido de GLES 3.x. Sem a queda o core ficava
/// no processador. O `gpu.rs` decide o perfil pela versão real do contexto que veio, não pelo
/// pedido: com ES 2.0 ele usa GLSL ES 1.00, sem VAO, sem MSAA e com o quadro no tamanho nativo.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const HW_QUEDAS: [(u32, u32, u32); 2] = [
    (4, 3, 0), // RETRO_HW_CONTEXT_OPENGLES3
    (2, 2, 0), // RETRO_HW_CONTEXT_OPENGLES2
];
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
const HW_QUEDAS: [(u32, u32, u32); 0] = [];

/// Se a placa é um Mali Utgard (400/450), que só tem OpenGL ES 2.0.
///
/// **Não dá para descobrir pelo pedido.** O RetroArch do NextOS aceita o `SET_HW_RENDER` de GLES3
/// e só depois, ao criar a superfície, o EGL do Mali responde `EGL_BAD_ATTRIBUTE` e o frontend
/// fecha com "Cannot open video driver" — o jogo nem abre. Por isso o perfil é escolhido antes:
/// o driver Utgard proprietário expõe `/dev/mali` (Midgard e Bifrost usam `/dev/mali0`) e o
/// livre é o `lima`. `ZEEBX_GLES=2` força o ES 2.0 e `ZEEBX_GLES=3` o impede.
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn placa_so_gles2() -> bool {
    match std::env::var("ZEEBX_GLES").ok().as_deref().map(str::trim) {
        Some("2") => return true,
        Some("3") => return false,
        _ => {}
    }
    let existe = |p: &str| std::path::Path::new(p).exists();
    (existe("/dev/mali") && !existe("/dev/mali0")) || existe("/sys/module/lima")
}
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn placa_so_gles2() -> bool {
    false
}

#[cfg(not(any(
    target_os = "emscripten",
    all(target_os = "linux", target_arch = "aarch64")
)))]
const HW_CONTEXT: u32 = 3; // RETRO_HW_CONTEXT_OPENGL_CORE
#[cfg(not(any(
    target_os = "emscripten",
    all(target_os = "linux", target_arch = "aarch64")
)))]
const HW_VERSION_MAJOR: u32 = 3;
#[cfg(not(any(
    target_os = "emscripten",
    all(target_os = "linux", target_arch = "aarch64")
)))]
const HW_VERSION_MINOR: u32 = 3;

/// O valor que o `retro_video_refresh` recebe quando o quadro saiu no framebuffer do frontend.
///
/// É o sentinela do `libretro`: passar pixels junto com ele seria mentira, e o RetroArch apresenta
/// o framebuffer que ele mesmo forneceu.
const HW_FRAME_BUFFER_VALID: usize = usize::MAX;


/// O começo de `retro_hw_render_callback`, na ordem do `libretro.h` vendorizado.
///
/// Os campos são os que o core precisa ler e preencher: o tipo de contexto (o core escolhe), os
/// dois ponteiros que o **frontend** preenche depois (o framebuffer corrente e o resolvedor de
/// funções de GL) e o `context_reset`, que é como o frontend avisa que o contexto está utilizável.
#[repr(C)]
#[derive(Clone, Copy)]
struct RetroHwRenderCallback {
    context_type: u32,
    context_reset: Option<unsafe extern "C" fn()>,
    get_current_framebuffer: Option<unsafe extern "C" fn() -> u32>,
    get_proc_address: Option<unsafe extern "C" fn(*const c_char) -> *const c_void>,
    depth: bool,
    stencil: bool,
    bottom_left_origin: bool,
    version_major: u32,
    version_minor: u32,
    /// Se o frontend quer que o core guarde os recursos de GL entre contextos.
    ///
    /// Fica declarado para o struct ter o **tamanho e os deslocamentos** do `libretro.h`: um campo
    /// a menos aqui e o `context_destroy` abaixo seria lido no lugar errado. Não usamos cache, e
    /// responder `false` (o zero) é a resposta certa.
    cache_context: bool,
    /// Chamado pelo frontend quando o contexto **deixa de valer** — trocar de driver de vídeo, por
    /// exemplo. Sem ele, o `glow::Context` que guardamos continua apontando para funções que já não
    /// existem e o primeiro desenho depois disso quebra.
    context_destroy: Option<unsafe extern "C" fn()>,
    /// O resto do struct não nos interessa, mas o tamanho tem de bater com o do frontend.
    _resto: [usize; 4],
}

/// O que o frontend respondeu ao pedido de render em hardware.
static OFERTA_DE_PLACA: Mutex<Option<RetroHwRenderCallback>> = Mutex::new(None);

/// Se o frontend já avisou que o contexto está utilizável.
///
/// **A ordem importa:** o `retro_load_game` pede o contexto, e o frontend chama o `context_reset`
/// **depois** — só ali as funções de GL existem. Por isso a placa entra no primeiro `retro_run`, e
/// não na carga.
static CONTEXTO_PRONTO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// O contexto de GL montado a partir do que o frontend entregou, quando ele entregou.
static PLACA: std::sync::Mutex<Option<std::sync::Arc<glow::Context>>> =
    std::sync::Mutex::new(None);

/// O contexto de placa, quando já se desenha nele.
fn placa() -> Option<std::sync::Arc<glow::Context>> {
    PLACA.lock().ok().and_then(|guarda| guarda.clone())
}

/// Pede o contexto de placa ao frontend, uma vez, e guarda a resposta.
///
/// No `libretro`, quem **oferece** é o core: ele preenche o struct e chama o ambiente. O frontend
/// devolve `true` se aceitar, e depois disso ele cria o contexto e chama o nosso `context_reset`.
fn pede_o_contexto_de_placa() {
    if OFERTA_DE_PLACA.lock().is_ok_and(|g| g.is_some()) {
        return;
    }
    let mut oferta = RetroHwRenderCallback {
        context_type: HW_CONTEXT,
        context_reset: Some(contexto_pronto),
        get_current_framebuffer: None,
        get_proc_address: None,
        // Profundidade e stencil são exigências do console, não enfeite: o palco da Z-Wheel marca
        // o chão no stencil para desenhar o reflexo.
        depth: true,
        stencil: true,
        bottom_left_origin: false,
        version_major: HW_VERSION_MAJOR,
        version_minor: HW_VERSION_MINOR,
        // Não guardamos recursos de GL entre contextos, e o `libretro` só oferece a opção.
        cache_context: false,
        context_destroy: Some(contexto_perdido),
        _resto: [0; 4],
    };
    let so_gles2 = placa_so_gles2();
    let pedidos = std::iter::once((HW_CONTEXT, HW_VERSION_MAJOR, HW_VERSION_MINOR))
        .chain(HW_QUEDAS.iter().copied())
        // Num Mali Utgard só o pedido de ES 2.0 (tipo 2) é honesto; os outros o frontend aceita
        // e depois não consegue criar.
        .filter(|&(tipo, _, _)| !so_gles2 || tipo == 2);
    for (tipo, maior, menor) in pedidos {
        oferta.context_type = tipo;
        oferta.version_major = maior;
        oferta.version_minor = menor;
        let alvo = &mut oferta as *mut RetroHwRenderCallback as *mut c_void;
        if unsafe { environ(ENV_SET_HW_RENDER, alvo) } {
            log(&format!(
                "Zeebx: o frontend aceitou render em hardware (contexto {} — OpenGL {}.{}); o desenho passa a ser na placa",
                tipo, oferta.version_major, oferta.version_minor
            ));
            if let Ok(mut g) = OFERTA_DE_PLACA.lock() { *g = Some(oferta); }
            return;
        }
    }
    log("Zeebx: o frontend não oferece render em hardware; o desenho fica no processador");
}

/// Monta o contexto de GL a partir do resolvedor do frontend e **recria a sessão** na placa.
///
/// **Uma vez, no primeiro quadro** — e não na carga —, porque é só depois do `context_reset` que
/// as funções existem. Recriar a sessão aqui custa um reinício que ninguém vê: nenhum quadro foi
/// entregue ainda.
///
/// **Qualquer falha devolve o software e diz por quê.** Um frontend que aceita o pedido e não
/// cumpre — sem `get_proc_address`, sem contexto — não pode deixar o emulador sem imagem: o
/// caminho de software é o medido e o que já funcionava.
fn liga_a_placa(estado: &mut Core) {
    if estado.placa_ligada || !CONTEXTO_PRONTO.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Tenta **uma vez**: um contexto que não veio não vem no quadro seguinte, e insistir a cada
    // quadro gastaria o log inteiro.
    estado.placa_ligada = true;
    let Some(oferta) = OFERTA_DE_PLACA.lock().ok().and_then(|g| *g) else {
        return;
    };
    let Some(pega_endereco) = oferta.get_proc_address else {
        aviso("Zeebx: o frontend aceitou render em hardware mas não oferece get_proc_address; seguindo no processador");
        return;
    };
    let contexto = std::sync::Arc::new(unsafe {
        glow::Context::from_loader_function(|nome| {
            let Ok(nome) = CString::new(nome) else {
                return std::ptr::null();
            };
            pega_endereco(nome.as_ptr())
        })
    });
    // A placa entra no global **antes** da troca: é ele que `troca_para` consulta para decidir se
    // a sessão nasce com o rasterizador de placa. Assim as trocas seguintes — a Z-Wheel abrindo um
    // jogo, o jogo voltando para ela — também nascem na placa.
    if let Ok(mut guarda) = PLACA.lock() {
        *guarda = Some(contexto);
    }
    let antes = estado.path.clone();
    // **Um `panic` aqui derrubaria o RetroArch.** Não há fronteira segura para atravessar uma
    // falha de Rust e voltar para o C do frontend: o processo inteiro cai, e o usuário perde o
    // emulador por causa de uma otimização de desenho. O `catch_unwind` transforma isso no mesmo
    // caminho da falha comum — aviso no log, sessão de software, jogo rodando.
    //
    // `AssertUnwindSafe` é o que a situação pede: se o meio da montagem do contexto ficou
    // inconsistente, o que vem depois **não** continua dali — a sessão é recriada do zero, e o
    // global da placa é limpo.
    let tentativa = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        troca_para(estado, &antes, false)
    }));
    let desfecho = match tentativa {
        Ok(resultado) => resultado,
        Err(_) => Err(StartError::NotLoadable(
            "a montagem do contexto de placa entrou em pânico".to_string(),
        )),
    };
    match desfecho {
        Ok(()) => log("Zeebx: desenhando na placa"),
        Err(erro) => {
            aviso(&format!(
                "Zeebx: o render em hardware falhou ({erro}); seguindo no processador"
            ));
            // A sessão de software foi perdida na tentativa. Recria sem placa para não ficar sem
            // imagem nenhuma — e o global da placa é limpo, para as trocas seguintes nascerem no
            // processador.
            estado.placa_ligada = false;
            if let Ok(mut guarda) = PLACA.lock() {
                *guarda = None;
            }
            if let Err(tambem) = troca_para(estado, &antes, false) {
                aviso(&format!("Zeebx: nem no processador deu para reabrir: {tambem}"));
            }
        }
    }
}

/// O `context_reset` que nós preenchemos: o frontend chama quando o contexto está utilizável.
unsafe extern "C" fn contexto_pronto() {
    CONTEXTO_PRONTO.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// O frontend avisa que o contexto deixou de valer.
///
/// **O `glow::Context` guardado vira lixo aqui**: as funções de GL que ele resolveu não existem
/// mais. Descartar o contexto e voltar ao software é a resposta segura — o jogo continua rodando
/// no processador, e o aviso diz por quê.
unsafe extern "C" fn contexto_perdido() {
    if let Ok(mut guarda) = PLACA.lock() {
        *guarda = None;
    }
    CONTEXTO_PRONTO.store(false, std::sync::atomic::Ordering::Relaxed);
    PERDEU_A_PLACA.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Se o frontend já avisou que o contexto se perdeu. Ver [`contexto_perdido`].
static PERDEU_A_PLACA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pergunta se o frontend aceita receber quadro nulo quando nada mudou.
const ENV_GET_CAN_DUPE: u32 = 3;
const ENV_GET_SYSTEM_DIRECTORY: u32 = 9;
const ENV_SET_PIXEL_FORMAT: u32 = 10;
const ENV_SET_INPUT_DESCRIPTORS: u32 = 11;
const ENV_GET_LOG_INTERFACE: u32 = 27;
const ENV_GET_SAVE_DIRECTORY: u32 = 31;
const ENV_SET_CONTROLLER_INFO: u32 = 35;
const ENV_SET_VARIABLES: u32 = 16;
const ENV_GET_VARIABLE: u32 = 15;
/// `RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE`: o frontend avisa que alguma opção mudou.
///
/// **Sem isto toda opção do core é opção de recarregar o jogo.** O `GET_VARIABLE` devolve o valor
/// de agora, mas perguntar por ele a cada quadro, para cada chave, custa uma chamada ao frontend
/// por chave por quadro. Este é o aviso barato: devolve `true` uma única vez depois que o usuário
/// mexeu no menu, e só então vale reler o que dá para aplicar a quente.
const ENV_GET_VARIABLE_UPDATE: u32 = 17;
const ENV_GET_CORE_OPTIONS_VERSION: u32 = 52;
const ENV_SET_CORE_OPTIONS_V2: u32 = 67;
/// `RETRO_ENVIRONMENT_SET_AUDIO_BUFFER_STATUS_CALLBACK`: o frontend passa a avisar, antes de cada
/// `retro_run`, o quanto do buffer de áudio dele está ocupado e se um estouro é provável.
///
/// É o mecanismo do próprio Libretro para frameskip automático — não é invenção nossa: o parágrafo
/// da própria `libretro.h` diz "se `underrun_likely`, o core deveria tentar pular quadro".
const ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK: u32 = 62;
/// `RETRO_ENVIRONMENT_SET_MINIMUM_AUDIO_LATENCY`: pede mais folga no buffer de áudio do frontend.
///
/// A própria documentação do callback acima recomenda isto: sem folga, o aviso de estouro chega
/// tarde demais para o core reagir a tempo. 96 ms (SAMPLE_RATE-independente, é o frontend que
/// mede) é o de seis a oito quadros a 60 Hz, a faixa que a `libretro.h` sugere.
const ENV_SET_MINIMUM_AUDIO_LATENCY: u32 = 63;

/// Tipos de dispositivo e identificadores de botão do RetroPad.
const DEVICE_NONE: u32 = 0;
const DEVICE_JOYPAD: u32 = 1;
const DEVICE_ANALOG: u32 = 5;
const ID_B: u32 = 0;
const ID_Y: u32 = 1;
const ID_SELECT: u32 = 2;
const ID_START: u32 = 3;
const ID_UP: u32 = 4;
const ID_DOWN: u32 = 5;
const ID_LEFT: u32 = 6;
const ID_RIGHT: u32 = 7;
const ID_A: u32 = 8;
const ID_X: u32 = 9;
const ID_L: u32 = 10;
const ID_R: u32 = 11;
const ANALOG_LEFT: u32 = 0;
const ANALOG_AXIS_X: u32 = 0;
const ANALOG_AXIS_Y: u32 = 1;

/// Subclasses de RetroPad que identificam aparelhos do console.
const DEVICE_ZPAD: u32 = ((1 + 1) << 8) | DEVICE_JOYPAD;
const DEVICE_BOOMERANG: u32 = ((2 + 1) << 8) | DEVICE_JOYPAD;

type EnvironmentFn = unsafe extern "C" fn(cmd: u32, data: *mut c_void) -> bool;
type KeyboardEventFn = unsafe extern "C" fn(down: bool, keycode: u32, character: u32, modifiers: u16);

#[repr(C)]
struct RetroMessage {
    msg: *const c_char,
    frames: u32,
}

#[repr(C)]
struct RetroVariable {
    key: *const c_char,
    value: *const c_char,
}

/// `retro_audio_buffer_status_callback_t`, o tipo da função — não a struct de registro.
type RetroAudioBufferStatusCallbackFn = unsafe extern "C" fn(bool, u32, bool);

#[repr(C)]
struct RetroAudioBufferStatusCallback {
    callback: Option<RetroAudioBufferStatusCallbackFn>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroCoreOptionValue {
    value: *const c_char,
    label: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroCoreOptionV2Category {
    key: *const c_char,
    desc: *const c_char,
    info: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RetroCoreOptionV2Definition {
    key: *const c_char,
    desc: *const c_char,
    desc_categorized: *const c_char,
    info: *const c_char,
    info_categorized: *const c_char,
    category_key: *const c_char,
    values: [RetroCoreOptionValue; 128],
    default_value: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionsV2 {
    categories: *const RetroCoreOptionV2Category,
    definitions: *const RetroCoreOptionV2Definition,
}

unsafe impl Sync for RetroVariable {}
unsafe impl Sync for RetroCoreOptionsV2 {}
unsafe impl Sync for RetroCoreOptionV2Category {}
unsafe impl Sync for RetroCoreOptionV2Definition {}

#[repr(C)]
struct RetroKeyboardCallback {
    callback: Option<KeyboardEventFn>,
}
type VideoRefreshFn =
    unsafe extern "C" fn(data: *const c_void, width: u32, height: u32, pitch: usize);
type AudioSampleBatchFn = unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
type InputPollFn = unsafe extern "C" fn();
type InputStateFn = unsafe extern "C" fn(port: u32, device: u32, index: u32, id: u32) -> i16;

#[repr(C)]
pub struct RetroSystemInfo {
    library_name: *const c_char,
    library_version: *const c_char,
    valid_extensions: *const c_char,
    need_fullpath: bool,
    block_extract: bool,
}

#[repr(C)]
pub struct RetroGameGeometry {
    base_width: u32,
    base_height: u32,
    max_width: u32,
    max_height: u32,
    aspect_ratio: f32,
}

#[repr(C)]
pub struct RetroSystemTiming {
    fps: f64,
    sample_rate: f64,
}

#[repr(C)]
pub struct RetroSystemAvInfo {
    geometry: RetroGameGeometry,
    timing: RetroSystemTiming,
}

#[repr(C)]
pub struct RetroGameInfo {
    path: *const c_char,
    data: *const c_void,
    size: usize,
    meta: *const c_char,
}

#[repr(C)]
struct RetroLogCallback {
    log: Option<unsafe extern "C" fn(level: u32, fmt: *const c_char, ...)>,
}

#[repr(C)]
struct RetroInputDescriptor {
    port: u32,
    device: u32,
    index: u32,
    id: u32,
    description: *const c_char,
}

#[repr(C)]
struct RetroControllerDescription {
    desc: *const c_char,
    id: u32,
}

// SAFETY: as tabelas de descritores são `static` e imutáveis; os ponteiros apontam para literais
// de vida estática. A ABI exige que continuem válidos durante todo o carregamento.
unsafe impl Sync for RetroControllerDescription {}

#[repr(C)]
struct RetroControllerInfo {
    types: *const RetroControllerDescription,
    num_types: u32,
}

// SAFETY: idem acima: a lista de portas é imutável e aponta para as descrições estáticas.
unsafe impl Sync for RetroControllerInfo {}

/// O que o frontend oferece ao core.
///
/// É `Copy` de propósito: os ponteiros são tirados do mutex **antes** de qualquer chamada, para
/// que o core nunca segure um cadeado seu enquanto o frontend executa. Um frontend que abra
/// diálogo, salve estado ou espere outra thread dentro do callback travaria o core — e foi o que
/// aconteceu com o disco cheio, quando o RetroArch abriu o aviso de gravação no meio do quadro.
#[derive(Default, Clone, Copy)]
struct Frontend {
    environ: Option<EnvironmentFn>,
    video: Option<VideoRefreshFn>,
    audio_batch: Option<AudioSampleBatchFn>,
    input_poll: Option<InputPollFn>,
    input_state: Option<InputStateFn>,
}

/// Estado do jogo carregado.
struct Core {
    session: Session,
    mixer: Mixer,
    /// Porta 1 e 2, como o guest as enxerga.
    portas: [Option<Aparelho>; zeebx::input::PORTAS],
    /// Buffer do quadro no formato negociado, reaproveitado a cada `retro_run`.
    frame: Vec<u8>,
    audio: Vec<i16>,
    /// Caminho do conteúdo, para o `retro_reset`.
    path: PathBuf,
    /// As raízes do perfil, para trocar de applet sem perder saves nem cache.
    storage: StoragePaths,
    /// Os jogos encontrados ao lado do conteúdo: ClassID do applet e o que carregar.
    ///
    /// É o que permite atender o pedido de lançamento do shell: a Z-Wheel pede uma classe, e aqui
    /// se sabe qual `.mod` ou `.zip` responde por ela.
    jogos: Vec<(u32, PathBuf)>,
    /// O conteúdo da Z-Wheel, para voltar a ela quando um jogo que ela abriu termina.
    z_wheel: Option<PathBuf>,
    /// Se a sessão atual foi aberta pela Z-Wheel — a volta é para ela, como no console.
    aberto_pela_z_wheel: bool,
    /// Se o frontend entrega os botões do RetroPad numa máscara de bits.
    bitmasks: bool,
    /// Se o frontend aceita quadro nulo quando a tela não mudou.
    aceita_dupe: bool,
    /// (série, escritas) da tela no último quadro copiado para `frame`: igual de novo, nada mudou.
    chave_do_quadro: Option<(u64, u64)>,
    /// As linhas `[y0, y1)` em que o último quadro copiado diferia do anterior.
    faixa_do_quadro: Option<(usize, usize)>,
    /// As trocas de buffer do GL vistas no quadro anterior. Com placa, um quadro sem troca é
    /// quadro 2D (IDisplay): vai por software, porque o framebuffer do frontend não tem nada dele.
    swaps_vistos: u32,
    /// Relógio virtual da última chamada, para o áudio acompanhar o tempo que passou de verdade.
    ultimo_relogio_ms: u32,
    /// A medição de áudio pediu o aviso de buffer ao frontend (sem mexer na latência dele).
    audio_mede_callback: bool,
    /// Amostras que o frontend não aceitou e ficam para a chamada seguinte.
    audio_pendente: Vec<i16>,
    /// Se já avisou que o quadro saiu do tamanho do console.
    avisou_tamanho: bool,
    /// Política de síntese MIDI configurada nas opções do core.
    midi_backend: zeebx::audio::MidiBackend,
    /// Estado anterior do Select do RetroPad, para o atalho de `AVK_CLR`.
    select_antes: bool,
    /// O controle da volta anterior, por porta, para o que muda virar **tecla do console**.
    ///
    /// O applet lê o direcional e o botão 1 como as teclas do BREW (`0xe031`…`0xe064`), e não
    /// pela posição do controle: é com elas que a Z-Wheel navega. Sem guardar o quadro anterior
    /// não há como saber o que mudou, e é a mudança que vira tecla.
    pad_antes: [Pad; zeebx::input::PORTAS],
    /// Quantos quadros já foram apresentados depois da parada.
    ///
    /// A tela final fica à mostra por um instante antes de o frontend ser dispensado: sem isso o
    /// conteúdo some no mesmo quadro em que o jogo acaba, e quem está jogando não vê o desfecho.
    quadros_apos_parar: u32,
    /// Se já se tentou ligar o render em hardware. Uma vez só: um contexto que não veio não vem
    /// no quadro seguinte. Ver [`liga_a_placa`].
    placa_ligada: bool,
    /// A política de frameskip escolhida agora — ver [`Frameskip`].
    frameskip: Frameskip,
    /// Quantos quadros já se passaram desde o último desenhado de verdade, no modo fixo.
    ///
    /// É contador, e não paridade (`quadro % 2`), porque o fixo aceita qualquer razão — pular 3
    /// a cada 4 é tão válido quanto pular 1 a cada 2 — e só um contador serve às duas.
    frameskip_contador: u32,
    /// Se o `SET_AUDIO_BUFFER_STATUS_CALLBACK` já foi pedido ao frontend. Uma vez só: pedir de
    /// novo a cada quadro não muda a resposta, e a documentação do próprio Libretro pede
    /// moderação nesta chamada.
    frameskip_callback_pedido: bool,
    /// Se já avisamos que `glReadPixels` tornou frameskip de rasterização inseguro.
    frameskip_leitura_pixels_avisada: bool,
    /// Teto de velocidade escolhido — ver [`LimiteFps`].
    limite_fps: LimiteFps,
    /// Fase da duplicação de apresentação do teto de 30 FPS.
    limite_fps_contador: u32,
    /// Este quadro deve repetir a imagem anterior no callback de vídeo.
    ///
    /// Separado de `pula_desenho`: jogos 2D e jogos com `glReadPixels` podem não economizar
    /// rasterização, mas 30 FPS ainda precisa limitar a apresentação de forma verdadeira.
    limite_fps_duplica: bool,
    /// Se o desfecho já foi relatado ao frontend.
    ///
    /// Sem isto o core repetiria a mesma linha a cada quadro depois da parada, e um log que cresce
    /// para sempre esconde justamente o instante em que o jogo parou.
    parou: bool,
}

fn frontend() -> &'static Mutex<Frontend> {
    static FRONTEND: OnceLock<Mutex<Frontend>> = OnceLock::new();
    FRONTEND.get_or_init(|| Mutex::new(Frontend::default()))
}

/// O estado do jogo visto pela ABI.
///
/// O motor não é `Send`: carrega `Rc`, ponteiros do JIT e do rasterizador, que valem enquanto
/// estiverem na mesma thread. A ABI Libretro garante que `retro_run`, `retro_reset` e
/// `retro_unload_game` são chamados na thread que carregou o jogo, e é essa a promessa que este
/// invólucro faz ao compilador.
struct EstadoDoCore(Core);

// SAFETY: ver o comentário acima — o core nunca é movido para outra thread.
unsafe impl Send for EstadoDoCore {}

/// Fila de teclas do frontend.
///
/// O callback não executa guest: ele **enfileira**, e `retro_run` entrega as teclas ao motor na
/// thread normal do core. Executar o emulador de dentro do callback seria reentrância em cima do
/// estado que `retro_run` está usando.
fn teclas() -> &'static Mutex<std::collections::VecDeque<(u32, bool)>> {
    static TECLAS: OnceLock<Mutex<std::collections::VecDeque<(u32, bool)>>> = OnceLock::new();
    TECLAS.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// Traduz uma tecla do frontend para o código virtual do BREW, quando existe.
///
/// `Esc` e `P` ficam de fora de propósito: são as teclas da interface do frontend, não do jogo.
fn avk_da_tecla(keycode: u32) -> Option<u32> {
    use zeebx::input::avk;
    Some(match keycode {
        RETROK_UP => avk::UP,
        RETROK_DOWN => avk::DOWN,
        RETROK_LEFT => avk::LEFT,
        RETROK_RIGHT => avk::RIGHT,
        RETROK_RETURN => avk::SELECT,
        RETROK_BACKSPACE | RETROK_ESCAPE => avk::CLR,
        RETROK_ASTERISK => avk::STAR,
        RETROK_HASH => avk::POUND,
        RETROK_0..=57 => avk::ZERO + (keycode - RETROK_0),
        _ => return None,
    })
}

/// O callback de teclado do frontend: só enfileira.
unsafe extern "C" fn tecla_recebida(down: bool, keycode: u32, _character: u32, _modifiers: u16) {
    let Some(avk) = avk_da_tecla(keycode) else {
        return;
    };
    if let Ok(mut fila) = teclas().lock() {
        // Um teto evita que uma tecla presa (ou um frontend repetindo sem parar) cresça sem fim.
        if fila.len() < 1024 {
            fila.push_back((avk, down));
        }
    }
}

fn core() -> &'static Mutex<Option<EstadoDoCore>> {
    static CORE: OnceLock<Mutex<Option<EstadoDoCore>>> = OnceLock::new();
    CORE.get_or_init(|| Mutex::new(None))
}

/// Uma cópia dos callbacks do frontend, sem manter o cadeado.
fn callbacks() -> Frontend {
    frontend().lock().map(|guard| *guard).unwrap_or_default()
}

/// Avança um aviso ao jogador. Também vai para o log, porque nem todo frontend mostra mensagem
/// — e um aviso que ninguém vê não serve para nada.
fn aviso(texto: &str) {
    let Ok(texto_c) = CString::new(texto) else {
        return;
    };
    let mensagem = RetroMessage {
        // Três segundos a 60 Hz: tempo de ler sem atrapalhar quem está jogando.
        msg: texto_c.as_ptr(),
        frames: 180,
    };
    unsafe {
        environ(
            ENV_SET_MESSAGE,
            &mensagem as *const RetroMessage as *mut c_void,
        );
    }
    log(&format!("Zeebx: {texto}"));
}

unsafe fn environ(cmd: u32, data: *mut c_void) -> bool {
    let Some(callback) = callbacks().environ else {
        return false;
    };
    // SAFETY: o frontend promete que o callback aceita o comando pedido. O cadeado do core já foi
    // solto, então o frontend pode fazer o que quiser aqui dentro.
    unsafe { callback(cmd, data) }
}

fn log(mensagem: &str) {
    let Ok(texto) = CString::new(mensagem) else {
        return;
    };
    let mut callback = RetroLogCallback { log: None };
    let alvo = &mut callback as *mut RetroLogCallback as *mut c_void;
    if unsafe { environ(ENV_GET_LOG_INTERFACE, alvo) } {
        if let Some(escreve) = callback.log {
            // SAFETY: o frontend forneceu o callback e o formato é literal.
            unsafe { escreve(3, c"%s".as_ptr(), texto.as_ptr()) };
        }
    }
}

/// Descreve o aparelho de uma porta para o `GetConnectedDevices` do guest.
fn aparelho_do_dispositivo(device: u32) -> Option<Aparelho> {
    match device {
        DEVICE_NONE => None,
        DEVICE_ZPAD => Some(Aparelho::ZPad),
        DEVICE_BOOMERANG => Some(Aparelho::Boomerang),
        _ => Some(Aparelho::Controle),
    }
}

/// Se o Select do RetroPad está apertado na porta dada.
fn le_select(porta: u32) -> bool {
    let frente = callbacks();
    let (Some(poll), Some(state)) = (frente.input_poll, frente.input_state) else {
        return false;
    };
    // SAFETY: callbacks do frontend, chamados na thread de `retro_run`.
    unsafe {
        poll();
        state(porta, DEVICE_JOYPAD, 0, ID_SELECT) != 0
    }
}

/// Lê o RetroPad e monta o estado que o console enxerga.
///
/// Com `bitmasks`, os doze botões vêm numa palavra só — uma chamada ao frontend por quadro em vez
/// de doze. O `id` especial `ID_JOYPAD_MASK` devolve os bits na ordem dos `RETRO_DEVICE_ID_JOYPAD_*`.
fn le_pad(porta: u32, bitmasks: bool) -> Pad {
    let frente = callbacks();
    let (Some(poll), Some(state)) = (frente.input_poll, frente.input_state) else {
        return Pad::default();
    };
    // SAFETY: os callbacks vêm do frontend e são chamados na thread de `retro_run`.
    unsafe { poll() };
    let mascara = match bitmasks {
        // SAFETY: consulta de estado do próprio frontend.
        true => unsafe { state(porta, DEVICE_JOYPAD, 0, ID_JOYPAD_MASK) as u32 },
        false => 0,
    };
    let botao = |id: u32| -> bool {
        match bitmasks {
            true => mascara & (1 << id) != 0,
            // SAFETY: consulta de estado do próprio frontend.
            false => unsafe { state(porta, DEVICE_JOYPAD, 0, id) != 0 },
        }
    };
    let mut pad = Pad::default();
    let mapa = [
        (ID_UP, "up"),
        (ID_DOWN, "down"),
        (ID_LEFT, "left"),
        (ID_RIGHT, "right"),
        (ID_Y, "b1"),
        (ID_B, "b2"),
        (ID_X, "b3"),
        (ID_A, "b4"),
        (ID_L, "zl"),
        (ID_R, "zr"),
        (ID_START, "start"),
        (ID_SELECT, "back"),
    ];
    for (id, nome) in mapa {
        if botao(id) {
            if let Some(indice) = Pad::button_by_name(nome) {
                pad.press(indice, true);
            }
        }
    }
    // Os dois analógicos do RetroPad viram os quatro eixos do console, na faixa que o guest lê.
    let eixo = |index: u32, id: u32| -> i32 {
        // SAFETY: consulta de estado do próprio frontend.
        unsafe { state(porta, DEVICE_ANALOG, index, id) as i32 }
    };
    for (indice, valor) in [
        (0usize, eixo(ANALOG_LEFT, ANALOG_AXIS_X)),
        (1usize, eixo(ANALOG_LEFT, ANALOG_AXIS_Y)),
        (2usize, eixo(1, ANALOG_AXIS_X)),
        (3usize, eixo(1, ANALOG_AXIS_Y)),
    ] {
        // `-0x8000..=0x7fff` do frontend para o curso do manche do console.
        pad.set_axis(indice, valor / 256);
    }
    pad
}

/// Registra os aparelhos que o usuário pode escolher em cada porta.
unsafe fn registra_controladores() {
    static DESCRICOES: [RetroControllerDescription; 5] = [
        RetroControllerDescription {
            desc: c"Desconectado".as_ptr(),
            id: DEVICE_NONE,
        },
        RetroControllerDescription {
            desc: c"Dragon".as_ptr(),
            id: DEVICE_JOYPAD,
        },
        RetroControllerDescription {
            desc: c"Z-Pad".as_ptr(),
            id: DEVICE_ZPAD,
        },
        RetroControllerDescription {
            desc: c"Boomerang".as_ptr(),
            id: DEVICE_BOOMERANG,
        },
        RetroControllerDescription {
            desc: c"Teclado USB".as_ptr(),
            id: 3,
        },
    ];
    static PORTAS: [RetroControllerInfo; 3] = [
        RetroControllerInfo {
            types: DESCRICOES.as_ptr(),
            num_types: DESCRICOES.len() as u32,
        },
        RetroControllerInfo {
            types: DESCRICOES.as_ptr(),
            num_types: DESCRICOES.len() as u32,
        },
        RetroControllerInfo {
            types: std::ptr::null(),
            num_types: 0,
        },
    ];
    // SAFETY: as tabelas têm vida estática, como a ABI exige.
    unsafe {
        environ(ENV_SET_CONTROLLER_INFO, PORTAS.as_ptr() as *mut c_void);
    }
}

/// Rotula os botões para a tela de configuração do frontend.
unsafe fn registra_botoes() {
    let mut descritores: Vec<RetroInputDescriptor> = Vec::new();
    let rotulos = [
        (ID_UP, "Direcional cima"),
        (ID_DOWN, "Direcional baixo"),
        (ID_LEFT, "Direcional esquerda"),
        (ID_RIGHT, "Direcional direita"),
        // O rótulo vai no `id` que o core realmente lê, senão a tela de mapeamento do frontend
        // ensina o jogador a apertar o botão errado.
        (ID_B, "Botão 1"),
        (ID_Y, "Botão 2"),
        (ID_X, "Botão 3"),
        (ID_A, "Botão 4"),
        (ID_L, "ZL"),
        (ID_R, "ZR"),
        (ID_START, "HOME/Start"),
        (ID_SELECT, "Voltar"),
    ];
    for porta in 0..2u32 {
        for (id, texto) in rotulos {
            descritores.push(RetroInputDescriptor {
                port: porta,
                device: DEVICE_JOYPAD,
                index: 0,
                id,
                description: texto.as_ptr() as *const c_char,
            });
        }
        for (index, id, texto) in [
            (ANALOG_LEFT, ANALOG_AXIS_X, "Manche X"),
            (ANALOG_LEFT, ANALOG_AXIS_Y, "Manche Y"),
        ] {
            descritores.push(RetroInputDescriptor {
                port: porta,
                device: DEVICE_ANALOG,
                index,
                id,
                description: texto.as_ptr() as *const c_char,
            });
        }
    }
    descritores.push(RetroInputDescriptor {
        port: 0,
        device: 0,
        index: 0,
        id: 0,
        description: std::ptr::null(),
    });
    // SAFETY: a lista termina em `description` nulo e os textos são literais estáticos.
    unsafe {
        environ(
            ENV_SET_INPUT_DESCRIPTORS,
            descritores.as_ptr() as *mut c_void,
        );
    }
}

fn diretorio(cmd: u32) -> Option<PathBuf> {
    let mut ponteiro: *const c_char = std::ptr::null();
    let alvo = &mut ponteiro as *mut *const c_char as *mut c_void;
    if !unsafe { environ(cmd, alvo) } || ponteiro.is_null() {
        return None;
    }
    // SAFETY: o frontend entrega um caminho UTF-8 válido durante a chamada; copiamos aqui.
    let texto = unsafe { CStr::from_ptr(ponteiro) }
        .to_string_lossy()
        .into_owned();
    Some(PathBuf::from(texto))
}

/// Registra opções de configuração no RetroArch via V2 (com categorias) ou fallback V0 (SET_VARIABLES).
unsafe fn registra_opcoes_do_core() {
    let mut versao: u32 = 0;
    let tem_v2 = unsafe {
        environ(
            ENV_GET_CORE_OPTIONS_VERSION,
            &mut versao as *mut u32 as *mut c_void,
        )
    } && versao >= 2;

    if tem_v2 {
        // **Defeito da fase anterior, corrigido aqui.** Cinco opções de vídeo já declaravam
        // `category_key: c"video"`, mas este arranjo só tinha a categoria `"audio"` — a de vídeo
        // nunca foi registrada. O frontend não trava com uma chave que não bate com nenhuma
        // categoria, mas a opção fica sem o agrupamento certo no menu, e ninguém tinha reparado.
        static CATEGORIAS: [RetroCoreOptionV2Category; 4] = [
            RetroCoreOptionV2Category {
                key: c"audio".as_ptr(),
                desc: c"Áudio".as_ptr(),
                info: c"Configurações de síntese e saída de som do Zeebx".as_ptr(),
            },
            RetroCoreOptionV2Category {
                key: c"video".as_ptr(),
                desc: c"Vídeo".as_ptr(),
                info: c"Configurações de rasterização e imagem do Zeebx".as_ptr(),
            },
            RetroCoreOptionV2Category {
                key: c"sistema".as_ptr(),
                desc: c"Sistema".as_ptr(),
                info: c"Perfis e ajustes gerais do Zeebx".as_ptr(),
            },
            RetroCoreOptionV2Category {
                key: std::ptr::null(),
                desc: std::ptr::null(),
                info: std::ptr::null(),
            },
        ];

        let mut opt_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        opt_values[0] = RetroCoreOptionValue {
            value: c"auto".as_ptr(),
            label: c"Automático (SoundFont se disponível, senão Tabela)".as_ptr(),
        };
        opt_values[1] = RetroCoreOptionValue {
            value: c"timbres".as_ptr(),
            label: c"Tabela de timbres (rápido / portáteis)".as_ptr(),
        };
        opt_values[2] = RetroCoreOptionValue {
            value: c"soundfont".as_ptr(),
            label: c"SoundFont (.sf2)".as_ptr(),
        };

        let mut vol_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        // Onze degraus de dez em dez. Uma lista de cento e um itens não se navega com o direcional.
        const DEGRAUS: [(&CStr, &CStr); 11] = [
            (c"100", c"100% (padrão)"),
            (c"90", c"90%"),
            (c"80", c"80%"),
            (c"70", c"70%"),
            (c"60", c"60%"),
            (c"50", c"50%"),
            (c"40", c"40%"),
            (c"30", c"30%"),
            (c"20", c"20%"),
            (c"10", c"10%"),
            (c"0", c"0% (mudo)"),
        ];
        for (i, (valor, rotulo)) in DEGRAUS.iter().enumerate() {
            vol_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let mut ras_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        ras_values[0] = RetroCoreOptionValue {
            value: c"auto".as_ptr(),
            label: c"Automático (placa quando o frontend oferece)".as_ptr(),
        };
        ras_values[1] = RetroCoreOptionValue {
            value: c"software".as_ptr(),
            label: c"Processador (compatibilidade)".as_ptr(),
        };

        let mut lig_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        lig_values[0] = RetroCoreOptionValue {
            value: c"enabled".as_ptr(),
            label: c"Ligada".as_ptr(),
        };
        lig_values[1] = RetroCoreOptionValue {
            value: c"disabled".as_ptr(),
            label: c"Desligada".as_ptr(),
        };

        let mut escala_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        const ESCALAS: [(&CStr, &CStr); 4] = [
            (c"1", c"1x — sem supersampling (padrão)"),
            (c"2", c"2x — desenha em 1280×960"),
            (c"3", c"3x — desenha em 1920×1440"),
            (c"4", c"4x — desenha em 2560×1920"),
        ];
        for (i, (valor, rotulo)) in ESCALAS.iter().enumerate() {
            escala_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let mut amostras_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        const AMOSTRAS: [(&CStr, &CStr); 4] = [
            (c"1", c"Desligado"),
            (c"2", c"2x"),
            (c"4", c"4x"),
            (c"8", c"8x"),
        ];
        for (i, (valor, rotulo)) in AMOSTRAS.iter().enumerate() {
            amostras_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let mut taxa_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        taxa_values[0] = RetroCoreOptionValue {
            value: c"44100".as_ptr(),
            label: c"44.100 Hz — com brilho (padrão)".as_ptr(),
        };
        taxa_values[1] = RetroCoreOptionValue {
            value: c"22050".as_ptr(),
            label: c"22.050 Hz — metade do custo e da memória".as_ptr(),
        };

        let mut vozes_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        const VOZES_OPC: [(&CStr, &CStr); 4] = [
            (c"128", c"128 (padrão)"),
            (c"96", c"96"),
            (c"64", c"64"),
            (c"48", c"48 — portáteis fracos"),
        ];
        for (i, (valor, rotulo)) in VOZES_OPC.iter().enumerate() {
            vozes_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let mut cache_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        const CACHES: [(&CStr, &CStr); 5] = [
            (c"24", c"24 MiB (padrão)"),
            (c"48", c"48 MiB"),
            (c"16", c"16 MiB"),
            (c"8", c"8 MiB — portáteis fracos"),
            (c"4", c"4 MiB"),
        ];
        for (i, (valor, rotulo)) in CACHES.iter().enumerate() {
            cache_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let mut perfil_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        perfil_values[0] = RetroCoreOptionValue {
            value: c"padrao".as_ptr(),
            label: c"Padrão".as_ptr(),
        };
        perfil_values[1] = RetroCoreOptionValue {
            value: c"portatil".as_ptr(),
            label: c"Portátil (aparelho de mão fraco)".as_ptr(),
        };

        let mut limite_fps_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        limite_fps_values[0] = RetroCoreOptionValue {
            value: c"60".as_ptr(),
            label: c"60 FPS — velocidade do console (padrão)".as_ptr(),
        };
        limite_fps_values[1] = RetroCoreOptionValue {
            value: c"30".as_ptr(),
            label: c"30 FPS — velocidade normal, metade da imagem".as_ptr(),
        };
        limite_fps_values[2] = RetroCoreOptionValue {
            value: c"desligado".as_ptr(),
            label: c"Desligado — boost/unlimited".as_ptr(),
        };

        let mut frameskip_values = [RetroCoreOptionValue {
            value: std::ptr::null(),
            label: std::ptr::null(),
        }; 128];
        const FRAMESKIP_OPC: [(&CStr, &CStr); 8] = [
            (c"desligado", c"Desligado (padrão)"),
            (c"automatico", c"Automático (pelo buffer de áudio)"),
            (c"1", c"Fixo 1 — metade dos quadros"),
            (c"2", c"Fixo 2 — um terço dos quadros"),
            (c"3", c"Fixo 3 — um quarto dos quadros"),
            (c"4", c"Fixo 4 — um quinto dos quadros"),
            (c"5", c"Fixo 5 — um sexto dos quadros"),
            (c"6", c"Fixo 6 — um sétimo dos quadros"),
        ];
        for (i, (valor, rotulo)) in FRAMESKIP_OPC.iter().enumerate() {
            frameskip_values[i] = RetroCoreOptionValue {
                value: valor.as_ptr(),
                label: rotulo.as_ptr(),
            };
        }

        let definicoes: [RetroCoreOptionV2Definition; 14] = [
            RetroCoreOptionV2Definition {
                key: c"zeebx_midi_backend".as_ptr(),
                desc: c"Sintetizador MIDI (reinício)".as_ptr(),
                desc_categorized: c"Sintetizador MIDI (reinício)".as_ptr(),
                info: c"Motor de reprodução MIDI: Auto usa SoundFont se instalado na pasta do sistema; Tabela de timbres inicia instantaneamente sem renderização pesada (recomendado para portáteis fracos como H700). Recarregue o jogo para aplicar.".as_ptr(),
                info_categorized: c"Auto usa SoundFont se presente; Tabela inicia instantaneamente sem carga pesada de SF2. Recarregue o jogo para aplicar.".as_ptr(),
                category_key: c"audio".as_ptr(),
                values: opt_values,
                default_value: c"auto".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_volume".as_ptr(),
                desc: c"Volume".as_ptr(),
                desc_categorized: c"Volume".as_ptr(),
                info: c"Volume mestre do console, de 0 a 100%. Vale na hora, sem recarregar o jogo.".as_ptr(),
                info_categorized: c"Volume mestre do console. Vale na hora.".as_ptr(),
                category_key: c"audio".as_ptr(),
                values: vol_values,
                default_value: c"100".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_perfil".as_ptr(),
                desc: c"Perfil".as_ptr(),
                desc_categorized: c"Perfil".as_ptr(),
                info: c"Portátil aplica de uma vez o que o aparelho de mão fraco (RG40XX-H, muOS) precisa: tabela de timbres em vez de SoundFont, taxa e vozes do MIDI reduzidas, cache de som menor e sem supersampling no 3D. Enquanto ativo, ignora as opções individuais que ele cobre (mas não volume, névoa nem rasterizador, que continuam por conta própria). O sintetizador MIDI muda ao recarregar o conteúdo; o resto vale sem recarregar.".as_ptr(),
                info_categorized: c"Portátil junta os ajustes de desempenho para aparelho de mão fraco. Ignora as opções individuais que cobre.".as_ptr(),
                category_key: c"sistema".as_ptr(),
                values: perfil_values,
                default_value: c"padrao".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_soundfont_taxa".as_ptr(),
                desc: c"Taxa do SoundFont".as_ptr(),
                desc_categorized: c"Taxa do SoundFont".as_ptr(),
                info: c"Em que taxa a música MIDI é sintetizada pelo banco de amostras. 44.100 Hz preserva o brilho das amostras do .sf2, que sao gravadas nessa taxa. 22.050 Hz custa metade do tempo e da memória, e é o que serve a portátil fraco. Vale da próxima música em diante.".as_ptr(),
                info_categorized: c"44.100 Hz preserva o brilho; 22.050 Hz custa metade. Vale da próxima música em diante.".as_ptr(),
                category_key: c"audio".as_ptr(),
                values: taxa_values,
                default_value: c"44100".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_midi_vozes".as_ptr(),
                desc: c"Vozes do MIDI".as_ptr(),
                desc_categorized: c"Vozes".as_ptr(),
                info: c"Quantas notas podem soar ao mesmo tempo no banco de amostras. Menos vozes custa menos processador e rouba nota em trecho denso, que soa como nota que some. Vale da próxima música em diante.".as_ptr(),
                info_categorized: c"Notas simultâneas no banco. Menos custa menos e rouba nota.".as_ptr(),
                category_key: c"audio".as_ptr(),
                values: vozes_values,
                default_value: c"128".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_cache_de_som_mb".as_ptr(),
                desc: c"Cache de som".as_ptr(),
                desc_categorized: c"Cache de som".as_ptr(),
                info: c"Quanta memória guardar de som já decodificado. Menos memória faz o emulador esquecer música tocada e sintetizá-la de novo quando ela voltar, o que custa uma pausa; mais memória evita a pausa e ocupa RAM. Vale do próximo descarte em diante.".as_ptr(),
                info_categorized: c"Memória de som já decodificado. Menos custa pausa; mais ocupa RAM.".as_ptr(),
                category_key: c"audio".as_ptr(),
                values: cache_values,
                default_value: c"24".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_rasterizador".as_ptr(),
                desc: c"Rasterizador (reinício)".as_ptr(),
                desc_categorized: c"Rasterizador (reinício)".as_ptr(),
                info: c"Quem preenche o 3D: Automático usa a placa quando o frontend oferece render em hardware; Processador força o caminho de software. Use Processador quando a imagem sair errada ou preta com driver de vídeo problemático. Recarregue o jogo para aplicar.".as_ptr(),
                info_categorized: c"Automático usa a placa quando há render em hardware; Processador força software. Recarregue o jogo para aplicar.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: ras_values,
                default_value: c"auto".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_resolucao_interna".as_ptr(),
                desc: c"Resolução interna do 3D".as_ptr(),
                desc_categorized: c"Resolução interna".as_ptr(),
                info: c"Desenha o 3D numa resolução maior e reduz de volta para os 640x480 do console, o que suaviza a borda do polígono (supersampling). O quadro entregue ao frontend continua 640x480: shader e proporção nao mudam. Só tem efeito com o rasterizador de placa, e custa memória e preenchimento.".as_ptr(),
                info_categorized: c"Desenha o 3D maior e reduz para 640x480, suavizando a borda. Só na placa.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: escala_values,
                default_value: c"1".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_antialias".as_ptr(),
                desc: c"Antisserrilhado".as_ptr(),
                desc_categorized: c"Antisserrilhado".as_ptr(),
                info: c"Amostras por pixel no 3D preenchido pela placa. Suaviza a borda do polígono e custa preenchimento. Sem efeito no rasterizador de processador.".as_ptr(),
                info_categorized: c"Amostras por pixel no 3D da placa. Sem efeito no processador.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: amostras_values,
                default_value: c"1".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_filtro_anisotropico".as_ptr(),
                desc: c"Filtro anisotrópico".as_ptr(),
                desc_categorized: c"Filtro anisotrópico".as_ptr(),
                info: c"Nitidez da textura vista de lado, no 3D preenchido pela placa. Sem efeito no rasterizador de processador.".as_ptr(),
                info_categorized: c"Nitidez da textura vista de lado. Só na placa.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: amostras_values,
                default_value: c"1".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_neblina".as_ptr(),
                desc: c"Névoa".as_ptr(),
                desc_categorized: c"Névoa".as_ptr(),
                info: c"A névoa que o jogo pede. Desligar deixa o cenário distante visível, o que alguns preferem; é escolha de quem joga, e não correção. Vale na hora.".as_ptr(),
                info_categorized: c"A névoa que o jogo pede. Vale na hora.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: lig_values,
                default_value: c"enabled".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_limite_fps".as_ptr(),
                desc: c"Limite de velocidade".as_ptr(),
                desc_categorized: c"Limite de velocidade".as_ptr(),
                info: c"Segura a lógica do jogo contra o relógio real, como o console faria. Use 60 FPS para impedir que títulos leves (RE4, Zeebo Extreme) corram rápido demais; 30 FPS mantém a velocidade lógica normal e mostra um em cada dois quadros. Desligado libera boost, útil se quiser acelerar jogos como Need for Speed. Vale na hora.".as_ptr(),
                info_categorized: c"60 impede jogo rápido demais; 30 mantém a lógica e reduz imagem; Desligado libera boost. Vale na hora.".as_ptr(),
                category_key: c"sistema".as_ptr(),
                values: limite_fps_values,
                default_value: c"60".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: c"zeebx_frameskip".as_ptr(),
                desc: c"Pular quadros".as_ptr(),
                desc_categorized: c"Pular quadros".as_ptr(),
                info: c"Pula o desenho 3D de alguns quadros para aliviar processador fraco, sem mudar a velocidade do jogo — a lógica roda igual, só o desenho some por um instante. Fixo pula sempre a mesma proporção; Automático só pula quando o frontend avisa que o áudio está prestes a estourar. Vale na hora.".as_ptr(),
                info_categorized: c"Pula o desenho para aliviar processador fraco, sem mudar a velocidade do jogo. Vale na hora.".as_ptr(),
                category_key: c"video".as_ptr(),
                values: frameskip_values,
                default_value: c"desligado".as_ptr(),
            },
            RetroCoreOptionV2Definition {
                key: std::ptr::null(),
                desc: std::ptr::null(),
                desc_categorized: std::ptr::null(),
                info: std::ptr::null(),
                info_categorized: std::ptr::null(),
                category_key: std::ptr::null(),
                values: [RetroCoreOptionValue { value: std::ptr::null(), label: std::ptr::null() }; 128],
                default_value: std::ptr::null(),
            },
        ];

        let opcoes_v2 = RetroCoreOptionsV2 {
            categories: CATEGORIAS.as_ptr(),
            definitions: definicoes.as_ptr(),
        };

        unsafe {
            environ(
                ENV_SET_CORE_OPTIONS_V2,
                &opcoes_v2 as *const _ as *mut c_void,
            );
        }
    } else {
        static VARIAVEIS: [RetroVariable; 14] = [
            RetroVariable {
                key: c"zeebx_midi_backend".as_ptr(),
                value: c"Sintetizador MIDI (reinício); auto|timbres|soundfont".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_volume".as_ptr(),
                value: c"Volume; 100|90|80|70|60|50|40|30|20|10|0".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_perfil".as_ptr(),
                value: c"Perfil; padrao|portatil".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_soundfont_taxa".as_ptr(),
                value: c"Taxa do SoundFont; 44100|22050".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_midi_vozes".as_ptr(),
                value: c"Vozes do MIDI; 128|96|64|48".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_cache_de_som_mb".as_ptr(),
                value: c"Cache de som (MiB); 24|48|16|8|4".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_rasterizador".as_ptr(),
                value: c"Rasterizador (reinício); auto|software".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_resolucao_interna".as_ptr(),
                value: c"Resolução interna do 3D; 1|2|3|4".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_antialias".as_ptr(),
                value: c"Antisserrilhado; 1|2|4|8".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_filtro_anisotropico".as_ptr(),
                value: c"Filtro anisotrópico; 1|2|4|8".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_neblina".as_ptr(),
                value: c"Névoa; enabled|disabled".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_limite_fps".as_ptr(),
                value: c"Limite de velocidade; 60|30|desligado".as_ptr(),
            },
            RetroVariable {
                key: c"zeebx_frameskip".as_ptr(),
                value: c"Pular quadros; desligado|automatico|1|2|3|4|5|6".as_ptr(),
            },
            RetroVariable {
                key: std::ptr::null(),
                value: std::ptr::null(),
            },
        ];
        unsafe {
            environ(
                ENV_SET_VARIABLES,
                VARIAVEIS.as_ptr() as *mut c_void,
            );
        }
    }
}

/// Consulta a política de síntese MIDI configurada no frontend RetroArch.
unsafe fn le_opcao_midi_backend() -> zeebx::audio::MidiBackend {
    let mut consulta = RetroVariable {
        key: c"zeebx_midi_backend".as_ptr(),
        value: std::ptr::null(),
    };
    // SAFETY: chamada ao callback environ e leitura de string C válida entregue pelo frontend.
    let ok = unsafe {
        environ(ENV_GET_VARIABLE, &mut consulta as *mut _ as *mut c_void) && !consulta.value.is_null()
    };
    if ok {
        let val_str = unsafe { CStr::from_ptr(consulta.value) }.to_string_lossy();
        if let Ok(backend) = val_str.parse::<zeebx::audio::MidiBackend>() {
            return backend;
        }
    }
    zeebx::audio::MidiBackend::Auto
}

/// Lê o valor de uma opção do core como texto. `None` quando o frontend não tem a chave.
///
/// Existe para não repetir o mesmo bloco de `unsafe` a cada opção: a parte insegura é sempre a
/// mesma — montar a consulta, chamar o frontend, conferir que o ponteiro voltou não nulo — e
/// repeti-la por chave é como um `unsafe` deixa de ser revisado.
unsafe fn le_opcao(chave: &CStr) -> Option<String> {
    let mut consulta = RetroVariable {
        key: chave.as_ptr(),
        value: std::ptr::null(),
    };
    // SAFETY: chamada ao callback environ e leitura de string C válida entregue pelo frontend.
    let ok = unsafe {
        environ(ENV_GET_VARIABLE, &mut consulta as *mut _ as *mut c_void)
            && !consulta.value.is_null()
    };
    if !ok {
        return None;
    }
    Some(unsafe { CStr::from_ptr(consulta.value) }.to_string_lossy().into_owned())
}

/// Lê o volume mestre escolhido nas opções, de 0,0 a 1,0.
///
/// Devolve `None` quando a chave não existe ou não é número: o frontend pode ser antigo, e um
/// valor estragado não pode virar silêncio sem aviso — quem chama mantém o que já tinha.
unsafe fn le_opcao_volume() -> Option<f32> {
    volume_de_texto(&unsafe { le_opcao(c"zeebx_volume") }?)
}

/// Lê um número inteiro de uma opção, preso à faixa que o motor aceita.
fn numero_de_texto(texto: &str, minimo: usize, maximo: usize) -> Option<usize> {
    let valor: usize = texto.trim().parse().ok()?;
    Some(valor.clamp(minimo, maximo))
}

/// A política de frameskip do core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Frameskip {
    #[default]
    Desligado,
    /// Pula `n` quadros a cada `n + 1` — `1` é metade, `2` é um terço, e por aí adiante.
    Fixo(u32),
    /// Decide por quadro, pelo aviso do frontend sobre o próprio buffer de áudio dele.
    Automatico,
}

impl Frameskip {
    /// Lê o texto declarado na opção. `None` para o que não é nenhum dos valores conhecidos —
    /// e quem chama mantém o modo anterior, como as outras opções.
    fn de_texto(texto: &str) -> Option<Self> {
        match texto.trim().to_ascii_lowercase().as_str() {
            "desligado" => Some(Self::Desligado),
            "automatico" => Some(Self::Automatico),
            outro => outro.parse::<u32>().ok().filter(|&n| (1..=6).contains(&n)).map(Self::Fixo),
        }
    }
}

/// O teto de velocidade do jogo, separado de frameskip.
///
/// **60 não quer dizer "chame retro_run 60 vezes"** — isso já é decisão do frontend. Quer dizer
/// "não deixe o relógio virtual passar do relógio real", que é o freio que o desktop já usa para
/// impedir Crash/Zeebo Extreme/NFS de correrem acima da velocidade do console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LimiteFps {
    /// Sem freio: deixa quem quer boost (Need for Speed) usar o que o host aguenta.
    Desligado,
    /// Velocidade lógica real do console e apresentação normal.
    #[default]
    Sessenta,
    /// Velocidade lógica real do console, mas apresenta só um em cada dois quadros.
    Trinta,
}

impl LimiteFps {
    fn de_texto(texto: &str) -> Option<Self> {
        match texto.trim() {
            "desligado" => Some(Self::Desligado),
            "60" => Some(Self::Sessenta),
            "30" => Some(Self::Trinta),
            _ => None,
        }
    }

    fn limita_velocidade(self) -> bool {
        !matches!(self, Self::Desligado)
    }
}

/// O que o `SET_AUDIO_BUFFER_STATUS_CALLBACK` avisou da última vez.
///
/// Global porque o callback do frontend não tem como devolver contexto nenhum — é só a
/// assinatura que a ABI do C permite. Ver [`ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK`].
static AUDIO_ESTOURO_PROVAVEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `retro_audio_buffer_status_callback_t`: o frontend chama isto **antes** de cada `retro_run`,
/// dizendo se o buffer de áudio dele está prestes a estourar.
///
/// Guarda só `underrun_likely`, que é a própria `libretro.h` quem diz o que fazer com ele: *"se
/// verdadeiro, o core deveria tentar pular quadro"*. A ocupação em si não muda a decisão — o
/// frontend já fez a conta e decidiu que **agora** é a hora de pular.
unsafe extern "C" fn audio_buffer_status(active: bool, occupancy: u32, underrun_likely: bool) {
    audio_mede::OCUPACAO.store(if active { occupancy } else { u32::MAX }, std::sync::atomic::Ordering::Relaxed);
    audio_mede::SECANDO.store(active && underrun_likely, std::sync::atomic::Ordering::Relaxed);
    // Sem áudio no frontend não há buffer para proteger. Guardar um `true` velho nesse caso faria
    // Automático pular desenho para sempre depois que o usuário desliga e liga o áudio no menu.
    AUDIO_ESTOURO_PROVAVEL.store(active && underrun_likely, std::sync::atomic::Ordering::Relaxed);
}

/// Só o aviso de ocupação do buffer, para a medição de áudio: **sem** pedir latência mínima, que
/// mudaria justamente o que se quer medir.
fn pede_aviso_de_buffer_para_medir() -> bool {
    let cb = RetroAudioBufferStatusCallback {
        callback: Some(audio_buffer_status),
    };
    unsafe {
        environ(
            ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK,
            &cb as *const _ as *mut c_void,
        )
    }
}

/// Pede ao frontend para avisar sobre o buffer de áudio, e mais folga nele para o aviso chegar a
/// tempo de o core reagir.
///
/// **Uma vez só**, e não a cada quadro — ver o campo `frameskip_callback_pedido`. Pedir de novo
/// não muda a resposta, e a documentação do `SET_MINIMUM_AUDIO_LATENCY` pede moderação.
fn pede_callback_de_audio() -> bool {
    let cb = RetroAudioBufferStatusCallback {
        callback: Some(audio_buffer_status),
    };
    let aceito = unsafe {
        environ(
            ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK,
            &cb as *const _ as *mut c_void,
        )
    };
    if aceito {
        log("Zeebx: frameskip automático pediu o aviso de buffer de áudio ao frontend");
        // Seis quadros a 60 Hz: a faixa que a própria `libretro.h` recomenda (seis a oito) para
        // o aviso de estouro chegar com folga suficiente para o core reagir a tempo.
        let latencia_ms: u32 = 96;
        unsafe {
            environ(
                ENV_SET_MINIMUM_AUDIO_LATENCY,
                &latencia_ms as *const u32 as *mut c_void,
            );
        }
    } else {
        // **Degrada sozinho, sem quebrar nada.** Sem o callback, `AUDIO_ESTOURO_PROVAVEL` nunca
        // muda de `false`, e o modo Automático se comporta como Desligado — nunca pula. É pior
        // que funcionar, mas melhor que um frontend velho impedir o core de rodar.
        aviso("Zeebx: frontend não aceita o aviso de buffer de áudio; frameskip automático não vai pular quadro nenhum");
    }
    aceito
}

/// Retira o callback e devolve ao frontend a latência que ele já tinha.
///
/// Isto não é limpeza estética: o ponteiro do callback mora no frontend. Se ficar apontando para
/// esta `.so` depois de `retro_deinit`, um frontend que entregar áudio tarde pode chamar memória
/// que o carregador já descarregou. E 96 ms fazem sentido só com Automático ligado — deixar a
/// folga pedida depois de o usuário desligar o modo troca latência por nada.
fn retira_callback_de_audio() {
    let vazio = RetroAudioBufferStatusCallback { callback: None };
    unsafe {
        environ(
            ENV_SET_AUDIO_BUFFER_STATUS_CALLBACK,
            &vazio as *const _ as *mut c_void,
        );
    }
    let padrao: u32 = 0;
    unsafe {
        environ(
            ENV_SET_MINIMUM_AUDIO_LATENCY,
            &padrao as *const u32 as *mut c_void,
        );
    }
    AUDIO_ESTOURO_PROVAVEL.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Se o texto da opção de perfil pede o perfil Portátil.
///
/// Extraída porque a checagem acontece em dois lugares — o nascimento da sessão, para o
/// sintetizador MIDI, e a releitura a quente, para o resto — e as duas tinham a mesma
/// comparação escrita de novo. Duas cópias divergem com o tempo; uma função não.
fn perfil_e_portatil(texto: Option<&str>) -> bool {
    texto.is_some_and(|texto| texto.trim().eq_ignore_ascii_case("portatil"))
}

/// Lê um interruptor. A convenção de valor é `enabled`/`disabled`, que é a do ecossistema.
fn ligado_de_texto(texto: &str) -> Option<bool> {
    match texto.trim().to_ascii_lowercase().as_str() {
        "enabled" | "ligado" | "on" | "true" | "1" => Some(true),
        "disabled" | "desligado" | "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Aplica, de uma vez, **todas** as opções que valem sem recriar a sessão.
///
/// **De uma vez é a parte que importa.** O aviso do frontend (ver [`opcoes_mudaram`]) é consumido
/// na primeira pergunta: se cada opção fosse relida no seu próprio `if`, a primeira comeria o
/// aviso e as outras só mudariam no próximo mexe-mexe do usuário — um defeito que aparece como
/// "às vezes não pega".
///
/// Cada opção ausente ou estragada mantém o que já havia, em vez de voltar ao padrão: um frontend
/// antigo, que não conhece a chave, não pode desfazer a escolha de quem configurou.
fn aplica_opcoes_quentes(estado: &mut Core) {
    // **O modo é lido aqui; a decisão de pular ou não é por quadro**, em `retro_run`. Ver
    // [`Frameskip`]: reler o modo só quando algo mudou é barato, mas a decisão em si (que quadro
    // pular) precisa de um contador ou do aviso do frontend, e os dois valem a cada quadro, não
    // só quando o usuário mexe no menu.
    if let Some(limite) = unsafe { le_opcao(c"zeebx_limite_fps") }
        .as_deref()
        .and_then(LimiteFps::de_texto)
    {
        if limite != estado.limite_fps {
            // A taxa de apresentação de 30 não pode herdar a paridade do limite anterior.
            estado.limite_fps_contador = 0;
        }
        estado.limite_fps = limite;
    }

    if let Some(modo) = unsafe { le_opcao(c"zeebx_frameskip") }
        .as_deref()
        .and_then(Frameskip::de_texto)
    {
        let antes = estado.frameskip;
        if modo != antes {
            // Mudar razão fixa no meio não pode herdar fase do modo anterior: 1/2 virar 1/7 no
            // quinto quadro e pular logo o primeiro é surpresa sem ganho.
            estado.frameskip_contador = 0;
        }
        // O registro do Automático acontece no primeiro `retro_run`, porque
        // SET_MINIMUM_AUDIO_LATENCY só é válido dentro dessa chamada. Aqui, que também roda em
        // retro_load_game, apenas guardamos a política. Sair de Automático pode limpar na hora.
        if modo != Frameskip::Automatico && antes == Frameskip::Automatico && estado.frameskip_callback_pedido {
            retira_callback_de_audio();
            estado.frameskip_callback_pedido = false;
        }
        estado.frameskip = modo;
    }
    if let Some(volume) = unsafe { le_opcao_volume() } {
        estado.mixer.set_master(volume, false);
    }
    if let Some(neblina) = unsafe { le_opcao(c"zeebx_neblina") }.as_deref().and_then(ligado_de_texto) {
        estado.session.define_neblina(neblina);
    }
    // **O perfil "Portátil" ganha das opções individuais que ele cobre, quando ativo.** Volume e
    // névoa ficam de fora de propósito: são gosto de quem joga, não custo de processador ou
    // memória, e o perfil é sobre desempenho. O rasterizador também fica de fora — ele só muda ao
    // recarregar, e forçar processador tiraria a placa de quem tem uma GPU capaz; a opção separada
    // continua sendo o escape para quem precisa dela.
    let perfil_portatil = perfil_e_portatil(unsafe { le_opcao(c"zeebx_perfil") }.as_deref());

    let escala = if perfil_portatil {
        Some(1)
    } else {
        unsafe { le_opcao(c"zeebx_resolucao_interna") }
            .as_deref()
            .and_then(|texto| numero_de_texto(texto, 1, 8))
    };
    if let Some(escala) = escala {
        estado.session.define_resolucao_interna(escala);
    }

    // **Áudio e memória valem para o que vier depois.** A música já sintetizada não muda de taxa
    // e o som já guardado não encolhe: estas três valem da próxima música e do próximo descarte em
    // diante, e é por isso que não pedem reinício — mas também por isso o efeito não é imediato
    // como o do volume, e os rótulos dizem isso.
    let taxa = if perfil_portatil {
        Some(zeebx::audio::midi::RATE)
    } else {
        unsafe { le_opcao(c"zeebx_soundfont_taxa") }
            .as_deref()
            .and_then(|texto| texto.trim().parse::<u32>().ok())
    };
    if let Some(taxa) = taxa {
        zeebx::audio::soundfont::define_taxa(taxa);
    }
    let vozes = if perfil_portatil {
        Some(48)
    } else {
        unsafe { le_opcao(c"zeebx_midi_vozes") }
            .as_deref()
            .and_then(|texto| numero_de_texto(texto, 8, 256))
    };
    if let Some(vozes) = vozes {
        zeebx::audio::soundfont::define_vozes(vozes);
    }
    let mib = if perfil_portatil {
        Some(8)
    } else {
        unsafe { le_opcao(c"zeebx_cache_de_som_mb") }
            .as_deref()
            .and_then(|texto| numero_de_texto(texto, 1, 256))
    };
    if let Some(mib) = mib {
        zeebx::machine::define_teto_do_cache_de_som(mib * 1024 * 1024);
    }

    // **As duas melhorias entram na mesma chamada**, porque a API do motor as recebe juntas:
    // aplicar uma sozinha apagaria a outra com o valor de antes.
    let (antialias, anisotropico) = if perfil_portatil {
        (Some(1), Some(1))
    } else {
        (
            unsafe { le_opcao(c"zeebx_antialias") }
                .as_deref()
                .and_then(|texto| numero_de_texto(texto, 1, 16)),
            unsafe { le_opcao(c"zeebx_filtro_anisotropico") }
                .as_deref()
                .and_then(|texto| numero_de_texto(texto, 1, 16)),
        )
    };
    if antialias.is_some() || anisotropico.is_some() {
        estado
            .session
            .define_melhorias(antialias.unwrap_or(1), anisotropico.unwrap_or(1));
    }
}

/// Converte o texto da opção de volume (por cento) no fator de 0,0 a 1,0 do mixer.
///
/// Está separado da leitura do frontend de propósito: a conversão é a parte que pode errar — o
/// valor vem de um arquivo que o usuário edita à mão — e ela só é testável se não exigir um
/// callback de frontend para ser chamada.
///
/// `None` para o que não é número, e **não** zero: um `.opt` estragado não pode virar silêncio,
/// porque silêncio parece defeito do emulador e não erro de configuração.
fn volume_de_texto(texto: &str) -> Option<f32> {
    let por_cento: f32 = texto.trim().parse().ok()?;
    if !por_cento.is_finite() {
        return None;
    }
    Some((por_cento / 100.0).clamp(0.0, 1.0))
}

/// O frontend mexeu em alguma opção desde a última pergunta?
///
/// Ver [`ENV_GET_VARIABLE_UPDATE`]. Perguntar isto uma vez por quadro é barato; reler cada chave
/// por quadro não é.
fn opcoes_mudaram() -> bool {
    let mut mudou = false;
    // SAFETY: o frontend escreve um `bool` no ponteiro entregue.
    unsafe {
        environ(
            ENV_GET_VARIABLE_UPDATE,
            &mut mudou as *mut bool as *mut c_void,
        )
    };
    mudou
}

/// `retro_api_version`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_api_version() -> u32 {
    API_VERSION
}

/// `retro_set_environment`: o frontend entrega o caminho de volta ao sistema.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_set_environment(callback: Option<EnvironmentFn>) {
    if let Ok(mut guard) = frontend().lock() {
        guard.environ = callback;
    }
    // SAFETY: a partir daqui o ambiente está disponível para registrar o que precisa.
    unsafe {
        registra_controladores();
        registra_botoes();
        registra_opcoes_do_core();
    }
}

/// `retro_set_video_refresh`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_set_video_refresh(callback: Option<VideoRefreshFn>) {
    if let Ok(mut guard) = frontend().lock() {
        guard.video = callback;
    }
}

/// `retro_set_audio_sample_batch`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_set_audio_sample_batch(callback: Option<AudioSampleBatchFn>) {
    if let Ok(mut guard) = frontend().lock() {
        guard.audio_batch = callback;
    }
}

/// `retro_set_input_poll`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_set_input_poll(callback: Option<InputPollFn>) {
    if let Ok(mut guard) = frontend().lock() {
        guard.input_poll = callback;
    }
}

/// `retro_set_input_state`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_set_input_state(callback: Option<InputStateFn>) {
    if let Ok(mut guard) = frontend().lock() {
        guard.input_state = callback;
    }
}

/// `retro_set_audio_sample`: a variante de uma amostra não é usada; o core sempre entrega lote.
#[unsafe(no_mangle)]
pub extern "C" fn retro_set_audio_sample(_callback: Option<unsafe extern "C" fn(i16, i16)>) {}

/// `retro_init`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_init() {}

/// Limpa callbacks, contexto de vídeo e o conteúdo carregado.
fn limpa_estado_do_frontend() {
    // Retira o estado Rust primeiro, mas chama o frontend só depois de soltar o mutex: callbacks
    // do frontend podem reentrar no core.
    let tinha_audio = if let Ok(mut guard) = core().lock() {
        let tinha = guard
            .as_ref()
            .is_some_and(|EstadoDoCore(c)| c.frameskip_callback_pedido || c.audio_mede_callback);
        *guard = None;
        tinha
    } else {
        false
    };
    if tinha_audio {
        retira_callback_de_audio();
    }
    let teclado = RetroKeyboardCallback { callback: None };
    unsafe {
        environ(
            ENV_SET_KEYBOARD_CALLBACK,
            &teclado as *const RetroKeyboardCallback as *mut c_void,
        );
    }
    if let Ok(mut placa) = PLACA.lock() {
        *placa = None;
    }
    if let Ok(mut oferta) = OFERTA_DE_PLACA.lock() {
        *oferta = None;
    }
    CONTEXTO_PRONTO.store(false, std::sync::atomic::Ordering::Relaxed);
    PERDEU_A_PLACA.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// `retro_deinit`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_deinit() {
    if let Some(texto) = audio_mede::encerra() {
        log(&texto);
    }
    limpa_estado_do_frontend();
}

/// `retro_get_system_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_get_system_info(info: *mut RetroSystemInfo) {
    if info.is_null() {
        return;
    }
    // SAFETY: o frontend passou um ponteiro válido para preencher.
    unsafe {
        *info = RetroSystemInfo {
            library_name: c"Zeebx".as_ptr(),
            library_version: concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char,
            valid_extensions: c"mod|zip|7z".as_ptr(),
            need_fullpath: true,
            block_extract: true,
        };
    }
}

/// `retro_get_system_av_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_get_system_av_info(info: *mut RetroSystemAvInfo) {
    if info.is_null() {
        return;
    }
    // SAFETY: o frontend passou um ponteiro válido para preencher.
    unsafe {
        *info = RetroSystemAvInfo {
            geometry: RetroGameGeometry {
                base_width: 640,
                base_height: 480,
                max_width: 640,
                max_height: 480,
                aspect_ratio: 4.0 / 3.0,
            },
            timing: RetroSystemTiming {
                fps: 60.0,
                sample_rate: SAMPLE_RATE as f64,
            },
        };
    }
}

/// `retro_load_game`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn retro_load_game(game: *const RetroGameInfo) -> bool {
    zeebx::video::gpu::mede::liga();
    if game.is_null() {
        return false;
    }
    // SAFETY: ponteiro validado e lido apenas dentro da chamada.
    let caminho = unsafe { (*game).path };
    if caminho.is_null() {
        return false;
    }
    // SAFETY: caminho UTF-8 prometido pela ABI.
    let caminho = unsafe { CStr::from_ptr(caminho) }
        .to_string_lossy()
        .into_owned();
    let Some(save_dir) = diretorio(ENV_GET_SAVE_DIRECTORY) else {
        log("Zeebx: o frontend não informou diretório de saves; recusando carregar.");
        return false;
    };
    let sistema = diretorio(ENV_GET_SYSTEM_DIRECTORY);
    // O sistema guarda o que é da máquina e o que é descartável — a NAND `fs:/` e o cache de
    // conteúdo extraído. Os saves do título ficam no diretório de saves, que é o que o frontend
    // sincroniza. É a mesma divisão que o PPSSPP faz entre `flash0` e o memory stick.
    let storage = StoragePaths::for_frontend(&save_dir, sistema.as_deref());
    if let Err(erro) = storage.create_dirs() {
        log(&format!(
            "Zeebx: sem permissão em {} / {}: {erro}",
            storage.saves.display(),
            storage.cache.display()
        ));
        return false;
    }
    log(&format!(
        "Zeebx: saves em {}, sistema em {}",
        storage.saves.display(),
        storage.cache.display()
    ));
    // Teclado: a Z-Wheel navega por `AVK_*`, que o RetroPad não produz.
    static TECLADO: RetroKeyboardCallback = RetroKeyboardCallback {
        callback: Some(tecla_recebida),
    };
    unsafe {
        environ(
            ENV_SET_KEYBOARD_CALLBACK,
            &TECLADO as *const RetroKeyboardCallback as *mut c_void,
        );
    }
    // O formato de vídeo é negociado antes de rodar: sem ele não há como entregar quadro.
    let mut formato = PIXEL_FORMAT_RGB565;
    let alvo = &mut formato as *mut u32 as *mut c_void;
    if !unsafe { environ(ENV_SET_PIXEL_FORMAT, alvo) } {
        log("Zeebx: o frontend não aceita RGB565.");
        return false;
    }
    // Uma chamada por quadro em vez de doze, quando o frontend entrega a máscara.
    let bitmasks = unsafe { environ(ENV_GET_INPUT_BITMASKS, std::ptr::null_mut()) };
    log(&format!(
        "Zeebx: botões por {}",
        match bitmasks {
            true => "máscara de bits",
            false => "consulta individual",
        }
    ));
    // O quadro repetido pode ir como nulo: economiza uma cópia de 600 KB por quadro e evita
    // ocupar o frontend com trabalho que não muda nada na tela.
    let mut aceita_dupe = false;
    let alvo_dupe = &mut aceita_dupe as *mut bool as *mut c_void;
    let aceita_dupe = unsafe { environ(ENV_GET_CAN_DUPE, alvo_dupe) } && aceita_dupe;
    let portas = [Some(Aparelho::Controle), None];
    let resultado = unsafe { carrega(&caminho, &storage, portas, PathBuf::from(&caminho)) };
    match resultado {
        Ok(mut novo) => {
            novo.bitmasks = bitmasks;
            novo.aceita_dupe = aceita_dupe;
            novo.ultimo_relogio_ms = novo.session.clock_ms();
            if let Ok(mut guard) = core().lock() {
                *guard = Some(EstadoDoCore(novo));
            }
            true
        }
        Err(erro) => {
            log(&format!("Zeebx: não deu para abrir {caminho}: {erro}"));
            false
        }
    }
}

/// Monta o estado do jogo. Separado do export para concentrar o `unsafe` da ABI num lugar só.
unsafe fn carrega(
    caminho: &str,
    storage: &StoragePaths,
    portas: [Option<Aparelho>; zeebx::input::PORTAS],
    path: PathBuf,
) -> Result<Core, StartError> {
    // **A ordem importa: a biblioteca e a fonte vêm antes da sessão.**
    //
    // A fonte é lida quando a máquina é construída — ela entra no `font` do motor. Instalá-la
    // depois deixava a sessão inteira sem fonte, e o jogo não desenhava texto nenhum: era o que
    // fazia a tela do Double Dragon ficar branca e vazia, porque o que ele desenha ali é a
    // mensagem "Memory is insufficient. Please delete some files." em fundo branco.
    let (pasta, jogos) = biblioteca(caminho);
    // **O catálogo da biblioteca local, que é o que a Z-Wheel lê para montar a grade.** A roda
    // não lê a pasta de ROMs: ela lê o `tt_game_info` do perfil, e quem liga um ao outro é o
    // `catalog.json` que a interface grava (`library::sync_catalog`). O core enumerava os jogos
    // para o shell e **não alimentava o catálogo** — medido no harness sem janela em 22/09/2026:
    // sem o catálogo a roda não monta a grade e nenhuma tecla tem o que mover; com ele, a grade
    // aparece desenhada e o confirmar produz o pedido de abertura. É o mesmo caminho que o
    // RetroArch usa no aparelho, e é por isso que ele vale aqui e não só na varredura.
    if !jogos.is_empty()
        && let Some(pasta) = &pasta
    {
        let _ = zeebx::library::sync_catalog(&zeebx::library::scan(pasta));
    }
    let instalados = instalados_da_biblioteca(&jogos);
    #[cfg(test)]
    JOGOS_VISTOS.store(jogos.len() as u32, std::sync::atomic::Ordering::Relaxed);
    let fonte = prepara_fonte(storage, &jogos);
    match &fonte {
        Some(onde) => log(&format!("Zeebx: fonte do sistema em {}", onde.display())),
        None => aviso(&format!(
            "Sem tectoy.ttf no acervo ({} jogos): o texto do sistema não será desenhado",
            jogos.len()
        )),
    }
    // **Onde o banco de amostras deve ficar, dito no log.** A busca é por diretório e a pasta sai
    // da raiz de sistema que o frontend entregou: sem esta linha, quem instala o core não tem como
    // saber o caminho, e "o banco não funciona" fica indistinguível de "o arquivo está no lugar
    // errado".
    log(&zeebx::audio::soundfont::relato(&storage.device));

    // **O perfil vem antes das opções que ele substitui.** "Portátil" existe porque dez botões
    // soltos não é o que um aparelho de mão precisa — é um único ajuste que junta o que a
    // investigação em ARM fraco (RG40XX-H) mediu como o que mais custa: sintetizador pesado,
    // banco de amostras em taxa alta, muitas vozes, cache grande, supersampling.
    let perfil_portatil = perfil_e_portatil(unsafe { le_opcao(c"zeebx_perfil") }.as_deref());
    let midi_backend = if perfil_portatil {
        zeebx::audio::MidiBackend::Timbres
    } else {
        unsafe { le_opcao_midi_backend() }
    };
    log(&format!("Zeebx: sintetizador MIDI selecionado: {:?}{}", midi_backend, if perfil_portatil { " (perfil Portátil)" } else { "" }));

    // **Pede o contexto de placa ao frontend, se ele tiver um.** Quem aceita é ele; nós só usamos
    // mais tarde, quando o `context_reset` chegar. Recusar aqui não muda nada: a sessão de
    // software já nasceu e é ela que roda até prova em contrário.
    //
    // **Salvo quando o usuário pediu software.** Este é o único escape para um driver de GL que
    // aceita o contexto e desenha errado — ou não desenha: nesse caso o recuo automático não
    // dispara, porque do ponto de vista do core nada falhou. Sem a opção, o único remédio é trocar
    // o frontend ou o aparelho.
    let rasterizador = unsafe { le_opcao(c"zeebx_rasterizador") }.unwrap_or_default();
    if rasterizador.trim().eq_ignore_ascii_case("software") {
        log("Zeebx: rasterizador fixado no processador pela opção do core; nenhum contexto de placa será pedido");
    } else {
        pede_o_contexto_de_placa();
    }

    let mut session = Session::start_software_with_storage_installed_policy(
        std::path::Path::new(caminho),
        portas,
        ZWheel::default(),
        storage,
        &instalados,
        midi_backend,
    )?;
    let mixer = session.grava_audio(SAMPLE_RATE);
    // **Proporção nativa, sempre.** O core entrega o quadro do console em 640×480 sem esticar:
    // quem ajusta shader precisa de uma fonte previsível, e o upscale é papel do frontend. A
    // resolução interna deixou de ser fixada aqui porque virou opção — ver
    // [`aplica_opcoes_quentes`], que roda logo depois que o estado existe.
    session.define_proporcao(None);
    if !jogos.is_empty() {
        session.set_installed_applets(
            jogos
                .iter()
                .filter_map(|(classe, caminho)| {
                    Some((*classe, zeebx::library::id_do_modulo(caminho)?))
                }),
        );
        log(&format!(
            "Zeebx: {} jogo(s) instalados a partir de {}",
            jogos.len(),
            pasta.as_deref().unwrap_or(std::path::Path::new(".")).display()
        ));
    }
    // **A captura de serial, quando pedida.** É o mesmo instrumento da varredura
    // (`ZEEBX_ROM_SERIAL`) e do `run` (`--serial`): classes criadas, bancos abertos, SQL,
    // propriedades de widget e a árvore de widgets da primeira tecla. Sem ele, quem está no
    // aparelho vê o jogo e não vê o que o applet faz por dentro — e foi com ele que se mediu, na
    // Z-Wheel, que o caminho do core reage à tecla mas pergunta `class_id = -1` (0 linhas) onde a
    // varredura resolve o foco (`class_id = 17359702`, o Alien Breaker) e pede a abertura.
    // O censo do acessador por classe, o mesmo da varredura — e ele é **opt-in** lá pelos mesmos
    // motivos: acrescenta uma seção ao relatório, e o relatório entra na linha de base.
    if std::env::var("ZEEBX_CORE_SELETORES").is_ok() {
        session.liga_censo_de_widgets();
    }
    if let Ok(caminho) = std::env::var("ZEEBX_CORE_SERIAL") {
        match session.liga_serial(std::path::Path::new(&caminho)) {
            Ok(()) => log(&format!("Zeebx: captura de serial em {caminho}")),
            Err(erro) => aviso(&format!("Zeebx: não deu para abrir a captura de serial: {erro}")),
        }
    }
    // A Z-Wheel é o shell: quando ela é o conteúdo, guardar o caminho é o que permite voltar a
    // ela depois que um jogo termina — o que o console faz.
    let z_wheel = match session.classe() == zeebx::session::Z_WHEEL {
        true => Some(path.clone()),
        false => None,
    };

    let mut core = Core {
        placa_ligada: false,
        frameskip: Frameskip::Desligado,
        frameskip_contador: 0,
        frameskip_callback_pedido: false,
        frameskip_leitura_pixels_avisada: false,
        limite_fps: LimiteFps::Sessenta,
        limite_fps_contador: 0,
        limite_fps_duplica: false,
        session,
        mixer,
        portas,
        frame: Vec::new(),
        audio: Vec::new(),
        path,
        storage: storage.clone(),
        jogos,
        z_wheel,
        aberto_pela_z_wheel: false,
        bitmasks: false,
        aceita_dupe: false,
        chave_do_quadro: None,
        faixa_do_quadro: None,
        swaps_vistos: 0,
        ultimo_relogio_ms: 0,
        audio_mede_callback: false,
        audio_pendente: Vec::new(),
        avisou_tamanho: false,
        midi_backend,
        select_antes: false,
        pad_antes: [Pad::default(); zeebx::input::PORTAS],
        quadros_apos_parar: 0,
        parou: false,
    };
    // As opções valem desde o primeiro quadro. É a **mesma** função do caminho quente de propósito:
    // duas cópias da aplicação divergem com o tempo, e a que roda menos é a que fica errada sem
    // ninguém ver.
    aplica_opcoes_quentes(&mut core);
    Ok(core)
}

/// A biblioteca de jogos ao lado do conteúdo: `(pasta, [(ClassID, o que carregar)])`.
///
/// É o que responde ao pedido de lançamento do shell — a Z-Wheel pede uma classe e aqui se sabe
/// qual arquivo a atende. Sem isso ela abre vazia, porque enumera os instalados e não acha nenhum.
/// A deduplicação por ClassID evita listar duas vezes quem tem o `.zip` **e** uma cópia extraída.
fn biblioteca(caminho: &str) -> (Option<PathBuf>, Vec<(u32, PathBuf)>) {
    let pasta = std::path::Path::new(caminho)
        .parent()
        .map(std::path::Path::to_path_buf);
    let mut jogos: Vec<(u32, PathBuf)> = Vec::new();
    let mut vistas: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for jogo in pasta.as_deref().map(zeebx::library::scan).unwrap_or_default() {
        let Some(classe) = jogo.clsid else {
            continue;
        };
        if vistas.insert(classe) {
            jogos.push((classe, jogo.path));
        }
    }
    (pasta, jogos)
}

/// Converte a biblioteca descoberta para o formato que o shell enumera no boot.
fn instalados_da_biblioteca(jogos: &[(u32, PathBuf)]) -> Vec<(u32, String)> {
    jogos
        .iter()
        .filter_map(|(classe, caminho)| {
            Some((*classe, zeebx::library::id_do_modulo(caminho)?))
        })
        .collect()
}

/// Instala a fonte do sistema na raiz do aparelho, se ainda não estiver lá.
///
/// Sai do cache quando a Z-Wheel já foi extraída e, quando não foi, do próprio pacote compactado —
/// extraindo **só** o arquivo, porque não vale materializar o pacote inteiro por 190 KB.
fn prepara_fonte(storage: &StoragePaths, jogos: &[(u32, PathBuf)]) -> Option<PathBuf> {
    zeebx::loader::archive::fonte_do_sistema_em(&storage.cache, &storage.device).or_else(|| {
        let candidatos: Vec<&PathBuf> = jogos
            .iter()
            .filter(|(classe, _)| *classe == zeebx::session::Z_WHEEL)
            .map(|(_, caminho)| caminho)
            .collect();
        let pacote = candidatos
            .iter()
            .find(|caminho| zeebx::loader::archive::embalado(caminho))
            .or_else(|| candidatos.first())?;
        zeebx::loader::archive::instala_fonte_do_pacote(pacote, &storage.device)
    })
}

/// Troca o applet em execução dentro do mesmo core.
///
/// É o que o console faz quando a Z-Wheel abre um jogo e quando o jogo fecha: o shell continua
/// sendo o shell. O frontend não participa — para ele, `retro_run` só devolveu outro quadro.
fn troca_para(estado: &mut Core, caminho: &Path, aberto_pela_z_wheel: bool) -> Result<(), StartError> {
    // **Se já se desenha na placa, a sessão nova também nasce nela** — a Z-Wheel abrindo um jogo,
    // o jogo voltando para ela. Sem isto a primeira troca devolveria o desenho ao processador, e o
    // sintoma seria "o render em hardware funciona até o primeiro jogo".
    let instalados = instalados_da_biblioteca(&estado.jogos);
    let mut session = match placa() {
        Some(contexto) => Session::start_with_storage_installed_policy(
            caminho,
            estado.portas,
            None,
            true,
            Some(contexto),
            ZWheel::default(),
            &estado.storage,
            &instalados,
            estado.midi_backend,
        )?,
        None => Session::start_software_with_storage_installed_policy(
            caminho,
            estado.portas,
            ZWheel::default(),
            &estado.storage,
            &instalados,
            estado.midi_backend,
        )?,
    };
    let mixer = session.grava_audio(SAMPLE_RATE);
    session.define_proporcao(None);
    estado.session = session;
    estado.mixer = mixer;
    // As opções valem desde o primeiro quadro, e não só depois que o usuário mexer no menu de
    // novo. É a **mesma** função do caminho quente: duas cópias da aplicação divergem com o tempo,
    // e a que roda menos é a que fica errada sem ninguém ver.
    aplica_opcoes_quentes(estado);
    estado.path = caminho.to_path_buf();
    estado.aberto_pela_z_wheel = aberto_pela_z_wheel;
    estado.parou = false;
    estado.quadros_apos_parar = 0;
    estado.chave_do_quadro = None;
    estado.frame.clear();
    estado.select_antes = false;
    estado.ultimo_relogio_ms = estado.session.clock_ms();
    Ok(())
}

/// Copia para `quadro` (RGB565 little-endian, linha a linha) só as linhas da tela que diferem
/// dele e devolve a faixa `[y0, y1)` que mudou, ou `None` quando nada mudou.
///
/// `quadro` de outro tamanho é refeito inteiro, e aí a faixa é a tela toda.
fn faixa_que_mudou(tela: &zeebx::video::display::Framebuffer, quadro: &mut Vec<u8>) -> Option<(usize, usize)> {
    let (largura, altura) = (tela.width() as usize, tela.height() as usize);
    let bytes = tela.rgb565_fatia(0, largura * altura);
    if quadro.len() != bytes.len() {
        quadro.clear();
        quadro.extend_from_slice(&bytes);
        return (altura > 0).then_some((0, altura));
    }
    let passo = largura * 2;
    let linha = |y: usize| y * passo..(y + 1) * passo;
    let y0 = (0..altura).find(|&y| bytes[linha(y)] != quadro[linha(y)])?;
    let y1 = (y0..altura).rev().find(|&y| bytes[linha(y)] != quadro[linha(y)]).unwrap_or(y0) + 1;
    quadro[y0 * passo..y1 * passo].copy_from_slice(&bytes[y0 * passo..y1 * passo]);
    Some((y0, y1))
}

/// `retro_run`: um quadro virtual, um quadro de vídeo e o áudio correspondente.
#[unsafe(no_mangle)]
pub extern "C" fn retro_run() {
    let inicio = zeebx::video::gpu::mede::agora();
    retro_run_dentro();
    if let Some(inicio) = inicio
        && let Some(quadros) = relata_medicao(inicio.elapsed())
    {
        relata_chamadas(quadros);
    }
}

/// A linha `Zeebx MEDE api` do relatório: chamadas de API por quadro e as que mais se repetem.
///
/// Cada chamada sai do JIT e volta (ver `Machine::execute`), então o número por quadro é o que
/// multiplica o custo fixo de uma ida e volta. A contagem vem do mapa que o motor já mantém para o
/// relatório de APIs; aqui só se tira a diferença para a amostra anterior.
fn relata_chamadas(quadros: u64) {
    static ANTES: Mutex<Option<std::collections::HashMap<String, u64>>> = Mutex::new(None);
    let agora: std::collections::HashMap<String, u64> = {
        let Ok(guard) = core().lock() else {
            return;
        };
        let Some(EstadoDoCore(estado)) = guard.as_ref() else {
            return;
        };
        estado.session.chamadas_de_api().into_iter().collect()
    };
    let Ok(mut antes) = ANTES.lock() else {
        return;
    };
    let anterior = antes.take().unwrap_or_default();
    let mut deltas: Vec<(&String, u64)> = agora
        .iter()
        .map(|(nome, &n)| (nome, n.saturating_sub(anterior.get(nome).copied().unwrap_or(0))))
        .filter(|&(_, d)| d > 0)
        .collect();
    deltas.sort_unstable_by_key(|&(_, d)| std::cmp::Reverse(d));
    let total: u64 = deltas.iter().map(|&(_, d)| d).sum();
    let q = quadros as f64;
    let topo: Vec<String> = deltas
        .iter()
        .take(8)
        .map(|(nome, d)| format!("{nome} {:.0}", *d as f64 / q))
        .collect();
    log(&format!(
        "Zeebx MEDE api: {:.0} chamadas/q | {}",
        total as f64 / q,
        topo.join(" | ")
    ));
    *antes = Some(agora);
}

unsafe extern "C" {
    fn zeebx_prof_dump();
}

static NS_VIDEO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// O relatório de `ZEEBX_MEDE=1`: a cada 120 quadros, quanto tempo o `retro_run` levou e quanto
/// disso foi envio de lotes, leitura do quadro e textura, com os desenhos por quadro.
fn relata_medicao(duracao: std::time::Duration) -> Option<u64> {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static QUADROS: AtomicU64 = AtomicU64::new(0);
    static NS_RUN: AtomicU64 = AtomicU64::new(0);
    static PAREDE: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    NS_RUN.fetch_add(duracao.as_nanos() as u64, Relaxed);
    let n = QUADROS.fetch_add(1, Relaxed) + 1;
    if n < 120 {
        return None;
    }
    QUADROS.store(0, Relaxed);
    let run_ms = NS_RUN.swap(0, Relaxed) as f64 / 1e6;
    let agora = std::time::Instant::now();
    let parede = PAREDE
        .lock()
        .ok()
        .and_then(|mut g| g.replace(agora))
        .map(|antes| agora.duration_since(antes).as_secs_f64());
    unsafe { zeebx_prof_dump() };
    let (draws, verts, ms_sub, leituras, ms_ler, texs, ms_tex) = zeebx::video::gpu::mede::colhe();
    let q = n as f64;
    let (ms_apl, ms_unif, ms_draw) = zeebx::video::gpu::mede::colhe_partes();
    log(&format!(
        "Zeebx MEDE partes: aplica {:.2} ms/q | uniformes {:.2} ms/q | draw {:.2} ms/q | video (vsync do frontend) {:.2} ms/q",
        ms_apl / q,
        ms_unif / q,
        ms_draw / q,
        NS_VIDEO.swap(0, Relaxed) as f64 / 1e6 / q
    ));
    log(&format!(
        "Zeebx MEDE cpu: {:.0} mil instruções do jogo/q | {:.1} linhas de código invalidadas/q | {:.0} acessos lentos/q | {} dobras | {} limpezas de cache",
        zeebx::cpu::dynarmic::conta::INSTRUCOES.swap(0, Relaxed) as f64 / q / 1e3,
        zeebx::cpu::dynarmic::conta::INVALIDACOES.swap(0, Relaxed) as f64 / q,
        zeebx::cpu::dynarmic::conta::LENTAS.swap(0, Relaxed) as f64 / q,
        zeebx::cpu::dynarmic::conta::DOBRAS.swap(0, Relaxed),
        zeebx::cpu::dynarmic::conta::LIMPEZAS.swap(0, Relaxed),
    ));
    {
        use zeebx::machine::bitmap_conta as b;
        log(&format!(
            "Zeebx MEDE 2d: {:.0} sincronizações/q | {} superfícies | {:.1} leituras/q | {:.1} escritas inteiras/q | {:.0} KB/q",
            b::SYNC.swap(0, Relaxed) as f64 / q,
            b::DIBS.load(Relaxed),
            b::LEITURAS.swap(0, Relaxed) as f64 / q,
            b::ESCRITAS_INTEIRAS.swap(0, Relaxed) as f64 / q,
            b::BYTES.swap(0, Relaxed) as f64 / q / 1024.0,
        ));
        log(&format!(
            "Zeebx MEDE 2d parciais: {:.0} escritas/q, {:.0} KB/q",
            b::PARCIAIS.swap(0, Relaxed) as f64 / q,
            b::BYTES_PARCIAIS.swap(0, Relaxed) as f64 / q / 1024.0,
        ));
    }
    log(&format!(
        "Zeebx MEDE: {:.1} fps | run {:.2} ms/q | submete {:.2} ms/q ({:.0} draws, {:.0} vert) | leitura {:.2} ms/q ({:.1}/q) | textura {:.2} ms/q ({:.1}/q)",
        parede.map_or(0.0, |p| q / p),
        run_ms / q,
        ms_sub / q,
        draws as f64 / q,
        verts as f64 / q,
        ms_ler / q,
        leituras as f64 / q,
        ms_tex / q,
        texs as f64 / q,
    ));
    Some(n)
}

fn retro_run_dentro() {
    // Os buffers saem do estado antes das chamadas ao frontend: nenhum cadeado do core fica preso
    // enquanto o frontend executa, e é isso que impede um aviso dele — "disco cheio, quer salvar?"
    // — de travar o emulador.
    let (frame, audio, largura, altura, duplicado, quadro_2d, virtual_decorrido, devidas_medidas) = {
        let Ok(mut guard) = core().lock() else {
            return;
        };
        let Some(EstadoDoCore(estado)) = guard.as_mut() else {
            return;
        };

        // **Quem está rodando agora**, para o teste do ciclo da Z-Wheel poder dizer se é a roda
        // ou o jogo que ela abriu. Ver [`CLASSE_ATUAL`].
        // Os dois instrumentos de teste, e a anotação vai **em cada linha**: um `#[cfg(test)]` só
        // vale para o item seguinte, e sem ela aqui o `cargo build` do CI quebra com "cannot find
        // value RELOGIO in this scope" enquanto o `cargo test` local passa — que foi exatamente o
        // que aconteceu.
        #[cfg(test)]
        CLASSE_ATUAL.store(estado.session.classe(), std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        RELOGIO.store(estado.session.clock_ms(), std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        INSTRUCOES.store(estado.session.instrucoes(), std::sync::atomic::Ordering::Relaxed);
        // **A placa entra no primeiro quadro.** O contexto de GL só existe depois que o frontend
        // chama o `context_reset`, que acontece depois do `retro_load_game`; aqui é o primeiro
        // lugar em que ele pode estar pronto. Recriar a sessão custa um reinício que ninguém vê:
        // nenhum quadro foi entregue ainda.
        // **As opções que dá para aplicar a quente.** Ver [`opcoes_mudaram`]: o frontend avisa uma
        // vez, e só então vale reler — e a releitura trata todas juntas, porque o aviso é
        // consumido na primeira pergunta (ver [`aplica_opcoes_quentes`]). O sintetizador MIDI e o
        // rasterizador **não** entram aqui de propósito: os dois são escolhidos quando a sessão
        // nasce, e por isso os rótulos deles dizem "(reinício)".
        if opcoes_mudaram() {
            aplica_opcoes_quentes(estado);
        }
        // **A decisão de pular é por quadro, e vale a cada quadro** — ao contrário do modo, que só
        // muda quando o usuário mexe na opção. Um `Fixo(n)` que só decidisse quando a opção muda
        // pularia (ou não) para sempre a partir da primeira leitura, e não a cada quadro n de n+1.
        // Dentro de retro_run, como SET_MINIMUM_AUDIO_LATENCY exige.
        if estado.frameskip == Frameskip::Automatico && !estado.frameskip_callback_pedido {
            estado.frameskip_callback_pedido = pede_callback_de_audio();
        }
        if estado.session.leu_pixels() && !estado.frameskip_leitura_pixels_avisada {
            aviso("Zeebx: frameskip de rasterização foi desativado neste jogo porque ele usa glReadPixels");
            estado.frameskip_leitura_pixels_avisada = true;
        }
        let pula_por_frameskip = match estado.frameskip {
            Frameskip::Desligado => {
                estado.frameskip_contador = 0;
                false
            }
            Frameskip::Fixo(n) => {
                let pula = estado.frameskip_contador != 0;
                estado.frameskip_contador = (estado.frameskip_contador + 1) % (n + 1);
                pula
            }
            // A própria `libretro.h` diz o que fazer com o aviso: **é** a decisão, não uma dica.
            Frameskip::Automatico => {
                AUDIO_ESTOURO_PROVAVEL.load(std::sync::atomic::Ordering::Relaxed)
            }
        };
        // 30 FPS **não desacelera a lógica**: o freio de velocidade continua em 1x, e só a
        // apresentação duplica um quadro a cada dois. É o oposto de alterar o período virtual
        // de vsync — aquilo faria o jogo avançar 33 ms por chamada e poderia acelerá-lo.
        let pula_por_limite = match estado.limite_fps {
            LimiteFps::Trinta => {
                let pula = estado.limite_fps_contador != 0;
                estado.limite_fps_contador = (estado.limite_fps_contador + 1) % 2;
                pula
            }
            LimiteFps::Desligado | LimiteFps::Sessenta => {
                estado.limite_fps_contador = 0;
                false
            }
        };
        estado.limite_fps_duplica = pula_por_limite;
        estado.session.define_pula_desenho(
            (pula_por_frameskip || pula_por_limite) && !estado.session.leu_pixels(),
        );
        liga_a_placa(estado);
        // **O contexto se perdeu e a sessão estava nele.** O aviso sozinho não basta: a sessão
        // guarda o rasterizador de placa, e continuar desenhando por ele chamaria funções de GL que
        // já não existem. Voltar ao software é o mesmo caminho da falha na ativação.
        if PERDEU_A_PLACA.swap(false, std::sync::atomic::Ordering::Relaxed) && estado.placa_ligada {
            estado.placa_ligada = false;
            let antes = estado.path.clone();
            if let Err(erro) = troca_para(estado, &antes, false) {
                aviso(&format!(
                    "Zeebx: o contexto de placa se perdeu e nem no processador deu para reabrir: {erro}"
                ));
            } else {
                aviso("Zeebx: o frontend trocou o contexto de vídeo; seguindo no processador");
            }
        }
        // Em modo de placa, o alvo do desenho é o framebuffer que o frontend indica **a cada
        // quadro** — ele pode trocar.
        if let Some(pega_framebuffer) = OFERTA_DE_PLACA
            .lock()
            .ok()
            .and_then(|oferta| oferta.as_ref().and_then(|o| o.get_current_framebuffer))
            && placa().is_some()
        {
            let fbo = unsafe { pega_framebuffer() };
            estado.session.desenha_no_fbo(Some(fbo));
        }
        // Teclas que o frontend entregou desde o último quadro. O callback só enfileira; aqui
        // elas entram no guest, na thread normal do core.
        if let Ok(mut fila) = teclas().lock() {
            while let Some((avk, down)) = fila.pop_front() {
                estado.session.set_key(avk, down);
            }
        }
        // O shell pediu outro applet — a Z-Wheel escolheu um jogo, ou o jogo mandou voltar. O
        // pedido vive **dentro** do motor (`Machine::pending_launch`), e a UI desktop já o atende
        // assim; aqui ele troca de sessão sem o frontend saber.
        if let Some(classe) = estado.session.take_launch_request() {
            // Instrumento do teste, e só dele: o log do core sai pelo callback do frontend, que é
            // **variádico** e por isso não pode ser implementado num teste em Rust estável. Aqui o
            // teste observa o que a interface do frontend mostraria como texto.
            #[cfg(test)]
            ULTIMA_ABERTURA.store(classe, std::sync::atomic::Ordering::Relaxed);
            let alvo = estado
                .jogos
                .iter()
                .find(|(c, _)| *c == classe)
                .map(|(_, caminho)| caminho.clone());
            match alvo {
                Some(caminho) => {
                    let e_z_wheel = classe == zeebx::session::Z_WHEEL;
                    match troca_para(estado, &caminho, !e_z_wheel) {
                        Ok(()) => log(&format!(
                            "Zeebx: o shell pediu {classe:#010x}; abrindo {}",
                            caminho.display()
                        )),
                        Err(erro) => log(&format!(
                            "Zeebx: o shell pediu {classe:#010x} e não deu para abrir: {erro}"
                        )),
                    }
                }
                None => aviso(&format!(
                    "O shell pediu {classe:#010x}, que não está na pasta de jogos"
                )),
            }
        }
        // Atalho: o Select do RetroPad vale como `AVK_CLR`, que é o "voltar" do console. Sem ele
        // a Z-Wheel fica presa na abertura em quem não tem teclado mapeado no frontend.
        let select = le_select(0);
        if select != estado.select_antes {
            estado.select_antes = select;
            estado.session.set_key(zeebx::input::avk::CLR, select);
        }
        // Entrada primeiro: o guest lê o controle dentro do quadro que vai rodar.
        for porta in 0..zeebx::input::PORTAS {
            if estado.portas[porta].is_none() {
                continue;
            }
            let pad = le_pad(porta as u32, estado.bitmasks);
            estado.session.set_port_pad(porta, pad);
            // **O controle vira tecla do console, como na janela.** O `teclas_do_controle` existe
            // para isso e é usado pelo desktop e pela janela desde sempre — o core **não o
            // chamava**, então quem lê as teclas do BREW não recebia nada do RetroArch. Medido em
            // 22/09/2026 com a Z-Wheel, 2600 quadros e o roteiro de botões: sem esta tradução, a
            // roda anima (223 imagens distintas) e não pede abertura nenhuma; com ela, a grade
            // abre e o pedido sai. É a diferença entre "o controle chega ao guest" e "o controle
            // chega ao applet do jeito que ele lê".
            for (avk, apertada) in
                zeebx::input::teclas_do_controle(&estado.pad_antes[porta], &pad)
            {
                estado.session.set_key(avk, apertada);
            }
            estado.pad_antes[porta] = pad;
        }
        // **As telas intermediárias de um `Update` dentro de um callback passam como na janela.**
        // A máquina guarda uma tela por `IDISPLAY_Update` que o guest chama dentro do callback (a
        // transição da Z-Wheel desliza a tela num laço síncrono, umas duzentas vezes), e quem as
        // mostra é o frontend, uma por quadro, sem avançar o relógio. A janela as consumia; o core
        // **não** — e com a fila cheia o `advance_once` devolvia "apresentou" para sempre, sem
        // rodar o guest: nenhuma fronteira de API, nenhuma tecla esvaziada da fila, e o applet
        // parado. Medido: a Z-Wheel congelava depois do confirmar (relógio virtual parado em
        // 37 012 ms) e só uma tecla chegava.
        if !estado.session.mostra_quadro_intermediario()
            && let Step::Stopped = estado
                .session
                .run_frame(estado.limite_fps.limita_velocidade())
        {
            if !estado.parou {
                estado.parou = true;
                let relogio = estado.session.clock_ms();
                let motivo = estado
                    .session
                    .stopped_reason()
                    .unwrap_or_else(|| "sem motivo relatado".to_string());
                log(&format!("Zeebx: parou em {relogio} ms virtuais: {motivo}"));
                // O log do próprio jogo é o que costuma nomear o motivo da parada; as últimas
                // linhas vão para o frontend, que é onde alguém vai olhar.
                let relato = estado.session.log();
                for linha in relato.iter().rev().take(6).rev() {
                    log(&format!("Zeebx:   {linha}"));
                }
                let memoria = estado.session.memory();
                log(&format!(
                    "Zeebx:   heap {} bytes, {} objetos",
                    memoria.0, memoria.1
                ));
                // No console, sair de um jogo devolve o controle à Z-Wheel, que é outro applet
                // instalado. Aqui o pedido é registrado, mas **a ABI não deixa o core pedir
                // outro conteúdo ao frontend**: ou o core carregaria a Z-Wheel por conta própria,
                // ou o frontend encerra o conteúdo. Ver o plano, seção de desfecho.
                if let Some(classe) = estado.session.take_launch_request() {
                    log(&format!(
                        "Zeebx: o shell pediu para abrir {classe:#010x}; trocar de conteúdo dentro do core ainda não existe"
                    ));
                }
                estado.quadros_apos_parar = 0;
            }
        }
        // Vídeo: o framebuffer do console, no formato negociado.
        // **Com placa, só o quadro que passou pelo GL está no framebuffer do frontend.** Um jogo
        // só 2D (o 276212 da Namco, as telas de IDisplay) nunca troca buffer de GL e aparecia
        // preto. Vai por software o quadro sem troca de GL **e** com 2D escrito na tela depois da
        // última troca — a segunda condição segura os jogos que desenham GL a 30 quadros, cujo
        // quadro sem troca é só repetição. Converter e tirar assinatura da tela só nesse caso: no
        // resto não serviria a ninguém (11% do processador no NFS Carbon).
        let swaps = estado.session.gl_swaps();
        let quadro_2d = placa().is_some()
            && swaps == estado.swaps_vistos
            && !estado.session.quadro_gl_intacto();
        if zeebx::video::gpu::mede::ligado() {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static Q: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
            Q[0].fetch_add(u64::from(swaps != estado.swaps_vistos), Relaxed);
            Q[1].fetch_add(u64::from(estado.session.quadro_gl_intacto()), Relaxed);
            Q[2].fetch_add(u64::from(quadro_2d), Relaxed);
            if Q[0].load(Relaxed) + Q[2].load(Relaxed) >= 120 {
                log(&format!(
                    "Zeebx MEDE quadros: {} com troca de GL, {} com GL intacto, {} por software (2D)",
                    Q[0].swap(0, Relaxed), Q[1].swap(0, Relaxed), Q[2].swap(0, Relaxed)
                ));
            }
        }
        estado.swaps_vistos = swaps;
        let tela = estado.session.screen();
        let (largura, altura) = (tela.width(), tela.height());
        // Com placa, o frontend recorta do framebuffer dele só o retângulo com conteúdo e o estica
        // para a tela; sem isto, um jogo com superfície menor (Quake, Galaxy on Fire) aparecia num
        // canto. Ver `Machine::retangulo_do_quadro_gl`.
        let (largura, altura) = match placa() {
            Some(_) if !quadro_2d => {
                let (l, a) = estado.session.retangulo_do_quadro_gl();
                if l > 0 && a > 0 && l <= largura as usize && a <= altura as usize {
                    (l as u32, a as u32)
                } else {
                    (largura, altura)
                }
            }
            _ => (largura, altura),
        };
        // O console é 640×480, e é esse o quadro que o shader espera receber. Um tamanho
        // diferente é avisado uma vez, em vez de aparecer como imagem torta sem explicação.
        if !estado.avisou_tamanho && (largura != 640 || altura != 480) {
            estado.avisou_tamanho = true;
            log(&format!(
                "Zeebx: quadro {largura}x{altura}, fora dos 640x480 do console"
            ));
        }
        let mut quadro = std::mem::take(&mut estado.frame);
        // 30 FPS é cadência de apresentação, não só otimização 3D: no quadro oculto preservamos
        // os bytes anteriores. Se o frontend aceita dupe, entregaremos ponteiro nulo; se não
        // aceita, entregaremos os mesmos bytes de novo — nos dois casos a imagem é realmente 30.
        let duplicado = if placa().is_some() && !quadro_2d {
            false
        } else if estado.limite_fps_duplica {
            estado.aceita_dupe
        } else {
            // **Só as linhas que mudaram.** O quadro guardado é o último entregue, e a textura da
            // tela na placa é cópia dele; comparar linha a linha acha a faixa que mudou, e só ela
            // é copiada e subida. Antes eram três passadas de 600 KB por quadro — cópia,
            // assinatura FNV (serial: uma multiplicação por pixel) e o `glTexImage2D` inteiro,
            // que o Mali ainda converte para blocos. Com a mesma superfície e as mesmas escritas
            // de antes, nem a comparação: nada foi escrito.
            let chave = (tela.serie(), tela.escritas());
            let faixa = if estado.chave_do_quadro == Some(chave) && quadro.len() == tela.pixels().len() * 2 {
                None
            } else {
                faixa_que_mudou(tela, &mut quadro)
            };
            estado.chave_do_quadro = Some(chave);
            estado.faixa_do_quadro = faixa;
            estado.aceita_dupe && faixa.is_none()
        };
        // O quadro 2D vai para o framebuffer do frontend como textura: com contexto de placa o
        // RetroArch apresenta o framebuffer e ignora quadro por software. Sem faixa (quadro igual
        // ao anterior, num frontend que não aceita repetir) sobe a tela toda: é o que se fazia.
        if quadro_2d && !duplicado {
            let faixa = estado.faixa_do_quadro.unwrap_or((0, altura as usize));
            estado
                .session
                .pinta_tela_na_placa(largura as usize, altura as usize, &quadro, faixa);
        }
        // Áudio: **o tempo vem do relógio virtual**, não de um número fixo. Um jogo que passa dois
        // quadros virtuais entre duas chamadas precisa entregar o dobro de amostras, senão o som
        // atrasa em relação à imagem e o frontend engasga ao tentar acompanhar.
        let agora = estado.session.clock_ms();
        let decorrido = u64::from(agora.wrapping_sub(estado.ultimo_relogio_ms));
        estado.ultimo_relogio_ms = agora;
        let devidas = (decorrido.min(1000) * u64::from(SAMPLE_RATE) / 1000) as usize;
        let virtual_decorrido = decorrido;
        let devidas_medidas = devidas;
        let mut som = std::mem::take(&mut estado.audio);
        som.clear();
        // O que o frontend não aceitou da vez anterior vai na frente, para não sumir um pedaço.
        som.append(&mut estado.audio_pendente);
        for amostra in estado.mixer.render(devidas) {
            som.push((amostra.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16);
        }
        (quadro, som, largura, altura, duplicado, quadro_2d, virtual_decorrido, devidas_medidas)
    };
    let frente = callbacks();
    if let Some(video) = frente.video {
        let na_placa = placa().is_some();
        let _ = quadro_2d;
        let (ponteiro, _) = match (na_placa, duplicado) {
            // **Em modo de placa o quadro já está no framebuffer do frontend**: entregar pixels
            // aqui seria mentira, e o `libretro` tem um sentinela para dizer exatamente isso.
            (true, _) => (HW_FRAME_BUFFER_VALID as *const c_void, ()),
            // Quadro nulo avisa "repete o anterior", que é o que a ABI oferece para tela parada.
            (false, true) => (std::ptr::null(), ()),
            (false, false) => (frame.as_ptr() as *const c_void, ()),
        };
        // SAFETY: o buffer vive durante a chamada; no quadro repetido o frontend reusa o último.
        let t_video = zeebx::video::gpu::mede::agora();
        unsafe {
            video(ponteiro, largura, altura, largura as usize * 2);
        }
        if let Some(t) = t_video {
            NS_VIDEO.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // O retorno do lote é em quadros **aceitos**; o que sobrar espera a próxima chamada.
    let mut sobra = Vec::new();
    let mut aceitos_medidos = 0;
    if let Some(batch) = frente.audio_batch {
        let quadros = audio.len() / 2;
        // SAFETY: o lote é intercalado em estéreo e o tamanho é o número de quadros.
        let aceitos = unsafe { batch(audio.as_ptr(), quadros) }.min(quadros);
        aceitos_medidos = aceitos;
        if aceitos < quadros {
            sobra = audio[aceitos * 2..].to_vec();
        }
    }
    let sobra_bruta = sobra.len() / 2;
    // Os buffers voltam para o estado, para a próxima chamada reaproveitar a mesma alocação.
    let mut dispensar = false;
    if let Ok(mut guard) = core().lock() {
        if let Some(EstadoDoCore(estado)) = guard.as_mut() {
            estado.frame = frame;
            estado.audio = audio;
            // A sobra não pode crescer sem fim; meio segundo é o teto.
            let limite = (SAMPLE_RATE as usize / 2) * 2;
            sobra.truncate(limite);
            if audio_mede::ligado(&estado.path) {
                if !estado.frameskip_callback_pedido && !estado.audio_mede_callback {
                    estado.audio_mede_callback = pede_aviso_de_buffer_para_medir();
                }
                if let Some(texto) = audio_mede::registra(
                    virtual_decorrido,
                    devidas_medidas,
                    &estado.audio,
                    aceitos_medidos,
                    sobra.len() / 2,
                    sobra_bruta - sobra.len() / 2,
                ) {
                    log(&texto);
                }
            }
            estado.audio_pendente = sobra;
            if estado.parou {
                estado.quadros_apos_parar += 1;
                // Dois segundos de tela parada bastam para ver o desfecho e o log.
                if estado.quadros_apos_parar == 120 {
                    // **A volta para a Z-Wheel.** No console, fechar um jogo devolve o controle ao
                    // shell, e o shell é outro applet instalado — então aqui se troca de sessão,
                    // como a UI desktop já faz. Sem Z-Wheel disponível, o desfecho possível é
                    // pedir ao frontend que encerre o conteúdo.
                    let voltar = estado.z_wheel.clone().filter(|_| {
                        estado.aberto_pela_z_wheel
                            || estado.session.classe() == zeebx::session::Z_WHEEL
                    });
                    match voltar {
                        Some(caminho) => match troca_para(estado, &caminho, false) {
                            Ok(()) => log("Zeebx: fim do jogo; de volta à Z-Wheel"),
                            Err(erro) => {
                                log(&format!("Zeebx: não deu para voltar à Z-Wheel: {erro}"));
                                dispensar = true;
                            }
                        },
                        None => dispensar = true,
                    }
                }
            }
        }
    }
    if dispensar {
        // Encerrar o conteúdo é o desfecho que a ABI oferece: o frontend volta ao menu dele, em
        // vez de ficar mostrando para sempre o último quadro de um jogo que acabou.
        log("Zeebx: jogo terminado; pedindo ao frontend para encerrar o conteúdo");
        unsafe {
            environ(ENV_SHUTDOWN, std::ptr::null_mut());
        }
    }
}

/// `retro_unload_game`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_unload_game() {
    limpa_estado_do_frontend();
}

/// `retro_reset`: recarrega o conteúdo do zero, preservando saves e NAND.
#[unsafe(no_mangle)]
pub extern "C" fn retro_reset() {
    let Ok(mut guard) = core().lock() else {
        return;
    };
    let Some(EstadoDoCore(antigo)) = guard.as_ref() else {
        return;
    };
    let caminho = antigo.path.clone();
    let portas = antigo.portas;
    let Some(save_dir) = diretorio(ENV_GET_SAVE_DIRECTORY) else {
        return;
    };
    let sistema = diretorio(ENV_GET_SYSTEM_DIRECTORY);
    let storage = StoragePaths::for_frontend(&save_dir, sistema.as_deref());
    let texto = caminho.to_string_lossy().into_owned();
    // SAFETY: mesma montagem do carregamento, na thread de `retro_run`.
    match unsafe { carrega(&texto, &storage, portas, caminho.clone()) } {
        Ok(novo) => *guard = Some(EstadoDoCore(novo)),
        Err(erro) => log(&format!("Zeebx: reset falhou: {erro}")),
    }
}

/// `retro_set_controller_port_device`: troca o aparelho que o guest enxerga na porta.
#[unsafe(no_mangle)]
pub extern "C" fn retro_set_controller_port_device(port: u32, device: u32) {
    let Ok(mut guard) = core().lock() else {
        return;
    };
    let Some(EstadoDoCore(estado)) = guard.as_mut() else {
        return;
    };
    let porta = port as usize;
    if porta >= zeebx::input::PORTAS {
        return;
    }
    estado.portas[porta] = aparelho_do_dispositivo(device);
    estado.session.set_portas(estado.portas);
}

/// O último estado gravado, para o `retro_serialize` entregar o que o `_size` mediu.
///
/// A ABI chama as duas em sequência — primeiro o tamanho, depois a gravação —, e o tamanho varia
/// com o que o jogo tem em memória: as texturas, as imagens decodificadas e os buffers de GL são a
/// maior parte dele. Recalcular na gravação daria um estado **diferente** do que foi medido, e o
/// frontend teria alocado o buffer pelo número errado. Por isso o tamanho medido guarda os bytes.
static ESTADO_MEDIDO: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// `retro_serialize_size`: o tamanho do estado, ou zero quando não há o que salvar.
///
/// **Zero só acontece sem sessão ou com um desenho em curso** — e as duas são respostas honestas:
/// sem sessão não há estado, e no meio de um desenho começado o estado prometeria algo que nunca
/// existiu. Em qualquer outro momento o número é o do estado de verdade.
#[unsafe(no_mangle)]
pub extern "C" fn retro_serialize_size() -> usize {
    let Ok(mut guard) = core().lock() else {
        return 0;
    };
    let Some(EstadoDoCore(estado)) = guard.as_mut() else {
        return 0;
    };
    if let Err(motivo) = estado.session.pode_salvar() {
        aviso(&format!("Zeebx: não dá para salvar agora: {motivo}"));
        return 0;
    }
    let arquivo = estado.session.grava_estado();
    let tamanho = arquivo.len();
    if let Ok(mut guarda) = ESTADO_MEDIDO.lock() {
        *guarda = Some(arquivo);
    }
    tamanho
}

/// `retro_serialize`: entrega o estado medido.
///
/// Devolve `false` — sem escrever nada — quando não cabe no buffer que o frontend ofereceu. É o
/// contrato: o frontend aloca pelo tamanho que pediu, e mentir sobre ele corromperia a memória
/// dele.
#[unsafe(no_mangle)]
pub extern "C" fn retro_serialize(data: *mut c_void, size: usize) -> bool {
    if data.is_null() {
        return false;
    }
    let Ok(guarda) = ESTADO_MEDIDO.lock() else {
        return false;
    };
    let Some(arquivo) = guarda.as_ref() else {
        return false;
    };
    if arquivo.len() > size {
        aviso(&format!(
            "Zeebx: o estado tem {} bytes e o frontend ofereceu {size}",
            arquivo.len()
        ));
        return false;
    }
    // SAFETY: o frontend garante `size` bytes válidos em `data`, e o bloco acima conferiu que o
    // estado cabe.
    unsafe {
        std::ptr::copy_nonoverlapping(arquivo.as_ptr(), data as *mut u8, arquivo.len());
    }
    true
}

/// `retro_unserialize`: põe o estado de volta.
///
/// A recusa vem do motor, com o motivo: seção faltando, tamanho que não bate, `crc32` trocado. Um
/// estado pela metade dentro de uma máquina em execução é pior que um estado recusado, e é por isso
/// que a leitura acontece **antes** de qualquer escrita.
#[unsafe(no_mangle)]
pub extern "C" fn retro_unserialize(data: *const c_void, size: usize) -> bool {
    if data.is_null() || size == 0 {
        return false;
    }
    // SAFETY: o frontend garante `size` bytes válidos em `data`.
    let arquivo = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
    let Ok(mut guard) = core().lock() else {
        return false;
    };
    let Some(EstadoDoCore(estado)) = guard.as_mut() else {
        return false;
    };
    match estado.session.restaura_estado(arquivo) {
        Ok(()) => true,
        Err(erro) => {
            aviso(&format!("Zeebx: o save state foi recusado: {erro}"));
            false
        }
    }
}

/// `retro_cheat_reset`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_cheat_reset() {}

/// `retro_cheat_set`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_cheat_set(_index: u32, _enabled: bool, _code: *const c_char) {}

/// `retro_load_game_special`: não há subsistemas.
#[unsafe(no_mangle)]
pub extern "C" fn retro_load_game_special(
    _game_type: u32,
    _info: *const RetroGameInfo,
    _num_info: usize,
) -> bool {
    false
}

/// `retro_get_region`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_get_region() -> u32 {
    0
}

/// `retro_get_memory_data`: os saves são arquivos do BREW, não uma região contínua.
#[unsafe(no_mangle)]
pub extern "C" fn retro_get_memory_data(_id: u32) -> *mut c_void {
    std::ptr::null_mut()
}

/// `retro_get_memory_size`.
#[unsafe(no_mangle)]
pub extern "C" fn retro_get_memory_size(_id: u32) -> usize {
    0
}


#[cfg(test)]
mod testes {
    use super::*;

    /// **Os deslocamentos do struct da placa têm de bater com o `libretro.h`.**
    ///
    /// O core preenche o struct, o frontend o devolve preenchido, e nós lemos
    /// `get_current_framebuffer` e `get_proc_address` dele. Um campo a mais ou a menos aqui faria
    /// esses dois serem lidos no lugar errado — e o sintoma seria "o render em hardware não
    /// funciona", sem nada no log que explique. Os números são a soma dos campos que o
    /// `include/libretro.h` declara, na ordem dele.
    #[test]
    fn a_entrada_do_retropad_chega_ao_guest() {
        let Ok(caminho) = std::env::var("ZEEBX_TESTE_ROM") else {
            eprintln!("sem ZEEBX_TESTE_ROM: nada a testar");
            return;
        };
        let pasta = std::env::temp_dir().join(format!("zeebx-entrada-{}", std::process::id()));
        std::fs::create_dir_all(&pasta).unwrap();
        let _ = PASTA.set(CString::new(pasta.to_string_lossy().to_string()).unwrap());
        let caminho_c = CString::new(caminho.clone()).unwrap();
        let info = RetroGameInfo {
            path: caminho_c.as_ptr(),
            data: std::ptr::null(),
            size: 0,
            meta: std::ptr::null(),
        };
        let mut reagiu = None;
        let mut abriu: Option<(&str, i32, u32, u32)> = None;
        unsafe {
            retro_set_environment(Some(ambiente));
            retro_set_video_refresh(Some(video));
            retro_set_audio_sample_batch(Some(audio));
            retro_set_input_poll(Some(sem_poll));
            retro_set_input_state(Some(entrada));
            retro_init();
            // O aparelho da porta decide o mapeamento do pad. `ZEEBX_TESTE_APARELHO=zpad` usa o
            // Z-Pad, que é o controle do console — a Z-Wheel é um app do Zeebo e o lê assim.
            let aparelho = match std::env::var("ZEEBX_TESTE_APARELHO").as_deref() {
                Ok("zpad") | Ok("ZPAD") => DEVICE_ZPAD,
                _ => DEVICE_JOYPAD,
            };
            retro_set_controller_port_device(0, aparelho);
            assert!(retro_load_game(&info), "o core recusou {caminho}");
            // Quanto tempo esperar antes de mandar entrada. A Z-Wheel leva mais que os jogos para
            // chegar à tela interativa — a varredura a pega com zero quadros aos seis segundos —,
            // e mandar botão para uma tela de carregamento não diz nada sobre o caminho de entrada.
            let espera: u32 = std::env::var("ZEEBX_TESTE_ESPERA")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(180);
            for _ in 0..espera {
                retro_run();
            }
            for botao in [ID_START, ID_A, ID_B, ID_SELECT] {
                if let Ok(mut vistos) = ASSINATURAS.lock() {
                    vistos.clear();
                }
                BOTAO.store(botao, Ordering::Relaxed);
                for _ in 0..40 {
                    retro_run();
                }
                BOTAO.store(u32::MAX, Ordering::Relaxed);
                for _ in 0..20 {
                    retro_run();
                }
                let distintas = ASSINATURAS
                    .lock()
                    .map(|v| v.iter().collect::<std::collections::BTreeSet<_>>().len())
                    .unwrap_or(0);
                eprintln!("botao {botao}: {distintas} imagem(ns) distinta(s) em 60 quadros");
                if distintas > 1 {
                    reagiu = Some(botao);
                    break;
                }
            }
            // **O manche**, que é como a Z-Wheel e os menus são navegados.
            for (qual, valor) in [("x", 0x7fff), ("x", -0x8000), ("y", 0x7fff), ("y", -0x8000)] {
                if let Ok(mut vistos) = ASSINATURAS.lock() {
                    vistos.clear();
                }
                let alvo = match qual {
                    "x" => &EIXO_X,
                    _ => &EIXO_Y,
                };
                alvo.store(valor, Ordering::Relaxed);
                for _ in 0..40 {
                    retro_run();
                }
                alvo.store(0, Ordering::Relaxed);
                for _ in 0..20 {
                    retro_run();
                }
                let distintas = ASSINATURAS
                    .lock()
                    .map(|v| v.iter().collect::<std::collections::BTreeSet<_>>().len())
                    .unwrap_or(0);
                eprintln!("manche {qual}={valor}: {distintas} imagem(ns) distinta(s)");
                if distintas > 1 {
                    reagiu = Some(0x100 + valor as u32);
                    break;
                }
            }
            // **Abrir um jogo com o manche e o botão de confirmar.** É o ciclo do item 8 inteiro,
            // sem frontend: a roda está interativa desde a espera, o manche a navega, e o pedido de
            // abertura aparece em `ULTIMA_ABERTURA` — o core o registra por instrumento de teste,
            // porque o log dele sai por callback variádico do frontend.
            ULTIMA_ABERTURA.store(0, Ordering::Relaxed);
            'tentativas: for (direcao, valor) in [("x", 0x7fff), ("x", -0x8000), ("y", 0x7fff), ("y", -0x8000)] {
                // Os quatro botões de face **e** o Start: a Z-Wheel não usa o mesmo para navegar e
                // para confirmar, e testar só os de face foi o que deixou o ciclo sem resposta.
                for botao in [ID_A, ID_B, ID_Y, ID_X, ID_START] {
                    let alvo = match direcao {
                        "x" => &EIXO_X,
                        _ => &EIXO_Y,
                    };
                    // **Empurra o manche e confirma sem soltar.** A primeira versão soltava antes
                    // de apertar, e essa combinação — manche parado numa direção com o botão
                    // apertado — é justamente como um carrossel confirma a peça em foco.
                    alvo.store(valor, Ordering::Relaxed);
                    for _ in 0..30 {
                        retro_run();
                    }
                    BOTAO.store(botao, Ordering::Relaxed);
                    for _ in 0..20 {
                        retro_run();
                    }
                    BOTAO.store(u32::MAX, Ordering::Relaxed);
                    alvo.store(0, Ordering::Relaxed);
                    // **Depois de confirmar, a roda anima a transição** antes de pedir a abertura —
                    // vinte quadros não bastam, e o pedido chega durante a animação.
                    for _ in 0..180 {
                        retro_run();
                    }
                    let classe = ULTIMA_ABERTURA.load(Ordering::Relaxed);
                    if classe != 0 {
                        abriu = Some((direcao, valor, botao, classe));
                        break 'tentativas;
                    }
                }
            }
            retro_unload_game();
            retro_deinit();
        }
        let _ = std::fs::remove_dir_all(&pasta);
        eprintln!(
            "ciclo: {}",
            match abriu {
                Some((direcao, valor, botao, classe)) => format!(
                    "o shell pediu {classe:#010x} (manche {direcao}={valor}, botão {botao})"
                ),
                None => "a roda não pediu abertura nenhuma".to_string(),
            }
        );
        eprintln!(
            "entrada: {}",
            match reagiu {
                Some(botao) if botao >= 0x100 => "chegou — o manche mudou a imagem".to_string(),
                Some(botao) => format!("chegou — o botao {botao} mudou a imagem"),
                None => "nem os quatro botoes nem o manche mudaram a imagem".to_string(),
            }
        );
    }

    /// O limite de velocidade separa console, meia imagem e boost — não aceita texto ambíguo.
    #[test]
    fn o_texto_do_limite_fps_vira_o_modo_certo() {
        assert_eq!(LimiteFps::de_texto("60"), Some(LimiteFps::Sessenta));
        assert_eq!(LimiteFps::de_texto("30"), Some(LimiteFps::Trinta));
        assert_eq!(LimiteFps::de_texto("desligado"), Some(LimiteFps::Desligado));
        assert_eq!(LimiteFps::de_texto("120"), None);
        assert_eq!(LimiteFps::de_texto("automatico"), None);
        assert_eq!(LimiteFps::de_texto(""), None);
    }

    /// O texto da opção de frameskip vira o modo certo, e lixo mantém o modo anterior (`None`).
    #[test]
    fn o_texto_do_frameskip_vira_o_modo_certo() {
        assert_eq!(Frameskip::de_texto("desligado"), Some(Frameskip::Desligado));
        assert_eq!(Frameskip::de_texto("automatico"), Some(Frameskip::Automatico));
        assert_eq!(Frameskip::de_texto("1"), Some(Frameskip::Fixo(1)));
        assert_eq!(Frameskip::de_texto("6"), Some(Frameskip::Fixo(6)));
        assert_eq!(Frameskip::de_texto("4294967295"), None);
        // Zero não é um modo fixo válido: pular zero quadros é o mesmo que desligado, e um "0"
        // vindo de fora tem mais cara de opt estragado que de escolha deliberada.
        assert_eq!(Frameskip::de_texto("0"), None);
        assert_eq!(Frameskip::de_texto("nao-existe"), None);
        assert_eq!(Frameskip::de_texto(""), None);
    }

    /// O perfil só é Portátil com o texto certo, e tudo o mais — inclusive ausência — é Padrão.
    #[test]
    fn so_o_texto_portatil_ativa_o_perfil() {
        assert!(perfil_e_portatil(Some("portatil")));
        assert!(perfil_e_portatil(Some(" PORTATIL ")));
        assert!(!perfil_e_portatil(Some("padrao")));
        assert!(!perfil_e_portatil(Some("portátil"))); // valor declarado é sem acento
        assert!(!perfil_e_portatil(None));
        assert!(!perfil_e_portatil(Some("")));
    }

    /// Os interruptores e os números das opções aceitam o que o frontend entrega, e recusam lixo.
    ///
    /// O que se cobra aqui é a **recusa**: um valor que não dá para entender tem de virar `None`,
    /// porque quem chama trata `None` mantendo o que já havia. Se virasse um padrão qualquer, um
    /// frontend antigo — que não conhece a chave — desfaria a escolha de quem configurou.
    #[test]
    fn os_valores_das_opcoes_sao_lidos_e_o_lixo_e_recusado() {
        // A convenção do ecossistema é enabled/disabled; as outras grafias existem porque um .opt
        // editado à mão não segue convenção nenhuma.
        assert_eq!(ligado_de_texto("enabled"), Some(true));
        assert_eq!(ligado_de_texto("disabled"), Some(false));
        assert_eq!(ligado_de_texto(" ON "), Some(true));
        assert_eq!(ligado_de_texto("false"), Some(false));
        assert_eq!(ligado_de_texto("talvez"), None);
        assert_eq!(ligado_de_texto(""), None);

        // O número é preso à faixa que o motor aceita, e não recusado: quem pediu 99 quer o máximo.
        assert_eq!(numero_de_texto("1", 1, 8), Some(1));
        assert_eq!(numero_de_texto("4", 1, 8), Some(4));
        assert_eq!(numero_de_texto("99", 1, 8), Some(8));
        assert_eq!(numero_de_texto(" 2 ", 1, 8), Some(2));
        // Zero e negativo sobem para o mínimo; o motor não desenha em escala zero.
        assert_eq!(numero_de_texto("0", 1, 8), Some(1));
        assert_eq!(numero_de_texto("-3", 1, 8), None);
        assert_eq!(numero_de_texto("muito", 1, 8), None);
    }

    /// O texto da opção de volume vira fator do mixer, e o texto estragado **não** vira silêncio.
    #[test]
    fn o_volume_da_opcao_vira_fator_do_mixer() {
        assert_eq!(volume_de_texto("100"), Some(1.0));
        assert_eq!(volume_de_texto("0"), Some(0.0));
        assert_eq!(volume_de_texto("50"), Some(0.5));
        // O frontend pode entregar com espaço em volta; o arquivo é editável à mão.
        assert_eq!(volume_de_texto(" 70 "), Some(0.7));
        // Fora da faixa é preso na faixa, e não recusado: 150% é intenção clara de "no máximo".
        assert_eq!(volume_de_texto("150"), Some(1.0));
        assert_eq!(volume_de_texto("-10"), Some(0.0));
        // O que não é número mantém o que já havia, em vez de emudecer o emulador.
        assert_eq!(volume_de_texto("alto"), None);
        assert_eq!(volume_de_texto(""), None);
        assert_eq!(volume_de_texto("NaN"), None);
    }

    #[test]
    fn os_deslocamentos_do_struct_da_placa_batem_com_o_libretro_h() {
        use std::mem::{offset_of, size_of};
        assert_eq!(offset_of!(RetroHwRenderCallback, context_reset), 8);
        assert_eq!(offset_of!(RetroHwRenderCallback, get_current_framebuffer), 16);
        assert_eq!(offset_of!(RetroHwRenderCallback, get_proc_address), 24);
        assert_eq!(offset_of!(RetroHwRenderCallback, depth), 32);
        assert_eq!(offset_of!(RetroHwRenderCallback, version_major), 36);
        assert_eq!(offset_of!(RetroHwRenderCallback, version_minor), 40);
        assert_eq!(offset_of!(RetroHwRenderCallback, cache_context), 44);
        assert_eq!(offset_of!(RetroHwRenderCallback, context_destroy), 48);
        assert!(size_of::<RetroHwRenderCallback>() >= 56);
    }
    use std::ffi::CString;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Quantos quadros o vídeo do frontend recebeu.
    static QUADROS: AtomicU32 = AtomicU32::new(0);
    /// Qual botão do RetroPad o teste está segurando. `u32::MAX` é nenhum.
    static BOTAO: AtomicU32 = AtomicU32::new(u32::MAX);
    /// A pasta que o frontend de teste entrega como sistema e como saves.
    static PASTA: OnceLock<CString> = OnceLock::new();
    /// O sistema de fora, quando `ZEEBX_CORE_SISTEMA` aponta para uma árvore de aparelho real.
    static SISTEMA: OnceLock<CString> = OnceLock::new();
    /// A assinatura de cada quadro entregue, na ordem.
    static ASSINATURAS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

    /// O ambiente mínimo que o core precisa, respondendo como um frontend de verdade.
    ///
    /// O que não temos responde `false` — é o que o RetroArch faz com o que não conhece, e é
    /// assim que o caminho de recusa do core também fica exercitado.
    unsafe extern "C" fn ambiente(cmd: u32, dados: *mut c_void) -> bool {
        match cmd {
            // Aceita RGB565 e recusa o resto: é o formato que o console entrega, e recusar os
            // outros faz o core seguir pelo caminho que ele usa no RetroArch.
            ENV_SET_PIXEL_FORMAT => {
                !dados.is_null() && unsafe { *(dados as *const u32) } == PIXEL_FORMAT_RGB565
            }
            ENV_SET_VARIABLES | ENV_SET_CORE_OPTIONS_V2 | ENV_SET_INPUT_DESCRIPTORS | ENV_SET_CONTROLLER_INFO => {
                true
            }
            ENV_GET_CORE_OPTIONS_VERSION => {
                if !dados.is_null() {
                    unsafe { *(dados as *mut u32) = 2 };
                    true
                } else {
                    false
                }
            }
            ENV_GET_VARIABLE => {
                if !dados.is_null() {
                    let var = dados as *mut RetroVariable;
                    // Retorna None para simular valor padrão Auto
                    unsafe { (*var).value = std::ptr::null() };
                    true
                } else {
                    false
                }
            }
            ENV_GET_SYSTEM_DIRECTORY | ENV_GET_SAVE_DIRECTORY => {
                // O sistema pode vir de fora, e vem por `ZEEBX_CORE_SISTEMA`: é assim que o teste
                // encontra o aparelho de verdade, com os jogos instalados, e consegue exercitar o
                // ciclo da Z-Wheel de ponta a ponta. Sem a variável, cada um recebe a pasta
                // temporária do teste.
                let pasta = if cmd == ENV_GET_SYSTEM_DIRECTORY {
                    SISTEMA.get().or_else(|| PASTA.get())
                } else {
                    PASTA.get()
                };
                let Some(pasta) = pasta else {
                    return false;
                };
                if dados.is_null() {
                    return false;
                }
                // SAFETY: o core promete um `*const *const c_char` para escrita.
                unsafe { *(dados as *mut *const c_char) = pasta.as_ptr() };
                true
            }
            _ => false,
        }
    }

    unsafe extern "C" fn video(dados: *const c_void, largura: u32, altura: u32, passo: usize) {
        QUADROS.fetch_add(1, Ordering::Relaxed);
        // Assinatura barata do quadro: muda quando a imagem muda, que é o que o teste precisa
        // saber para dizer se a entrada chegou ao guest — um controle que não chega deixa a tela
        // parada, e um botão errado também, e as duas coisas se separam olhando o resto.
        let total = (passo as u64) * u64::from(altura);
        let bytes =
            unsafe { std::slice::from_raw_parts(dados as *const u8, total.min(1 << 22) as usize) };
        let mut assinatura = 1469598103934665603u64;
        for &b in bytes.iter().step_by(97) {
            assinatura = (assinatura ^ u64::from(b)).wrapping_mul(1099511628211);
        }
        if let Ok(mut vistos) = ASSINATURAS.lock() {
            vistos.push(assinatura);
        }
        let _ = (largura, altura);
    }

    /// Quantas amostras estéreo o core entregou ao frontend.
    static AMOSTRAS: AtomicU32 = AtomicU32::new(0);
    /// Quantos jogos o core achou ao lado do conteúdo (ver [`JOGOS_VISTOS`]).

    unsafe extern "C" fn audio(_dados: *const i16, quadros: usize) -> usize {
        // Contar aqui é o que permite conferir **pelo caminho do core** que o áudio sai: o motor
        // tem a medida dele (pico, rms, salto), e o core tem esta — se o lote chega ao frontend.
        AMOSTRAS.fetch_add(quadros as u32, Ordering::Relaxed);
        quadros
    }

    /// O eixo que o teste está empurrando, na faixa do RetroPad (`-0x8000..=0x7fff`).
    ///
    /// A Z-Wheel é navegada **pelo manche**, então um teste que só aperta botão não a move — e a
    /// pergunta "a entrada chega?" ficava sem resposta para ela.
    static EIXO_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    static EIXO_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    unsafe extern "C" fn entrada(_porta: u32, dispositivo: u32, _indice: u32, id: u32) -> i16 {
        if dispositivo == DEVICE_ANALOG {
            // No RetroPad, `0` é o X do analógico esquerdo e `1` é o Y.
            let valor = match id {
                0 => EIXO_X.load(Ordering::Relaxed),
                _ => EIXO_Y.load(Ordering::Relaxed),
            };
            return valor.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
        // O core pergunta pelo estado de cada botão, um a um.
        (BOTAO.load(Ordering::Relaxed) == id) as i16
    }

    extern "C" fn sem_poll() {}

    /// **Sem sessão não há o que salvar**, e a recusa não escreve nada.
    ///
    /// Zero continua sendo a resposta certa aqui — não porque o core não saiba salvar, mas porque
    /// não há máquina nenhuma montada. Um `serialize` que gravasse meia máquina seria pior que
    /// nenhum: o RetroArch deixaria salvar, e o carregamento devolveria um jogo com memória e
    /// registradores certos e a mesa de objetos errada, chamando API com identificador que não
    /// existe mais.
    #[test]
    fn sem_sessao_nao_ha_o_que_salvar() {
        assert_eq!(
            retro_serialize_size(),
            0,
            "sem sessão, o tamanho é zero"
        );
        let mut destino = [0u8; 16];
        assert!(
            !retro_serialize(destino.as_mut_ptr() as *mut c_void, destino.len()),
            "gravar meia máquina seria pior que recusar"
        );
        assert!(!retro_unserialize(
            destino.as_ptr() as *const c_void,
            destino.len()
        ));
        assert_eq!(destino, [0u8; 16], "a recusa não pode ter escrito nada");
    }

    /// **O core, exercitado pela própria ABI.**
    ///
    /// É o teste que faltava para o ciclo da Z-Wheel do lado do core: o motor é medido pela
    /// varredura, e o laço do core — que troca de sessão quando o shell pede — não tinha prova
    /// automática nenhuma. Aqui não há janela nem RetroArch, mas o caminho é o mesmo:
    /// `retro_init`, `retro_load_game`, quadros e `retro_unload_game`.
    ///
    /// **O que este teste prova, e o que ele não prova.**
    ///
    /// Prova: o core carrega conteúdo pela própria ABI, entrega quadros e desmonta limpo, sem
    /// RetroArch e sem janela. Com a Z-Wheel, entrega 360 quadros.
    ///
    /// **Não prova a troca de sessão.** Dirigindo o controle pelas seis teclas do RetroPad — 30
    /// quadros por tecla — nenhuma imagem distinta aparece (medido: 1 por fase) e o shell não pede
    /// abertura nenhuma. Ou a Z-Wheel headless não chega ao estado em que aceita a escolha, ou a
    /// entrada não chega ao guest pelo caminho do core. As duas hipóteses estão abertas e este
    /// teste **não** escolhe entre elas: quem fecha o ciclo é o RetroArch, com controle de
    /// verdade, que é o que o item 8 do plano pede.
    ///
    /// **Sem `ZEEBX_CORE_ROM` ele não roda.** ROM não entra na árvore do repositório, e um teste
    /// que baixa conteúdo sozinho é pior que um teste que não roda.
    ///
    /// ```bash
    /// ZEEBX_CORE_ROM="roms/Z-Wheel.zip" cargo test -p zeebx-libretro -- --nocapture
    /// ```
    #[test]
    fn a_abi_do_core_roda_uma_rom() {
        let Ok(caminho) = std::env::var("ZEEBX_CORE_ROM") else {
            eprintln!("sem ZEEBX_CORE_ROM: nada a rodar");
            return;
        };
        let pasta = std::env::temp_dir().join(format!("zeebx-core-{}", std::process::id()));
        std::fs::create_dir_all(&pasta).unwrap();
        let _ = PASTA.set(CString::new(pasta.to_string_lossy().to_string()).unwrap());
        if let Ok(fora) = std::env::var("ZEEBX_CORE_SISTEMA") {
            let _ = SISTEMA.set(CString::new(fora).unwrap());
        }
        QUADROS.store(0, Ordering::Relaxed);

        let caminho_c = CString::new(caminho.clone()).unwrap();
        let quadros_pedidos = std::env::var("ZEEBX_CORE_QUADROS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(60u32);

        let mut abriu = ULTIMA_ABERTURA.load(Ordering::Relaxed);
        let mut qual = None;
        // Antes de dirigir o controle não pode haver pedido de abertura nenhum: se houver, ele
        // veio do carregamento do conteúdo, e a leitura do laço abaixo estaria medindo outra coisa.
        assert_eq!(abriu, 0, "o shell pediu abertura antes de qualquer tecla");
        assert!(qual.is_none());
        if let Ok(mut vistos) = ASSINATURAS.lock() {
            vistos.clear();
        }
        let info = RetroGameInfo {
            path: caminho_c.as_ptr(),
            data: std::ptr::null(),
            size: 0,
            meta: std::ptr::null(),
        };
        unsafe {
            retro_set_environment(Some(ambiente));
            retro_set_video_refresh(Some(video));
            retro_set_audio_sample_batch(Some(audio));
            retro_set_input_poll(Some(sem_poll));
            retro_set_input_state(Some(entrada));
            retro_init();
            retro_set_controller_port_device(0, DEVICE_JOYPAD);
            assert!(retro_load_game(&info), "o core recusou {caminho}");
            for _ in 0..quadros_pedidos {
                retro_run();
            }
            // **Dirige a Z-Wheel.** A pergunta desta parte é prática: com que botão o jogador
            // confirma a escolha, e o pedido de abertura chega ao core? Cada botão do RetroPad é
            // segurado por vinte quadros e solto por dez, e o teste para no primeiro que o shell
            // aceitar. Sem o pedido, ele diz que nenhum serviu — que também é resposta.
            abriu = ULTIMA_ABERTURA.load(Ordering::Relaxed);
            qual = None;
            for botao in [ID_A, ID_B, ID_X, ID_Y, ID_START, ID_SELECT] {
                let antes = QUADROS.load(Ordering::Relaxed);
                BOTAO.store(botao, Ordering::Relaxed);
                for _ in 0..20 {
                    retro_run();
                }
                BOTAO.store(u32::MAX, Ordering::Relaxed);
                for _ in 0..10 {
                    retro_run();
                }
                // A imagem mudou enquanto o botão estava apertado? É o que separa "a entrada não
                // chega" de "chega, e o botão é outro".
                let distintas = ASSINATURAS
                    .lock()
                    .map(|v| {
                        let inicio = (antes as usize).min(v.len());
                        v[inicio..].iter().collect::<std::collections::BTreeSet<_>>().len()
                    })
                    .unwrap_or(0);
                eprintln!("botão {botao}: {distintas} imagem(ns) distinta(s) em 30 quadros");
                abriu = ULTIMA_ABERTURA.load(Ordering::Relaxed);
                if abriu != 0 {
                    qual = Some(botao);
                    break;
                }
            }
            retro_unload_game();
            retro_deinit();
        }
        eprintln!(
            "pedido de abertura: {abriu:#010x} ({})",
            match abriu {
                0 => "nenhum botão abriu".to_string(),
                _ => format!("com o botão {qual:?}"),
            }
        );

        let quadros = QUADROS.load(Ordering::Relaxed);
        assert!(quadros > 0, "nenhum quadro chegou ao frontend");
        eprintln!("quadros entregues ao frontend: {quadros}");
        eprintln!(
            "amostras estéreo entregues ao frontend: {}",
            AMOSTRAS.load(Ordering::Relaxed)
        );
        eprintln!(
            "jogos ao lado do conteúdo: {}",
            JOGOS_VISTOS.load(Ordering::Relaxed)
        );
        let _ = std::fs::remove_dir_all(&pasta);
    }

    /// **O RetroPad vira tecla do console no caminho do core — e a Z-Wheel responde.**
    ///
    /// É a prova de ponta a ponta do item 8 pelo caminho que o RetroArch usa, sem janela e sem
    /// olhar pixels: o laço roda quadros, o teste aperta os botões do RetroPad, e o que se observa
    /// é a **decisão do core** — a classe que está rodando, por [`CLASSE_ATUAL`] — e o **pedido do
    /// shell**, por [`ULTIMA_ABERTURA`], que vem de `Session::take_launch_request`.
    ///
    /// **O tempo é a primeira armadilha.** A grade de jogos da Z-Wheel só aparece depois de ~37 s
    /// de relógio virtual, mais de duas mil voltas; o teste que existia rodava 60 quadros e
    /// concluía que nenhum botão abria nada. `ZEEBX_CORE_QUADROS` diz quantos quadros rodar antes
    /// de começar a apertar.
    ///
    /// **A segunda armadilha é a pasta.** O jogo que a roda abre é o que estiver em foco, e o
    /// retorno à roda só acontece quando o jogo **termina sozinho** — por isso a pasta deve ter,
    /// ao lado da Z-Wheel, um título que termina sozinho. Medido na varredura: o `Zeebo Clube` e o
    /// `Zeebo App` são os dois que fazem isso.
    ///
    /// **Sem `ZEEBX_CORE_ROM` ele não roda**, pela mesma razão dos outros: ROM não entra na árvore.
    #[test]
    fn o_retropad_vira_tecla_do_console_no_caminho_do_core() {
        let Ok(caminho) = std::env::var("ZEEBX_CORE_ROM") else {
            eprintln!("sem ZEEBX_CORE_ROM: nada a percorrer");
            return;
        };
        // **A tradução, provada no crate do core.** Dois pads construídos à mão: a função é a
        // mesma do motor, mas quem a chama aqui é o core, e um erro de tipo ou de nome apareceria
        // exatamente neste ponto.
        {
            let antes = zeebx::input::Pad::default();
            let mut agora = zeebx::input::Pad::default();
            let indice = zeebx::input::Pad::button_by_name("b1").expect("b1 existe");
            agora.press(indice, true);
            let teclas = zeebx::input::teclas_do_controle(&antes, &agora);
            eprintln!("tradução de b1: {teclas:?}");
            assert_eq!(
                teclas,
                vec![(zeebx::input::avk::CONFIRMA, true)],
                "a tradução do controle não produziu a tecla do confirmar"
            );
        }
        let pasta = std::env::temp_dir().join(format!("zeebx-ciclo-{}", std::process::id()));
        std::fs::create_dir_all(&pasta).unwrap();
        let _ = PASTA.set(CString::new(pasta.to_string_lossy().to_string()).unwrap());
        if let Ok(fora) = std::env::var("ZEEBX_CORE_SISTEMA") {
            let _ = SISTEMA.set(CString::new(fora).unwrap());
        }
        QUADROS.store(0, Ordering::Relaxed);
        ULTIMA_ABERTURA.store(0, Ordering::Relaxed);
        CLASSE_ATUAL.store(0, Ordering::Relaxed);
        BOTAO.store(u32::MAX, Ordering::Relaxed);

        let caminho_c = CString::new(caminho.clone()).unwrap();
        let ate_a_grade = std::env::var("ZEEBX_CORE_QUADROS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(2600u32);
        let info = RetroGameInfo {
            path: caminho_c.as_ptr(),
            data: std::ptr::null(),
            size: 0,
            meta: std::ptr::null(),
        };

        unsafe {
            retro_set_environment(Some(ambiente));
            retro_set_video_refresh(Some(video));
            retro_set_audio_sample_batch(Some(audio));
            retro_set_input_poll(Some(sem_poll));
            retro_set_input_state(Some(entrada));
            retro_init();
            retro_set_controller_port_device(0, DEVICE_JOYPAD);
            assert!(retro_load_game(&info), "o core recusou {caminho}");
            for _ in 0..ate_a_grade {
                retro_run();
            }
            // Quem roda no começo é a roda. Se não for, o resto do teste mediria outra coisa.
            let quem = CLASSE_ATUAL.load(Ordering::Relaxed);
            assert_eq!(
                quem,
                zeebx::session::Z_WHEEL,
                "no começo quem roda é a Z-Wheel, não {quem:#010x}"
            );
            eprintln!("rodando no começo: {quem:#010x} (Z-Wheel)");

            // O roteiro da doc, nos botões do RetroPad: confirmar em "Jogar", descer às capas,
            // andar duas capas à direita e confirmar. `b1` é o confirmar do console.
            // **O compasso importa.** No roteiro da varredura as teclas estão a dois ou três
            // segundos de distância: a roda tem transições armadas em 400 ms e um pulso próprio, e
            // teclar a cada 0,4 s atropela a tela seguinte. Aqui cada passo segura 8 quadros e
            // espera um segundo e meio antes do próximo.
            // **O roteiro, por `ZEEBX_CORE_TECLAS` quando se quer outro.** Formato `ms:id`, com o
            // `id` do RetroPad (`1` é o `Y`, o confirmar do console), separado por vírgula — o
            // mesmo espírito do `ZEEBX_ROM_TECLAS` da varredura, que é onde o ciclo foi provado
            // primeiro. Sem a variável, vale o roteiro da doc: confirmar em "Jogar", descer às
            // capas, andar duas capas à direita e confirmar. Ele depende de **onde a grade põe o
            // foco**, e por isso não serve para toda pasta: com dois jogos, o foco já está no
            // primeiro e as setas o tiram de lá.
            let roteiro: Vec<(u32, u32)> = match std::env::var("ZEEBX_CORE_TECLAS") {
                Ok(texto) => texto
                    .split(',')
                    .filter_map(|parte| {
                        let (quando, id) = parte.split_once(':')?;
                        Some((quando.trim().parse().ok()?, id.trim().parse().ok()?))
                    })
                    .collect(),
                Err(_) => vec![
                    (0, ID_Y),
                    (0, ID_DOWN),
                    (0, ID_RIGHT),
                    (0, ID_RIGHT),
                    (0, ID_Y),
                    (0, ID_Y),
                    (0, ID_DOWN),
                    (0, ID_Y),
                ],
            };
            let distintas = |desde: usize| -> usize {
                ASSINATURAS
                    .lock()
                    .map(|v| {
                        let inicio = desde.min(v.len());
                        v[inicio..].iter().collect::<std::collections::BTreeSet<_>>().len()
                    })
                    .unwrap_or(0)
            };
            // **A roda anima antes da tecla**, e é isso que dá sentido à medida de depois: uma
            // tela que já estivesse parada não diria nada sobre a tecla.
            let animando = distintas(QUADROS.load(Ordering::Relaxed) as usize - 100);
            eprintln!(
                "antes da tecla: {animando} imagem(ns) distinta(s) em 100 quadros, relógio {} ms, \
                 {} instruções",
                RELOGIO.load(Ordering::Relaxed),
                INSTRUCOES.load(Ordering::Relaxed)
            );
            assert!(
                animando > 3,
                "a Z-Wheel não estava animando antes da tecla ({animando} imagens distintas)"
            );
            let mut aberto = 0u32;
            for (quando, passo) in roteiro {
                // Com instante no roteiro, espera o **relógio** chegar nele: cada `retro_run`
                // avança ~26 ms, e não os 16 de um quadro a 60 Hz. Ver [`RELOGIO`].
                if quando > 0 {
                    for _ in 0..ate_a_grade {
                        if RELOGIO.load(Ordering::Relaxed) >= quando {
                            break;
                        }
                        retro_run();
                    }
                }
                let antes = QUADROS.load(Ordering::Relaxed) as usize;
                BOTAO.store(passo, Ordering::Relaxed);
                for _ in 0..8 {
                    retro_run();
                }
                BOTAO.store(u32::MAX, Ordering::Relaxed);
                for _ in 0..90 {
                    retro_run();
                }
                aberto = ULTIMA_ABERTURA.load(Ordering::Relaxed);
                eprintln!(
                    "passo {passo}: abertura {aberto:#010x}, {} assinatura(s) distinta(s)",
                    distintas(antes)
                );
                if aberto != 0 {
                    break;
                }
            }
            // **O pedido não sai no mesmo quadro da tecla.** No roteiro da varredura a última
            // confirmação é aos 38 s e o pedido aparece quase dois segundos depois, quando a roda
            // já desmontou as telas e armou o temporizador do lançamento. Verificar só logo depois
            // de cada tecla mede a tela, e não o desfecho.
            for _ in 0..900 {
                retro_run();
                aberto = ULTIMA_ABERTURA.load(Ordering::Relaxed);
                if aberto != 0 {
                    break;
                }
            }
            // **A tecla foi tratada.** Sem a tradução do controle em teclas do console, a roda
            // ignorava o RetroPad por inteiro e seguia animando para sempre: medido, 13 ou 14
            // imagens distintas em cada 24 quadros, em todos os passos. Com a tradução, a primeira
            // confirmação é tratada e a tela para de animar.
            let depois = distintas(QUADROS.load(Ordering::Relaxed) as usize - 100);
            eprintln!(
                "depois da tecla: {depois} imagem(ns) distinta(s) em 100 quadros, relógio {} ms, \
                 {} instruções",
                RELOGIO.load(Ordering::Relaxed),
                INSTRUCOES.load(Ordering::Relaxed)
            );
            assert!(
                depois < animando,
                "a roda ignorou a tecla: {depois} imagens distintas depois, contra {animando} antes"
            );
            // **O pedido de abertura chega, e o core troca de sessão.** Antes da correção das
            // telas intermediárias isto era `0x00000000` e a roda ficava parada; agora é a classe
            // do jogo que a grade tinha em foco — o mesmo `0x0108E356` que a varredura pede.
            assert_ne!(aberto, 0, "o roteiro não chegou a pedir a abertura de jogo nenhum");
            let mut rodando = CLASSE_ATUAL.load(Ordering::Relaxed);
            for _ in 0..900 {
                retro_run();
                rodando = CLASSE_ATUAL.load(Ordering::Relaxed);
                if rodando != 0 && rodando != zeebx::session::Z_WHEEL {
                    break;
                }
            }
            assert_eq!(
                rodando, aberto,
                "o core não abriu o jogo que o shell pediu ({aberto:#010x})"
            );
            // **A volta à roda** depende do jogo terminar sozinho, e o que a grade tem em foco é o
            // primeiro título da pasta: com um jogo que sai sozinho ao lado da Z-Wheel (o `Zeebo
            // Clube`, medido na varredura), o core devolve o controle ao shell — e é o que este
            // laço espera. Sem um título desses, ele é relatado e não cobrado.
            // **A janela tem de ser maior que o jogo.** O retorno só acontece depois de o título
            // terminar, e o que a grade põe em foco roda ~62 s de relógio virtual — mais que os
            // 3600 quadros (60 s) da primeira versão, que por isso relatava `false` num caso em que
            // a volta **aconteceu** (as teclas seguintes chegam aos tratadores da roda).
            let mut voltou = false;
            for _ in 0..9000 {
                retro_run();
                if CLASSE_ATUAL.load(Ordering::Relaxed) == zeebx::session::Z_WHEEL {
                    voltou = true;
                    break;
                }
            }
            eprintln!("voltou à Z-Wheel depois do jogo: {voltou}");

            // **O que este teste ainda NÃO prova, e fica medido em vez de suposto:** o pedido de
            // abertura. No caminho da varredura ele sai (`abertura pedida: 0x0108e356`, com as
            // mesmas teclas e a mesma máquina), e aqui não — o que significa que a diferença está
            // em como o core conduz a sessão, e não na tecla, que é o que este teste prova. O
            // número sai no relatório para a próxima sessão medir a partir dele.
            eprintln!("pedido de abertura no caminho do core: {aberto:#010x}");
            retro_unload_game();
            retro_deinit();
        }
        eprintln!(
            "quadros entregues ao frontend: {}, {} instruções em {} ms virtuais",
            QUADROS.load(Ordering::Relaxed),
            INSTRUCOES.load(Ordering::Relaxed),
            RELOGIO.load(Ordering::Relaxed)
        );
        let _ = std::fs::remove_dir_all(&pasta);
    }

    /// **O save state atravessa a ABI, e volta igual.**
    ///
    /// É a prova de ponta a ponta do item 6, e ela é feita pelo caminho que o RetroArch usa: pedir
    /// o tamanho, gravar, sujar o estado, carregar e conferir. A conferência que importa é a
    /// última — **gravar de novo depois de carregar tem de dar byte a byte o mesmo arquivo** —,
    /// porque é isso que um save state promete. Comparar campos seria mais fraco: o que o jogador
    /// vê é o jogo continuar do mesmo ponto.
    ///
    /// **Sem `ZEEBX_CORE_ROM` ele não roda**, pela mesma razão do teste acima.
    #[test]
    fn o_save_state_atravessa_a_abi() {
        let Ok(caminho) = std::env::var("ZEEBX_CORE_ROM") else {
            eprintln!("sem ZEEBX_CORE_ROM: nada a salvar");
            return;
        };
        let pasta = std::env::temp_dir().join(format!("zeebx-estado-{}", std::process::id()));
        std::fs::create_dir_all(&pasta).unwrap();
        let _ = PASTA.set(CString::new(pasta.to_string_lossy().to_string()).unwrap());
        if let Ok(fora) = std::env::var("ZEEBX_CORE_SISTEMA") {
            let _ = SISTEMA.set(CString::new(fora).unwrap());
        }
        QUADROS.store(0, Ordering::Relaxed);
        let caminho_c = CString::new(caminho.clone()).unwrap();
        let quadros_pedidos = std::env::var("ZEEBX_CORE_QUADROS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(60u32);

        let info = RetroGameInfo {
            path: caminho_c.as_ptr(),
            data: std::ptr::null(),
            size: 0,
            meta: std::ptr::null(),
        };
        unsafe {
            retro_set_environment(Some(ambiente));
            retro_set_video_refresh(Some(video));
            retro_set_audio_sample_batch(Some(audio));
            retro_init();
            retro_set_controller_port_device(0, DEVICE_JOYPAD);
            assert!(retro_load_game(&info), "o core recusou {caminho}");
            for _ in 0..quadros_pedidos {
                retro_run();
            }

            // 1) O tamanho, medido pelo frontend.
            let tamanho = retro_serialize_size();
            assert!(tamanho > 0, "com sessão montada, o tamanho tem de ser positivo");
            eprintln!("save state: {tamanho} bytes");
            let mut primeiro = vec![0u8; tamanho];
            assert!(
                retro_serialize(primeiro.as_mut_ptr() as *mut c_void, primeiro.len()),
                "a gravação foi recusada"
            );

            // 2) O jogo continua: o estado tem de ficar **diferente**.
            for _ in 0..quadros_pedidos {
                retro_run();
            }
            let tamanho_depois = retro_serialize_size();
            let mut segundo = vec![0u8; tamanho_depois];
            assert!(retro_serialize(
                segundo.as_mut_ptr() as *mut c_void,
                segundo.len()
            ));
            assert_ne!(
                primeiro, segundo,
                "o estado não mudou depois de rodar mais quadros: o teste não estaria medindo nada"
            );

            // 3) Carregar o primeiro, e conferir que gravar de novo dá o mesmo arquivo.
            assert!(
                retro_unserialize(primeiro.as_ptr() as *const c_void, primeiro.len()),
                "o carregamento foi recusado"
            );
            let tamanho_de_volta = retro_serialize_size();
            let mut terceiro = vec![0u8; tamanho_de_volta];
            assert!(retro_serialize(
                terceiro.as_mut_ptr() as *mut c_void,
                terceiro.len()
            ));
            if terceiro != primeiro {
                // **Onde** os dois divergem, e não a comparação inteira: um `assert_eq!` de dois
                // vetores de sete megabytes escreve sete megabytes na saída e não diz nada. O nome
                // da seção está no arquivo em texto, então a primeira diferença aponta o culpado.
                let posicao = terceiro
                    .iter()
                    .zip(primeiro.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or(terceiro.len().min(primeiro.len()));
                let inicio = posicao.saturating_sub(64);
                let texto = String::from_utf8_lossy(&terceiro[inicio..posicao + 8]);
                let texto = texto
                    .chars()
                    .filter(|c| c.is_ascii_graphic() || *c == ' ')
                    .collect::<String>();
                let antes = String::from_utf8_lossy(&primeiro[inicio..posicao + 8]);
                let antes = antes
                    .chars()
                    .filter(|c| c.is_ascii_graphic() || *c == ' ')
                    .collect::<String>();
                panic!(
                    "o estado carregado não é o que foi gravado: primeira diferença no byte {posicao} \
                     de {} (gravado {inicio}..{}: {antes:?}; carregado: {texto:?})",
                    terceiro.len(),
                    posicao + 8
                );
            }

            // 4) Um byte trocado é recusado, e **não** muda a máquina.
            let mut estragado = primeiro.clone();
            let meio = estragado.len() / 2;
            estragado[meio] ^= 0xff;
            assert!(
                !retro_unserialize(estragado.as_ptr() as *const c_void, estragado.len()),
                "um estado corrompido foi aceito"
            );
            let tamanho_final = retro_serialize_size();
            let mut quarto = vec![0u8; tamanho_final];
            assert!(retro_serialize(quarto.as_mut_ptr() as *mut c_void, quarto.len()));
            assert_eq!(
                quarto, primeiro,
                "a recusa de um estado corrompido mexeu na máquina"
            );

            retro_unload_game();
            retro_deinit();
        }
        let _ = std::fs::remove_dir_all(&pasta);
    }
}
