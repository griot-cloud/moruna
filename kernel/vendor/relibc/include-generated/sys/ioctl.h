#ifndef _RELIBC_SYS_IOCTL_H
#define _RELIBC_SYS_IOCTL_H

#include <bits/winsize.h> // for winsize from termios.h

// Shamelessly copy-pasted from musl

#define _IOC(a,b,c,d) ( ((a)<<30) | ((b)<<8) | (c) | ((d)<<16) )
#define _IOC_NONE  0U
#define _IOC_WRITE 1U
#define _IOC_READ  2U

#define _IO(a,b) _IOC(_IOC_NONE,(a),(b),0)
#define _IOW(a,b,c) _IOC(_IOC_WRITE,(a),(b),sizeof(c))
#define _IOR(a,b,c) _IOC(_IOC_READ,(a),(b),sizeof(c))
#define _IOWR(a,b,c) _IOC(_IOC_READ|_IOC_WRITE,(a),(b),sizeof(c))


/**
 * Equivalent to `tcgetattr(fd, argp)`.
 * Get the current serial port settings.
 */
#define TCGETS 21505

/**
 * Equivalent to `tcsetattr(fd, TCSANOW, argp)`.
 * Set the current serial port settings.
 */
#define TCSETS 21506

/**
 * Equivalent to `tcsetattr(fd, TCSADRAIN, argp)`.
 * Allow the output buffer to drain, and set the current serial port settings.
 */
#define TCSETSW 21507

/**
 * Equivalent to `tcsetattr(fd, TCSAFLUSH, argp)`.
 * Allow the output buffer to drain, discard pending input, and set the
 * current serial port settings.
 */
#define TCSETSF 21508

/**
 * Equivalent to `tcsendbreak(fd, arg)`.
 */
#define TCSBRK 21513

/**
 * Equivalent to `tcflow(fd, arg)`.
 */
#define TCXONC 21514

/**
 * Equivalent to `tcflush(fd, arg)`.
 */
#define TCFLSH 21515

/**
 * Make the given terminal the controlling terminal of the calling process.
 */
#define TIOCSCTTY 21518

/**
 * When successful, equivalent to `*argp = tcgetpgrp(fd)`.
 * Get the process group ID of the foreground process group on this terminal.
 */
#define TIOCGPGRP 21519

/**
 * Equivalent to `tcsetpgrp(fd, *argp)`.
 */
#define TIOCSPGRP 21520

/**
 * Get window size.
 */
#define TIOCGWINSZ 21523

/**
 * Set window size.
 */
#define TIOCSWINSZ 21524

/**
 * If the given terminal was the controlling terminal of the calling process,
 * give up this controlling terminal.
 */
#define TIOCNOTTY 21538

/**
 * When successful, equivalent to `*argp = tcgetsid(fd)`.
 * Get the session ID of the given terminal.
 */
#define TIOCGSID 21545

/**
 * Get the number of bytes in the input buffer.
 */
#define FIONREAD 21531

/**
 * Not recommended, should use POSIX `O_NONBLOCK` instead.
 */
#define FIONBIO 21537

/**
 * Set close-on-exec flag.
 */
#define FIOCLEX 21585

/**
 * Set (if `*lock` is nonzero) or remove (if `*lock` is zero) the lock on
 * the pseudoterminal slave device.
 */
#define TIOCSPTLCK 1074025521

/**
 * Place the current lock state of the pseudoterminal slave device in the
 * location pointed to by `lock`.
 */
#define TIOCGPTLCK 2147767353

/**
 * Obtain device unit number, which can be used to generate the filename of
 * the pseudo-terminal slave device.
 */
#define TIOCGPTN 2147767344

/**
 * POSIX `sockatmark()` from `sys/socket.h` is intended to replace this.
 */
#define SIOCATMARK 35077

#if (defined(__linux__) || defined(__moruna__))
#define TCGETA 21509
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETA 21510
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETAW 21511
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETAF 21512
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCEXCL 21516
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCNXCL 21517
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCOUTQ 21521
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSTI 21522
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCMGET 21525
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCMBIS 21526
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCMBIC 21527
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCMSET 21528
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGSOFTCAR 21529
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSSOFTCAR 21530
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCINQ FIONREAD
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCLINUX 21532
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCCONS 21533
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGSERIAL 21534
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSSERIAL 21535
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT 21536
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSETD 21539
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGETD 21540
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSBRKP 21541
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSBRK 21543
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCCBRK 21544
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGRS485 21550
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSRS485 21551
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGDEV 2147767346
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCGETX 21554
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETX 21555
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETXF 21556
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TCSETXW 21557
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSIG 1074025526
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCVHANGUP 21559
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGPKT 2147767352
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGEXCL 2147767360
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGPTPEER 21569
#endif

