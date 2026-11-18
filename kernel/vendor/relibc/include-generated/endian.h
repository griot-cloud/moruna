#ifndef _RELIBC_ENDIAN_H
#define _RELIBC_ENDIAN_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint16_t` from big endian representation to host endian
 * respresentation.
 */
uint16_t be16toh(uint16_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint32_t` from big endian representation to host endian
 * respresentation.
 */
uint32_t be32toh(uint32_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint64_t` from big endian representation to host endian
 * respresentation.
 */
uint64_t be64toh(uint64_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint16_t` from host endian representation to big endian
 * respresentation.
 */
uint16_t htobe16(uint16_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint32_t` from host endian representation to big endian
 * respresentation.
 */
uint32_t htobe32(uint32_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint64_t` from host endian representation to big endian
 * respresentation.
 */
uint64_t htobe64(uint64_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint16_t` from host endian representation to little endian
 * respresentation.
 */
uint16_t htole16(uint16_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint32_t` from host endian representation to little endian
 * respresentation.
 */
uint32_t htole32(uint32_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint64_t` from host endian representation to little endian
 * respresentation.
 */
uint64_t htole64(uint64_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint16_t` from little endian representation to host endian
 * respresentation.
 */
uint16_t le16toh(uint16_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint32_t` from little endian representation to host endian
 * respresentation.
 */
uint32_t le32toh(uint32_t x);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/be16toh.html>.
 *
 * Convert a `uint64_t` from little endian representation to host endian
 * respresentation.
 */
uint64_t le64toh(uint64_t x);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_ENDIAN_H */

#include <machine/endian.h>
