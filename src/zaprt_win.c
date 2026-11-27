/* zaprt_win — freestanding Windows port of the Zappr C runtime.
   Compiled with: clang --target=x86_64-w64-windows-gnu -ffreestanding
   -nostdlib -I src/winshim, linked by ld.lld -m i386pep against a
   generated kernel32 import lib. Console I/O goes through WriteFile/
   ReadFile; threads collapse to sequential execution (no CRT, no
   scheduler — parallel/spawn run inline in order). */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdarg.h>
#include <math.h>
#include <pthread.h>
#include <time.h>
#include <ctype.h>
#include <unistd.h>
#include <sys/wait.h>
#include <fcntl.h>

/* ---------- kernel32 imports ---------- */
__declspec(dllimport) void *GetStdHandle(unsigned long);
__declspec(dllimport) int WriteFile(void *, const void *, unsigned long, unsigned long *, void *);
__declspec(dllimport) int ReadFile(void *, void *, unsigned long, unsigned long *, void *);
__declspec(dllimport) unsigned long GetEnvironmentVariableA(const char *, char *, unsigned long);
__declspec(dllimport) void GetSystemTimeAsFileTime(void *);
__declspec(dllimport) void ExitProcess(unsigned int);
__declspec(dllimport) char *GetCommandLineA(void);
__declspec(dllimport) void *GetProcessHeap(void);
__declspec(dllimport) void *HeapAlloc(void *, unsigned long, size_t);
__declspec(dllimport) int HeapFree(void *, unsigned long, void *);

#define ZW_STDIN  ((void *)(unsigned long long)-10)
#define ZW_STDOUT ((void *)(unsigned long long)-11)
#define ZW_STDERR ((void *)(unsigned long long)-12)

/* ---------- malloc: bump allocator (arena semantics, never freed) ---------- */
static unsigned char *zheap;
static size_t zheap_used, zheap_cap = 512u * 1024u * 1024u;
#define ZHMASK 15u
static void zheap_init(void) {
  if (!zheap) zheap = (unsigned char *)HeapAlloc(GetProcessHeap(), 0, zheap_cap);
}
void *malloc(size_t n) {
  zheap_init();
  if (!zheap) return NULL;
  size_t total = (n + ZHMASK + 8u) & ~(size_t)ZHMASK;
  if (zheap_used + total > zheap_cap) return NULL;
  unsigned char *blk = zheap + zheap_used;
  *(size_t *)blk = total;
  zheap_used += total;
  return blk + 8;
}
void *calloc(size_t a, size_t b) {
  size_t n = a * b;
  void *p = malloc(n);
  if (p) memset(p, 0, n);
  return p;
}
void *realloc(void *p, size_t n) {
  if (!p) return malloc(n);
  size_t old = *(size_t *)((unsigned char *)p - 8);
  void *q = malloc(n);
  if (!q) return NULL;
  memcpy(q, p, old - 8 < n ? old - 8 : n);
  return q;
}
void free(void *p) { (void)p; }

/* ---------- stdout/stderr: line-buffered through JS ---------- */
struct ZWFile { char buf[65536]; size_t n; int iserr; };
static ZWFile z_stdout = {0}, z_stderr = { .iserr = 1 };
FILE *stdin = NULL, *stdout = &z_stdout, *stderr = &z_stderr;

static void zw_flush(ZWFile *f) {
  if (!f->n) return;
  void *h = GetStdHandle(f->iserr ? (unsigned long)-12 : (unsigned long)-11);
  unsigned long wrote = 0;
  WriteFile(h, f->buf, (unsigned long)f->n, &wrote, NULL);
  f->n = 0;
}
static void zw_putc(ZWFile *f, int c) {
  if (f->n >= sizeof(f->buf) - 1) zw_flush(f);
  f->buf[f->n++] = (char)c;
  if (c == '\n' || f->iserr) zw_flush(f);
}
static void zw_write(ZWFile *f, const char *s, size_t n) {
  for (size_t i = 0; i < n; i++) zw_putc(f, (unsigned char)s[i]);
}

