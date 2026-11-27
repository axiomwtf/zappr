#ifndef ZW_SYS_WAIT_H
#define ZW_SYS_WAIT_H
#ifndef _ZW_PID_T
#define _ZW_PID_T
typedef int pid_t;
#endif
#define WIFEXITED(s) (((s) & 0x7f) == 0)
#define WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define WIFSIGNALED(s) (((s) & 0x7f) != 0 && ((s) & 0x7f) != 0x7f)
#endif
