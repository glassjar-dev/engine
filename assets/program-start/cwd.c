// Start the program in the directory the host asked for. wasi-libc keeps its own working
// directory, which starts at "/" whatever the runtime says, so this runs before main.
#include <stdlib.h>
#include <unistd.h>

__attribute__((constructor)) static void glassjar_cwd(void) {
  const char *dir = getenv("GLASSJAR_CWD");
  if (dir) chdir(dir);
}
