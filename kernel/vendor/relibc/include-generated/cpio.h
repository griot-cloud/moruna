#ifndef _RELIBC_CPIO_H
#define _RELIBC_CPIO_H

#define MAGIC "070707"


/**
 * Read by owner.
 */
#define C_IRUSR 256

/**
 * Write by owner.
 */
#define C_IWUSR 128

/**
 * Execute by owner.
 */
#define C_IXUSR 64

/**
 * Read by group.
 */
#define C_IRGRP 32

/**
 * Write by group.
 */
#define C_IWGRP 16

/**
 * Execute by group.
 */
#define C_IXGRP 8

/**
 * Read by others.
 */
#define C_IROTH 4

/**
 * Write by others.
 */
#define C_IWOTH 2

/**
 * Execute by others.
 */
#define C_IXOTH 1

/**
 * Set user ID.
 */
#define C_ISUID 2048

/**
 * Set group ID.
 */
#define C_ISGID 1024

/**
 * On directories, restricted deletion flag.
 */
#define C_ISVTX 512

/**
 * Directory.
 */
#define C_ISDIR 16384

/**
 * FIFO.
 */
#define C_ISFIFO 4096

/**
 * Regular file.
 */
#define C_ISREG 32768

/**
 * Block special.
 */
#define C_ISBLK 24576

/**
 * Character special.
 */
#define C_ISCHR 8192

/**
 * Reserved.
 */
#define C_ISCTG 36864

/**
 * Symbolic link.
 */
#define C_ISLNK 40960

/**
 * Socket.
 */
#define C_ISSOCK 49152

#endif  /* _RELIBC_CPIO_H */
