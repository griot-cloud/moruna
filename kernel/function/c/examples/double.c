/*
 * double: doubles every Int64 column of each batch and passes the other columns through, as
 * moruna-function's example does (kernel/function/rust/examples/double). A value whose double
 * does not fit an Int64 refuses the batch.
 *
 * The answer to a batch is a new struct array: each Int64 column doubled into buffers this
 * program allocates, each other column moved out of the input batch as it is (the C Data
 * Interface's "moving child arrays"), then the input batch released. The input's schema is the
 * answer's too, and moruna_emit takes both.
 */
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "moruna_function.h"

/* An array this program allocated: an Int64 column, or the answer's struct array. */
struct owned {
  const void *buffers[2];
  void *validity;
  int64_t *values;
  struct ArrowArray **children; /* the struct array's: pointers into columns */
  struct ArrowArray *columns;
  int64_t n_columns;
};

static void release_owned(struct ArrowArray *array) {
  struct owned *o = array->private_data;
  for (int64_t i = 0; i < o->n_columns; i++) {
    if (o->columns[i].release != NULL)
      o->columns[i].release(&o->columns[i]);
  }
  free(o->columns);
  free(o->children);
  free(o->validity);
  free(o->values);
  free(o);
  array->release = NULL;
}

static int valid(const struct ArrowArray *column, int64_t at) {
  const uint8_t *bits = column->buffers[0];
  return column->null_count == 0 || bits == NULL || ((bits[at >> 3] >> (at & 7)) & 1);
}

/*
 * Doubles the Int64 column `in`, rows from `at`, `rows` of them, into `out`: 0; -1 with `why`
 * written when a valid value's double is past Int64; -2 out of memory.
 */
static int double_column(const struct ArrowArray *in, int64_t at, int64_t rows,
                         const char *name, struct ArrowArray *out, char *why, size_t why_len) {
  struct owned *o = calloc(1, sizeof *o);
  if (o == NULL)
    return -2;
  o->values = malloc(rows > 0 ? (size_t)rows * sizeof(int64_t) : 1);
  int nulls = in->null_count != 0 && in->buffers[0] != NULL;
  if (nulls)
    o->validity = calloc(1, rows > 0 ? (size_t)(rows + 7) / 8 : 1);
  if (o->values == NULL || (nulls && o->validity == NULL)) {
    free(o->values);
    free(o->validity);
    free(o);
    return -2;
  }
  const int64_t *values = (const int64_t *)in->buffers[1] + in->offset + at;
  int64_t null_count = 0;
  for (int64_t i = 0; i < rows; i++) {
    if (!valid(in, in->offset + at + i)) {
      o->values[i] = 0;
      null_count++;
      continue;
    }
    if (nulls)
      ((uint8_t *)o->validity)[i >> 3] |= (uint8_t)(1u << (i & 7));
    int64_t v = values[i];
    if (v > INT64_MAX / 2 || v < INT64_MIN / 2) {
      snprintf(why, why_len, "%" PRId64 " doubled is past Int64 in %s", v, name);
      free(o->values);
      free(o->validity);
      free(o);
      return -1;
    }
    o->values[i] = v * 2;
  }
  o->buffers[0] = o->validity;
  o->buffers[1] = o->values;
  *out = (struct ArrowArray){
      .length = rows,
      .null_count = null_count,
      .offset = 0,
      .n_buffers = 2,
      .n_children = 0,
      .buffers = o->buffers,
      .children = NULL,
      .dictionary = NULL,
      .release = release_owned,
      .private_data = o,
  };
  return 0;
}

/*
 * The answer to the batch `in` of `schema`, in `out`; `in` is released whatever happens. 0; -1
 * with `why` written when the batch is refused; -2 out of memory.
 */
static int double_batch(struct ArrowArray *in, const struct ArrowSchema *schema,
                        struct ArrowArray *out, char *why, size_t why_len) {
  int64_t n = in->n_children;
  struct owned *o = calloc(1, sizeof *o);
  if (o != NULL) {
    o->columns = calloc(n > 0 ? (size_t)n : 1, sizeof *o->columns);
    o->children = calloc(n > 0 ? (size_t)n : 1, sizeof *o->children);
  }
  if (o == NULL || o->columns == NULL || o->children == NULL) {
    if (o != NULL) {
      free(o->columns);
      free(o->children);
    }
    free(o);
    in->release(in);
    return -2;
  }
  *out = (struct ArrowArray){
      .length = in->length,
      .null_count = 0,
      .offset = 0,
      .n_buffers = 1,
      .n_children = n,
      .buffers = o->buffers,
      .children = o->children,
      .dictionary = NULL,
      .release = release_owned,
      .private_data = o,
  };
  int r = 0;
  for (int64_t i = 0; i < n && r == 0; i++) {
    struct ArrowArray *column = in->children[i];
    const struct ArrowSchema *field = schema->children[i];
    o->children[i] = &o->columns[i];
    o->n_columns = i + 1;
    if (strcmp(field->format, "l") == 0) {
      r = double_column(column, in->offset, in->length, field->name, &o->columns[i], why,
                        why_len);
    } else {
      /* Moved: the answer's now, its rows the struct's from the input's offset. */
      o->columns[i] = *column;
      o->columns[i].offset += in->offset;
      o->columns[i].length -= in->offset;
      column->release = NULL;
    }
  }
  in->release(in);
  if (r != 0)
    out->release(out);
  return r;
}

int main(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, out;
  struct ArrowSchema schema;
  while (moruna_next(task, &in, &schema) == 1) {
    char why[128];
    r = double_batch(&in, &schema, &out, why, sizeof why);
    if (r != 0) {
      schema.release(&schema);
      moruna_error(task, r == -1 ? why : "out of memory");
      break;
    }
    if (moruna_emit(task, &out, &schema) < 0)
      break;
  }
  return moruna_finish(task);
}
