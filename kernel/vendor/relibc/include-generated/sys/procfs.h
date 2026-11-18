#ifndef _RELIBC_SYS_PROCFS_H
#define _RELIBC_SYS_PROCFS_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/user.h>

#define ELF_PRARGSZ 80

typedef void *psaddr_t;

typedef elf_gregset_t *prgregset_t;

typedef elf_fpregset_t prfpregset_t;

typedef pid_t lwpid_t;

struct elf_siginfo {
  int si_signo;
  int si_code;
  int si_errno;
};

struct time {
  long tv_sec;
  long tv_usec;
};

struct elf_prstatus {
  struct elf_siginfo pr_info;
  short pr_cursig;
  unsigned long pr_sigpend;
  unsigned long pr_sighold;
  pid_t pr_pid;
  pid_t pr_ppid;
  pid_t pr_pgrp;
  pid_t pr_sid;
  struct time pr_utime;
  struct time pr_stime;
  struct time pr_cutime;
  struct time pr_cstime;
  elf_gregset_t pr_reg;
  int pr_fpvalid;
};

typedef struct elf_prstatus prstatus_t;

struct elf_prpsinfo {
  char pr_state;
  char pr_sname;
  char pr_zomb;
  char pr_nice;
  unsigned int pr_flag;
  unsigned int pr_uid;
  unsigned int pr_gid;
  int pr_pid;
  int pr_ppid;
  int pr_pgrp;
  int pr_sid;
  char pr_fname[16];
  char pr_psargs[ELF_PRARGSZ];
};

typedef struct elf_prpsinfo prpsinfo_t;

#endif  /* _RELIBC_SYS_PROCFS_H */
