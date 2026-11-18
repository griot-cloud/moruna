#include "support.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

_Thread_local struct counts counts;

void *counted_alloc(size_t n) {
  void *p = calloc(1, n > 0 ? n : 1);
  if (p == NULL)
    abort();
  counts.allocs++;
  return p;
}

void counted_free(void *p) {
  if (p == NULL)
    return;
  counts.frees++;
  free(p);
}

/* What an array this side built owns: its buffers, their pointers and its children. */
struct owned {
  const void **buffers;
  int64_t n_buffers;
  struct ArrowArray **children;
  int64_t n_children;
  struct ArrowArray *dictionary;
};

static void release_array(struct ArrowArray *a) {
  struct owned *o = a->private_data;
  for (int64_t i = 0; i < o->n_children; i++) {
    if (o->children[i]->release != NULL)
      o->children[i]->release(o->children[i]);
    counted_free(o->children[i]);
  }
  if (o->dictionary != NULL) {
    if (o->dictionary->release != NULL)
      o->dictionary->release(o->dictionary);
    counted_free(o->dictionary);
  }
  for (int64_t i = 0; i < o->n_buffers; i++)
    counted_free((void *)o->buffers[i]);
  counted_free(o->buffers);
  counted_free(o->children);
  counted_free(o);
  counts.arrays_released++;
  a->release = NULL;
}

/* An array of length n with n_buffers buffers, each a copy of the bytes given. */
static struct owned *array(struct ArrowArray *out, int64_t n, int64_t n_buffers,
                           const void *const *bytes, const size_t *lens) {
  struct owned *o = counted_alloc(sizeof *o);
  o->buffers = counted_alloc((size_t)n_buffers * sizeof *o->buffers);
  o->n_buffers = n_buffers;
  for (int64_t i = 0; i < n_buffers; i++) {
    if (bytes[i] == NULL)
      continue;
    void *b = counted_alloc(lens[i]);
    memcpy(b, bytes[i], lens[i]);
    o->buffers[i] = b;
  }
  *out = (struct ArrowArray){
      .length = n,
      .null_count = 0,
      .offset = 0,
      .n_buffers = n_buffers,
      .n_children = 0,
      .buffers = o->buffers,
      .children = NULL,
      .dictionary = NULL,
      .release = release_array,
      .private_data = o,
  };
  return o;
}

void int64_column(struct ArrowArray *out, const int64_t *values, const uint8_t *valid, int64_t n) {
  uint8_t bits[64] = {0};
  int64_t nulls = 0;
  for (int64_t i = 0; valid != NULL && i < n; i++) {
    if (valid[i])
      bits[i >> 3] |= (uint8_t)(1u << (i & 7));
    else
      nulls++;
  }
  const void *bytes[2] = {valid != NULL ? bits : NULL, values};
  size_t lens[2] = {sizeof bits, (size_t)n * sizeof *values};
  array(out, n, 2, bytes, lens);
  out->null_count = nulls;
}

void utf8_column(struct ArrowArray *out, const int32_t *offsets, const char *data, int64_t n) {
  const void *bytes[3] = {NULL, offsets, data};
  size_t lens[3] = {0, (size_t)(n + 1) * sizeof *offsets, strlen(data) + 1};
  array(out, n, 3, bytes, lens);
}

void dictionary_column(struct ArrowArray *out, const int32_t *keys, int64_t n,
                       const int32_t *offsets, const char *data, int64_t m) {
  const void *bytes[2] = {NULL, keys};
  size_t lens[2] = {0, (size_t)n * sizeof *keys};
  struct owned *o = array(out, n, 2, bytes, lens);
  o->dictionary = counted_alloc(sizeof *o->dictionary);
  utf8_column(o->dictionary, offsets, data, m);
  out->dictionary = o->dictionary;
}

void batch_of(struct ArrowArray *out, struct ArrowArray *columns, int64_t n, int64_t rows) {
  const void *bytes[1] = {NULL};
  size_t lens[1] = {0};
  struct owned *o = array(out, rows, 1, bytes, lens);
  o->children = counted_alloc((size_t)n * sizeof *o->children);
  o->n_children = n;
  for (int64_t i = 0; i < n; i++) {
    o->children[i] = counted_alloc(sizeof **o->children);
    *o->children[i] = columns[i];
    columns[i].release = NULL;
  }
  out->n_children = n;
  out->children = o->children;
}

/* What a schema this side built owns: its children and dictionary. */
struct owned_schema {
  struct ArrowSchema **children;
  int64_t n;
  struct ArrowSchema *dictionary;
};

static void release_schema(struct ArrowSchema *s) {
  struct owned_schema *o = s->private_data;
  for (int64_t i = 0; i < o->n; i++) {
    if (o->children[i]->release != NULL)
      o->children[i]->release(o->children[i]);
    counted_free(o->children[i]);
  }
  if (o->dictionary != NULL) {
    if (o->dictionary->release != NULL)
      o->dictionary->release(o->dictionary);
    counted_free(o->dictionary);
  }
  counted_free(o->children);
  counted_free(o);
  counts.schemas_released++;
  s->release = NULL;
}

void schema_of(struct ArrowSchema *out, const char *format, const char *name,
               struct ArrowSchema *children, int64_t n) {
  struct owned_schema *o = counted_alloc(sizeof *o);
  o->children = counted_alloc((size_t)n * sizeof *o->children);
  o->n = n;
  for (int64_t i = 0; i < n; i++) {
    o->children[i] = counted_alloc(sizeof **o->children);
    *o->children[i] = children[i];
    children[i].release = NULL;
  }
  *out = (struct ArrowSchema){
      .format = format,
      .name = name,
      .metadata = NULL,
      .flags = 0, /* not nullable, as arrow-rs makes a column with no nulls */
      .n_children = n,
      .children = o->children,
      .dictionary = NULL,
      .release = release_schema,
      .private_data = o,
  };
}

void schema_with_dictionary(struct ArrowSchema *out, const char *format, const char *name,
                            struct ArrowSchema *dictionary) {
  schema_of(out, format, name, NULL, 0);
  struct owned_schema *o = out->private_data;
  o->dictionary = counted_alloc(sizeof *o->dictionary);
  *o->dictionary = *dictionary;
  dictionary->release = NULL;
  out->dictionary = o->dictionary;
}

size_t moruna_test_array_size(void) { return sizeof(struct ArrowArray); }

size_t moruna_test_schema_size(void) { return sizeof(struct ArrowSchema); }

void moruna_test_codes(int *codes) {
  const int named[6] = {MORUNA_E_DONE,  MORUNA_E_NO_PLANE, MORUNA_E_NO_RINGS,
                        MORUNA_E_RINGS, MORUNA_E_REFUSED,  MORUNA_E_WINDOW_SMALL};
  memcpy(codes, named, sizeof named);
}

void check_failed(struct moruna_task *task, const char *what, int line) {
  char text[256];
  snprintf(text, sizeof text, "CHECK failed at scenarios.c:%d: %s", line, what);
  moruna_log(task, text);
}
