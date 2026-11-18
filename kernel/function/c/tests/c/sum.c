/*
 * The aggregate test as a program of its own, for the core (item 5.2b): moruna_test_sum, a batch
 * written before the first and the sum after the end, both answering no input. On a host the
 * tests run the same function on the data plane in memory (tests/header.rs).
 *
 * Its total is a running one, the whole input's only if one instance sees every batch: it
 * declares one (system design 3E), and a job runs it alone whatever its CPUs.
 */
#include "moruna_function.h"

MORUNA_PROCESSES(1);

int moruna_test_sum(void);

int main(void) { return moruna_test_sum(); }
