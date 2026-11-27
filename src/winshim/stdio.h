#ifndef ZW_WIN_STDIO_H
#define ZW_WIN_STDIO_H
#include <stdarg.h>
#include <stddef.h>
typedef struct ZWFile ZWFile;
typedef ZWFile FILE;
extern FILE *stdin, *stdout, *stderr;
int printf(const char *, ...);
int fprintf(FILE *, const char *, ...);
int snprintf(char *, size_t, const char *, ...);
int vsnprintf(char *, size_t, const char *, va_list);
int putchar(int);
int fputc(int, FILE *);
int fputs(const char *, FILE *);
int fflush(FILE *);
char *fgets(char *, int, FILE *);
size_t fread(void *, size_t, size_t, FILE *);
size_t fwrite(const void *, size_t, size_t, FILE *);
FILE *fopen(const char *, const char *);
int fclose(FILE *);
FILE *popen(const char *, const char *);
int pclose(FILE *);
int fseek(FILE *, long, int);
long ftell(FILE *);
int feof(FILE *);
#define EOF (-1)
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#endif
