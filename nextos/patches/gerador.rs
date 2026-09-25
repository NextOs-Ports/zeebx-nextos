/// O ambiente de uma unidade reduzido ao que muda o **código** do shader: o modo e, no
/// `GL_COMBINE`, função, fontes e operandos. Cores e escalas continuam uniformes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ChaveEnv {
    modo: i32,
    cmb: [i32; 2],
    src: [[i32; 3]; 2],
    op: [[i32; 3]; 2],
}

impl ChaveEnv {
    fn de(env: &TexEnv) -> Self {
        let modo = codigo_env(env.modo);
        if modo != 4 {
            return Self { modo, cmb: [0; 2], src: [[0; 3]; 2], op: [[0; 3]; 2] };
        }
        Self {
            modo,
            cmb: env.combina.map(codigo_funcao),
            src: env.fontes.map(|l| l.map(codigo_fonte)),
            op: env.operandos.map(|l| l.map(codigo_operando)),
        }
    }
}

/// Uma combinação de estado com shader próprio no ES 2.0.
///
/// **Por que existe.** O shader geral decide tudo por uniformes inteiros, arrays e laços: é o
/// pipeline fixo inteiro rodando por pixel. Numa placa de desktop isso sai quase de graça; no
/// processador de pixels do Mali-400/450 cada ramo custa, e o `discard` desliga o teste de
/// profundidade antecipado. Medido no RE4 no Mali-450: 270 ms de placa por quadro com 55
/// desenhos e 328 vértices. Com a combinação fixada no código, o compilador apaga os ramos
/// mortos e o fragmento vira poucas instruções.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Variante {
    t0: Option<ChaveEnv>,
    t1: Option<ChaveEnv>,
    neblina: bool,
    /// O código de [`codigo_alfa`]; 7 é teste desligado, e só então não há `discard`.
    alfa: i32,
}

