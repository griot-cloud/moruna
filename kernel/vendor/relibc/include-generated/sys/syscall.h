#ifndef _RELIBC_SYS_SYSCALL_H
#define _RELIBC_SYS_SYSCALL_H

#include <bits/size-t.h>


#if (defined(__linux__) && defined(__aarch64__))
#define __NR_accept 202
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_accept 43
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_accept4 242
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_accept4 288
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_acct 89
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_acct 163
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_add_key 217
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_add_key 248
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_adjtimex 171
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_adjtimex 159
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_arch_specific_syscall 244
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_bind 200
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_bind 49
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_bpf 280
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_bpf 321
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_brk 214
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_brk 12
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_capget 90
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_capget 125
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_capset 91
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_capset 126
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_chdir 49
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_chdir 80
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_chroot 51
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_chroot 161
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clock_adjtime 266
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clock_adjtime 305
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clock_getres 114
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clock_getres 229
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clock_gettime 113
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clock_gettime 228
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clock_nanosleep 115
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clock_nanosleep 230
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clock_settime 112
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clock_settime 227
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clone 220
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clone 56
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_clone3 435
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_clone3 435
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_close 57
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_close 3
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_close_range 436
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_close_range 436
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_connect 203
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_connect 42
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_copy_file_range 285
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_copy_file_range 326
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_delete_module 106
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_delete_module 176
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_dup 23
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_dup 32
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_dup3 24
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_dup3 292
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_epoll_create1 20
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_create1 291
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_epoll_ctl 21
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_ctl 233
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_epoll_pwait 22
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_pwait 281
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_epoll_pwait2 441
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_pwait2 441
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_eventfd2 19
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_eventfd2 290
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_execve 221
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_execve 59
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_execveat 281
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_execveat 322
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_exit 93
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_exit 60
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_exit_group 94
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_exit_group 231
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_faccessat 48
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_faccessat 269
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_faccessat2 439
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_faccessat2 439
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fadvise64 223
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fadvise64 221
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fallocate 47
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fallocate 285
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fanotify_init 262
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fanotify_init 300
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fanotify_mark 263
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fanotify_mark 301
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fchdir 50
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fchdir 81
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fchmod 52
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fchmod 91
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fchmodat 53
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fchmodat 268
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fchown 55
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fchown 93
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fchownat 54
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fchownat 260
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fcntl 25
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fcntl 72
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fdatasync 83
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fdatasync 75
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fgetxattr 10
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fgetxattr 193
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_finit_module 273
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_finit_module 313
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_flistxattr 13
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_flistxattr 196
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_flock 32
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_flock 73
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fremovexattr 16
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fremovexattr 199
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fsconfig 431
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fsconfig 431
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fsetxattr 7
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fsetxattr 190
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fsmount 432
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fsmount 432
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fsopen 430
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fsopen 430
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fspick 433
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fspick 433
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fstat 80
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fstat 5
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fstatfs 44
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fstatfs 138
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_fsync 82
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fsync 74
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ftruncate 46
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ftruncate 77
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_futex 98
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_futex 202
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_get_mempolicy 236
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_get_mempolicy 239
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_get_robust_list 100
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_get_robust_list 274
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getcpu 168
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getcpu 309
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getcwd 17
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getcwd 79
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getdents64 61
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getdents64 217
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getegid 177
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getegid 108
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_geteuid 175
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_geteuid 107
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getgid 176
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getgid 104
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getgroups 158
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getgroups 115
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getitimer 102
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getitimer 36
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getpeername 205
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpeername 52
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getpgid 155
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpgid 121
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getpid 172
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpid 39
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getppid 173
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getppid 110
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getpriority 141
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpriority 140
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getrandom 278
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getrandom 318
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getresgid 150
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getresgid 120
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getresuid 148
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getresuid 118
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getrlimit 163
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getrlimit 97
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getrusage 165
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getrusage 98
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getsid 156
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getsid 124
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getsockname 204
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getsockname 51
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getsockopt 209
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getsockopt 55
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_gettid 178
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_gettid 186
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_gettimeofday 169
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_gettimeofday 96
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getuid 174
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getuid 102
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_getxattr 8
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getxattr 191
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_init_module 105
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_init_module 175
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_inotify_add_watch 27
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_inotify_add_watch 254
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_inotify_init1 26
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_inotify_init1 294
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_inotify_rm_watch 28
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_inotify_rm_watch 255
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_cancel 3
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_cancel 210
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_destroy 1
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_destroy 207
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_getevents 4
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_getevents 208
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_pgetevents 292
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_pgetevents 333
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_setup 0
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_setup 206
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_submit 2
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_submit 209
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_uring_enter 426
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_uring_enter 426
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_uring_register 427
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_uring_register 427
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_io_uring_setup 425
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_io_uring_setup 425
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ioctl 29
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ioctl 16
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ioprio_get 31
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ioprio_get 252
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ioprio_set 30
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ioprio_set 251
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_kcmp 272
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_kcmp 312
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_kexec_file_load 294
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_kexec_file_load 320
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_kexec_load 104
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_kexec_load 246
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_keyctl 219
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_keyctl 250
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_kill 129
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_kill 62
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_lgetxattr 9
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lgetxattr 192
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_linkat 37
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_linkat 265
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_listen 201
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_listen 50
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_listxattr 11
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_listxattr 194
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_llistxattr 12
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_llistxattr 195
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_lookup_dcookie 18
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lookup_dcookie 212
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_lremovexattr 15
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lremovexattr 198
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_lseek 62
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lseek 8
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_lsetxattr 6
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lsetxattr 189
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_madvise 233
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_madvise 28
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mbind 235
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mbind 237
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_membarrier 283
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_membarrier 324
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_memfd_create 279
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_memfd_create 319
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_migrate_pages 238
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_migrate_pages 256
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mincore 232
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mincore 27
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mkdirat 34
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mkdirat 258
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mknodat 33
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mknodat 259
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mlock 228
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mlock 149
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mlock2 284
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mlock2 325
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mlockall 230
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mlockall 151
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mmap 222
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mmap 9
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mount 40
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mount 165
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mount_setattr 442
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mount_setattr 442
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_move_mount 429
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_move_mount 429
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_move_pages 239
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_move_pages 279
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mprotect 226
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mprotect 10
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_getsetattr 185
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_getsetattr 245
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_notify 184
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_notify 244
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_open 180
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_open 240
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_timedreceive 183
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_timedreceive 243
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_timedsend 182
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_timedsend 242
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mq_unlink 181
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mq_unlink 241
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_mremap 216
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mremap 25
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_msgctl 187
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_msgctl 71
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_msgget 186
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_msgget 68
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_msgrcv 188
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_msgrcv 70
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_msgsnd 189
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_msgsnd 69
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_msync 227
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_msync 26
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_munlock 229
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_munlock 150
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_munlockall 231
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_munlockall 152
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_munmap 215
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_munmap 11
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_name_to_handle_at 264
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_name_to_handle_at 303
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_nanosleep 101
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_nanosleep 35
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_newfstatat 79
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_newfstatat 262
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_nfsservctl 42
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_nfsservctl 180
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_open_by_handle_at 265
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_open_by_handle_at 304
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_open_tree 428
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_open_tree 428
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_openat 56
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_openat 257
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_openat2 437
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_openat2 437
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_perf_event_open 241
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_perf_event_open 298
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_personality 92
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_personality 135
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pidfd_getfd 438
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pidfd_getfd 438
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pidfd_open 434
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pidfd_open 434
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pidfd_send_signal 424
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pidfd_send_signal 424
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pipe2 59
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pipe2 293
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pivot_root 41
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pivot_root 155
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pkey_alloc 289
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pkey_alloc 330
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pkey_free 290
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pkey_free 331
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pkey_mprotect 288
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pkey_mprotect 329
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ppoll 73
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ppoll 271
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_prctl 167
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_prctl 157
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pread64 67
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pread64 17
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_preadv 69
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_preadv 295
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_preadv2 286
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_preadv2 327
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_prlimit64 261
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_prlimit64 302
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_process_madvise 440
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_process_madvise 440
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_process_vm_readv 270
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_process_vm_readv 310
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_process_vm_writev 271
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_process_vm_writev 311
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pselect6 72
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pselect6 270
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_ptrace 117
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ptrace 101
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pwrite64 68
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pwrite64 18
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pwritev 70
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pwritev 296
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_pwritev2 287
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pwritev2 328
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_quotactl 60
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_quotactl 179
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_read 63
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_read 0
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_readahead 213
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_readahead 187
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_readlinkat 78
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_readlinkat 267
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_readv 65
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_readv 19
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_reboot 142
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_reboot 169
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_recvfrom 207
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_recvfrom 45
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_recvmmsg 243
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_recvmmsg 299
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_recvmsg 212
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_recvmsg 47
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_remap_file_pages 234
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_remap_file_pages 216
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_removexattr 14
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_removexattr 197
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_renameat 38
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_renameat 264
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_renameat2 276
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_renameat2 316
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_request_key 218
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_request_key 249
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_restart_syscall 128
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_restart_syscall 219
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rseq 293
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rseq 334
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigaction 134
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigaction 13
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigpending 136
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigpending 127
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigprocmask 135
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigprocmask 14
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigqueueinfo 138
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigqueueinfo 129
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigreturn 139
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigreturn 15
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigsuspend 133
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigsuspend 130
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_sigtimedwait 137
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_sigtimedwait 128
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_rt_tgsigqueueinfo 240
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rt_tgsigqueueinfo 297
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_get_priority_max 125
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_get_priority_max 146
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_get_priority_min 126
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_get_priority_min 147
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_getaffinity 123
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_getaffinity 204
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_getattr 275
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_getattr 315
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_getparam 121
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_getparam 143
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_getscheduler 120
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_getscheduler 145
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_rr_get_interval 127
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_rr_get_interval 148
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_setaffinity 122
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_setaffinity 203
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_setattr 274
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_setattr 314
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_setparam 118
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_setparam 142
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_setscheduler 119
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_setscheduler 144
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sched_yield 124
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sched_yield 24
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_seccomp 277
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_seccomp 317
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_semctl 191
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_semctl 66
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_semget 190
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_semget 64
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_semop 193
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_semop 65
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_semtimedop 192
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_semtimedop 220
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sendfile 71
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sendfile 40
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sendmmsg 269
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sendmmsg 307
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sendmsg 211
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sendmsg 46
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sendto 206
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sendto 44
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_set_mempolicy 237
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_set_mempolicy 238
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_set_robust_list 99
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_set_robust_list 273
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_set_tid_address 96
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_set_tid_address 218
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setdomainname 162
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setdomainname 171
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setfsgid 152
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setfsgid 123
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setfsuid 151
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setfsuid 122
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setgid 144
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setgid 106
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setgroups 159
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setgroups 116
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sethostname 161
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sethostname 170
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setitimer 103
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setitimer 38
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setns 268
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setns 308
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setpgid 154
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setpgid 109
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setpriority 140
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setpriority 141
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setregid 143
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setregid 114
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setresgid 149
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setresgid 119
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setresuid 147
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setresuid 117
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setreuid 145
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setreuid 113
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setrlimit 164
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setrlimit 160
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setsid 157
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setsid 112
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setsockopt 208
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setsockopt 54
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_settimeofday 170
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_settimeofday 164
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setuid 146
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setuid 105
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_setxattr 5
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_setxattr 188
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_shmat 196
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_shmat 30
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_shmctl 195
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_shmctl 31
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_shmdt 197
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_shmdt 67
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_shmget 194
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_shmget 29
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_shutdown 210
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_shutdown 48
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sigaltstack 132
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sigaltstack 131
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_signalfd4 74
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_signalfd4 289
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_socket 198
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_socket 41
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_socketpair 199
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_socketpair 53
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_splice 76
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_splice 275
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_statfs 43
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_statfs 137
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_statx 291
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_statx 332
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_swapoff 225
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_swapoff 168
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_swapon 224
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_swapon 167
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_symlinkat 36
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_symlinkat 266
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sync 81
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sync 162
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sync_file_range 84
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sync_file_range 277
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_syncfs 267
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_syncfs 306
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_syscalls 443
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_sysinfo 179
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sysinfo 99
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_syslog 116
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_syslog 103
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_tee 77
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_tee 276
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_tgkill 131
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_tgkill 234
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timer_create 107
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timer_create 222
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timer_delete 111
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timer_delete 226
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timer_getoverrun 109
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timer_getoverrun 225
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timer_gettime 108
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timer_gettime 224
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timer_settime 110
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timer_settime 223
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timerfd_create 85
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timerfd_create 283
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timerfd_gettime 87
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timerfd_gettime 287
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_timerfd_settime 86
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_timerfd_settime 286
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_times 153
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_times 100
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_tkill 130
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_tkill 200
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_truncate 45
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_truncate 76
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_umask 166
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_umask 95
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_umount2 39
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_umount2 166
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_uname 160
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_uname 63
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_unlinkat 35
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_unlinkat 263
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_unshare 97
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_unshare 272
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_userfaultfd 282
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_userfaultfd 323
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_utimensat 88
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_utimensat 280
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_vhangup 58
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_vhangup 153
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_vmsplice 75
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_vmsplice 278
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_wait4 260
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_wait4 61
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_waitid 95
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_waitid 247
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_write 64
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_write 1
#endif