int printf(const char *fmt, ...) {
  va_list ap; va_start(ap, fmt);
  char buf[8192];
  int n = vsnprintf(buf, sizeof buf, fmt, ap);
  va_end(ap);
  if (n > 0) zw_write(&z_stdout, buf, (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1);
  return n;
}
int fprintf(FILE *f, const char *fmt, ...) {
  va_list ap; va_start(ap, fmt);
  char buf[8192];
  int n = vsnprintf(buf, sizeof buf, fmt, ap);
  va_end(ap);
  if (n > 0) {
    ZWFile *z = (ZWFile *)f;
    zw_write(z ? z : &z_stdout, buf, (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1);
  }
  return n;
}
int putchar(int c) { zw_putc(&z_stdout, c); return c; }
int fputc(int c, FILE *f) { zw_putc((ZWFile *)f, c); return c; }
int fputs(const char *s, FILE *f) { zw_write((ZWFile *)f, s, strlen(s)); return 0; }
int fflush(FILE *f) { if (f) zw_flush((ZWFile *)f); else { zw_flush(&z_stdout); zw_flush(&z_stderr); } return 0; }
int feof(FILE *f) { (void)f; return 0; }
int fseek(FILE *f, long off, int whence) { (void)f; (void)off; (void)whence; return -1; }
long ftell(FILE *f) { (void)f; return -1; }

/* stdin: buffered ReadFile on the console/input handle */
static char z_stdin_buf[8192];
static size_t z_stdin_len, z_stdin_pos;
static int z_stdin_eof;

static int zw_fill(void) {
  if (z_stdin_eof) return 0;
  void *h = GetStdHandle((unsigned long)-10);
  unsigned long got = 0;
  if (!ReadFile(h, z_stdin_buf, sizeof z_stdin_buf, &got, NULL) || got == 0) {
    z_stdin_eof = 1; return 0;
  }
  z_stdin_len = got; z_stdin_pos = 0;
  return 1;
}
static int zw_getc(void) {
  if (z_stdin_pos >= z_stdin_len) { if (!zw_fill()) return -1; }
  return (unsigned char)z_stdin_buf[z_stdin_pos++];
}

char *fgets(char *s, int size, FILE *f) {
  (void)f;
  if (size <= 1) return NULL;
  int i = 0, c;
  while (i < size - 1 && (c = zw_getc()) >= 0) {
    s[i++] = (char)c;
    if (c == '\n') break;
  }
  if (i == 0) return NULL;
  s[i] = 0;
  return s;
}
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *f) {
  (void)f;
  char *d = ptr;
  size_t want = size * nmemb, got = 0;
  int c;
  while (got < want && (c = zw_getc()) >= 0) { d[got++] = (char)c; }
  return got / (size ? size : 1);
}
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *f) {
  (void)f;
  zw_write(&z_stdout, ptr, size * nmemb);
  return nmemb;
}
FILE *fopen(const char *path, const char *mode) {
  (void)path; (void)mode;
  return NULL; /* no filesystem on wasm — read(path) returns Err */
}
int fclose(FILE *f) { (void)f; return 0; }
FILE *popen(const char *cmd, const char *mode) { (void)cmd; (void)mode; return NULL; }
int pclose(FILE *f) { (void)f; return -1; }

/* ---------- vsnprintf core ---------- */
static void zw_rev(char *s, size_t n) { for (size_t i = 0; i < n / 2; i++) { char t = s[i]; s[i] = s[n-1-i]; s[n-1-i] = t; } }
static size_t zw_u64toa(uint64_t v, int base, char *out) {
  char tmp[32]; size_t n = 0;
  do { int d = v % base; tmp[n++] = d < 10 ? '0' + d : 'a' + d - 10; v /= base; } while (v);
  for (size_t i = 0; i < n; i++) out[i] = tmp[n - 1 - i];
  return n;
}
static size_t zw_i64toa(int64_t v, char *out) {
  if (v < 0) { out[0] = '-'; return 1 + zw_u64toa((uint64_t)(-(v + 1)) + 1, 10, out + 1); }
  return zw_u64toa((uint64_t)v, 10, out);
}

/* ---------- exact dtoa: binary64 -> exact decimal via bignum ----------
   v = mant * 2^e2 exactly. For e2 >= 0 the value is the integer mant<<e2;
   for e2 < 0 it is mant * 5^-e2 * 10^e2. We build that integer in a
   small bignum, print it, and hand the formatter an exact digit string
   plus a power-of-ten exponent — rounding is then exact, matching
   correctly-rounded printf on any platform. */

#define ZB_LIMBS 100
typedef struct { uint32_t d[ZB_LIMBS]; int n; } ZBig;

static void zb_zero(ZBig *b) { b->n = 1; b->d[0] = 0; }
static void zb_set_u64(ZBig *b, uint64_t v) {
  b->n = 0;
  while (v) { b->d[b->n++] = (uint32_t)(v & 0xffffffffu); v >>= 32; }
  if (!b->n) { b->n = 1; b->d[0] = 0; }
}
static void zb_shl(ZBig *b, int bits) {
  int limbs = bits / 32, rem = bits % 32;
  if (b->n == 1 && b->d[0] == 0) return;
  if (limbs) {
    for (int i = b->n - 1; i >= 0; i--) b->d[i + limbs] = b->d[i];
    for (int i = 0; i < limbs; i++) b->d[i] = 0;
    b->n += limbs;
  }
  if (rem) {
    uint32_t carry = 0;
    for (int i = limbs; i < b->n; i++) {
      uint32_t v = b->d[i];
      b->d[i] = (v << rem) | carry;
      carry = v >> (32 - rem);
    }
    if (carry) b->d[b->n++] = carry;
  }
}
static void zb_mul_small(ZBig *b, uint32_t m) {
  uint64_t carry = 0;
  for (int i = 0; i < b->n; i++) {
    uint64_t t = (uint64_t)b->d[i] * m + carry;
    b->d[i] = (uint32_t)t;
    carry = t >> 32;
  }
  while (carry) { b->d[b->n++] = (uint32_t)(carry & 0xffffffffu); carry >>= 32; }
}
/* divide by small, return remainder */
static uint32_t zb_div_small(ZBig *b, uint32_t div) {
  uint64_t rem = 0;
  for (int i = b->n - 1; i >= 0; i--) {
    uint64_t cur = (rem << 32) | b->d[i];
    b->d[i] = (uint32_t)(cur / div);
    rem = cur % div;
  }
  while (b->n > 1 && !b->d[b->n - 1]) b->n--;
  return (uint32_t)rem;
}
/* exact significant decimal digits of |v| (v finite, nonzero).
   out: digit chars, no leading zeros, no trailing zeros stripped —
   returns length; *x10 = power of ten such that v = int(digits) * 10^x10.
   digits buffer must hold ~800 chars (denormals need ~767). */
