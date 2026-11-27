#ifndef ZW_TIME_H
#define ZW_TIME_H
#include <stddef.h>
typedef long time_t;
typedef long suseconds_t;
typedef struct timespec { time_t tv_sec; long tv_nsec; } timespec;
struct tm { int tm_sec, tm_min, tm_hour, tm_mday, tm_mon, tm_year, tm_wday, tm_yday, tm_isdst; };
#define CLOCK_REALTIME 0
#define CLOCK_MONOTONIC 1
time_t time(time_t *);
int clock_gettime(int, timespec *);
struct tm *gmtime_r(const time_t *, struct tm *);
struct tm *localtime_r(const time_t *, struct tm *);
size_t strftime(char *, size_t, const char *, const struct tm *);
int nanosleep(const timespec *, timespec *);
double difftime(time_t, time_t);
#endif
