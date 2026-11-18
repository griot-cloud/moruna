#ifndef _RELIBC_TERMIOS_H
#define _RELIBC_TERMIOS_H

#include <bits/pid-t.h>   // for pid_t from sys/types.h
#include <bits/winsize.h> // for winsize


/**
 * Suspend output.
 */
#define TCOOFF 0

/**
 * Restart output.
 */
#define TCOON 1

/**
 * Transmit a `STOP` character, intended to suspend input data.
 */
#define TCIOFF 2

/**
 * Transmit a `START` character, intended to restart input data.
 */
#define TCION 3

/**
 * Flush pending input.
 */
#define TCIFLUSH 0

/**
 * Flush untransmitted output.
 */
#define TCOFLUSH 1

/**
 * Flush bot pending input and untransmitted output.
 */
#define TCIOFLUSH 2

/**
 * Change attributes immediately.
 */
#define TCSANOW 0

/**
 * Change attributes when output has drained.
 */
#define TCSADRAIN 1

/**
 * Change attributes when output has drained; also flush pending input.
 */
#define TCSAFLUSH 2

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * SWTCH character (not supported under Linux).
 * Used in System V to switch shells in `shell layers`, a predecessor to shell
 * job control.
 */
#define VSWTCH 7

/**
 * SUSP character.
 * Send `SIGTSTP` signal.
 * Canonical and Non-Canonical mode.
 */
#define VSUSP 10

/**
 * Size of the array `c_cc` for control characters.
 */
#define NCCS 32

/**
 * Ignore break condition.
 */
#define IGNBRK 1

/**
 * Signal interrupt on break.
 */
#define BRKINT 2

/**
 * Ignore characters with parity errors.
 */
#define IGNPAR 4

/**
 * Mark parity errors.
 */
#define PARMRK 8

/**
 * Enable input parity check.
 */
#define INPCK 16

/**
 * Strip character.
 */
#define ISTRIP 32

/**
 * Map NL to CR on input.
 */
#define INLCR 64

/**
 * Ignore CR.
 */
#define IGNCR 128

/**
 * Map CR to NL on input.
 */
#define ICRNL 256

/**
 * Enable any character to restart output.
 */
#define IXANY 2048

/**
 * Post-process output.
 */
#define OPOST 1

/**
 * Map CR to NL on output.
 */
#define OCRNL 8

/**
 * No CR output at column 0.
 */
#define ONOCR 16

/**
 * NL performs CR function.
 */
#define ONLRET 32

/**
 * Use fill characters for delay.
 */
#define OFILL 64

/**
 * Fill is DEL.
 */
#define OFDEL 128

/**
 * Hang up.
 */
#define B0 0

/**
 * 50 baud.
 */
#define B50 1

/**
 * 75 baud.
 */
#define B75 2

/**
 * 110 baud.
 */
#define B110 3

/**
 * 134.5 baud.
 */
#define B134 4

/**
 * 150 baud.
 */
#define B150 5

/**
 * 200 baud.
 */
#define B200 6

/**
 * 300 baud.
 */
#define B300 7

/**
 * 600 baud.
 */
#define B600 8

/**
 * 1200 baud.
 */
#define B1200 9

/**
 * 1800 baud.
 */
#define B1800 10

/**
 * 2400 baud.
 */
#define B2400 11

/**
 * 4800 baud.
 */
#define B4800 12

/**
 * 9600 baud.
 */
#define B9600 13

/**
 * 19200 baud.
 */
#define B19200 14

/**
 * 38400 baud.
 */
#define B38400 15

/**
 * 5 bits.
 */
#define CS5 0

/**
 * Enable echo.
 */
#define ECHO 8

#if (defined(__linux__) || defined(__moruna__))
/**
 * INTR character.
 * Send a `SIGINT` signal.
 * Canonical and Non-Canonical mode.
 */
#define VINTR 0
#endif