static size_t zw_exact_digits(double v, char *out, int *x10) {
  union { double d; uint64_t u; } cv;
  cv.d = v < 0 ? -v : v;
  uint64_t bits = cv.u;
  int be = (int)((bits >> 52) & 0x7ff);
  uint64_t frac = bits & ((1ull << 52) - 1);
  uint64_t mant;
  int e2;
  if (be == 0) { mant = frac; e2 = -1074; }
  else { mant = frac | (1ull << 52); e2 = be - 1075; }

  ZBig b;
  zb_set_u64(&b, mant);
  if (e2 >= 0) {
    zb_shl(&b, e2);
    *x10 = 0;
  } else {
    int k = -e2;
    /* multiply by 5^k: v = mant * 5^k * 10^-k */
    for (int i = 0; i < k; i++) zb_mul_small(&b, 5);
    *x10 = e2; /* = -k */
  }
  /* b now holds the exact integer; extract decimal digits */
  uint32_t groups[400]; int ng = 0;
  ZBig t = b;
  if (t.n == 1 && t.d[0] == 0) { groups[ng++] = 0; }
  else {
    while (!(t.n == 1 && t.d[0] == 0)) groups[ng++] = zb_div_small(&t, 1000000000u);
  }
  /* most significant group without padding, rest %09u */
  size_t o = 0;
  o += (size_t)snprintf(out + o, 32, "%u", groups[ng - 1]);
  for (int i = ng - 2; i >= 0; i--) o += (size_t)snprintf(out + o, 16, "%09u", groups[i]);
  out[o] = 0;
  /* strip trailing zeros into the exponent (they carry no information) */
  while (o > 1 && out[o - 1] == '0') { o--; (*x10)++; }
  out[o] = 0;
  return o;
}

/* Round the exact digit string to `keep` significant digits.
   digits/E are updated in place (E = scientific exponent, value =
   d[0].d[1..] * 10^E). Returns new length (<= keep, may be less after
   carry -> "1"). */
static size_t zw_round_sig(char *d, size_t L, int keep, int *E) {
  if ((int)L <= keep) return L;
  /* decide round up: first dropped digit, then sticky, then half-even */
  int up = 0;
  char first = d[keep];
  int sticky = 0;
  for (size_t i = keep + 1; i < L; i++) if (d[i] != '0') { sticky = 1; break; }
  if (first > '5') up = 1;
  else if (first == '5') up = sticky || ((d[keep - 1] - '0') & 1);
  if (!up) { d[keep] = 0; return keep; }
  /* increment the kept prefix */
  int i = keep - 1;
  while (i >= 0 && d[i] == '9') { d[i] = '0'; i--; }
  if (i < 0) {
    /* all nines: 999.. -> 1000.. => "1" + zeros, E++ */
    d[0] = '1';
    for (int j = 1; j < keep; j++) d[j] = '0';
    d[keep] = 0;
    (*E)++;
    return keep;
  }
  d[i]++;
  d[keep] = 0;
  return keep;
}

/* strip trailing zeros (for %g) */
static size_t zw_strip_zeros(char *d, size_t L) {
  while (L > 1 && d[L - 1] == '0') L--;
  d[L] = 0;
  return L;
}

/* render exact digits per %f/%e/%g. v finite nonzero handled by caller
   for sign; inf/nan handled here. */
