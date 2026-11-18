#ifndef _RELIBC_TAR_H
#define _RELIBC_TAR_H


#define TMAGIC "ustar"
#define TVERSION "00"


/**
 * Block size for tar archives (512 bytes).
 */
#define BLOCKSIZE 512

/**
 * Default record size for tar archives (10KB, consisting of 20 blocks).
 */
#define RECORDSIZE (BLOCKSIZE * 20)

/**
 * Field lengths in tar headers
 */
#define NAME_SIZE 100

#define MODE_SIZE 8

#define UID_SIZE 8

#define GID_SIZE 8

#define SIZE_SIZE 12

#define MTIME_SIZE 12

#define CHKSUM_SIZE 8

#define LINKNAME_SIZE 100

#define MAGIC_SIZE 6

#define VERSION_SIZE 2

#define UNAME_SIZE 32

#define GNAME_SIZE 32

#define DEVMAJOR_SIZE 8

#define DEVMINOR_SIZE 8

#define PREFIX_SIZE 155

#define HEADER_SIZE 512

/**
 * Bits used in the mode field - value in octal
 */
#define TSUID 2048

#define TSGID 1024

#define TSVTX 512

#define TUREAD 256

#define TUWRITE 128

#define TUEXEC 64

#define TGREAD 32

#define TGWRITE 16

#define TGEXEC 8

#define TOREAD 4

#define TOWRITE 2

#define TOEXEC 1

/**
 * Values used in typeflag field
 */
#define REGTYPE 48

#define AREGTYPE 0

#define LNKTYPE 49

#define SYMTYPE 50

#define CHRTYPE 51

#define BLKTYPE 52

#define DIRTYPE 53

#define FIFOTYPE 54

#define CONTTYPE 55

#define TMAGLEN 6

#define TVERSLEN 2

/**
 * Reserved for future standards
 */
#define XHDRTYPE 120

#define XGLTYPE 103

#endif  /* _RELIBC_TAR_H */