/// O fragmento em GLSL ES 1.00 para uma [`Variante`], em linha reta.
///
/// Segue caso a caso o shader geral ([`FRAGMENTO`]), que por sua vez segue o `TexEnv` do
/// rasterizador de software.
fn fragmento_es2(v: &Variante) -> String {
    let mut f = String::from(
        "precision mediump float;\n\
         varying vec4 vcor;\nvarying vec2 vuv;\nvarying float vfog;\nvarying vec2 vuv1;\n\
         uniform sampler2D amostra;\nuniform sampler2D amostra1;\n\
         uniform vec4 cor_env;\nuniform vec4 cor_env1;\n\
         uniform float escala_rgb;\nuniform float escala_alfa;\n\
         uniform float escala_rgb1;\nuniform float escala_alfa1;\n\
         uniform vec3 cor_neblina;\nuniform float ref_alfa;\n\
         void main() {\n    vec4 primaria = vcor;\n    vec4 cor = primaria;\n",
    );
    for (unidade, chave) in [(0, v.t0), (1, v.t1)] {
        let Some(c) = chave else { continue };
        let (amostra, uv, suf) = match unidade {
            0 => ("amostra", "vuv", ""),
            _ => ("amostra1", "vuv1", "1"),
        };
        f.push_str(&format!("    {{\n    vec4 t = texture2D({amostra}, {uv});\n"));
        match c.modo {
            0 => f.push_str("    cor = t;\n"),
            1 => f.push_str("    cor = vec4(cor.rgb * (1.0 - t.a) + t.rgb * t.a, cor.a);\n"),
            2 => f.push_str("    cor = vec4(min(cor.rgb + t.rgb, vec3(1.0)), cor.a * t.a);\n"),
            4 => {
                let fonte = |q: i32| match q {
                    0 => "t".to_string(),
                    1 => format!("cor_env{suf}"),
                    2 => "primaria".to_string(),
                    _ => "ant".to_string(),
                };
                let op_rgb = |op: i32, x: String| match op {
                    0 => format!("{x}.rgb"),
                    1 => format!("(vec3(1.0) - {x}.rgb)"),
                    2 => format!("vec3({x}.a)"),
                    _ => format!("vec3(1.0 - {x}.a)"),
                };
                let op_alfa = |op: i32, x: String| match op {
                    1 | 3 => format!("(1.0 - {x}.a)"),
                    _ => format!("{x}.a"),
                };
                f.push_str("    vec4 ant = cor;\n");
                for i in 0..3 {
                    f.push_str(&format!(
                        "    vec3 r{i} = {};\n    float a{i} = {};\n",
                        op_rgb(c.op[0][i], fonte(c.src[0][i])),
                        op_alfa(c.op[1][i], fonte(c.src[1][i])),
                    ));
                }
                let rgb = match c.cmb[0] {
                    0 => "r0",
                    2 => "r0 + r1",
                    3 => "r0 + r1 - 0.5",
                    4 => "r0 * r2 + r1 * (1.0 - r2)",
                    5 => "r0 - r1",
                    6 | 7 => "vec3(4.0 * dot(r0 - 0.5, r1 - 0.5))",
                    _ => "r0 * r1",
                };
                let alfa = match c.cmb[1] {
                    0 => "a0",
                    2 => "a0 + a1",
                    3 => "a0 + a1 - 0.5",
                    4 => "a0 * a2 + a1 * (1.0 - a2)",
                    5 => "a0 - a1",
                    _ => "a0 * a1",
                };
                f.push_str(&format!(
                    "    vec3 rgb = clamp(({rgb}) * escala_rgb{suf}, 0.0, 1.0);\n    float alfa = clamp(({alfa}) * escala_alfa{suf}, 0.0, 1.0);\n"
                ));
                if c.cmb[0] == 7 {
                    f.push_str("    alfa = rgb.r;\n");
                }
                f.push_str("    cor = vec4(rgb, alfa);\n");
            }
            _ => f.push_str("    cor = cor * t;\n"),
        }
        f.push_str("    }\n");
    }
    if v.neblina {
        f.push_str("    cor = vec4(mix(cor_neblina, cor.rgb, clamp(vfog, 0.0, 1.0)), cor.a);\n");
    }
    let falha = match v.alfa {
        0 => Some("true"),
        1 => Some("!(cor.a < ref_alfa)"),
        2 => Some("!(cor.a == ref_alfa)"),
        3 => Some("!(cor.a <= ref_alfa)"),
        4 => Some("!(cor.a > ref_alfa)"),
        5 => Some("!(cor.a != ref_alfa)"),
        6 => Some("!(cor.a >= ref_alfa)"),
        _ => None,
    };
    if let Some(cond) = falha {
        f.push_str(&format!("    if ({cond}) {{ discard; }}\n"));
    }
    f.push_str("    gl_FragColor = cor;\n}\n");
    f
}

/// Liga o vértice traduzido para GLSL ES 1.00 com um fragmento gerado por [`fragmento_es2`].
unsafe fn liga_es2(gl: &glow::Context, fragmento: &str) -> Result<glow::Program, String> {
    unsafe {
        let programa = gl.create_program()?;
        let mut shaders = Vec::new();
        let vertice = format!(
            "#version 100\nprecision highp float;\n{}",
            fonte_es2(glow::VERTEX_SHADER, VERTICE)
        );
        let fragmento = format!("#version 100\n{fragmento}");
        for (tipo, texto) in [(glow::VERTEX_SHADER, vertice), (glow::FRAGMENT_SHADER, fragmento)] {
            let shader = gl.create_shader(tipo)?;
            gl.shader_source(shader, &texto);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let erro = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                for s in shaders {
                    gl.delete_shader(s);
                }
                gl.delete_program(programa);
                return Err(erro);
            }
            gl.attach_shader(programa, shader);
            shaders.push(shader);
        }
        for (indice, nome) in ATRIBUTOS.iter().enumerate() {
            gl.bind_attrib_location(programa, indice as u32, nome);
        }
        gl.link_program(programa);
        for s in shaders {
            gl.detach_shader(programa, s);
            gl.delete_shader(s);
        }
        if !gl.get_program_link_status(programa) {
            let erro = gl.get_program_info_log(programa);
            gl.delete_program(programa);
            return Err(erro);
        }
        Ok(programa)
    }
}
