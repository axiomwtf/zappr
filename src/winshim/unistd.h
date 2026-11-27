#ifndef ZW_WIN_UNISTD_H
#define ZW_WIN_UNISTD_H
#include <stddef.h>
typedef long ssize_t;
typedef int pid_t;
ssize_t read(int, void *, size_t);
ssize_t write(int, const void *, size_t);
int close(int);
int pipe(int *);
int dup2(int, int);
pid_t fork(void);
int execvp(const char *, char *const *);
int waitpid(pid_t, int *, int);
unsigned int sleep(unsigned int);
int usleep(unsigned int);
void _exit(int);
extern char **environ;
#define STDIN_FILENO 0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2
#endif
