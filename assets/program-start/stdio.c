// glibc shows a pending printf prompt before a program waits for input; wasi-libc (musl)
// doesn't, so `printf("Name: "); scanf(...)` would wait with nothing on screen. Programs
// link this with `--wrap=__stdio_read` to behave like glibc.
#include <stdio.h>

size_t __real___stdio_read(FILE *f, unsigned char *buf, size_t len);

size_t __wrap___stdio_read(FILE *f, unsigned char *buf, size_t len) {
  if (f == stdin) fflush(stdout);
  return __real___stdio_read(f, buf, len);
}