static void zw_fmt_float(char *out, size_t cap, double v, char spec, int prec, int alt) {
  size_t o = 0;
  #define ZW_PUTC(c) do { if (o + 1 < cap) out[o++] = (char)(c); } while (0)
  #define ZW_PUTN(s, l) do { const char *_s = (s); size_t _l = (l); \
    for (size_t _i = 0; _i < _l && o + 1 < cap; _i++) out[o++] = _s[_i]; } while (0)
  #define ZW_PADCH(c, n) do { int _n = (n); while (_n-- > 0) ZW_PUTC(c); } while (0)
  if (v != v) { ZW_PUTN(spec == 'F' || spec == 'E' || spec == 'G' ? "NAN" : "nan", 3); out[o] = 0; return; }
  if (v == 1.0/0.0 || v == -1.0/0.0) {
    int inf = v < 0;
    if (inf) ZW_PUTC('-');
    ZW_PUTN((spec == 'F' || spec == 'E' || spec == 'G') ? "INF" : "inf", 3);
    out[o] = 0; return;
  }
  if (v == 0) {
    int negzero = signbit(v);
    if (negzero) ZW_PUTC('-');
    if (spec == 'f' || spec == 'F') {
      int p = prec < 0 ? 6 : prec;
      ZW_PUTC('0');
      if (p > 0 || alt) { ZW_PUTC('.'); ZW_PADCH('0', p); }
    } else if (spec == 'e' || spec == 'E') {
      int p = prec < 0 ? 6 : prec;
      ZW_PUTC('0');
      if (p > 0 || alt) { ZW_PUTC('.'); ZW_PADCH('0', p); }
      ZW_PUTC(spec == 'E' ? 'E' : 'e'); ZW_PUTN("+00", 3);
    } else {
      ZW_PUTC('0');
    }
    out[o] = 0; return;
  }
  int neg = v < 0;
  if (neg) { ZW_PUTC('-'); v = -v; }

  char digs[840]; int x10;
  size_t L = zw_exact_digits(v, digs, &x10);
  int E = (int)L - 1 + x10; /* scientific exponent: v = d[0].d[1..] * 10^E */

  if (spec == 'f' || spec == 'F') {
    int fr = prec < 0 ? 6 : prec;
    int point = E + 1;           /* digits before decimal point */
    int keep = point + fr;       /* significant digits to keep */
    if (keep <= 0) {
      /* rounds to zero, or to 1 unit in the last place when keep == 0 */
      ZW_PUTC('0');
      if (fr > 0 || alt) {
        ZW_PUTC('.');
        int round_up = 0;
        if (keep == 0) {
          /* first digit sits at place -fr-1: value/ulp = d[0]/10-ish */
          int sticky = 0;
          for (size_t i = 1; i < L; i++) if (digs[i] != '0') { sticky = 1; break; }
          int d0 = digs[0] - '0';
          if (d0 > 5) round_up = 1;
          else if (d0 == 5) round_up = sticky; /* kept part is 0 = even */
        }
        if (round_up) {
          ZW_PADCH('0', fr - 1); ZW_PUTC('1');
        } else {
          ZW_PADCH('0', fr);
        }
      }
      out[o] = 0; return;
    }
    zw_round_sig(digs, L, keep, &E);
    L = keep < (int)L ? keep : (int)L;
    if ((int)L < keep) { /* pad with zeros */
      while ((int)L < keep && L < sizeof(digs) - 1) digs[L++] = '0';
      digs[L] = 0;
    }
    /* point may have moved if rounding overflowed (handled via E bump) */
    point = E + 1;
    if (point <= 0) {
      ZW_PUTN("0.", 2);
      ZW_PADCH('0', -point);
      ZW_PUTN(digs, L);
      /* trailing frac already exactly fr digits by construction */
    } else if (point >= (int)L) {
      ZW_PUTN(digs, L);
      ZW_PADCH('0', point - (int)L);
      if (fr > 0 || alt) { ZW_PUTC('.'); ZW_PADCH('0', fr); }
    } else {
      ZW_PUTN(digs, point);
      ZW_PUTC('.');
      ZW_PUTN(digs + point, L - point);
    }
  } else {
    /* e/E/g/G: work in significant digits */
    int p = prec < 0 ? 6 : prec;
    int sci, sig;
    if (spec == 'g' || spec == 'G') {
      if (p == 0) p = 1;
      sig = p;
      /* decide style after rounding to sig digits (exponent may bump) */
      zw_round_sig(digs, L, sig, &E);
      L = sig;
      sci = (E < -4 || E >= p);
    } else {
      sig = p + 1;
      zw_round_sig(digs, L, sig, &E);
      L = sig;
      sci = 1;
    }
    if (!sci) {
      /* %g fixed style: precision = sig-1-E frac digits */
      int fr = sig - 1 - E;
      if (fr < 0) fr = 0;
      int point = E + 1;
      if (point <= 0) {
        ZW_PUTN("0.", 2);
        ZW_PADCH('0', -point);
        ZW_PUTN(digs, L);
      } else if (point >= (int)L) {
        ZW_PUTN(digs, L);
        ZW_PADCH('0', point - (int)L);
        if (fr > 0) { ZW_PUTC('.'); ZW_PADCH('0', fr); }
      } else {
        ZW_PUTN(digs, point);
        ZW_PUTC('.');
        ZW_PUTN(digs + point, L - point);
        ZW_PADCH('0', fr - ((int)L - point));
      }
      if (!alt) {
        while (o > 0 && out[o-1] == '0') o--;
        if (o > 0 && out[o-1] == '.') o--;
      }
    } else {
      /* scientific: d[0] [. d[1..sig-1]] e±XX */
      ZW_PUTC(digs[0]);
      int frac = sig - 1;
      if (frac > 0 || alt) {
        ZW_PUTC('.');
        ZW_PUTN(digs + 1, frac);
        if ((spec == 'g' || spec == 'G') && !alt) {
          while (o > 0 && out[o-1] == '0') o--;
          if (o > 0 && out[o-1] == '.') o--;
        }
      }
      ZW_PUTC((spec == 'E' || spec == 'G') ? 'E' : 'e');
      int ex = E;
      if (ex < 0) { ZW_PUTC('-'); ex = -ex; } else ZW_PUTC('+');
      char eb[12]; int en = 0;
      do { eb[en++] = (char)('0' + ex % 10); ex /= 10; } while (ex);
      if (en < 2) ZW_PADCH('0', 2 - en);
      while (en) ZW_PUTC(eb[--en]);
    }
  }
  out[o] = 0;
  #undef ZW_PUTC
  #undef ZW_PUTN
  #undef ZW_PADCH
}
static int zw_vformat(char *out, size_t cap, const char *fmt, va_list ap) {
  size_t o = 0;
  #define OUTC(c) do { if (o + 1 < cap) out[o++] = (char)(c); } while (0)
  #define OUTS(s) do { const char *_s = (s); while (*_s) OUTC(*_s++); } while (0)
  while (*fmt && o + 1 < cap) {
    if (*fmt != '%') { OUTC(*fmt++); continue; }
    fmt++;
    if (*fmt == '%') { OUTC('%'); fmt++; continue; }
    int left = 0, zero = 0, plus = 0, space = 0, alt = 0;
    while (*fmt == '-' || *fmt == '0' || *fmt == '+' || *fmt == ' ' || *fmt == '#') {
      if (*fmt == '-') left = 1;
      else if (*fmt == '0') zero = 1;
      else if (*fmt == '+') plus = 1;
      else if (*fmt == ' ') space = 1;
      else alt = 1;
      fmt++;
    }
    int width = 0;
    if (*fmt == '*') { width = va_arg(ap, int); if (width < 0) { /* left-justify */ } fmt++; }
    while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
    int prec = -1;
    if (*fmt == '.') {
      fmt++;
      if (*fmt == '*') { prec = va_arg(ap, int); fmt++; }
      else { prec = 0; while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0'); }
    }
    int lng = 0;
    while (*fmt == 'l' || *fmt == 'h' || *fmt == 'z' || *fmt == 't') { if (*fmt == 'l') lng++; fmt++; }
    char conv = *fmt++;
    char tmp[128]; size_t tn = 0;
    switch (conv) {
      case 'd': case 'i': {
        int64_t v = lng >= 2 ? (int64_t)va_arg(ap, long long) : (int64_t)va_arg(ap, int);
        tn = zw_i64toa(v, tmp);
        break;
      }
      case 'u': case 'x': case 'X': case 'o': {
        uint64_t v = lng >= 2 ? (uint64_t)va_arg(ap, unsigned long long) : (uint64_t)va_arg(ap, unsigned int);
        int base = conv == 'x' || conv == 'X' ? 16 : conv == 'o' ? 8 : 10;
        tn = zw_u64toa(v, base, tmp);
        if (conv == 'X') for (size_t i = 0; i < tn; i++) if (tmp[i] >= 'a') tmp[i] -= 32;
        if (alt && conv == 'x') { memmove(tmp + 2, tmp, tn); tmp[0] = '0'; tmp[1] = 'x'; tn += 2; }
        if (alt && conv == 'X') { memmove(tmp + 2, tmp, tn); tmp[0] = '0'; tmp[1] = 'X'; tn += 2; }
        if (alt && conv == 'o' && tn && tmp[0] != '0') { memmove(tmp + 1, tmp, tn); tmp[0] = '0'; tn += 1; }
        break;
      }
      case 'c': tmp[0] = (char)va_arg(ap, int); tn = 1; break;
      case 's': {
        const char *s = va_arg(ap, const char *);
        if (!s) s = "(null)";
        if (prec >= 0) { tn = strlen(s); if (tn > (size_t)prec) tn = prec; }
        else tn = strlen(s);
        /* pad below */
        int pad = width - (int)tn;
        if (!left) while (pad-- > 0) OUTC(' ');
        for (size_t i = 0; i < tn; i++) OUTC(s[i]);
        if (left) while (pad-- > 0) OUTC(' ');
        goto next;
      }
      case 'p': {
        void *p = va_arg(ap, void *);
        tn = snprintf(tmp, sizeof tmp, "0x%x", (unsigned)(uintptr_t)p);
        break;
      }
      case 'f': case 'F': case 'e': case 'E': case 'g': case 'G': case 'a': case 'A': {
        double v = va_arg(ap, double);
        zw_fmt_float(tmp, sizeof tmp, v, conv, prec, alt);
        tn = strlen(tmp);
        int pad = width - (int)tn;
        int neg = tn && tmp[0] == '-';
        char signc = neg ? '-' : plus ? '+' : space ? ' ' : 0;
        int body = (int)tn + (signc ? 1 : 0);
        int padn = width - body; if (padn < 0) padn = 0;
        if (!left) { if (zero && prec < 0) { if (signc) OUTC(signc); while (padn-- > 0) OUTC('0'); } else while (padn-- > 0) OUTC(' '); }
        if (signc) OUTC(signc);
        for (size_t i = neg ? 1 : 0; i < tn; i++) OUTC(tmp[i]);
        if (left) while (padn-- > 0) OUTC(' ');
        goto next;
      }
      default:
        OUTC('%'); OUTC(conv); goto next;
    }
    /* integer path padding */
    {
      int neg = tn && tmp[0] == '-';
      char signc = neg ? '-' : plus ? '+' : space ? ' ' : 0;
      int digits = (int)tn - (neg ? 1 : 0);
      int padn = width - digits - (signc ? 1 : 0); if (padn < 0) padn = 0;
      if (!left) {
        if (zero && prec < 0) { if (signc) OUTC(signc); while (padn-- > 0) OUTC('0'); }
        else while (padn-- > 0) OUTC(' ');
      }
      if (signc) OUTC(signc);
      if (prec >= 0 && prec > digits) { int z = prec - digits; while (z-- > 0) OUTC('0'); }
      for (size_t i = neg ? 1 : 0; i < tn; i++) OUTC(tmp[i]);
      if (left) while (padn-- > 0) OUTC(' ');
    }
    next:;
  }
  out[o] = 0;
  return (int)o;
  #undef OUTC
  #undef OUTS
}
int vsnprintf(char *s, size_t n, const char *fmt, va_list ap) {
  return zw_vformat(s, n, fmt, ap);
}
int snprintf(char *s, size_t n, const char *fmt, ...) {
  va_list ap; va_start(ap, fmt);
  int r = zw_vformat(s, n, fmt, ap);
  va_end(ap);
  return r;
}
int sprintf(char *s, const char *fmt, ...) {
  va_list ap; va_start(ap, fmt);
  int r = zw_vformat(s, 1u << 30, fmt, ap);
  va_end(ap);
  return r;
}

/* ---------- string/memory ---------- */
void *memcpy(void *d, const void *s, size_t n) { unsigned char *dd = d; const unsigned char *ss = s; for (size_t i = 0; i < n; i++) dd[i] = ss[i]; return d; }
void *memmove(void *d, const void *s, size_t n) { unsigned char *dd = d; const unsigned char *ss = s; if (dd < ss) { for (size_t i = 0; i < n; i++) dd[i] = ss[i]; } else { for (size_t i = n; i-- > 0;) dd[i] = ss[i]; } return d; }
void *memset(void *d, int c, size_t n) { unsigned char *dd = d; for (size_t i = 0; i < n; i++) dd[i] = (unsigned char)c; return d; }
int memcmp(const void *a, const void *b, size_t n) { const unsigned char *x = a, *y = b; for (size_t i = 0; i < n; i++) { if (x[i] != y[i]) return x[i] - y[i]; } return 0; }
void *memchr(const void *s, int c, size_t n) { const unsigned char *p = s; for (size_t i = 0; i < n; i++) if (p[i] == (unsigned char)c) return (void *)(p + i); return NULL; }
size_t strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }
int strcmp(const char *a, const char *b) { while (*a && *a == *b) { a++; b++; } return (unsigned char)*a - (unsigned char)*b; }
int strncmp(const char *a, const char *b, size_t n) { for (size_t i = 0; i < n; i++) { if (!a[i] || a[i] != b[i]) return (unsigned char)a[i] - (unsigned char)b[i]; } return 0; }
int strcasecmp(const char *a, const char *b) { while (*a && *b) { int d = tolower((unsigned char)*a) - tolower((unsigned char)*b); if (d) return d; a++; b++; } return tolower((unsigned char)*a) - tolower((unsigned char)*b); }
int strncasecmp(const char *a, const char *b, size_t n) { for (size_t i = 0; i < n && a[i] && b[i]; i++) { int d = tolower((unsigned char)a[i]) - tolower((unsigned char)b[i]); if (d) return d; } return 0; }
char *strcpy(char *d, const char *s) { char *r = d; while ((*d++ = *s++)); return r; }
char *strncpy(char *d, const char *s, size_t n) { size_t i = 0; for (; i < n && s[i]; i++) d[i] = s[i]; for (; i < n; i++) d[i] = 0; return d; }
char *strcat(char *d, const char *s) { char *r = d + strlen(d); while ((*r++ = *s++)); return d; }
char *strchr(const char *s, int c) { for (; *s; s++) if (*s == c) return (char *)s; return c == 0 ? (char *)s : NULL; }
char *strrchr(const char *s, int c) { const char *last = NULL; for (; *s; s++) if (*s == c) last = s; if (c == 0) return (char *)s; return (char *)last; }
char *strstr(const char *h, const char *n) { size_t nl = strlen(n); if (!nl) return (char *)h; for (; *h; h++) { size_t i = 0; while (i < nl && h[i] == n[i]) i++; if (i == nl) return (char *)h; } return NULL; }
char *strdup(const char *s) { size_t n = strlen(s) + 1; char *p = malloc(n); if (p) memcpy(p, s, n); return p; }
char *strndup(const char *s, size_t n) { char *p = malloc(n + 1); if (p) { memcpy(p, s, n); p[n] = 0; } return p; }
char *strtok(char *s, const char *delim) { static char *save; return strtok_r(s, delim, &save); }
char *strtok_r(char *s, const char *delim, char **save) {
  char *p = s ? s : *save;
  if (!p) return NULL;
  while (*p && strchr(delim, *p)) p++;
  if (!*p) { *save = p; return NULL; }
  char *start = p;
  while (*p && !strchr(delim, *p)) p++;
  if (*p) { *p++ = 0; }
  *save = p;
  return start;
}
size_t strspn(const char *s, const char *accept) { size_t n = 0; while (s[n] && strchr(accept, s[n])) n++; return n; }
size_t strcspn(const char *s, const char *reject) { size_t n = 0; while (s[n] && !strchr(reject, s[n])) n++; return n; }
char *strpbrk(const char *s, const char *accept) { for (; *s; s++) if (strchr(accept, *s)) return (char *)s; return NULL; }
int strcoll(const char *a, const char *b) { return strcmp(a, b); }
char *strerror(int e) { (void)e; return "error"; }

