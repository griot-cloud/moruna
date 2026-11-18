/*
 * What the tests' C functions share: arrays and schemas they build and own, every allocation
 * and release counted on the thread that makes it, and a check that ends a test function.
 */
#ifndef MORUNA_TEST_SUPPORT_H
#define MORUNA_TEST_SUPPORT_H

#include <stddef.h>
#include <stdint.h>

#include "moruna_function.h"

/* This thread's count of what the C side allocated and freed, and of its releases called. */
struct counts {
  long allocs;
  long frees;
  long arrays_released;
  long schemas_released;
};
extern _Thread_local struct counts counts;

void *counted_alloc(size_t n);
void counted_free(void *p);

/* A column of n Int64 values, null where valid is not NULL and valid[i] is 0. */
void int64_column(struct ArrowArray *out, const int64_t *values, const uint8_t *valid, int64_t n);
/* A column of n Float64 values, none null. */
void float64_column(struct ArrowArray *out, const double *values, int64_t n);
/* A Utf8 column of n strings, its offsets as given (n + 1 of them). */
void utf8_column(struct ArrowArray *out, const int32_t *offsets, const char *data, int64_t n);
/* An Int32 dictionary column of n keys into a Utf8 dictionary of m strings. */
void dictionary_column(struct ArrowArray *out, const int32_t *keys, int64_t n,
                       const int32_t *offsets, const char *data, int64_t m);
/* A struct array of n columns, each moved into it, of rows rows. */
void batch_of(struct ArrowArray *out, struct ArrowArray *columns, int64_t n, int64_t rows);
/* A schema of the given format with n children, each moved into it. */
void schema_of(struct ArrowSchema *out, const char *format, const char *name,
               struct ArrowSchema *children, int64_t n);
/* As schema_of, with a dictionary moved into it. */
void schema_with_dictionary(struct ArrowSchema *out, const char *format, const char *name,
                            struct ArrowSchema *dictionary);

/* The sizes of the two structures as this compiler lays them out. */
size_t moruna_test_array_size(void);
size_t moruna_test_schema_size(void);
/* The header's exit codes, from MORUNA_E_DONE to MORUNA_E_WINDOW_SMALL, into codes[6]. */
void moruna_test_codes(int *codes);

void check_failed(struct moruna_task *task, const char *what, int line);

/* Ends the test function with 99 and a line in the log when cond is false. */
#define CHECK(task, cond)                    \
  do {                                       \
    if (!(cond)) {                           \
      check_failed((task), #cond, __LINE__); \
      return 99;                             \
    }                                        \
  } while (0)

#endif /* MORUNA_TEST_SUPPORT_H */
