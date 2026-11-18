/*
 * moruna_function.h: a native Moruna function in C, and in every language that links C
 * (architecture/moruna-os-system.md, 5B).
 *
 * A function is a program the data plane feeds Arrow batches. It links libmoruna_function.a,
 * which is moruna-function (kernel/function/rust, 5A) under these six calls and moruna_params
 * (its params, item 5.5), into one static program for the core. Batches cross by the Arrow C
 * Data Interface
 * (https://arrow.apache.org/docs/format/CDataInterface.html): a batch is a struct array whose
 * children are its columns, and its schema a struct schema whose children are its fields.
 *
 *     int main(void) {
 *         struct moruna_task *task;
 *         int r = moruna_open(&task);
 *         if (r < 0)
 *             return -r;
 *         struct ArrowArray in, out;
 *         struct ArrowSchema schema;
 *         while (moruna_next(task, &in, &schema) == 1) {
 *             ... build out from in, then in.release(&in) ...
 *             if (moruna_emit(task, &out, &schema) < 0)
 *                 break;
 *         }
 *         return moruna_finish(task);
 *     }
 *
 * examples/double.c is a whole function.
 *
 * A task ends with an exit code, which moruna_finish says on the task's rings and returns for
 * main to return; a call that fails returns it negated (-MORUNA_E_REFUSED, say). They are
 * moruna-function's codes (moruna_function::code), named below.
 *
 * The first failure ends the task: its line goes to the task's log, every later moruna_next and
 * moruna_emit returns its code, no batch is given back to the engine after it, and
 * moruna_finish returns it. A task's calls are made one at a time, from any thread.
 */
#ifndef MORUNA_FUNCTION_H
#define MORUNA_FUNCTION_H

/* Every batch answered and given back, and the engine posts no more. */
#define MORUNA_E_DONE 0
/* The program was not started on the data plane. */
#define MORUNA_E_NO_PLANE 2
/* The task's plane holds no rings. */
#define MORUNA_E_NO_RINGS 3
/* The rings broke. */
#define MORUNA_E_RINGS 5
/* The function refused a batch (moruna_error), a batch could not be read or written, or the
   function finished holding a batch or before the engine's end. */
#define MORUNA_E_REFUSED 7
/* One row of an output batch takes more than a window. */
#define MORUNA_E_WINDOW_SMALL 8

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * The Arrow C Data Interface's two structures, as its specification defines them, under its
 * guard, so this header and Arrow's own (arrow/c/abi.h, nanoarrow.h) can both be included.
 */
#ifndef ARROW_C_DATA_INTERFACE
#define ARROW_C_DATA_INTERFACE

#define ARROW_FLAG_DICTIONARY_ORDERED 1
#define ARROW_FLAG_NULLABLE 2
#define ARROW_FLAG_MAP_KEYS_SORTED 4

struct ArrowSchema {
  /* Array type description */
  const char* format;
  const char* name;
  const char* metadata;
  int64_t flags;
  int64_t n_children;
  struct ArrowSchema** children;
  struct ArrowSchema* dictionary;

  /* Release callback */
  void (*release)(struct ArrowSchema*);
  /* Opaque producer-specific data */
  void* private_data;
};

struct ArrowArray {
  /* Array data description */
  int64_t length;
  int64_t null_count;
  int64_t offset;
  int64_t n_buffers;
  int64_t n_children;
  const void** buffers;
  struct ArrowArray** children;
  struct ArrowArray* dictionary;

  /* Release callback */
  void (*release)(struct ArrowArray*);
  /* Opaque producer-specific data */
  void* private_data;
};

#endif /* ARROW_C_DATA_INTERFACE */

/* A task: the function's half of its rings. Only a pointer to one is ever held. */
struct moruna_task;

/*
 * Attaches the program to its task's rings, once: 0, with *task set; or the exit code negated,
 * with *task NULL and a line in the log saying why (-MORUNA_E_NO_PLANE, as in a second call;
 * -MORUNA_E_NO_RINGS).
 */
int  moruna_open(struct moruna_task **task);