#if (defined(__linux__) && defined(__aarch64__))
#define __NR_writev 66
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_writev 20
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_access 21
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_afs_syscall 183
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_alarm 37
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_arch_prctl 158
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_chmod 90
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_chown 92
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_creat 85
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_create_module 174
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_dup2 33
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_create 213
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_ctl_old 214
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_wait 232
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_epoll_wait_old 215
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_eventfd 284
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_fork 57
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_futimesat 261
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_get_kernel_syms 177
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_get_thread_area 211
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getdents 78
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpgrp 111
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_getpmsg 181
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_inotify_init 253
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ioperm 173
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_iopl 172
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lchown 94
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_link 86
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_lstat 6
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mkdir 83
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_mknod 133
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_modify_ldt 154
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_open 2
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pause 34
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_pipe 22
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_poll 7
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_putpmsg 182
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_query_module 178
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_readlink 89
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rename 82
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_rmdir 84
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_security 185
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_select 23
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_set_thread_area 205
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_signalfd 282
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_stat 4
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_symlink 88
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_sysfs 139
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_time 201
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_tuxcall 184
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_unlink 87
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_uselib 134
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_ustat 136
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_utime 132
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_utimes 235
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_vfork 58
#endif

#if (defined(__linux__) && defined(__x86_64__))
#define __NR_vserver 236
#endif

#endif  /* _RELIBC_SYS_SYSCALL_H */
