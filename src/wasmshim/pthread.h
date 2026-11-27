#ifndef ZW_PTHREAD_H
#define ZW_PTHREAD_H
#include <stddef.h>
typedef unsigned int pthread_t;
typedef struct { int _x; } pthread_mutex_t;
typedef struct { int _x; } pthread_cond_t;
typedef struct { int _x; } pthread_attr_t;
typedef void *(*pthread_start_t)(void *);
#define PTHREAD_MUTEX_INITIALIZER {0}
#define PTHREAD_COND_INITIALIZER {0}
int pthread_create(pthread_t *, const pthread_attr_t *, pthread_start_t, void *);
int pthread_join(pthread_t, void **);
int pthread_detach(pthread_t);
int pthread_mutex_init(pthread_mutex_t *, const pthread_mutex_t *);
int pthread_mutex_lock(pthread_mutex_t *);
int pthread_mutex_unlock(pthread_mutex_t *);
int pthread_mutex_destroy(pthread_mutex_t *);
int pthread_cond_init(pthread_cond_t *, const pthread_cond_t *);
int pthread_cond_wait(pthread_cond_t *, pthread_mutex_t *);
int pthread_cond_signal(pthread_cond_t *);
int pthread_cond_broadcast(pthread_cond_t *);
int pthread_cond_destroy(pthread_cond_t *);
pthread_t pthread_self(void);
void pthread_exit(void *);
int pthread_attr_init(pthread_attr_t *);
int pthread_attr_destroy(pthread_attr_t *);
#endif