/* ---------- ctype ---------- */
int isspace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f'; }
int isdigit(int c) { return c >= '0' && c <= '9'; }
int isalpha(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int iscntrl(int c) { return c >= 0 && c < 32; }
int isprint(int c) { return c >= 32 && c < 127; }
int isgraph(int c) { return c > 32 && c < 127; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int isblank(int c) { return c == ' ' || c == '\t'; }
int toupper(int c) { return (c >= 'a' && c <= 'z') ? c - 32 : c; }
int tolower(int c) { return (c >= 'A' && c <= 'Z') ? c + 32 : c; }

/* ---------- stdlib extras ---------- */
int atoi(const char *s) { return (int)strtol(s, NULL, 10); }
long atol(const char *s) { return strtol(s, NULL, 10); }
long long atoll(const char *s) { return strtoll(s, NULL, 10); }
long strtol(const char *s, char **end, int base) { return (long)strtoll(s, end, base); }
long long strtoll(const char *s, char **end, int base) {
  while (isspace((unsigned char)*s)) s++;
  int neg = 0;
  if (*s == '-') { neg = 1; s++; } else if (*s == '+') s++;
  if (base == 0) base = 10;
  unsigned long long v = 0;
  if (base == 16 && s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) s += 2;
  for (;; s++) {
    int d;
    if (*s >= '0' && *s <= '9') d = *s - '0';
    else if (*s >= 'a' && *s <= 'z') d = *s - 'a' + 10;
    else if (*s >= 'A' && *s <= 'Z') d = *s - 'A' + 10;
    else break;
    if (d >= base) break;
    v = v * base + d;
  }
  if (end) *end = (char *)s;
  return neg ? -(long long)v : (long long)v;
}
unsigned long strtoul(const char *s, char **end, int base) { return (unsigned long)strtoll(s, end, base); }
double strtod(const char *s, char **end) {
  while (isspace((unsigned char)*s)) s++;
  int neg = 0;
  if (*s == '-') { neg = 1; s++; } else if (*s == '+') s++;
  /* collect up to 19 significant digits into an integer, track exponent */
  unsigned long long mant = 0;
  int ndig = 0, exp10 = 0, any = 0;
  while (isdigit((unsigned char)*s)) {
    if (ndig < 19) { mant = mant * 10 + (*s - '0'); ndig++; }
    else exp10++;
    any = 1; s++;
  }
  if (*s == '.') {
    s++;
    while (isdigit((unsigned char)*s)) {
      if (ndig < 19) { mant = mant * 10 + (*s - '0'); ndig++; exp10--; }
      any = 1; s++;
    }
  }
  if (!any) { if (end) *end = (char *)s; return 0; }
  if (*s == 'e' || *s == 'E') {
    s++; int eneg = 0;
    if (*s == '-') { eneg = 1; s++; } else if (*s == '+') s++;
    int e = 0; while (isdigit((unsigned char)*s)) e = e * 10 + (*s++ - '0');
    exp10 += eneg ? -e : e;
  }
  /* scale by powers of ten; 10^22 is the largest exactly-representable
     power, so chunk by 22 — each step is a single correctly-rounded op */
  static const double pow10[] = {
    1e0,1e1,1e2,1e3,1e4,1e5,1e6,1e7,1e8,1e9,1e10,1e11,1e12,1e13,1e14,1e15,
    1e16,1e17,1e18,1e19,1e20,1e21,1e22};
  double v = (double)mant;
  int e = exp10;
  while (e >= 22) { v *= 1e22; e -= 22; }
  while (e <= -22) { v /= 1e22; e += 22; }
  if (e > 0) v *= pow10[e];
  else if (e < 0) v /= pow10[-e];
  if (end) *end = (char *)s;
  return neg ? -v : v;
}
float strtof(const char *s, char **end) { return (float)strtod(s, end); }
int abs(int x) { return x < 0 ? -x : x; }
long labs(long x) { return x < 0 ? -x : x; }
long long llabs(long long x) { return x < 0 ? -x : x; }
void exit(int code) { fflush(NULL); ExitProcess((unsigned int)code); }
void _exit(int code) { ExitProcess((unsigned int)code); }
void abort(void) { ExitProcess(134); }
int system(const char *cmd) { (void)cmd; return -1; }
char *getenv(const char *name) {
  static char z_envvals[8][512];
  static int z_envslot;
  char *out = z_envvals[z_envslot = (z_envslot + 1) & 7];
  unsigned long n = GetEnvironmentVariableA(name, out, 512);
  if (n == 0) return NULL;
  if (n >= 512) n = 511;
  out[n] = 0;
  return out;
}
int setenv(const char *name, const char *val, int ov) { (void)name; (void)val; (void)ov; return 0; }
int unsetenv(const char *name) { (void)name; return 0; }
char **environ = NULL;

/* ---------- math ---------- */
double floor(double x) { long long i = (long long)x; return (double)i > x ? (double)(i - 1) : (double)i; }
double ceil(double x) { long long i = (long long)x; return (double)i < x ? (double)(i + 1) : (double)i; }
double fabs(double x) { return x < 0 ? -x : x; }
double fmod(double a, double b) { return a - (double)(long long)(a / b) * b; }
int isfinite(double x) { return x == x && x != 1.0/0.0 && x != -1.0/0.0; }
int signbit(double x) { union { double d; uint64_t u; } c; c.d = x; return (int)(c.u >> 63); }

/* ---------- pthread: sequential collapse ---------- */
int pthread_create(pthread_t *th, const pthread_attr_t *attr, pthread_start_t fn, void *arg) {
  (void)th; (void)attr;
  fn(arg); /* run inline — single-threaded wasm */
  return 0;
}
int pthread_join(pthread_t th, void **ret) { (void)th; (void)ret; return 0; }
int pthread_detach(pthread_t th) { (void)th; return 0; }
int pthread_mutex_init(pthread_mutex_t *m, const pthread_mutex_t *a) { (void)m; (void)a; return 0; }
int pthread_mutex_lock(pthread_mutex_t *m) { (void)m; return 0; }
int pthread_mutex_unlock(pthread_mutex_t *m) { (void)m; return 0; }
int pthread_mutex_destroy(pthread_mutex_t *m) { (void)m; return 0; }
int pthread_cond_init(pthread_cond_t *c, const pthread_cond_t *a) { (void)c; (void)a; return 0; }
int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m) { (void)c; (void)m; return 0; }
int pthread_cond_signal(pthread_cond_t *c) { (void)c; return 0; }
int pthread_cond_broadcast(pthread_cond_t *c) { (void)c; return 0; }
int pthread_cond_destroy(pthread_cond_t *c) { (void)c; return 0; }
pthread_t pthread_self(void) { return 1; }
void pthread_exit(void *r) { (void)r; }
int pthread_attr_init(pthread_attr_t *a) { (void)a; return 0; }
int pthread_attr_destroy(pthread_attr_t *a) { (void)a; return 0; }

/* ---------- unistd/process: stubs ---------- */
ssize_t read(int fd, void *buf, size_t n) { (void)fd; (void)buf; (void)n; return -1; }
ssize_t write(int fd, const void *buf, size_t n) {
  if (fd == 1 || fd == 2) {
    void *h = GetStdHandle(fd == 2 ? (unsigned long)-12 : (unsigned long)-11);
    unsigned long wrote = 0;
    WriteFile(h, buf, (unsigned long)n, &wrote, NULL);
    return (ssize_t)wrote;
  }
  return -1;
}
int close(int fd) { (void)fd; return 0; }
int pipe(int fds[2]) { (void)fds; return -1; }
int dup2(int a, int b) { (void)a; (void)b; return -1; }
pid_t fork(void) { return -1; }
int execvp(const char *f, char *const *a) { (void)f; (void)a; return -1; }
int waitpid(pid_t p, int *st, int o) { (void)p; (void)st; (void)o; return -1; }
unsigned int sleep(unsigned int s) { (void)s; return 0; }
int usleep(unsigned int u) { (void)u; return 0; }

/* ---------- time ---------- */
static double z_now(void) {
  unsigned long long ft;
  GetSystemTimeAsFileTime((void *)&ft);
  return (double)(ft - 116444736000000000ull) / 1e7;
}
time_t time(time_t *t) { time_t v = (time_t)z_now(); if (t) *t = v; return v; }
int clock_gettime(int clk, timespec *ts) {
  (void)clk;
  double t = z_now();
  ts->tv_sec = (time_t)t;
  ts->tv_nsec = (long)((t - (double)ts->tv_sec) * 1e9);
  return 0;
}
/* days-from-civil inverse: civil-from-days (Howard Hinnant) */
static void z_civil_from_days(long z, int *y, int *m, int *d) {
  z += 719468;
  long era = (z >= 0 ? z : z - 146096) / 146097;
  unsigned doe = (unsigned)(z - era * 146097);
  unsigned yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365;
  long yy = (long)yoe + era * 400;
  unsigned doy = doe - (365*yoe + yoe/4 - yoe/100);
  unsigned mp = (5*doy + 2)/153;
  *d = (int)(doy - (153*mp+2)/5 + 1);
  *m = (int)(mp < 10 ? mp+3 : mp-9);
  *y = (int)(yy + (*m <= 2));
}
struct tm *gmtime_r(const time_t *t, struct tm *out) {
  long days = *t / 86400;
  long rem = *t % 86400;
  if (rem < 0) { rem += 86400; days--; }
  z_civil_from_days(days, &out->tm_year, &out->tm_mon, &out->tm_mday);
  out->tm_year -= 1900; out->tm_mon -= 1;
  out->tm_hour = (int)(rem / 3600); rem %= 3600;
  out->tm_min = (int)(rem / 60); out->tm_sec = (int)(rem % 60);
  out->tm_wday = (int)((days + 4) % 7); if (out->tm_wday < 0) out->tm_wday += 7;
  out->tm_yday = 0; out->tm_isdst = 0;
  return out;
}
struct tm *localtime_r(const time_t *t, struct tm *out) { return gmtime_r(t, out); }
size_t strftime(char *s, size_t n, const char *fmt, const struct tm *tm) {
  size_t o = 0;
  for (const char *f = fmt; *f && o + 1 < n; f++) {
    if (*f != '%') { s[o++] = *f; continue; }
    f++;
    char tmp[16]; int tn = 0;
    switch (*f) {
      case 'Y': tn = snprintf(tmp, sizeof tmp, "%04d", tm->tm_year + 1900); break;
      case 'm': tn = snprintf(tmp, sizeof tmp, "%02d", tm->tm_mon + 1); break;
      case 'd': tn = snprintf(tmp, sizeof tmp, "%02d", tm->tm_mday); break;
      case 'H': tn = snprintf(tmp, sizeof tmp, "%02d", tm->tm_hour); break;
      case 'M': tn = snprintf(tmp, sizeof tmp, "%02d", tm->tm_min); break;
      case 'S': tn = snprintf(tmp, sizeof tmp, "%02d", tm->tm_sec); break;
      case 'j': tn = snprintf(tmp, sizeof tmp, "%03d", tm->tm_yday + 1); break;
      case 's': tn = snprintf(tmp, sizeof tmp, "%lld", (long long)time(NULL)); break;
      case '%': tmp[0] = '%'; tn = 1; break;
      default: tmp[0] = *f; tn = 1; break;
    }
    for (int i = 0; i < tn && o + 1 < n; i++) s[o++] = tmp[i];
  }
  s[o] = 0;
  return o;
}
int nanosleep(const timespec *req, timespec *rem) { (void)req; if (rem) *rem = *req; return 0; }
double difftime(time_t a, time_t b) { return (double)(a - b); }

/* ---------- process entry ---------- */
extern int main(int argc, char **argv);

static void z_win_argv(int *argc, char ***argv) {
  char *cmd = GetCommandLineA();
  static char *args[1024];
  int n = 0;
  if (cmd) {
    while (*cmd == ' ' || *cmd == '\t') cmd++;
    /* keep argv[0] — z_argv_set skips it like every platform */
    while (*cmd) {
      while (*cmd == ' ' || *cmd == '\t') cmd++;
      if (!*cmd) break;
      if (*cmd == '"') {
        cmd++;
        args[n++] = cmd;
        while (*cmd && *cmd != '"') cmd++;
        if (*cmd) *cmd++ = 0;
      } else {
        args[n++] = cmd;
        while (*cmd && *cmd != ' ' && *cmd != '\t') cmd++;
        if (*cmd) *cmd++ = 0;
      }
      if (n >= 1000) break;
    }
  }
  *argc = n; *argv = args;
}

int mainCRTStartup(void) {
  int argc = 0; char **argv = NULL;
  z_win_argv(&argc, &argv);
  int rc = main(argc, argv);
  ExitProcess((unsigned)rc);
  return rc;
}

/* mingw-isms clang emits even freestanding */
void __main(void) {}
void ___chkstk_ms(void) {}
void ___chkstk(void) {}