#if defined(__redox__)
/**
 * INTR character.
 * Send a `SIGINT` signal.
 * Canonical and Non-Canonical mode.
 */
#define VINTR 8
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * QUIT character.
 * Send a `SIGQUIT` signal.
 * Canonical and Non-Canonical mode.
 */
#define VQUIT 1
#endif

#if defined(__redox__)
/**
 * QUIT character.
 * Send a `SIGQUIT` signal.
 * Canonical and Non-Canonical mode.
 */
#define VQUIT 9
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * ERASE character.
 * Erases the previous not-yet-erased character, but does not erase past `EOF`
 * or beginning-of-line.
 * Canonical mode only.
 */
#define VERASE 2
#endif

#if defined(__redox__)
/**
 * ERASE character.
 * Erases the previous not-yet-erased character, but does not erase past `EOF`
 * or beginning-of-line.
 * Canonical mode only.
 */
#define VERASE 3
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * KILL character.
 * Erases the input since the last `EOF` or beginning-of-line.
 * Canonical mode only.
 */
#define VKILL 3
#endif

#if defined(__redox__)
/**
 * KILL character.
 * Erases the input since the last `EOF` or beginning-of-line.
 * Canonical mode only.
 */
#define VKILL 5
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * EOF character.
 * End of file. Causes the pending tty buffer to be sent to the waiting user
 * program without waiting for end-of-line.
 * Canonical mode only.
 */
#define VEOF 4
#endif

#if defined(__redox__)
/**
 * EOF character.
 * End of file. Causes the pending tty buffer to be sent to the waiting user
 * program without waiting for end-of-line.
 * Canonical mode only.
 */
#define VEOF 0
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * TIME value.
 * Timeout in decideconds for noncanonical read.
 * Non-Canonical mode only.
 */
#define VTIME 5
#endif

#if defined(__redox__)
/**
 * TIME value.
 * Timeout in decideconds for noncanonical read.
 * Non-Canonical mode only.
 */
#define VTIME 17
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * MIN value.
 * Minimum number of characters for noncanonical read.
 * Non-Canonical mode only.
 */
#define VMIN 6
#endif

#if defined(__redox__)
/**
 * MIN value.
 * Minimum number of characters for noncanonical read.
 * Non-Canonical mode only.
 */
#define VMIN 16
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * START character.
 * Restarts output stopped by the STOP character.
 * Canonical and Non-Canonical mode.
 */
#define VSTART 8
#endif

#if defined(__redox__)
/**
 * START character.
 * Restarts output stopped by the STOP character.
 * Canonical and Non-Canonical mode.
 */
#define VSTART 12
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * STOP character.
 * Stops the output until the START character is typed.
 * Canonical and Non-Canonical mode.
 */
#define VSTOP 9
#endif

#if defined(__redox__)
/**
 * STOP character.
 * Stops the output until the START character is typed.
 * Canonical and Non-Canonical mode.
 */
#define VSTOP 13
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * EOL character.
 * Additional end-of-line character.
 * Canonical mode only.
 */
#define VEOL 11
#endif

#if defined(__redox__)
/**
 * EOL character.
 * Additional end-of-line character.
 * Canonical mode only.
 */
#define VEOL 1
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * REPRINT character.
 * Reprints unread characters.
 */
#define VREPRINT 12
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * REPRINT character.
 * Reprints unread characters.
 */
#define VREPRINT 6
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * DISCARD character (not supported under Linux).
 * Toggles discarding pending output.
 */
#define VDISCARD 13
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * DISCARD character (not supported under Linux).
 * Toggles discarding pending output.
 */
#define VDISCARD 15
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * WERASE character.
 * Word erase.
 */
#define VWERASE 14
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * WERASE character.
 * Word erase.
 */
#define VWERASE 4
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * LNEXT character.
 * Literal next. Quotes the next input character, depriving it of a possible
 * special meaning.
 */
