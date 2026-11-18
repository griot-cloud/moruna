#ifndef _RELIBC_CRYPT_H
#define _RELIBC_CRYPT_H

/**
 * See <https://www.man7.org/linux/man-pages/man3/crypt.3.html>.
 */
typedef struct {
  int initialized;
  char buff[256];
} crypt_data;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://www.man7.org/linux/man-pages/man3/crypt.3.html>.
 */
char *crypt_r(const char *key, const char *setting, crypt_data *data);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_CRYPT_H */
