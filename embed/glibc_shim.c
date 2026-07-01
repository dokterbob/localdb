#include <stdlib.h>

/*
 * glibc >= 2.38 redirects strtol/strtoll/strtoull calls (via header macros,
 * when _GNU_SOURCE or C23 mode is active) to internal versioned symbols
 * __isoc23_strtol/__isoc23_strtoll/__isoc23_strtoull. These add ISO C23
 * "0b"/"0B" binary-literal support but are otherwise identical for every
 * other input. ort's prebuilt ONNX Runtime archive (fastembed's
 * ort-download-binaries-rustls-tls -> ort-sys, see pykeio/ort#523) was
 * compiled against such headers, so it references these symbols even
 * though nothing in ONNX Runtime parses binary literals. Providing them
 * ourselves as plain pass-throughs satisfies the link on glibc < 2.38
 * (e.g. ubuntu-22.04, Linux Mint 21.x) without pulling in the newer glibc
 * floor. See issue #133.
 */

__attribute__((weak)) long __isoc23_strtol(const char *nptr, char **endptr, int base) {
    return strtol(nptr, endptr, base);
}

__attribute__((weak)) long long __isoc23_strtoll(const char *nptr, char **endptr, int base) {
    return strtoll(nptr, endptr, base);
}

__attribute__((weak)) unsigned long long __isoc23_strtoull(const char *nptr, char **endptr, int base) {
    return strtoull(nptr, endptr, base);
}