#define VLNEXT 15
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * LNEXT character.
 * Literal next. Quotes the next input character, depriving it of a possible
 * special meaning.
 */
#define VLNEXT 14
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * EOL2 character.
 * Another end-of-line character.
 */
#define VEOL2 16
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * EOL2 character.
 * Another end-of-line character.
 */
#define VEOL2 2
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * Map uppercase characters to lowercase on input.
 */
#define IUCLC 512
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Enable start/stop output control.
 */
#define IXON 1024
#endif

#if defined(__redox__)
/**
 * Enable start/stop output control.
 */
#define IXON 512
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Enable start/stop input control.
 */
#define IXOFF 4096
#endif

#if defined(__redox__)
/**
 * Enable start/stop input control.
 */
#define IXOFF 1024
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * Ring bell when input queue is full.
 *
 * # Implemetation
 * Linux acts as if this is always set.
 */
#define IMAXBEL 8192
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * Input is UTF8; this allows character-erase to be correctly performed in
 * cooked mode.
 */
#define IUTF8 16384
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * Map lowercase characters to uppercase on output.
 */
#define OLCUC 2
#endif

#if defined(__redox__)
/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man3/termios.3.html>.
 *
 * Map lowercase characters to uppercase on output.
 */
#define OLCUC 4
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Map NL to CR-NL on output.
 */
#define ONLCR 4
#endif

#if defined(__redox__)
/**
 * Map NL to CR-NL on output.
 */
#define ONLCR 2
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Select vertical-tab delays.
 */
#define VTDLY 16384
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Vertical-tab delay type 0.
 */
#define VT0 0
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Vertical-tab delay type 1.
 */
#define VT1 16384
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B57600 4097
#endif

#if defined(__redox__)
#define B57600 16
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B115200 4098
#endif

#if defined(__redox__)
#define B115200 17
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B230400 4099
#endif

#if defined(__redox__)
#define B230400 18
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B460800 4100
#endif

#if defined(__redox__)
#define B460800 19
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B500000 4101
#endif

#if defined(__redox__)
#define B500000 20
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B576000 4102
#endif

#if defined(__redox__)
#define B576000 21
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B921600 4103
#endif

#if defined(__redox__)
#define B921600 22
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B1000000 4104
#endif

#if defined(__redox__)
#define B1000000 23
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B1152000 4105
#endif

#if defined(__redox__)
#define B1152000 24
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B1500000 4106
#endif

#if defined(__redox__)
#define B1500000 25
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B2000000 4107
#endif

#if defined(__redox__)
#define B2000000 26
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B2500000 4108
#endif

#if defined(__redox__)
#define B2500000 27
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B3000000 4109
#endif

#if defined(__redox__)
#define B3000000 28
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B3500000 4110
#endif

#if defined(__redox__)
#define B3500000 29
#endif

#if (defined(__linux__) || defined(__moruna__))
#define B4000000 4111
#endif

#if defined(__redox__)
#define B4000000 30
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Character size.
 */
#define CSIZE 48
#endif

#if defined(__redox__)
/**
 * Character size.
 */
#define CSIZE 768
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * 6 bits.
 */
#define CS6 16
#endif

#if defined(__redox__)
/**
 * 6 bits.
 */
#define CS6 256
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * 7 bits.
 */
#define CS7 32
#endif

#if defined(__redox__)
/**
 * 7 bits.
 */
#define CS7 512
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * 8 bits.
 */
#define CS8 48
#endif

#if defined(__redox__)
/**
 * 8 bits.
 */
#define CS8 768
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Send two stop bits, else one.
 */
#define CSTOPB 64
#endif

#if defined(__redox__)
/**
 * Send two stop bits, else one.
 */
#define CSTOPB 1024
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Enable receiver.
 */
#define CREAD 128
#endif

#if defined(__redox__)
/**
 * Enable receiver.
 */
#define CREAD 2048
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Parity enable.
 */
