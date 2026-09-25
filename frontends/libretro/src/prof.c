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

#define ZEEBX_PROF_MAX (1u << 20)
static unsigned long zeebx_prof_pcs[ZEEBX_PROF_MAX];
static volatile unsigned zeebx_prof_n;
static int zeebx_prof_on;

static void zeebx_prof_handler(int sig, siginfo_t *si, void *ctx) {
    (void)sig; (void)si;
    ucontext_t *uc = (ucontext_t *)ctx;
    unsigned i = __atomic_fetch_add(&zeebx_prof_n, 1, __ATOMIC_RELAXED);
    if (i < ZEEBX_PROF_MAX)
        zeebx_prof_pcs[i] = (unsigned long)uc->uc_mcontext.pc;
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

void zeebx_prof_dump(void) {
    if (!zeebx_prof_on)
        return;
    unsigned n = zeebx_prof_n;
    if (n > ZEEBX_PROF_MAX)
        n = ZEEBX_PROF_MAX;
    FILE *f = fopen("/tmp/zeebx-prof.txt.tmp", "w");
    if (!f)
        return;
    for (unsigned i = 0; i < n; i++) {
        Dl_info d;
        unsigned long pc = zeebx_prof_pcs[i];
        if (dladdr((void *)pc, &d) && d.dli_fname)
            fprintf(f, "%s %lx\n", d.dli_fname, pc - (unsigned long)d.dli_fbase);
        else
            fprintf(f, "JIT %lx\n", pc);
    }
    fclose(f);
    rename("/tmp/zeebx-prof.txt.tmp", "/tmp/zeebx-prof.txt");
}
#else
void zeebx_prof_dump(void) {}
#endif
