/*
 * The tests' C functions: each is a function's main, run by tests/header.rs as a task on the
 * data plane in memory (moruna_function::host::Plane), which checks what it answered, released,
 * logged and said. A CHECK that fails ends the function with 99 and a line in the log.
 */
#include <string.h>

#include "support.h"

/* A batch of one Int64 column "a", in's first column plus add, built and owned here. */
static void plus(const struct ArrowArray *in, int64_t add, struct ArrowArray *batch,
                 struct ArrowSchema *schema) {
  const struct ArrowArray *a = in->children[0];
  const int64_t *from = (const int64_t *)a->buffers[1] + a->offset + in->offset;
  int64_t values[64];
  int64_t n = in->length < 64 ? in->length : 64;
  for (int64_t i = 0; i < n; i++)
    values[i] = from[i] + add;
  struct ArrowArray column;
  int64_column(&column, values, NULL, n);
  batch_of(batch, &column, 1, n);
  struct ArrowSchema field;
  schema_of(&field, "l", "a", NULL, 0);
  schema_of(schema, "+s", "", &field, 1);
}

/* A batch of one Int64 column "a" of the n values given. */
static void ints(const int64_t *values, int64_t n, struct ArrowArray *batch,
                 struct ArrowSchema *schema) {
  struct ArrowArray column;
  int64_column(&column, values, NULL, n);
  batch_of(batch, &column, 1, n);
  struct ArrowSchema field;
  schema_of(&field, "l", "a", NULL, 0);
  schema_of(schema, "+s", "", &field, 1);
}

/* Every batch written back as it came, moruna_emit taking the producer's own structures. */
static int echo(struct moruna_task *task) {
  struct ArrowArray in;
  struct ArrowSchema schema;
  int r;
  while ((r = moruna_next(task, &in, &schema)) == 1) {
    r = moruna_emit(task, &in, &schema);
    CHECK(task, in.release == NULL && schema.release == NULL);
    if (r < 0)
      break;
  }
  CHECK(task, in.release == NULL && schema.release == NULL);
  return moruna_finish(task);
}

int moruna_test_echo(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  return echo(task);
}

/* Each answer built here and written after the input is released: it still answers it. */
int moruna_test_copy(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, out;
  struct ArrowSchema schema, out_schema;
  while ((r = moruna_next(task, &in, &schema)) == 1) {
    plus(&in, 1000, &out, &out_schema);
    in.release(&in);
    schema.release(&schema);
    CHECK(task, in.release == NULL && schema.release == NULL);
    long arrays = counts.arrays_released, schemas = counts.schemas_released;
    CHECK(task, moruna_emit(task, &out, &out_schema) == 0);
    CHECK(task, out.release == NULL && out_schema.release == NULL);
    /* Released once each before the call returned: the struct array and its column, the
       schema and its field. */
    CHECK(task, counts.arrays_released == arrays + 2);
    CHECK(task, counts.schemas_released == schemas + 2);
  }
  CHECK(task, r == 0);
  r = moruna_finish(task);
  CHECK(NULL, counts.allocs == counts.frees);
  return r;
}

/* Two batches held: answers go to the older, and both are given back once both are released. */
int moruna_test_hold_two(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray a, b, out;
  struct ArrowSchema sa, sb, so;
  CHECK(task, moruna_next(task, &a, &sa) == 1);
  CHECK(task, moruna_next(task, &b, &sb) == 1);
  plus(&b, 100, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == 0);
  /* The newer released first: nothing is given back yet, and a is still the one answered. */
  b.release(&b);
  sb.release(&sb);
  plus(&a, 100, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == 0);
  a.release(&a);
  sa.release(&sa);
  CHECK(task, moruna_next(task, &a, &sa) == 0);
  CHECK(task, a.release == NULL && sa.release == NULL);
  r = moruna_finish(task);
  CHECK(NULL, counts.allocs == counts.frees);
  return r;
}