#define PARENB 256
#endif

#if defined(__redox__)
/**
 * Parity enable.
 */
#define PARENB 4096
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Odd parity, else even.
 */
#define PARODD 512
#endif

#if defined(__redox__)
/**
 * Odd parity, else even.
 */
#define PARODD 8192
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Hang up on last close.
 */
#define HUPCL 1024
#endif

#if defined(__redox__)
/**
 * Hang up on last close.
 */
#define HUPCL 16384
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Ignore modem status lines.
 */
#define CLOCAL 2048
#endif

#if defined(__redox__)
/**
 * Ignore modem status lines.
 */
#define CLOCAL 32768
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Enable signals.
 */
#define ISIG 1
#endif

#if defined(__redox__)
/**
 * Enable signals.
 */
#define ISIG 128
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Canoncal input (erase and kill processing).
 */
#define ICANON 2
#endif

#if defined(__redox__)
/**
 * Canoncal input (erase and kill processing).
 */
#define ICANON 256
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Echo erase character as error-correcting backspace.
 */
#define ECHOE 16
#endif

#if defined(__redox__)
/**
 * Echo erase character as error-correcting backspace.
 */
#define ECHOE 2
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Echo KILL.
 */
#define ECHOK 32
#endif

#if defined(__redox__)
/**
 * Echo KILL.
 */
#define ECHOK 4
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Echo NL.
 */
#define ECHONL 64
#endif

#if defined(__redox__)
/**
 * Echo NL.
 */
#define ECHONL 16
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Disable flush after interrupt or quit.
 */
#define NOFLSH 128
#endif

#if defined(__redox__)
/**
 * Disable flush after interrupt or quit.
 */
#define NOFLSH 2147483648
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Send `SIGTTOU` for background output.
 */
#define TOSTOP 256
#endif

#if defined(__redox__)
/**
 * Send `SIGTTOU` for background output.
 */
#define TOSTOP 4194304
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Enable extended input character processing.
 */
#define IEXTEN 32768
#endif

#if defined(__redox__)
/**
 * Enable extended input character processing.
 */
#define IEXTEN 1024
#endif

/**
 * Used for terminal modes.
 */
typedef unsigned int tcflag_t;

/**
 * Used for terminal special characters.
 */
typedef unsigned char cc_t;

/**
 * Used for terminal baud rates.
 */
typedef unsigned int speed_t;

#if (defined(__linux__) || defined(__moruna__))
/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/termios.h.html>.
 */
struct termios {
  /**
   * Input modes.
   */
  tcflag_t c_iflag;
  /**
   * Output modes.
   */
  tcflag_t c_oflag;
  /**
   * Control modes.
   */
  tcflag_t c_cflag;
  /**
   * Local modes.
   */
  tcflag_t c_lflag;
  cc_t c_line;
  /**
   * Control characters.
   */
  cc_t c_cc[NCCS];
  speed_t __c_ispeed;
  speed_t __c_ospeed;
};
#endif

#if defined(__redox__)
/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/termios.h.html>.
 */
