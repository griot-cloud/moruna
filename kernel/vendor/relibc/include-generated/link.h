#ifndef _RELIBC_LINK_H
#define _RELIBC_LINK_H

#include <stdint.h>
#include <stddef.h>
#include <elf.h>
#if UINTPTR_MAX > 0xffffffff
#define ElfW(type) Elf64_ ## type
#else
#define ElfW(type) Elf32_ ## type
#endif


struct dl_phdr_info {
  size_t dlpi_addr;
  const char *dlpi_name;
  const ElfW(Phdr) *dlpi_phdr;
  uint16_t dlpi_phnum;
  unsigned long long dlpi_adds;
  unsigned long long dlpi_subs;
  size_t dlpi_tls_modid;
  void *dlpi_tls_data;
};

typedef int (*dl_iterate_phdr_callback)(struct dl_phdr_info *info, size_t size, void *data);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Non-POSIX, see <https://man7.org/linux/man-pages/man3/dl_iterate_phdr.3.html>
 */
int dl_iterate_phdr(dl_iterate_phdr_callback callback, void *data);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_LINK_H */
