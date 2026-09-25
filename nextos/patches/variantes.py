p='src/video/gpu.rs'; s=open(p).read()
def rep(a,b,n=1):
    global s
    c=s.count(a)
    assert c==n, (c,a[:70])
    s=s.replace(a,b)

rep("        self.uniformes = Uniformes::default();\n",
    "        self.uniformes = Uniformes::default();\n        for (_, u) in self.variantes.values_mut() {\n            *u = Uniformes::default();\n        }\n")
rep("            gl.delete_program(self.programa);\n",
    "            gl.delete_program(self.programa);\n            for (programa, _) in self.variantes.values() {\n                if *programa != self.programa {\n                    gl.delete_program(*programa);\n                }\n            }\n")

rep("""        let gl = &self.gl;
        unsafe {
            gl.use_program(Some(self.programa));
            gl.bind_vertex_array(self.vao);""", """        if self.es2 {
            let neblina = self.fill.neblina;
            let chave = Variante {
                t0: textura.map(|_| ChaveEnv::de(&self.fill.env_textura)),
                t1: textura1.map(|_| ChaveEnv::de(&self.fill.unidade1.env)),
                neblina: neblina.ligada && neblina.permitida,
                alfa: match self.fill.teste_alfa {
                    true => codigo_alfa(self.fill.func_alfa),
                    false => 7,
                },
            };
            if !self.variantes.contains_key(&chave) {
                match unsafe { liga_es2(&self.gl, &fragmento_es2(&chave)) } {
                    Ok(programa) => {
                        self.variantes.insert(chave, (programa, Uniformes::default()));
                    }
                    Err(erro) => {
                        eprintln!("Zeebx: variante de shader ES 2.0 recusada ({erro}); usando o geral");
                        self.variantes.insert(chave, (self.programa, Uniformes::default()));
                    }
                }
            }
            self.variante_atual = Some(chave);
        } else {
            self.variante_atual = None;
        }
        let gl = &self.gl;
        let (programa, uniformes) = match self.variante_atual.and_then(|c| self.variantes.get(&c)) {
            Some((p, u)) => (*p, u),
            None => (self.programa, &self.uniformes),
        };
        unsafe {
            gl.use_program(Some(programa));
            gl.bind_vertex_array(self.vao);""")
i=s.index("        let (programa, uniformes) = match self.variante_atual")
i=s.index("        unsafe {\n            gl.use_program(Some(programa));", i)
j=s.index("mede::soma(&mede::NS_SUBMETE, t_mede);", i)
bloco=s[i:j]
n1=bloco.count("&self.uniformes, self.programa")
bloco=bloco.replace("&self.uniformes, self.programa","uniformes, programa").replace("                &self.uniformes,\n                self.programa,","                uniformes,\n                programa,")
assert "self.programa" not in bloco and "self.uniformes" not in bloco, bloco
s=s[:i]+bloco+s[j:]
gerador=open('nextos/patches/gerador.rs').read()
rep("/// Compila o par de shaders, tentando GLSL 3.30 e caindo para ES 3.00 — ou",
    gerador+"\n/// Compila o par de shaders, tentando GLSL 3.30 e caindo para ES 3.00 — ou")
open(p,'w').write(s); print('ok', n1)