#if (defined(__linux__) || defined(__moruna__))
#define FIONCLEX 21584
#endif

#if (defined(__linux__) || defined(__moruna__))
#define FIOASYNC 21586
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERCONFIG 21587
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERGWILD 21588
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERSWILD 21589
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGLCKTRMIOS 21590
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSLCKTRMIOS 21591
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERGSTRUCT 21592
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERGETLSR 21593
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERGETMULTI 21594
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSERSETMULTI 21595
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCMIWAIT 21596
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCGICOUNT 21597
#endif

#if (defined(__linux__) || defined(__moruna__))
#define FIOQSIZE 21600
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_DATA 0
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_FLUSHREAD 1
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_FLUSHWRITE 2
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_STOP 4
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_START 8
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_NOSTOP 16
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_DOSTOP 32
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCPKT_IOCTL 64
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCSER_TEMT 1
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_LE 1
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_DTR 2
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_RTS 4
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_ST 8
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_SR 16
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_CTS 32
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_CAR 64
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_RNG 128
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_DSR 256
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_CD TIOCM_CAR
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_RI TIOCM_RNG
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_OUT1 8192
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_OUT2 16384
#endif

#if (defined(__linux__) || defined(__moruna__))
#define TIOCM_LOOP 32768
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_TTY 0
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_SLIP 1
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_MOUSE 2
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_PPP 3
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_STRIP 4
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_AX25 5
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_X25 6
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_6PACK 7
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_MASC 8
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_R3964 9
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_PROFIBUS_FDL 10
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_IRDA 11
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_SMSBLOCK 12
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_HDLC 13
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_SYNC_PPP 14
#endif

#if (defined(__linux__) || defined(__moruna__))
#define N_HCI 15
#endif

#if (defined(__linux__) || defined(__moruna__))
#define FIOSETOWN 35073
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSPGRP 35074
#endif

#if (defined(__linux__) || defined(__moruna__))
#define FIOGETOWN 35075
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGPGRP 35076
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGSTAMP 35078
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGSTAMPNS 35079
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCADDRT 35083
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDELRT 35084
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCRTMSG 35085
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFNAME 35088
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFLINK 35089
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFCONF 35090
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Get the active flag word of the device.
 */
#define SIOCGIFFLAGS 35091
#endif

#if (defined(__linux__) || defined(__moruna__))
/**
 * Set the active flag word of the device.
 */
#define SIOCSIFFLAGS 35092
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFADDR 35093
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFADDR 35094
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFDSTADDR 35095
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFDSTADDR 35096
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFBRDADDR 35097
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFBRDADDR 35098
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFNETMASK 35099
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFNETMASK 35100
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFMETRIC 35101
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFMETRIC 35102
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFMEM 35103
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFMEM 35104
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFMTU 35105
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFMTU 35106
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFNAME 35107
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFHWADDR 35108
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFENCAP 35109
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFENCAP 35110
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFHWADDR 35111
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFSLAVE 35113
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFSLAVE 35120
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCADDMULTI 35121
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDELMULTI 35122
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFINDEX 35123
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOGIFINDEX SIOCGIFINDEX
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFPFLAGS 35124
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFPFLAGS 35125
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDIFADDR 35126
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFHWBROADCAST 35127
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFCOUNT 35128
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFBR 35136
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFBR 35137
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFTXQLEN 35138
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFTXQLEN 35139
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDARP 35155
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGARP 35156
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSARP 35157
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDRARP 35168
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGRARP 35169
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSRARP 35170
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCGIFMAP 35184
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCSIFMAP 35185
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCADDDLCI 35200
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDELDLCI 35201
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCDEVPRIVATE 35312
#endif

#if (defined(__linux__) || defined(__moruna__))
#define SIOCPROTOPRIVATE 35296
#endif

struct sgttyb {
  char sg_ispeed;
  char sg_ospeed;
  char sg_erase;
  char sg_kill;
  unsigned short sg_flags;
};

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Non-POSIX, see <https://www.man7.org/linux/man-pages/man2/ioctl.2.html>.
 *
 * Manipulates the underlying device parameters of special files.
 *
 * Upon success, usually returns `0`. A few operations use the return value
 * as an output parameter and returns a nonnegative value on success. Upon
 * failure, `-1` is returned and errno set to indicate the error.
 */
int ioctl(int fd, unsigned long request, void *out);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* _RELIBC_SYS_IOCTL_H */
