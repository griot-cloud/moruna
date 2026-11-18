/*
 * A C function that leans on its C library (item 5.2b): each batch is answered by a thread it
 * starts with pthread_create, which copies the batch's first column into memory from malloc,
 * counts its rows in a thread_local, and says so on stderr with fprintf; the function writes the
 * copy back. On the core this is relibc on Moruna's platform, one runtime with std (system design
 * 5B); on a host, the host's C library.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "moruna_function.h"

/* Rows this thread has copied: each thread's own. */
static _Thread_local int64_t rows_here;

struct job {
  const struct ArrowArray *in;
  struct ArrowArray out;
  struct ArrowSchema *schema;
  int64_t rows_seen;
};

struct owned {
  const void *buffers[2];
  int64_t *values;
  struct ArrowArray *child;
  struct ArrowArray **children;
};

static void release_column(struct ArrowArray *a) {
  struct owned *o = a->private_data;
  free(o->values);
  free(o);
  a->release = NULL;
}

static void release_batch(struct ArrowArray *a) {
  struct owned *o = a->private_data;
  if (o->child->release != NULL)
    o->child->release(o->child);
  free(o->child);
  free(o->children);
  free(o);
  a->release = NULL;
}

/* The batch `in`'s first column (Int64) copied into a new batch of one column. */
static void *copy(void *arg) {
  struct job *job = arg;
  const struct ArrowArray *in = job->in;
  const struct ArrowArray *column = in->children[0];
  int64_t n = in->length;
  struct owned *c = calloc(1, sizeof *c);
  c->values = malloc(n > 0 ? (size_t)n * sizeof(int64_t) : 1);
  memcpy(c->values, (const int64_t *)column->buffers[1] + column->offset + in->offset,
         (size_t)n * sizeof(int64_t));
  c->buffers[1] = c->values;
  struct owned *b = calloc(1, sizeof *b);
  b->child = calloc(1, sizeof *b->child);
  b->children = calloc(1, sizeof *b->children);
  *b->child = (struct ArrowArray){
      .length = n, .n_buffers = 2, .buffers = c->buffers, .release = release_column,
      .private_data = c};
  b->children[0] = b->child;
  job->out = (struct ArrowArray){
      .length = n, .n_buffers = 1, .buffers = b->buffers, .n_children = 1,
      .children = b->children, .release = release_batch, .private_data = b};
  rows_here += n;
  job->rows_seen = rows_here;
  fprintf(stderr, "copied %lld rows on a thread of its own\n", (long long)n);
  return NULL;
}

int main(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  struct ArrowArray in;
  struct ArrowSchema schema;
  while ((r = moruna_next(task, &in, &schema)) == 1) {
    struct job job = {.in = &in, .schema = &schema};
    pthread_t thread;
    if (pthread_create(&thread, NULL, copy, &job) != 0 || pthread_join(thread, NULL) != 0) {
      moruna_error(task, "pthread_create or pthread_join failed");
      break;
    }
    in.release(&in);
    /* A thread's thread_local starts at zero: this thread copied nothing itself. */
    if (job.rows_seen != job.out.length || rows_here != 0) {
      moruna_error(task, "a thread_local was not the thread's own");
      job.out.release(&job.out);
      schema.release(&schema);
      break;
    }
    if (moruna_emit(task, &job.out, &schema) < 0)
      break;
  }
  return moruna_finish(task);
}
