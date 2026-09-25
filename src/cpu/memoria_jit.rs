//! A memória do guest do jeito que o Dynarmic a enxerga, com fastmem quando o host deixa.
//!
//! **Por que existe.** Com só a tabela de páginas, cada leitura e escrita do código recompilado
//! custa oito instruções no A53 (alinhamento, índice, carga da tabela, teste de nulo, máscara) antes
//! do acesso de verdade, e toda página que precisa ser vigiada — código executado, superfícies,
//! vtables só de leitura — manda **leitura e escrita** para a callback, porque a tabela não
//! distingue uma da outra. No Need for Speed Carbon eram 2.200 idas à callback por quadro.
//!
//! O fastmem troca isso por uma arena de 4 GiB reservada no host em que o endereço do guest é o
//! deslocamento: o acesso vira uma instrução só. O que precisa ser visto passa a ser separado por
//! **proteção de página**: página de código e página vigiada ficam só de leitura na arena, então
//! a leitura segue rápida e só a escrita falta; na falta o Dynarmic recompila aquele bloco com a
//! tabela de páginas, que continua nula ali, e a escrita chega à callback como antes.
//!
//! **A mesma memória em dois lugares.** O host (as APIs BREW, o carregador, o save state) escreve
//! em qualquer página a qualquer hora, inclusive nas que a arena protege. Por isso cada região é
//! uma memória anônima compartilhada, mapeada duas vezes: a vista do host, sempre gravável, e a
//! cópia na arena, com as proteções. O `mremap` com tamanho antigo zero duplica o mapeamento sem
//! arquivo nenhum — o kernel do aparelho é o 3.14, que ainda não tem `memfd_create`.
//!
//! Sem Linux, com página do host diferente de 4 KB, ou com `ZEEBX_SEM_FASTMEM=1`, cai no jeito
//! antigo: vetores no heap e só a tabela de páginas.

use crate::cpu::mem::{GuestMemory, MemError};

/// A página do guest, que também tem de ser a do host para a proteção valer por página.
const PAGINA: usize = 4096;

/// Bytes válidos além do fim de cada região na vista do host. Uma leitura de quatro bytes nos
/// últimos bytes de uma página que termina a região cruza a borda pela tabela de páginas; com a
/// folga ela lê memória do processo em vez de sair do mapeamento.
const FOLGA: usize = 16;

/// Uma região do mapa do guest e onde ela mora no host.
pub(super) struct Regiao {
    pub name: &'static str,
    pub base: u32,
    pub len: usize,
    pub writable: bool,
    pub executavel: bool,
    /// Início da vista do host: sempre gravável, `len + FOLGA` bytes válidos.
    pub ptr: *mut u8,
}

impl Regiao {
    fn contem(&self, addr: u32, len: u32) -> bool {
        let (a, b) = (u64::from(addr), u64::from(self.base));
        a >= b && a + u64::from(len) <= b + self.len as u64
    }
}

enum Dono {
    /// Sem fastmem: a memória é do heap.
    Vetores(#[allow(dead_code)] Vec<Vec<u8>>),
    /// Com fastmem: as vistas e a arena são mapeamentos, desfeitos no `Drop`.
    #[cfg(target_os = "linux")]
    Mapas { vistas: Vec<(*mut u8, usize)>, arena: *mut u8 },
}

pub(super) struct MemoriaJit {
    regioes: Vec<Regiao>,
    /// A arena de 4 GiB (endereço do guest = deslocamento), ou nulo sem fastmem.
    arena: *mut u8,
    dono: Dono,
}

/// Tamanho reservado para a arena: os 4 GiB do guest e uma sobra para o acesso de até 16 bytes
/// que começa no último endereço.
#[cfg(target_os = "linux")]
const ARENA: usize = (1 << 32) + 64 * 1024;

impl Default for MemoriaJit {
    fn default() -> Self {
        Self {
            regioes: Vec::new(),
            arena: std::ptr::null_mut(),
            dono: Dono::Vetores(Vec::new()),
        }
    }
}

impl MemoriaJit {
    /// Copia o mapa montado pelo carregador.
    pub fn nova(mem: &GuestMemory) -> Self {
        #[cfg(target_os = "linux")]
        if std::env::var_os("ZEEBX_SEM_FASTMEM").is_none()
            && let Some(m) = unsafe { Self::com_arena(mem) }
        {
            return m;
        }
        Self::em_vetores(mem)
    }

