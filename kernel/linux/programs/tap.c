/* KD2's tap (SDD 30 k): a task of the data plane, unmodified static musl, that passes its
   Arrow IPC stream through unchanged (what it reads on descriptor 0 it writes on 1) and, through
   its port, hands every byte it reads to the embedder on the host side of vsock, so the embedder
   holds exactly the rows the stage's sink is given and can judge each one.

   Its port is number 1 (the stage's one port, `tap`). Each port call is the kernel's native
   port_call, made with the `syscall` instruction: the request goes to the engine, which forwards
   it over the host protocol as a `port_call` and maps the embedder's `port_reply` into this
   process read only. Requests, each answered with its own first 16 bytes:
     "KD2T" u32 seq, u64 offset, then up to 60000 bytes of the stream from that offset;
     "KD2P" u32 seq, u64 0: a ping, PINGS of them, timed;
     "KD2S" u32 0, u64 total, then a line of figures: the last call.
   Exits 0 once its input has ended and every call was answered; 2 when a reply is not its
   request's head, 3 when a call fails. */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define PORT_CALL 0x10102UL
#define PORT 1
#define CHUNK 60000
#define PINGS 200
#define HEAD 16

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}

/* port_call(port, request, len) -> (reply address in rax, length in rdx); a negative rax is the
   kernel's error code. */
static long port_call(const void *req, size_t len, const unsigned char **reply, size_t *reply_len) {
    long rax;
    unsigned long rdx = len;
    __asm__ volatile("syscall"
                     : "=a"(rax), "+d"(rdx)
                     : "a"(PORT_CALL), "D"((unsigned long)PORT), "S"(req)
                     : "rcx", "r11", "memory");
    *reply = (const unsigned char *)rax;
    *reply_len = rdx;
    return rax;
}

static unsigned char request[HEAD + CHUNK];

static int call(const char *tag, uint32_t seq, uint64_t word, const void *data, size_t n) {
    memcpy(request, tag, 4);
    memcpy(request + 4, &seq, 4);
    memcpy(request + 8, &word, 8);
    if (n) memcpy(request + HEAD, data, n);
    const unsigned char *reply;
    size_t reply_len;
    long r = port_call(request, HEAD + n, &reply, &reply_len);
    if (r < 0) {
        fprintf(stderr, "tap: port_call %s %u failed with kernel error %ld\n", tag, seq, -r);
        exit(3);
    }
    if (reply_len != HEAD || memcmp(reply, request, HEAD) != 0) {
        fprintf(stderr, "tap: the reply to %s %u is %zu bytes, not its request's head\n", tag, seq,
                reply_len);
        exit(2);
    }
    return 0;
}

static int cmp(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

int main(void) {
    static unsigned char buf[CHUNK];
    uint64_t offset = 0, calls = 0;
    uint64_t t0 = now_ns();
    for (;;) {
        ssize_t n = read(0, buf, sizeof buf);
        if (n < 0) {
            perror("tap: read");
            return 1;
        }
        if (n == 0) break;
        for (ssize_t done = 0; done < n;) {
            ssize_t w = write(1, buf + done, (size_t)(n - done));
            if (w <= 0) {
                perror("tap: write");
                return 1;
            }
            done += w;
        }
        call("KD2T", (uint32_t)calls, offset, buf, (size_t)n);
        offset += (uint64_t)n;
        calls++;
    }
    uint64_t streamed_ns = now_ns() - t0;
    static uint64_t rtt[PINGS];
    for (uint32_t i = 0; i < PINGS; i++) {
        uint64_t s = now_ns();
        call("KD2P", i, 0, 0, 0);
        rtt[i] = now_ns() - s;
    }
    qsort(rtt, PINGS, sizeof rtt[0], cmp);
    char line[256];
    int len = snprintf(line, sizeof line,
                       "tap: %llu bytes in %llu calls in %llu ms; %d pings: least %llu us, median %llu us",
                       (unsigned long long)offset, (unsigned long long)calls,
                       (unsigned long long)(streamed_ns / 1000000), PINGS,
                       (unsigned long long)(rtt[0] / 1000),
                       (unsigned long long)(rtt[PINGS / 2] / 1000));
    call("KD2S", 0, offset, line, (size_t)len);
    fprintf(stderr, "%s\n", line);
    return 0;
}
