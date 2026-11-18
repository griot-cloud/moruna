#ifndef _RELIBC_BITS_WINSIZE_H
#define _RELIBC_BITS_WINSIZE_H

struct winsize {
  /**
   * Rows, in characters.
   */
  unsigned short ws_row;
  /**
   * Columns, in characters.
   */
  unsigned short ws_col;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/TIOCSWINSZ.2const.html>.
   *
   * Unused.
   */
  unsigned short ws_xpixel;
  /**
   * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/TIOCSWINSZ.2const.html>.
   *
   * Unused.
   */
  unsigned short ws_ypixel;
};

#endif  /* _RELIBC_BITS_WINSIZE_H */