/* which 0: the batch still held at the finish; 1: the finish before the engine's end. */
int moruna_test_keep(int which) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, more;
  struct ArrowSchema schema, more_schema;
  CHECK(task, moruna_next(task, &in, &schema) == 1);
  schema.release(&schema);
  if (which == 0)
    CHECK(task, moruna_next(task, &more, &more_schema) == 0);
  else
    in.release(&in);
  r = moruna_finish(task);
  if (which == 0) {
    /* Still the function's after the finish, and still readable. */
    CHECK(NULL, ((const int64_t *)in.children[0]->buffers[1])[0] == 0);
    in.release(&in);
  }
  return r;
}

/* The function's refusal ends the task; every later call says so. */
int moruna_test_refuse(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, more, out;
  struct ArrowSchema schema, more_schema, so;
  CHECK(task, moruna_next(task, &in, &schema) == 1);
  moruna_error(task, "no column named x");
  CHECK(task, moruna_next(task, &more, &more_schema) == -MORUNA_E_REFUSED);
  CHECK(task, more.release == NULL && more_schema.release == NULL);
  plus(&in, 1, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == -MORUNA_E_REFUSED);
  CHECK(task, out.release == NULL && so.release == NULL);
  moruna_error(task, NULL);
  in.release(&in);
  schema.release(&schema);
  r = moruna_finish(task);
  CHECK(NULL, counts.allocs == counts.frees);
  return r;
}

/* A batch moruna_emit cannot write, case by case: refused, and released once all the same. */
int moruna_test_emit_bad(int which) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, out, columns[2];
  struct ArrowSchema schema, so, field;
  memset(&out, 0, sizeof out);
  memset(&so, 0, sizeof so);
  CHECK(task, moruna_next(task, &in, &schema) == 1);
  static const int64_t values[2] = {1, 2};
  static const int32_t offsets[3] = {0, 3, 1};
  static const int32_t keys[2] = {0, 1};
  static const int32_t words[3] = {0, 1, 2};
  switch (which) {
  case 0: /* the batch NULL */
  case 1: /* the schema NULL */
    ints(values, 2, &out, &so);
    break;
  case 2: /* the batch released */
    ints(values, 2, &out, &so);
    out.release(&out);
    break;
  case 3: /* not a struct */
    ints(values, 2, &out, &field);
    schema_of(&so, "l", "a", NULL, 0);
    field.release(&field);
    break;
  case 4: /* two columns, one field */
    int64_column(&columns[0], values, NULL, 2);
    int64_column(&columns[1], values, NULL, 2);
    batch_of(&out, columns, 2, 2);
    schema_of(&field, "l", "a", NULL, 0);
    schema_of(&so, "+s", "", &field, 1);
    break;
  case 5: { /* a dictionary column */
    dictionary_column(&columns[0], keys, 2, words, "ab", 2);
    batch_of(&out, columns, 1, 2);
    struct ArrowSchema dictionary;
    schema_of(&dictionary, "u", "", NULL, 0);
    schema_with_dictionary(&field, "i", "d", &dictionary);
    schema_of(&so, "+s", "", &field, 1);
    break;
  }
  case 6: { /* nulls in the struct array */
    ints(values, 2, &out, &so);
    uint8_t *bits = counted_alloc(1);
    bits[0] = 1;
    ((const void **)out.buffers)[0] = bits;
    out.null_count = 1;
    break;
  }
  case 7: /* a column with a buffer missing */
    ints(values, 2, &out, &so);
    out.children[0]->n_buffers = 1;
    break;
  case 8: /* Utf8 offsets that go back */
    utf8_column(&columns[0], offsets, "abc", 2);
    batch_of(&out, columns, 1, 2);
    schema_of(&field, "u", "s", NULL, 0);
    schema_of(&so, "+s", "", &field, 1);
    break;
  case 9: /* a field with no format */
    ints(values, 2, &out, &so);
    so.children[0]->format = NULL;
    break;
  case 10: /* a schema's children NULL */
    ints(values, 2, &out, &so);
    so.children = NULL;
    break;
  default:
    return 98;
  }
  r = moruna_emit(task, which == 0 ? NULL : &out, which == 1 ? NULL : &so);
  CHECK(task, r == -MORUNA_E_REFUSED);
  /* moruna_emit took what it was given and released it; what it was not given is released
     here, so every allocation of this side is freed once. */
  if (which == 0)
    out.release(&out);
  if (which == 1)
    so.release(&so);
  CHECK(task, out.release == NULL && so.release == NULL);
  CHECK(task, counts.allocs == counts.frees);
  in.release(&in);
  schema.release(&schema);
  return moruna_finish(task);
}

