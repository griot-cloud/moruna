/*
 * The aggregate test as a program of its own, for the core (item 5.2b): moruna_test_sum, a batch
 * written before the first and the sum after the end, both answering no input. On a host the
 * tests run the same function on the data plane in memory (tests/header.rs).
 */
int moruna_test_sum(void);

int main(void) { return moruna_test_sum(); }