struct termios {
  /**
   * Input modes.
   */
  tcflag_t c_iflag;
  /**
   * Output modes.
   */
  tcflag_t c_oflag;
  /**
   * Control modes.
   */
  tcflag_t c_cflag;
  /**
   * Local modes.
   */
  tcflag_t c_lflag;
  /**
   * Control characters.
   */
  cc_t c_cc[NCCS];
};
#endif

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcgetattr.html>.
 *
 * Gets the parameters associated with the terminal referred to by `fildes`
 * and stores them in the termios structure referenced by `termios_p`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcgetattr(int fildes, struct termios *termios_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcsetattr.html>.
 *
 * Sets the parameters associated with the terminal referred to by `fildes`
 * from the termios structure referred to by `termios_p`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcsetattr(int fildes, int optional_actions, const struct termios *termios_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcgetsid.html>.
 *
 * Obtains the process group ID of the session for which the terminal
 * specified by `fildes` is the controlling terminal.
 *
 * Upon success, returns the process group ID of the session associated with
 * the terminal. Upon failure, returns `-1` and sets errno to indicate the
 * error.
 */
pid_t tcgetsid(int fildes);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cfgetispeed.html>.
 *
 * Extracts the input baud rate from the termios structure reference to by
 * `termios_p`.
 *
 * Returns a value representing the input baud rate.
 *
 * # Implementation
 * Infallible. Always returns a value.
 */
speed_t cfgetispeed(const struct termios *termios_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cfgetospeed.html>.
 *
 * Extracts the output baud rate from the termios structure reference to by
 * `termios_p`.
 *
 * Returns a value representing the output baud rate.
 *
 * # Implementation
 * Infallible. Always returns a value.
 */
speed_t cfgetospeed(const struct termios *termios_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cfsetispeed.html>.
 *
 * Sets the input baud rate stored in the structure pointed to by `termios_p`
 * to `speed`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int cfsetispeed(struct termios *termios_p, speed_t speed);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/cfsetospeed.html>.
 *
 * Sets the output baud rate stored in the structure pointed to by `termios_p`
 * to `speed`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int cfsetospeed(struct termios *termios_p, speed_t speed);

/**
 * Non-POSIX, 4.4 BSD extension, see
 * <https://www.man7.org/linux/man-pages/man3/cfsetispeed.3.html>.
 *
 * Sets both the input and output baud rate stored in the structure pointed to
 * by `termios_p` to `speed`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 *
 * # Implementation
 * Calls `cfsetispeed()` and if successful then calls `cfsetospeed()`.
 */
int cfsetspeed(struct termios *termios_p, speed_t speed);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcflush.html>.
 *
 * Discards data written to the object referred to by `fildes` but not
 * transmitted, or data received but not read, depending on the value of
 * `queue_selector`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcflush(int fildes, int queue_selector);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcdrain.html>.
 *
 * Blocks until all output written to the object referred to by `fildes` is
 * transmitted.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcdrain(int fildes);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcsendbreak.html>.
 *
 * Causes transmission of a continuous stream of zero-valued bits for at least
 * 0.25 seconds, and not more than 0.5 seconds.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 *
 * # Implementation
 * This function does not accept a non-zero duration. `_duration` is ignored
 * and assumed to be `0`. Matches behaviour of musl.
 */
int tcsendbreak(int fildes, int _duration);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcgetwinsize.html>.
 *
 * Gets the terminal window size associated with the terminal referred to by
 * `fildes` and stores it in the winsize structure pointed to by `winsize_p`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcgetwinsize(int fildes, struct winsize *winsize_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcsetwinsize.html>.
 *
 * Sets the terminal window size associated with the terminal referred to by
 * `fildes` and stores it in the winsize structure pointed to by `winsize_p`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 *
 * # Safety
 * It is undefined behaviour if the value of the winsize structure pointed to
 * by `winsize_p` was not derived from the result of a call to
 * `tcsgetwinsize()` on `fildes`.
 */
int tcsetwinsize(int fildes, const struct winsize *winsize_p);

/**
 * See <https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcflow.html>.
 *
 * Suspends or restarts transmission or reception of data on the object
 * referred to by `fildes`, depending on the value of `action`.
 *
 * Upon success, returns `0`. Upon failure, returns `-1` and sets errno to
 * indicate the error.
 */
int tcflow(int fildes, int action);

/**
 * Non-POSIX, BSD extension, see <https://www.man7.org/linux/man-pages/man3/cfmakeraw.3.html>.
 *
 * Sets the terminal to something similar to "raw" mode: input is available
 * character by character, echoing is disabled, and all special processing of
 * terminal input and output characters is disabled.
 */
void cfmakeraw(struct termios *termios_p);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_TERMIOS_H */
