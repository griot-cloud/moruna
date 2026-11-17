// The header compiled as C++ (5B: C, C++ and Zig link the library): every call's type as C++
// sees it, and the two structures' sizes, which the tests compare with arrow-rs's.
#include <cstddef>

#include "moruna_function.h"

extern "C" std::size_t moruna_test_cpp_sizes(void) {
  // Each call taken at its type, so a declaration C++ reads otherwise fails to compile here.
  int (*open_call)(moruna_task **) = moruna_open;
  int (*next_call)(moruna_task *, ArrowArray *, ArrowSchema *) = moruna_next;
  int (*emit_call)(moruna_task *, ArrowArray *, ArrowSchema *) = moruna_emit;
  void (*log_call)(moruna_task *, int, const char *) = moruna_log;
  void (*error_call)(moruna_task *, const char *) = moruna_error;
  int (*finish_call)(moruna_task *) = moruna_finish;
  (void)open_call;
  (void)next_call;
  (void)emit_call;
  (void)log_call;
  (void)error_call;
  (void)finish_call;
  return sizeof(ArrowArray) * 1000 + sizeof(ArrowSchema);
}
