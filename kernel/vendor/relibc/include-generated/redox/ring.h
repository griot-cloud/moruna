#ifndef _REDOX_RING_H
#define _REDOX_RING_H

#include <stdint.h>
#include <sys/types.h>
#include <signal.h>
typedef struct redox_ring redox_ring;
typedef struct redox_ring_sq redox_ring_sq;
typedef struct redox_ring_cq redox_ring_cq;


/**
 * # Return
 *
 * Returns 0 on success, or `-errno` on error.
 */
int redox_ring_register_files(struct redox_ring *ring, const int *files, unsigned int nr_files);

/**
 * # Return
 *
 * Returns 0 on success, or `-errno` on error.
 */
int redox_ring_queue_init_with_path(unsigned int entries,
                                    unsigned int sqe_size,
                                    unsigned int cqe_size,
                                    unsigned int pool_size,
                                    unsigned int chunk_size,
                                    unsigned int sized,
                                    struct redox_ring *ring,
                                    unsigned int flags,
                                    const char *path);

int redox_ring_alloc_buf(struct redox_ring *ring, uint8_t **out_ptr);

int redox_ring_alloc_sized(struct redox_ring *ring,
                           unsigned int size,
                           unsigned int align,
                           uint8_t **out_ptr);

int redox_ring_free_buf(struct redox_ring *ring, unsigned int offset);

int redox_ring_free_buf_sized(struct redox_ring *ring, uint8_t *ptr);

void *redox_ring_get_sqe(struct redox_ring *ring);

int redox_ring_submit(struct redox_ring *ring);

int redox_ring_wait_cqe(struct redox_ring *ring, void *cqe);

void redox_ring_cqe_seen(struct redox_ring *ring);

void redox_ring_queue_exit(struct redox_ring *ring);

size_t redox_ring_sizeof(void);

#endif  /* _REDOX_RING_H */
