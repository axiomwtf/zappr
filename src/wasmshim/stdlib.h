#ifndef ZW_STDLIB_H
#define ZW_STDLIB_H
#include <stddef.h>
void *malloc(size_t);
void *calloc(size_t, size_t);
void *realloc(void *, size_t);
void free(void *);
int atoi(const char *);
long atol(const char *);
long long atoll(const char *);
long strtol(const char *, char **, int);
long long strtoll(const char *, char **, int);
unsigned long strtoul(const char *, char **, int);
double strtod(const char *, char **);
float strtof(const char *, char **);
int abs(int);
long labs(long);
long long llabs(long long);
void exit(int);
void _exit(int);
void abort(void);
int system(const char *);
char *getenv(const char *);
int setenv(const char *, const char *, int);
int unsetenv(const char *);
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
#endif