    fn em_vetores(mem: &GuestMemory) -> Self {
        let mut vetores = Vec::new();
        let mut regioes = Vec::new();
        for r in mem.regions() {
            let mut v = Vec::with_capacity(r.bytes.len() + FOLGA);
            v.extend_from_slice(&r.bytes);
            v.resize(r.bytes.len() + FOLGA, 0);
            regioes.push(Regiao {
                name: r.name,
                base: r.base,
                len: r.bytes.len(),
                writable: r.writable,
                executavel: r.executavel,
                ptr: v.as_mut_ptr(),
            });
            vetores.push(v);
        }
        Self {
            regioes,
            arena: std::ptr::null_mut(),
            dono: Dono::Vetores(vetores),
        }
    }

    #[cfg(target_os = "linux")]
    unsafe fn com_arena(mem: &GuestMemory) -> Option<Self> {
        use libc::{
            MAP_ANONYMOUS, MAP_FAILED, MAP_NORESERVE, MAP_PRIVATE, MAP_SHARED, MREMAP_FIXED,
            MREMAP_MAYMOVE, PROT_NONE, PROT_READ, PROT_WRITE,
        };
        if unsafe { libc::sysconf(libc::_SC_PAGESIZE) } != PAGINA as libc::c_long {
            return None;
        }
        // Duas regiões na mesma página não teriam como ter proteções diferentes.
        let mut faixas: Vec<(u64, u64)> = mem
            .regions()
            .iter()
            .map(|r| {
                let de = u64::from(r.base) / PAGINA as u64;
                (de, (u64::from(r.base) + r.bytes.len() as u64).div_ceil(PAGINA as u64))
            })
            .collect();
        faixas.sort();
        if mem.regions().iter().any(|r| r.base as usize % PAGINA != 0)
            || faixas.windows(2).any(|w| w[1].0 < w[0].1)
        {
            return None;
        }
        let arena = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ARENA,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
                -1,
                0,
            )
        };
        if arena == MAP_FAILED {
            return None;
        }
        let arena = arena.cast::<u8>();
        let mut m = Self {
            regioes: Vec::new(),
            arena,
            dono: Dono::Mapas {
                vistas: Vec::new(),
                arena,
            },
        };
        for r in mem.regions() {
            let tamanho = (r.bytes.len() + FOLGA).next_multiple_of(PAGINA);
            let vista = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    tamanho,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if vista == MAP_FAILED {
                return None; // o `Drop` desfaz o que já foi mapeado
            }
            let vista = vista.cast::<u8>();
            if let Dono::Mapas { vistas, .. } = &mut m.dono {
                vistas.push((vista, tamanho));
            }
            // Só as páginas com algum byte não nulo: a memória nova já é zero, e copiar os 64 MB
            // zerados do heap tocaria (e alocaria) cada página deles.
            for (i, pedaco) in r.bytes.chunks(PAGINA).enumerate() {
                if pedaco.iter().any(|&b| b != 0) {
                    unsafe {
                        std::ptr::copy_nonoverlapping(pedaco.as_ptr(), vista.add(i * PAGINA), pedaco.len())
                    };
                }
            }
            // Na arena entram só as páginas da região; a folga é da vista.
            let na_arena = r.bytes.len().next_multiple_of(PAGINA);
            if na_arena > 0 {
                let destino = unsafe { arena.add(r.base as usize) };
                let feito = unsafe {
                    libc::mremap(
                        vista.cast(),
                        0,
                        na_arena,
                        MREMAP_MAYMOVE | MREMAP_FIXED,
                        destino.cast::<libc::c_void>(),
                    )
                };
                if feito != destino.cast() {
                    return None;
                }
                if !r.writable {
                    unsafe { libc::mprotect(destino.cast(), na_arena, PROT_READ) };
                }
                // A página que a região não completa: o resto dela não é do guest, e acessá-lo
                // tem de continuar sendo falta. Fica sem acesso, e a tabela também a deixa nula.
                if r.bytes.len() % PAGINA != 0 {
                    let ultima = unsafe { destino.add(na_arena - PAGINA) };
                    unsafe { libc::mprotect(ultima.cast(), PAGINA, PROT_NONE) };
                }
            }
            m.regioes.push(Regiao {
                name: r.name,
                base: r.base,
                len: r.bytes.len(),
                writable: r.writable,
                executavel: r.executavel,
                ptr: vista,
            });
        }
        Some(m)
    }

    /// A arena para `Config::fastmem`, ou `None` sem fastmem.
    pub fn arena(&self) -> Option<*mut u8> {
        (!self.arena.is_null()).then_some(self.arena)
    }

    pub fn regioes(&self) -> &[Regiao] {
        &self.regioes
    }

    fn regiao(&self, addr: u32, len: u32) -> Option<&Regiao> {
        self.regioes.iter().find(|r| r.contem(addr, len))
    }

    pub fn executavel(&self, addr: u32) -> bool {
        self.regiao(addr, 4).is_some_and(|r| r.executavel)
    }

    pub fn read(&self, addr: u32, len: u32) -> Result<&[u8], MemError> {
        let r = self.regiao(addr, len).ok_or(MemError::Unmapped { addr, len })?;
        Ok(unsafe { std::slice::from_raw_parts(r.ptr.add((addr - r.base) as usize), len as usize) })
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), MemError> {
        let len = data.len() as u32;
        let r = self.regiao(addr, len).ok_or(MemError::Unmapped { addr, len })?;
        if !r.writable {
            return Err(MemError::ReadOnly { addr, region: r.name });
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), r.ptr.add((addr - r.base) as usize), data.len())
        };
        Ok(())
    }

    /// O início da `pagina` na vista do host, quando ela é gravável e cabe inteira numa região:
    /// o que a tabela de páginas do Dynarmic pode apontar.
    pub fn ponteiro_da_pagina(&self, pagina: u32) -> *mut u8 {
        let inicio = u64::from(pagina) * PAGINA as u64;
        let fim = inicio + PAGINA as u64;
        self.regioes
            .iter()
            .find(|r| r.writable && inicio >= u64::from(r.base) && fim <= u64::from(r.base) + r.len as u64)
            .map_or(std::ptr::null_mut(), |r| unsafe {
                r.ptr.add((inicio - u64::from(r.base)) as usize)
            })
    }

    /// Com fastmem, deixa a `pagina` gravável ou só de leitura na arena. Só mexe nas páginas que
    /// a tabela também pode apontar (inteiras numa região gravável); as outras já nascem com a
    /// proteção certa e não mudam.
    pub fn protege(&self, pagina: u32, gravavel: bool) {
        #[cfg(target_os = "linux")]
        if !self.arena.is_null() && !self.ponteiro_da_pagina(pagina).is_null() {
            let prot = if gravavel {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                libc::PROT_READ
            };
            unsafe {
                libc::mprotect(self.arena.add(pagina as usize * PAGINA).cast(), PAGINA, prot);
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (pagina, gravavel);
    }
}

impl Drop for MemoriaJit {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Dono::Mapas { vistas, arena } = &self.dono {
            for &(p, n) in vistas {
                unsafe { libc::munmap(p.cast(), n) };
            }
            unsafe { libc::munmap(arena.cast(), ARENA) };
        }
    }
}
