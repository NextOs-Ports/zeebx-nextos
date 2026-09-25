/* Amostrador de CPU para medir o core no aparelho, sem perf.
 *
 * Ligado só com ZEEBX_PROF=1: um setitimer(ITIMER_PROF) de 1 ms entrega SIGPROF à thread que
 * estiver gastando CPU, e o tratador guarda o PC num vetor. A cada chamada de
 * zeebx_prof_dump() (feita pelo relatório de ZEEBX_MEDE) o vetor é escrito em
 * /tmp/zeebx-prof.txt como "módulo deslocamento", para simbolizar no PC com addr2line/nm.
 * PC fora de qualquer módulo é código gerado pelo JIT. */
#if defined(__linux__) && defined(__aarch64__)
#define _GNU_SOURCE
#include <dlfcn.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <ucontext.h>

#define ZEEBX_PROF_MAX (1u << 18)
#define ZEEBX_PROF_PROF 12
/* Cada amostra: o PC e até ZEEBX_PROF_PROF-1 endereços de retorno, subindo pelos x29. */
static unsigned long zeebx_prof_pcs[ZEEBX_PROF_MAX][ZEEBX_PROF_PROF];
static volatile unsigned zeebx_prof_n;
static int zeebx_prof_on;

static void zeebx_prof_handler(int sig, siginfo_t *si, void *ctx) {
    (void)sig; (void)si;
    ucontext_t *uc = (ucontext_t *)ctx;
    unsigned i = __atomic_fetch_add(&zeebx_prof_n, 1, __ATOMIC_RELAXED);
    if (i >= ZEEBX_PROF_MAX)
        return;
    unsigned long *s = zeebx_prof_pcs[i];
    s[0] = (unsigned long)uc->uc_mcontext.pc;
    /* O x30 (lr) cobre a função folha que ainda não empilhou o quadro. */
    s[1] = (unsigned long)uc->uc_mcontext.regs[30];
    unsigned long fp = (unsigned long)uc->uc_mcontext.regs[29];
    unsigned long sp = (unsigned long)uc->uc_mcontext.sp;
    for (int k = 2; k < ZEEBX_PROF_PROF; k++) {
        /* Só segue quadros na pilha desta thread, crescendo para cima, alinhados. */
        if (fp < sp || fp - sp > (8u << 20) || (fp & 7)) {
            s[k] = 0;
            continue;
        }
        unsigned long *quadro = (unsigned long *)fp;
        s[k] = quadro[1];
        unsigned long proximo = quadro[0];
        if (proximo <= fp) {
            for (int r = k + 1; r < ZEEBX_PROF_PROF; r++) s[r] = 0;
            break;
        }
        fp = proximo;
    }
}

__attribute__((constructor)) static void zeebx_prof_init(void) {
    const char *e = getenv("ZEEBX_PROF");
    if (!e || !*e)
        return;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = zeebx_prof_handler;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGPROF, &sa, NULL);
    struct itimerval it = {{0, 1000}, {0, 1000}};
    setitimer(ITIMER_PROF, &it, NULL);
    zeebx_prof_on = 1;
}

/* ZEEBX_PROF_DESDE=R: o arquivo só leva as amostras depois do R-ésimo relatório (R*120 quadros).
 * Sem isto a carga do jogo (JIT compilando, blake3, descompressão) entra no perfil do quadro: no
 * NFS a 1800 quadros ela era 20% das amostras. */
static unsigned zeebx_prof_marca, zeebx_prof_relatorios;

void zeebx_prof_dump(void) {
    if (!zeebx_prof_on)
        return;
    unsigned n = zeebx_prof_n;
    if (n > ZEEBX_PROF_MAX)
        n = ZEEBX_PROF_MAX;
    const char *desde = getenv("ZEEBX_PROF_DESDE");
    if (desde && ++zeebx_prof_relatorios == (unsigned)atoi(desde))
        zeebx_prof_marca = n;
    FILE *f = fopen("/tmp/zeebx-prof.txt.tmp", "w");
    if (!f)
        return;
    for (unsigned i = zeebx_prof_marca; i < n; i++) {
        for (int k = 0; k < ZEEBX_PROF_PROF; k++) {
            Dl_info d;
            unsigned long pc = zeebx_prof_pcs[i][k];
            if (k > 0 && pc == 0)
                break;
            if (k > 0)
                fputc(';', f);
            if (dladdr((void *)pc, &d) && d.dli_fname)
                fprintf(f, "%s %lx", d.dli_fname, pc - (unsigned long)d.dli_fbase);
            else
                fprintf(f, "JIT %lx", pc);
        }
        fputc('\n', f);
    }
    fclose(f);
    rename("/tmp/zeebx-prof.txt.tmp", "/tmp/zeebx-prof.txt");
}
#else
void zeebx_prof_dump(void) {}
#endif
