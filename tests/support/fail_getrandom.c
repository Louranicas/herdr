/* LD_PRELOAD entropy-failure shim for tests/server_epoch.rs.
 *
 * Provenance. The r18-server-epoch drill (heb residual #2, EVIDENCE.md
 * "Failure injection — entropy unavailable", captured 2026-08-19) ran the
 * server under "an LD_PRELOAD shim [that] fails every libc getrandom" and
 * recorded the fail-closed refusal: exit with "cannot mint the server
 * incarnation token: OS entropy unavailable (OS Error: 5)". The drill
 * retained the shim's observed behaviour but not its source; this file is
 * that shim reconstructed from the recorded behaviour (2026-08-24) and
 * committed so the fail-closed property is pinned by a test in the tree
 * rather than by an external harness that ran once.
 *
 * One deliberate divergence from the drill, forced by the committed tree.
 * The drill's binary minted the token at the top of main, so its
 * fail-everything shim hit the mint first and the recorded refusal was the
 * mint's own. The committed tree minted later on purpose (see
 * `run_server`: after logging, before the API socket), and on the pinned
 * toolchain the Rust standard library's OWN randomness (hash-map keys)
 * also traverses the libc `getrandom` symbol, earlier than the mint. A
 * shim failing every call therefore kills that std draw first — the server
 * still refuses to serve, but via a std panic pinning a foreign diagnostic
 * instead of the mint's refusal. So this shim starves exactly the class
 * the mint draws from: CRYPTO-GRADE requests, `flags == 0`, which is what
 * `getrandom::fill` issues — every such call fails with EIO ("OS Error:
 * 5", as the drill recorded), no exceptions and no counter. Non-crypto
 * draws (`GRND_INSECURE`, std's hash keys) are delegated to the real
 * function, which is what lets the client-path counter-probe (`--help`
 * exiting 0) prove the shim fired on the mint rather than breaking the
 * binary wholesale.
 *
 * A server that still starts under this shim can only have minted its
 * incarnation token from something other than crypto-grade OS entropy —
 * the stable or fabricated fallback the test exists to rule out. It does
 * NOT intercept raw `syscall(SYS_getrandom, ...)`: nothing on the mint
 * path uses it, and the test's exit-code assertion would catch the shim
 * silently missing.
 *
 * Compiled at test run time by tests/server_epoch.rs: cc -shared -fPIC.
 * Linux only, where LD_PRELOAD symbol interposition is meaningful — and
 * where the drill ran.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stddef.h>
#include <sys/types.h>

#ifndef GRND_INSECURE
#define GRND_INSECURE 0x0004
#endif

ssize_t getrandom(void *buf, size_t buflen, unsigned int flags) {
    if (flags & GRND_INSECURE) {
        static ssize_t (*real)(void *, size_t, unsigned int);
        if (!real) {
            real = (ssize_t (*)(void *, size_t, unsigned int))dlsym(RTLD_NEXT, "getrandom");
        }
        if (real) {
            return real(buf, buflen, flags);
        }
        /* No real function to delegate to: report the non-crypto draw as
         * unsupported so the caller can take its own fallback. */
        errno = ENOSYS;
        return -1;
    }
    (void)buf;
    (void)buflen;
    errno = EIO; /* "OS Error: 5", as the drill recorded */
    return -1;
}
