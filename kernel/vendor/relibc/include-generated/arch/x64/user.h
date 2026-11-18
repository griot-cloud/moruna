#ifndef _X64_USER_H
#define _X64_USER_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

struct user_fpregs_struct {
  uint16_t cwd;
  uint16_t swd;
  uint16_t ftw;
  uint16_t fop;
  uint64_t rip;
  uint64_t rdp;
  uint32_t mxcsr;
  uint32_t mxcr_mask;
  uint32_t st_space[32];
  uint32_t xmm_space[64];
  uint32_t padding[24];
};

struct user_regs_struct {
  unsigned long r15;
  unsigned long r14;
  unsigned long r13;
  unsigned long r12;
  unsigned long rbp;
  unsigned long rbx;
  unsigned long r11;
  unsigned long r10;
  unsigned long r9;
  unsigned long r8;
  unsigned long rax;
  unsigned long rcx;
  unsigned long rdx;
  unsigned long rsi;
  unsigned long rdi;
  unsigned long orig_rax;
  unsigned long rip;
  unsigned long cs;
  unsigned long eflags;
  unsigned long rsp;
  unsigned long ss;
  unsigned long fs_base;
  unsigned long gs_base;
  unsigned long ds;
  unsigned long es;
  unsigned long fs;
  unsigned long gs;
};

struct user {
  struct user_regs_struct regs;
  int u_fpvalid;
  struct user_fpregs_struct i387;
  unsigned long u_tsize;
  unsigned long u_dsize;
  unsigned long u_ssize;
  unsigned long start_code;
  unsigned long start_stack;
  long signal;
  int reserved;
  struct user_regs_struct *u_ar0;
  struct user_fpregs_struct *u_fpstate;
  unsigned long magic;
  char u_comm[32];
  unsigned long u_debugreg[8];
};

typedef unsigned long (*elf_gregset_t)[27];

typedef unsigned long elf_greg_t;

typedef struct user_fpregs_struct elf_fpregset_t;

#endif  /* _X64_USER_H */