/* An aggregate: a batch written before the first is taken, then column "a" summed over every
   batch, each released as soon as it is read, and the sum written after the engine's end. Both
   answer no input. */
int moruna_test_sum(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, out;
  struct ArrowSchema schema, so;
  static const int64_t before[1] = {-1};
  ints(before, 1, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == 0);
  int64_t sum = 0;
  while ((r = moruna_next(task, &in, &schema)) == 1) {
    const struct ArrowArray *a = in.children[0];
    const int64_t *values = (const int64_t *)a->buffers[1] + a->offset + in.offset;
    for (int64_t i = 0; i < in.length; i++)
      sum += values[i];
    in.release(&in);
    schema.release(&schema);
  }
  CHECK(task, r == 0);
  ints(&sum, 1, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == 0);
  r = moruna_finish(task);
  CHECK(NULL, counts.allocs == counts.frees);
  return r;
}

/* Lines into the log, as given. */
int moruna_test_log(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  moruna_log(task, "plain");
  moruna_log(task, "two\nlines");
  moruna_log(task, "");
  moruna_log(task, NULL);
  moruna_log(NULL, "a line with no task");
  return echo(task);
}

/* Calls with no task, and a second open, fail without harm; what moruna_emit is given is
   released all the same. */
int moruna_test_null_task(void) {
  struct moruna_task *task, *second = (struct moruna_task *)&task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  CHECK(task, moruna_open(&second) == -MORUNA_E_NO_PLANE && second == NULL);
  CHECK(task, moruna_open(NULL) == -MORUNA_E_REFUSED);
  struct ArrowArray in, out;
  struct ArrowSchema schema, so;
  CHECK(task, moruna_next(NULL, &in, &schema) == -MORUNA_E_NO_PLANE);
  CHECK(task, in.release == NULL && schema.release == NULL);
  static const int64_t values[1] = {7};
  ints(values, 1, &out, &so);
  CHECK(task, moruna_emit(NULL, &out, &so) == -MORUNA_E_NO_PLANE);
  CHECK(task, out.release == NULL && so.release == NULL);
  CHECK(task, counts.allocs == counts.frees);
  moruna_error(NULL, "nobody to end");
  CHECK(task, moruna_finish(NULL) == MORUNA_E_NO_PLANE);
  return echo(task);
}

/* A NULL out: the task fails. */
int moruna_test_null_out(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowSchema schema;
  CHECK(task, moruna_next(task, NULL, &schema) == -MORUNA_E_REFUSED);
  CHECK(task, schema.release == NULL);
  return moruna_finish(task);
}

/* A column moved out of the batch keeps the batch held after the batch's own release: which 0
   releases it before the finish, 1 after, the column still readable then. */
int moruna_test_child_moved(int which) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in, out;
  struct ArrowSchema schema, so;
  CHECK(task, moruna_next(task, &in, &schema) == 1);
  CHECK(task, in.n_children == 2);
  struct ArrowArray kept = *in.children[1];
  in.children[1]->release = NULL;
  in.release(&in);
  schema.release(&schema);
  static const int64_t values[1] = {42};
  ints(values, 1, &out, &so);
  CHECK(task, moruna_emit(task, &out, &so) == 0);
  if (which == 0)
    kept.release(&kept);
  CHECK(task, moruna_next(task, &in, &schema) == 0);
  r = moruna_finish(task);
  if (which == 1) {
    const int32_t *offsets = (const int32_t *)kept.buffers[1] + kept.offset;
    const char *data = kept.buffers[2];
    CHECK(NULL, kept.length == 3 && offsets[3] - offsets[0] == 6);
    CHECK(NULL, memcmp(data + offsets[0], "xyyzzz", 6) == 0);
    kept.release(&kept);
  }
  CHECK(NULL, counts.allocs == counts.frees);
  return r;
}
