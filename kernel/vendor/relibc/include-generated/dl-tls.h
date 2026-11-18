#ifndef _RELIBC_DL_TLS_H
#define _RELIBC_DL_TLS_H

struct dl_tls_index {
  uintptr_t ti_module;
  uintptr_t ti_offset;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

void *__tls_get_addr(struct dl_tls_index *ti);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_DL_TLS_H */