/*
 * The next batch: 1, with *out and *schema filled, both the function's to release; 0 once the
 * engine posts no more; or the exit code negated once the task has failed (-MORUNA_E_RINGS,
 * -MORUNA_E_REFUSED when the input record failed the engine's check, or an earlier failure's).
 * *out and *schema are left released unless 1 is returned.
 *
 * The batch is read in place: its buffers are the engine's memory, read-only, and valid until
 * the batch's release is called (or, if a child array was moved out of it, until the last of
 * them is released). The batch is given back to the engine at the task's next moruna_next or
 * moruna_finish after that, and in the order batches were taken: one released before an older
 * batch is given back with it. A function may hold several batches; the engine posts more as
 * they are given back.
 */
int  moruna_next(struct moruna_task *task, struct ArrowArray *out, struct ArrowSchema *schema); /* 1 batch, 0 end, <0 error */

/*
 * Writes a batch as the answer to the oldest batch the task holds (taken by moruna_next and not
 * yet given back), or to none when it holds none (before its first batch, or after the engine's
 * end, as an aggregate writes its total): copied once into a window, as one record, or as
 * several in order when its rows take more than a window. 0, or the exit code negated with the
 * task failed: -MORUNA_E_REFUSED the batch cannot be carried (not a struct array without
 * nulls; a malformed array; a dictionary column), -MORUNA_E_WINDOW_SMALL one row takes more
 * than a window, -MORUNA_E_RINGS the rings broke.
 *
 * It takes ownership of *batch and *schema whatever it returns: both are released by the time
 * it returns, and their release members are set to NULL.
 */
int  moruna_emit(struct moruna_task *task, struct ArrowArray *batch, struct ArrowSchema *schema); /* takes ownership */

/*
 * Writes line to the task's log, as given, a record a line; a line with newlines is several.
 */
void moruna_log(struct moruna_task *task, const char *line);

/*
 * Ends the task with message, the function's refusal of a batch: the line "<task>: <message>"
 * goes to the log, and the task ends with MORUNA_E_REFUSED, or with the code of an earlier
 * failure.
 */
void moruna_error(struct moruna_task *task, const char *message);

/*
 * Ends the task: says its exit code on the rings, where the engine reads it, frees the task and
 * returns the code, for main to return. MORUNA_E_DONE needs every batch given back and the
 * engine's end seen by moruna_next. A batch the function still holds stays valid until it is
 * released.
 */
int  moruna_finish(struct moruna_task *task);

/*
 * The function's params, the JSON object its task was given, as text: MORUNA_PARAMS, which every
 * function's process is given, native or image, as {"threshold":0.5}; "{}" when it was given
 * none, or when the program runs outside a job. Needs no task. The text is read once and lives as
 * long as the program: never free it. getenv("MORUNA_PARAMS") reads the same, and
 * getenv("MORUNA_ARGS") the run's arguments.
 */
const char *moruna_params(void);

#ifdef __cplusplus
}
#endif

/*
 * A job runs a function one instance per CPU of the job, each a process of its own. A function
 * that keeps state across batches (a running total) declares the most instances it can run, 1 to
 * 64, once at file scope:
 *
 *     MORUNA_PROCESSES(1);
 *
 * A ceiling, never a count. It is the program's ELF note (.note.moruna, owner "Moruna", type 1,
 * "MORUNA_PROCESSES=N"), which the engine reads before it starts the function. Off ELF (a host's
 * Mach-O build) it declares nothing.
 */
#if defined(__ELF__)
#define MORUNA_PROCESSES(n)                                                                      \
  __asm__(".pushsection .note.moruna,\"a\",@note\n.balign 4\n.long 7\n.long 2f - 1f\n.long 1\n"  \
          ".asciz \"Moruna\"\n.balign 4\n1: .ascii \"MORUNA_PROCESSES=" #n "\"\n2: .balign 4\n" \
          ".popsection")
#else
#define MORUNA_PROCESSES(n) extern int moruna_processes_declared_
#endif

#endif /* MORUNA_FUNCTION_H */
