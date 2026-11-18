// this file is both C and CPP to check thread_local on C++ environment

#include <stdio.h>
#include <threads.h>


int global_var = 42;
thread_local int tls_var = 21;

#ifdef __cplusplus
int init_tls_val() { 
    return 7; 
}
thread_local int tls_var_dyn = init_tls_val();
#endif

#ifdef __cplusplus
extern "C" {
#endif /* __cplusplus */
void print()
{
    fprintf(stdout, "sharedlib: global_var == %d\n", global_var);
    fprintf(stdout, "sharedlib: tls_var == %d\n", tls_var);
#ifdef __cplusplus
    fprintf(stdout, "sharedlib: tls_var_dyn == %d\n", tls_var_dyn);
#endif
}
#ifdef __cplusplus
}
#endif /* __cplusplus */
