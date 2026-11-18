/*
 * params (item 5.5): a function that reads its params as every function does, native or image.
 * moruna_params() gives the JSON text of MORUNA_PARAMS; the C library's getenv must give the same
 * (on the core its environ is built from std's environment, the process's start slot env). It
 * says its four variables in its log, as one line, and answers no input batch with one record
 * before it takes its first: one Float64 row, its params' threshold, read back. Every batch it
 * is given after that is released unanswered, and it ends at the engine's end.
 *
 * On the core it is the program c-params, which the boot's functions test gives
 * {"threshold":0.5}; on a host, tests/params.rs runs it with the variables set.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "support.h"

/* Its one record answers no batch, so each instance would write its own: it runs alone (system
   design 3E). */
MORUNA_PROCESSES(1);

/* The variable name's value, or "(none)". */
static const char *variable(const char *name) {
  const char *value = getenv(name);
  return value != NULL ? value : "(none)";
}

int main(void) {
  struct moruna_task *task;
  int r = moruna_open(&task);
  if (r < 0)
    return -r;
  const char *params = moruna_params();
  const char *seen = getenv("MORUNA_PARAMS");
  if (seen == NULL || strcmp(seen, params) != 0) {
    moruna_error(task, "getenv(\"MORUNA_PARAMS\") is not what moruna_params() gives");
    return moruna_finish(task);
  }
  char line[512];
  snprintf(line, sizeof line, "params: entry %s, role %s, params %s, args %s",
           variable("MORUNA_ENTRY"), variable("MORUNA_ROLE"), params, variable("MORUNA_ARGS"));
  moruna_log(task, line);
  static const char key[] = "\"threshold\":";
  const char *at = strstr(params, key);
  char *end = NULL;
  double threshold = at != NULL ? strtod(at + strlen(key), &end) : 0.0;
  if (at == NULL || end == at + strlen(key)) {
    moruna_error(task, "its params name no threshold");
    return moruna_finish(task);
  }
  struct ArrowArray column, batch, in;
  struct ArrowSchema field, schema, given;
  float64_column(&column, &threshold, 1);
  batch_of(&batch, &column, 1, 1);
  schema_of(&field, "g", "threshold", NULL, 0);
  schema_of(&schema, "+s", "", &field, 1);
  if (moruna_emit(task, &batch, &schema) < 0)
    return moruna_finish(task);
  while ((r = moruna_next(task, &in, &given)) == 1) {
    in.release(&in);
    given.release(&given);
  }
  return moruna_finish(task);
}
