#define _POSIX_C_SOURCE 200809L
/* zaprt — Zappr C runtime, emitted verbatim into every native build.
   One tagged value type; everything is a value (SPEC §7.4). */

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

typedef enum {
  Z_NOTHING, Z_INT, Z_FLOAT, Z_STR, Z_BOOL, Z_BYTES,
  Z_LIST, Z_MAP, Z_TUPLE, Z_REC, Z_FUNC, Z_TYPE,
  Z_ERR, Z_POISON, Z_CHAN, Z_UI, Z_WEAK, Z_VARIANT
} ZTag;

typedef struct ZVal ZVal;
typedef struct ZRef ZRef;
typedef struct ZEnv ZEnv;
typedef struct ZBind ZBind;
typedef struct ZVec ZVec;
typedef struct ZPair ZPair;
typedef struct ZZMap ZZMap;
typedef struct ZRec ZRec;
typedef struct ZErrV ZErrV;
typedef struct ZPoisV ZPoisV;
typedef struct ZChan ZChan;
typedef struct ZUi ZUi;
typedef struct ZWeakV ZWeakV;
typedef struct ZVarV ZVarV;
typedef struct ZTypeV ZTypeV;
typedef struct ZFuncV ZFuncV;
typedef ZVal (*ZFp)(ZEnv *, ZVal *, int, int);
typedef struct { const char *name; ZFp fn; const char **params; int nparams; } ZFnEnt;
extern ZFnEnt z_fns[];

struct ZVal {
  ZTag tag;
  size_t aux;
  union {
    long long i;
    double f;
    int b;
    char *s;
    struct { unsigned char *p; size_t n; } by;
    ZVec *vec;
    ZZMap *map;
    ZRec *rec;
    ZFuncV *fn;
    char *tname;
    ZErrV *err;
    ZPoisV *poi;
    ZChan *chan;
    ZUi *ui;
    ZWeakV *weak;
    ZVarV *var;
  } u;
};

struct ZVec { ZVal *v; size_t n, cap; };
struct ZPair { ZVal k, v; };
struct ZZMap { ZPair *p; size_t n, cap; };
struct ZRec {
  char *ty;
  char **names;
  char **tynames;
  ZVal *vals;
  size_t n;
};
struct ZErrV { char *msg; char *origin; };
struct ZPoisV { char *kind, *origin, *expected, *actual; };
struct ZChan {
  ZVal *q; size_t head, tail, n, cap;
  int closed;
  pthread_mutex_t mu;
  pthread_cond_t cv;
};
struct ZUi {
  char *kind;
  ZPair *props; size_t pn, pcap;
  ZUi **ch; size_t cn, ccap;
  ZVal handlers;
};
struct ZWeakV { ZVal v; };
struct ZVarV { char *name; ZVal inner; };
struct ZTypeV {
  char *repr;
  ZPair *fields; size_t fn, fcap;
};
struct ZFuncV { const char *name; ZEnv *env; ZVal want; };
struct ZRef { ZVal v; };
struct ZBind { char *name; ZRef *ref; ZBind *next; };
struct ZEnv { ZEnv *parent; ZBind *binds; };

#define ZNOTHING ((ZVal){Z_NOTHING, 0, {0}})
static const char *z_type_repr(ZVal *v);
static void z_die(const char *msg);
static char *z_val_str(ZVal v);
static int z_eqv_raw(ZVal *a, ZVal *b);
static ZVal z_eqv(ZVal a, ZVal b);
static ZVal z_ne(ZVal a, ZVal b);
static double z_num(ZVal v);
static ZVal z_unwrap(ZVal v);
static int z_truthy(ZVal v);
static int z_cmp(ZVal a, ZVal b);
static ZVal z_index(ZVal v, long long i);
static ZVal z_field(ZVal obj, const char *name);
static void z_field_set(ZVal obj, const char *name, ZVal v);
static char *z_json_escape(const char *s);
static ZVal z_args_record(int supplied, ZVec *rest, int consumed);
static ZVal z_call_val(ZVal f, ZVal *args, int n);
static ZVal z_call_qname(const char *name, ZVal *pos, int npos, ZEnv *env);
static ZVal z_invoke(ZVal f, ZVal *pos, int npos, ZVal named, ZEnv *env);
static ZVal z_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env);
static void z_ui_render(ZUi *u, int depth);

static ZEnv *z_global;
static void z_die(const char *msg) {
  fprintf(stderr, "zap: runtime error: %s\n", msg);
  exit(1);
}

/* ---------- allocation (arena-style: never freed, process lifetime) ---------- */
static void *z_alloc(size_t n) { void *p = malloc(n ? n : 1); return p; }
static char *z_strdup(const char *s) {
  size_t n = strlen(s ? s : "") + 1;
  char *p = z_alloc(n);
  memcpy(p, s ? s : "", n);
  return p;
}
static char *z_strndup(const char *s, size_t n) {
  char *p = z_alloc(n + 1);
  memcpy(p, s, n);
  p[n] = 0;
  return p;
}

/* ---------- constructors ---------- */
static ZVal z_int(long long i) { ZVal v = {0}; v.tag = Z_INT; v.u.i = i; return v; }
static ZVal z_float(double f) { ZVal v = {0}; v.tag = Z_FLOAT; v.u.f = f; return v; }
static ZVal z_bool(int b) { ZVal v = {0}; v.tag = Z_BOOL; v.u.b = !!b; return v; }
static ZVal z_str(const char *s) { ZVal v = {0}; v.tag = Z_STR; v.u.s = z_strdup(s); return v; }
static ZVal z_strn(const char *s, size_t n) { ZVal v = {0}; v.tag = Z_STR; v.u.s = z_strndup(s, n); return v; }
static ZVal z_str_take(char *s) { ZVal v = {0}; v.tag = Z_STR; v.u.s = s ? s : z_strdup(""); return v; }
static ZVal z_errv(const char *msg, const char *origin) {
  ZVal v = {0}; v.tag = Z_ERR;
  ZErrV *e = z_alloc(sizeof(ZErrV));
  e->msg = z_strdup(msg); e->origin = z_strdup(origin ? origin : "");
  v.u.err = e; return v;
}
static ZVal z_poisonv(const char *kind, const char *origin, const char *expected, const char *actual) {
  ZVal v = {0}; v.tag = Z_POISON;
  ZPoisV *p = z_alloc(sizeof(ZPoisV));
  p->kind = z_strdup(kind); p->origin = z_strdup(origin ? origin : "");
  p->expected = z_strdup(expected); p->actual = z_strdup(actual);
  v.u.poi = p; return v;
}
static ZVal z_fnval(const char *name, ZEnv *env) {
  ZVal v = {0}; v.tag = Z_FUNC;
  ZFuncV *f = z_alloc(sizeof(ZFuncV));
  f->name = name; f->env = env; f->want = ZNOTHING;
  v.u.fn = f; return v;
}
static ZVal z_type_prim(const char *repr) {
  ZVal v = {0}; v.tag = Z_TYPE; v.u.tname = z_strdup(repr); return v;
}
static ZVal z_type_generic(const char *name, ZVal *args, int n) {
  /* repr = "Name(t1, t2)" using each arg type's canonical repr */
  size_t cap = strlen(name) + 8;
  for (int i = 0; i < n; i++) cap += strlen(z_type_repr(&args[i])) + 2;
  char *buf = z_alloc(cap);
  strcpy(buf, name); strcat(buf, "(");
  for (int i = 0; i < n; i++) {
    if (i) strcat(buf, ", ");
    strcat(buf, z_type_repr(&args[i]));
  }
  strcat(buf, ")");
  return z_type_prim(buf);
}
static ZVal z_type_tuple(ZVal *args, int n) {
  size_t cap = 8;
  for (int i = 0; i < n; i++) cap += strlen(z_type_repr(&args[i])) + 2;
  char *buf = z_alloc(cap);
  strcpy(buf, "(");
  for (int i = 0; i < n; i++) {
    if (i) strcat(buf, ", ");
    strcat(buf, z_type_repr(&args[i]));
  }
  strcat(buf, ")");
  return z_type_prim(buf);
}
static ZVal z_type_union(ZVal *args, int n) {
  size_t cap = 8;
  for (int i = 0; i < n; i++) cap += strlen(z_type_repr(&args[i])) + 3;
  char *buf = z_alloc(cap);
  buf[0] = 0;
  for (int i = 0; i < n; i++) {
    if (i) strcat(buf, " | ");
    strcat(buf, z_type_repr(&args[i]));
  }
  return z_type_prim(buf);
}
static ZVal z_type_ref(const char *name) {
  char buf[256];
  snprintf(buf, sizeof buf, "&%s", name);
  return z_type_prim(buf);
}
static ZVal z_type_rec(ZPair *fields, size_t n, const char *tyname) {
  /* record type value: keep fields so satisfies() can check structurally */
  ZVal v = {0}; v.tag = Z_TYPE;
  /* build canonical repr "(name: ty, ...)" */
  size_t cap = 16;
  for (size_t i = 0; i < n; i++) {
    cap += strlen(z_val_str(fields[i].k)) + strlen(z_type_repr(&fields[i].v)) + 4;
  }
  char *buf = z_alloc(cap);
  strcpy(buf, "(");
  for (size_t i = 0; i < n; i++) {
    if (i) strcat(buf, ", ");
    strcat(buf, z_val_str(fields[i].k));
    strcat(buf, ": ");
    strcat(buf, z_type_repr(&fields[i].v));
  }
  strcat(buf, ")");
  v.u.tname = buf;
  /* stash fields in a parallel structure via a hidden Z_REC? keep simple:
     record satisfies() parses the repr */
  (void)tyname;
  return v;
}

/* ---------- aggregates ---------- */
static ZVec *z_vec_new(void) { ZVec *v = z_alloc(sizeof(ZVec)); v->v = NULL; v->n = v->cap = 0; return v; }
static void z_vec_push(ZVec *v, ZVal x) {
  if (v->n == v->cap) { v->cap = v->cap ? v->cap * 2 : 8; v->v = realloc(v->v, v->cap * sizeof(ZVal)); }
  v->v[v->n++] = x;
}
static ZVal z_tuple(ZVal *items, int n) {
  ZVal v = {0}; v.tag = Z_TUPLE;
  ZVec *z = z_vec_new();
  for (int i = 0; i < n; i++) z_vec_push(z, items[i]);
  v.u.vec = z; return v;
}
static ZVal z_listv(ZVal *items, int n) {
  ZVal v = z_tuple(items, n); v.tag = Z_LIST; return v;
}
static ZZMap *z_map_new_p(void) { ZZMap *m = z_alloc(sizeof(ZZMap)); m->p = NULL; m->n = m->cap = 0; return m; }
static ZVal z_map_new(void) { ZVal v = {0}; v.tag = Z_MAP; v.u.map = z_map_new_p(); return v; }
static void z_map_setp(ZZMap *m, ZVal k, ZVal v) {
  for (size_t i = 0; i < m->n; i++)
    if (z_eqv_raw(&m->p[i].k, &k)) { m->p[i].v = v; return; }
  if (m->n == m->cap) { m->cap = m->cap ? m->cap * 2 : 8; m->p = realloc(m->p, m->cap * sizeof(ZPair)); }
  m->p[m->n].k = k; m->p[m->n].v = v; m->n++;
}
static ZVal z_mapv(ZPair *pairs, int n) {
  ZVal v = z_map_new();
  for (int i = 0; i < n; i++) z_map_setp(v.u.map, pairs[i].k, pairs[i].v);
  return v;
}
static ZVal z_rec_new(const char *ty) {
  ZVal v = {0}; v.tag = Z_REC;
  ZRec *r = z_alloc(sizeof(ZRec));
  r->ty = z_strdup(ty ? ty : ""); r->names = NULL; r->tynames = NULL; r->vals = NULL; r->n = 0;
  v.u.rec = r; return v;
}
static void z_rec_add(ZRec *r, const char *name, const char *tyname, ZVal v) {
  r->names = realloc(r->names, (r->n + 1) * sizeof(char *));
  r->tynames = realloc(r->tynames, (r->n + 1) * sizeof(char *));
  r->vals = realloc(r->vals, (r->n + 1) * sizeof(ZVal));
  r->names[r->n] = z_strdup(name);
  r->tynames[r->n] = z_strdup(tyname ? tyname : "any");
  r->vals[r->n] = v;
  r->n++;
}
static ZVal z_chan_new(void) {
  ZVal v = {0}; v.tag = Z_CHAN;
  ZChan *c = z_alloc(sizeof(ZChan));
  c->q = NULL; c->head = c->tail = c->n = c->cap = 0; c->closed = 0;
  pthread_mutex_init(&c->mu, NULL);
  pthread_cond_init(&c->cv, NULL);
  v.u.chan = c; return v;
}
static ZVal z_chan_pair(void) {
  ZVal a = z_chan_new(), b = a;
  return z_tuple((ZVal[]){a, b}, 2);
}
static void z_chan_send(ZChan *c, ZVal v) {
  pthread_mutex_lock(&c->mu);
  if (c->n == c->cap) { c->cap = c->cap ? c->cap * 2 : 16; c->q = realloc(c->q, c->cap * sizeof(ZVal)); }
  c->q[c->tail] = v; c->tail = (c->tail + 1) % c->cap; c->n++;
  pthread_cond_signal(&c->cv);
  pthread_mutex_unlock(&c->mu);
}
static ZVal z_chan_recv(ZVal ch) {
  ZChan *c = ch.u.chan;
  pthread_mutex_lock(&c->mu);
  while (c->n == 0 && !c->closed) pthread_cond_wait(&c->cv, &c->mu);
  ZVal v = ZNOTHING;
  if (c->n > 0) {
    v = c->q[c->head];
    c->head = (c->head + 1) % c->cap; c->n--;
    pthread_cond_signal(&c->cv);
  } else if (c->closed) {
    v = ZNOTHING;
  }
  pthread_mutex_unlock(&c->mu);
  return v;
}
static ZVal z_chan_close(ZVal ch) {
  ZChan *c = ch.u.chan;
  pthread_mutex_lock(&c->mu); c->closed = 1; pthread_cond_broadcast(&c->cv); pthread_mutex_unlock(&c->mu);
  return ZNOTHING;
}
static ZVal z_ui_new(const char *kind) {
  ZVal v = {0}; v.tag = Z_UI;
  ZUi *u = z_alloc(sizeof(ZUi));
  u->kind = z_strdup(kind); u->props = NULL; u->pn = u->pcap = 0;
  u->ch = NULL; u->cn = u->ccap = 0; u->handlers = z_listv(NULL, 0);
  v.u.ui = u; return v;
}
static void z_ui_set(ZUi *u, const char *name, ZVal v) {
  for (size_t i = 0; i < u->pn; i++)
    if (!strcmp(u->props[i].k.u.s, name)) { u->props[i].v = v; return; }
  if (u->pn == u->pcap) { u->pcap = u->pcap ? u->pcap * 2 : 8; u->props = realloc(u->props, u->pcap * sizeof(ZPair)); }
  u->props[u->pn].k = z_str(name); u->props[u->pn].v = v; u->pn++;
}
static ZVal z_ui_get(ZUi *u, const char *name) {
  for (size_t i = 0; i < u->pn; i++)
    if (!strcmp(u->props[i].k.u.s, name)) return u->props[i].v;
  return ZNOTHING;
}
static ZVal z_weak(ZVal v) {
  ZVal r = {0}; r.tag = Z_WEAK;
  ZWeakV *w = z_alloc(sizeof(ZWeakV)); w->v = v; r.u.weak = w; return r;
}
static ZVal z_variant(const char *name, ZVal inner) {
  ZVal v = {0}; v.tag = Z_VARIANT;
  ZVarV *x = z_alloc(sizeof(ZVarV));
  x->name = z_strdup(name); x->inner = inner; v.u.var = x; return v;
}

/* ---------- env ---------- */
static ZEnv *z_env_new(ZEnv *parent) {
  ZEnv *e = z_alloc(sizeof(ZEnv)); e->parent = parent; e->binds = NULL; return e;
}
static ZRef *z_env_find(ZEnv *e, const char *name) {
  for (; e; e = e->parent)
    for (ZBind *b = e->binds; b; b = b->next)
      if (!strcmp(b->name, name)) return b->ref;
  return NULL;
}
static void z_env_define(ZEnv *e, const char *name, ZVal v) {
  ZRef *r = z_alloc(sizeof(ZRef)); r->v = v;
  ZBind *b = z_alloc(sizeof(ZBind));
  b->name = (char *)name; b->ref = r; b->next = e->binds; e->binds = b;
}
static ZVal z_env_get(ZEnv *e, const char *name) {
  ZRef *r = z_env_find(e, name);
  if (!r) return z_errv("unknown name", name);
  return r->v;
}
static int z_env_set(ZEnv *e, const char *name, ZVal v) {
  ZRef *r = z_env_find(e, name);
  if (r) { r->v = v; return 1; }
  z_env_define(e, name, v);
  return 0;
}
static ZVal z_args_record(int supplied, ZVec *rest, int consumed) {
  /* args = {count: supplied, rest: list(rest)} — consumed params already bound */
  ZVal rec = z_rec_new("args");
  ZRec *r = rec.u.rec;
  z_rec_add(r, "count", "int", z_int(supplied));
  ZVal lv = {0}; lv.tag = Z_LIST; lv.u.vec = rest;
  z_rec_add(r, "rest", "List(any)", lv);
  (void)consumed;
  return rec;
}

/* ---------- display ---------- */
typedef struct { char *p; size_t n, cap; } ZSb;
static void sb_need(ZSb *b, size_t add) {
  if (b->n + add + 1 > b->cap) {
    b->cap = (b->cap + add + 1) * 2 + 64;
    b->p = realloc(b->p, b->cap);
  }
}
static void sb_add(ZSb *b, const char *s) {
  size_t n = strlen(s);
  sb_need(b, n);
  memcpy(b->p + b->n, s, n); b->n += n; b->p[b->n] = 0;
}
static void sb_addn(ZSb *b, const char *s, size_t n) {
  sb_need(b, n); memcpy(b->p + b->n, s, n); b->n += n; b->p[b->n] = 0;
}
static void sb_ch(ZSb *b, char c) { sb_need(b, 1); b->p[b->n++] = c; b->p[b->n] = 0; }
static void sb_val(ZSb *b, ZVal v);

static const char *z_tag_name(ZTag t) {
  switch (t) {
    case Z_INT: return "int"; case Z_FLOAT: return "float"; case Z_STR: return "str";
    case Z_BOOL: return "bool"; case Z_BYTES: return "bytes"; case Z_LIST: return "List(any)";
    case Z_MAP: return "Map(any, any)"; case Z_TUPLE: return "tuple"; case Z_REC: return "record";
    case Z_FUNC: return "fn"; case Z_TYPE: return "type"; case Z_ERR: return "Err";
    case Z_POISON: return "poison"; case Z_CHAN: return "channel"; case Z_UI: return "ui";
    case Z_WEAK: return "weak"; case Z_VARIANT: return "variant"; case Z_NOTHING: return "null";
  }
  return "?";
}

static void sb_val(ZSb *b, ZVal v) {
  char tmp[64];
  switch (v.tag) {
    case Z_INT: snprintf(tmp, sizeof tmp, "%lld", v.u.i); sb_add(b, tmp); break;
    case Z_FLOAT:
      if (isfinite(v.u.f) && floor(v.u.f) == v.u.f) snprintf(tmp, sizeof tmp, "%.1f", v.u.f);
      else snprintf(tmp, sizeof tmp, "%.17g", v.u.f);
      sb_add(b, tmp); break;
    case Z_STR: sb_add(b, v.u.s); break;
    case Z_BOOL: sb_add(b, v.u.b ? "true" : "false"); break;
    case Z_BYTES: snprintf(tmp, sizeof tmp, "<%zu bytes>", v.u.by.n); sb_add(b, tmp); break;
    case Z_NOTHING: sb_add(b, "null"); break;
    case Z_LIST: case Z_TUPLE: {
      sb_ch(b, v.tag == Z_LIST ? '[' : '(');
      for (size_t i = 0; i < v.u.vec->n; i++) {
        if (i) sb_add(b, ", ");
        sb_val(b, v.u.vec->v[i]);
      }
      sb_ch(b, v.tag == Z_LIST ? ']' : ')');
      break;
    }
    case Z_MAP: {
      sb_add(b, "{");
      for (size_t i = 0; i < v.u.map->n; i++) {
        if (i) sb_add(b, ", ");
        sb_val(b, v.u.map->p[i].k);
        sb_add(b, ": ");
        sb_val(b, v.u.map->p[i].v);
      }
      sb_ch(b, '}');
      break;
    }
    case Z_REC: {
      sb_add(b, v.u.rec->ty && *v.u.rec->ty ? v.u.rec->ty : "rec");
      sb_ch(b, '(');
      for (size_t i = 0; i < v.u.rec->n; i++) {
        if (i) sb_add(b, ", ");
        sb_add(b, v.u.rec->names[i]);
        sb_add(b, ": ");
        sb_val(b, v.u.rec->vals[i]);
      }
      sb_ch(b, ')');
      break;
    }
    case Z_FUNC: sb_add(b, "<fn "); sb_add(b, v.u.fn->name); sb_ch(b, '>'); break;
    case Z_TYPE: sb_add(b, v.u.tname); break;
    case Z_ERR: sb_add(b, "Err("); sb_add(b, v.u.err->msg); sb_ch(b, ')'); break;
    case Z_POISON:
      snprintf(tmp, sizeof tmp, "%s(expected %s, got %s, at %s)",
               v.u.poi->kind, v.u.poi->expected, v.u.poi->actual, v.u.poi->origin);
      sb_add(b, tmp); break;
    case Z_CHAN: sb_add(b, "<channel>"); break;
    case Z_UI: sb_add(b, "<ui."); sb_add(b, v.u.ui->kind); sb_ch(b, '>'); break;
    case Z_WEAK: sb_add(b, "<weak>"); break;
    case Z_VARIANT:
      sb_add(b, v.u.var->name); sb_ch(b, '('); sb_val(b, v.u.var->inner); sb_ch(b, ')');
      break;
  }
}
static char *z_val_str(ZVal v) {
  ZSb b = {0};
  sb_val(&b, v);
  if (!b.p) return z_strdup("");
  return b.p;
}
static ZVal z_str_catv(ZVal a, ZVal b) {
  char *sa = z_val_str(a), *sb = z_val_str(b);
  size_t la = strlen(sa), lb = strlen(sb);
  char *p = z_alloc(la + lb + 1);
  memcpy(p, sa, la); memcpy(p + la, sb, lb + 1);
  ZVal v = {0}; v.tag = Z_STR; v.u.s = p; return v;
}
static const char *z_type_repr(ZVal *v);
static char *z_val_str(ZVal v);
static int z_eqv_raw(ZVal *a, ZVal *b);
static ZVal z_eqv(ZVal a, ZVal b);
static ZVal z_ne(ZVal a, ZVal b);
static double z_num(ZVal v);
static ZVal z_unwrap(ZVal v);
static int z_truthy(ZVal v);
static int z_cmp(ZVal a, ZVal b);
static ZVal z_index(ZVal v, long long i);
static ZVal z_field(ZVal obj, const char *name);
static void z_field_set(ZVal obj, const char *name, ZVal v);
static char *z_json_escape(const char *s);
static ZVal z_args_record(int supplied, ZVec *rest, int consumed);
static ZVal z_call_val(ZVal f, ZVal *args, int n);
static ZVal z_call_qname(const char *name, ZVal *pos, int npos, ZEnv *env);
static ZVal z_invoke(ZVal f, ZVal *pos, int npos, ZVal named, ZEnv *env);
static ZVal z_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env);
static void z_ui_render(ZUi *u, int depth);

/* unwrap weak refs one level */
static ZVal z_unwrap(ZVal v) {
  if (v.tag == Z_WEAK) return v.u.weak->v;
  return v;
}

/* ---------- truthiness / equality ---------- */
static int z_truthy(ZVal v) {
  v = z_unwrap(v);
  switch (v.tag) {
    case Z_BOOL: return v.u.b;
    case Z_INT: return v.u.i != 0;
    case Z_FLOAT: return v.u.f != 0.0;
    case Z_STR: return v.u.s[0] != 0;
    case Z_BYTES: return v.u.by.n != 0;
    case Z_LIST: case Z_TUPLE: return v.u.vec->n != 0;
    case Z_MAP: return v.u.map->n != 0;
    case Z_REC: return v.u.rec->n != 0;
    case Z_NOTHING: case Z_ERR: case Z_POISON: return 0;
    default: return 1;
  }
}

static int z_eqv_raw(ZVal *a, ZVal *b) {
  ZVal x = z_unwrap(*a), y = z_unwrap(*b);
  if ((x.tag == Z_INT || x.tag == Z_FLOAT) && (y.tag == Z_INT || y.tag == Z_FLOAT))
    return z_num(x) == z_num(y);
  if (x.tag != y.tag) return 0;
  switch (x.tag) {
    case Z_INT: return x.u.i == y.u.i;
    case Z_FLOAT: return x.u.f == y.u.f;
    case Z_STR: return !strcmp(x.u.s, y.u.s);
    case Z_BOOL: return x.u.b == y.u.b;
    case Z_BYTES: return x.u.by.n == y.u.by.n && !memcmp(x.u.by.p, y.u.by.p, x.u.by.n);
    case Z_NOTHING: return 1;
    case Z_TYPE: return !strcmp(x.u.tname, y.u.tname);
    case Z_FUNC: return !strcmp(x.u.fn->name, y.u.fn->name);
    case Z_CHAN: return x.u.chan == y.u.chan;
    case Z_UI: return x.u.ui == y.u.ui;
    case Z_WEAK: { ZVal *pa=&x.u.weak->v,*pb=&y.u.weak->v; return z_eqv_raw(pa,pb); }
    case Z_LIST: case Z_TUPLE: {
      if (x.u.vec->n != y.u.vec->n) return 0;
      for (size_t i = 0; i < x.u.vec->n; i++)
        if (!z_eqv_raw(&x.u.vec->v[i], &y.u.vec->v[i])) return 0;
      return 1;
    }
    case Z_MAP: {
      if (x.u.map->n != y.u.map->n) return 0;
      for (size_t i = 0; i < x.u.map->n; i++) {
        int found = 0;
        for (size_t j = 0; j < y.u.map->n; j++)
          if (z_eqv_raw(&x.u.map->p[i].k, &y.u.map->p[j].k) && z_eqv_raw(&x.u.map->p[i].v, &y.u.map->p[j].v)) { found = 1; break; }
        if (!found) return 0;
      }
      return 1;
    }
    case Z_REC: {
      ZRec *a = x.u.rec, *b = y.u.rec;
      if (a->n != b->n || strcmp(a->ty, b->ty)) return 0;
      for (size_t i = 0; i < a->n; i++) {
        int found = 0;
        for (size_t j = 0; j < b->n; j++)
          if (!strcmp(a->names[i], b->names[j]) && z_eqv_raw(&a->vals[i], &b->vals[j])) { found = 1; break; }
        if (!found) return 0;
      }
      return 1;
    }
    case Z_ERR: return !strcmp(x.u.err->msg, y.u.err->msg);
    case Z_POISON: return !strcmp(x.u.poi->kind, y.u.poi->kind);
    case Z_VARIANT: return !strcmp(x.u.var->name, y.u.var->name) && z_eqv_raw(&x.u.var->inner, &y.u.var->inner);
    default: return 0;
  }
}
static ZVal z_eqv(ZVal a, ZVal b) { return z_bool(z_eqv_raw(&a, &b)); }
static ZVal z_ne(ZVal a, ZVal b) { return z_bool(!z_eqv_raw(&a, &b)); }

/* ---------- numbers ---------- */
static int z_is_num(ZVal v);
static double z_num(ZVal v);
static int z_is_num(ZVal v) { v = z_unwrap(v); return v.tag == Z_INT || v.tag == Z_FLOAT; }
static double z_num(ZVal v) { v = z_unwrap(v); return v.tag == Z_INT ? (double)v.u.i : v.u.f; }
static ZVal z_arith(const char *op, ZVal a, ZVal b) {
  a = z_unwrap(a); b = z_unwrap(b);
  if (z_is_num(a) && z_is_num(b)) {
    int both_int = a.tag == Z_INT && b.tag == Z_INT;
    if (!strcmp(op, "+")) {
      if (both_int) return z_int(a.u.i + b.u.i);
      return z_float(z_num(a) + z_num(b));
    }
    if (!strcmp(op, "-")) {
      if (both_int) return z_int(a.u.i - b.u.i);
      return z_float(z_num(a) - z_num(b));
    }
    if (!strcmp(op, "*")) {
      if (both_int) return z_int(a.u.i * b.u.i);
      return z_float(z_num(a) * z_num(b));
    }
    if (!strcmp(op, "/")) {
      double d = z_num(b);
      if (d == 0.0) return z_errv("division by zero", "arith");
      if (both_int && a.u.i % b.u.i == 0) return z_int(a.u.i / b.u.i);
      return z_float(z_num(a) / d);
    }
    if (!strcmp(op, "%")) {
      if (b.u.i == 0) return z_errv("division by zero", "arith");
      return z_int(a.u.i % b.u.i);
    }
  }
  /* str + anything → concat */
  if (!strcmp(op, "+") && (a.tag == Z_STR || b.tag == Z_STR))
    return z_str_catv(a, b);
  /* list + list → concat */
  if (!strcmp(op, "+") && a.tag == Z_LIST && b.tag == Z_LIST) {
    ZVal r = z_listv(NULL, 0);
    for (size_t i = 0; i < a.u.vec->n; i++) z_vec_push(r.u.vec, a.u.vec->v[i]);
    for (size_t i = 0; i < b.u.vec->n; i++) z_vec_push(r.u.vec, b.u.vec->v[i]);
    return r;
  }
  char buf[256];
  snprintf(buf, sizeof buf, "cannot apply `%s` to %s and %s", op,
           z_tag_name(a.tag), z_tag_name(b.tag));
  return z_errv(buf, "arith");
}
static ZVal z_add(ZVal a, ZVal b) { return z_arith("+", a, b); }
static ZVal z_sub(ZVal a, ZVal b) { return z_arith("-", a, b); }
static ZVal z_mul(ZVal a, ZVal b) { return z_arith("*", a, b); }
static ZVal z_div(ZVal a, ZVal b) { return z_arith("/", a, b); }
static ZVal z_mod(ZVal a, ZVal b) { return z_arith("%", a, b); }
static ZVal z_neg(ZVal a) {
  a = z_unwrap(a);
  if (a.tag == Z_INT) return z_int(-a.u.i);
  if (a.tag == Z_FLOAT) return z_float(-a.u.f);
  return z_errv("cannot negate value", "unary");
}
static int z_cmp(ZVal a, ZVal b) {
  a = z_unwrap(a); b = z_unwrap(b);
  if (z_is_num(a) && z_is_num(b)) {
    double x = z_num(a), y = z_num(b);
    return x < y ? -1 : x > y ? 1 : 0;
  }
  if (a.tag == Z_STR && b.tag == Z_STR) return strcmp(a.u.s, b.u.s);
  if (a.tag == Z_BOOL && b.tag == Z_BOOL) return a.u.b - b.u.b;
  return 0;
}
static ZVal z_lt(ZVal a, ZVal b) { return z_bool(z_cmp(a, b) < 0); }
static ZVal z_gt(ZVal a, ZVal b) { return z_bool(z_cmp(a, b) > 0); }
static ZVal z_le(ZVal a, ZVal b) { return z_bool(z_cmp(a, b) <= 0); }
static ZVal z_ge(ZVal a, ZVal b) { return z_bool(z_cmp(a, b) >= 0); }
static ZVal z_ne_num(ZVal a, ZVal b) { return z_ne(a, b); }

/* ---------- index / field ---------- */
static ZVal z_index(ZVal v, long long i) {
  v = z_unwrap(v);
  if ((v.tag == Z_TUPLE || v.tag == Z_LIST) && v.u.vec) {
    if (i >= 0 && (size_t)i < v.u.vec->n) return v.u.vec->v[i];
    return ZNOTHING;
  }
  if (v.tag == Z_STR) {
    size_t len = strlen(v.u.s);
    if (i >= 0 && (size_t)i < len) return z_strn(v.u.s + i, 1);
    return ZNOTHING;
  }
  if (v.tag == Z_REC && v.u.rec) {
    if (i >= 0 && (size_t)i < v.u.rec->n) return v.u.rec->vals[i];
    return ZNOTHING;
  }
  return z_errv("cannot index value", "index");
}
static ZVal z_field(ZVal obj, const char *name) {
  obj = z_unwrap(obj);
  if (obj.tag == Z_ERR || obj.tag == Z_POISON) return obj;
  if (obj.tag == Z_REC && obj.u.rec) {
    ZRec *r = obj.u.rec;
    for (size_t i = 0; i < r->n; i++)
      if (!strcmp(r->names[i], name)) return r->vals[i];
    /* tuple-style .0 .1 */
    if (name[0] == '_' && !strncmp(name, "__idx_", 6))
      return z_index(obj, atoll(name + 6));
    return z_errv("no such field", name);
  }
  if ((obj.tag == Z_TUPLE || obj.tag == Z_LIST) && obj.u.vec) {
    if (!strncmp(name, "__idx_", 6)) return z_index(obj, atoll(name + 6));
    return z_errv("no such field", name);
  }
  if (obj.tag == Z_MAP && obj.u.map) {
    ZVal k = z_str(name);
    for (size_t i = 0; i < obj.u.map->n; i++)
      if (z_eqv_raw(&obj.u.map->p[i].k, &k)) return obj.u.map->p[i].v;
    return ZNOTHING;
  }
  if (obj.tag == Z_UI && obj.u.ui) return z_ui_get(obj.u.ui, name);
  if (obj.tag == Z_TYPE) return z_errv("field of type value", name);
  if (obj.tag == Z_NOTHING) return z_errv("field of null", name);
  return z_errv("no field access on this value", name);
}
static void z_field_set(ZVal obj, const char *name, ZVal v) {
  obj = z_unwrap(obj);
  if (obj.tag == Z_REC && obj.u.rec) {
    ZRec *r = obj.u.rec;
    for (size_t i = 0; i < r->n; i++)
      if (!strcmp(r->names[i], name)) { r->vals[i] = v; return; }
    if (!strncmp(name, "__idx_", 6)) {
      long long i = atoll(name + 6);
      if (i >= 0 && (size_t)i < r->n) { r->vals[i] = v; return; }
    }
    z_rec_add(r, name, "any", v);
    return;
  }
  if (obj.tag == Z_UI && obj.u.ui) { z_ui_set(obj.u.ui, name, v); return; }
  if (obj.tag == Z_MAP && obj.u.map) { z_map_setp(obj.u.map, z_str(name), v); return; }
  if ((obj.tag == Z_TUPLE || obj.tag == Z_LIST) && obj.u.vec && !strncmp(name, "__idx_", 6)) {
    long long i = atoll(name + 6);
    if (i >= 0 && (size_t)i < obj.u.vec->n) { obj.u.vec->v[i] = v; return; }
  }
}


/* ---------- type repr / satisfies ---------- */
static const char *z_type_repr(ZVal *v) {
  ZVal x = z_unwrap(*v);
  if (x.tag == Z_TYPE) return x.u.tname;
  return z_tag_name(x.tag);
}

static int z_sat_rec(ZVal v, const char *r, size_t n, size_t *used);

static void sat_ws(const char *r, size_t n, size_t *i) {
  while (*i < n && isspace((unsigned char)r[*i])) (*i)++;
}

/* union: top-level '|' splits */
static int z_sat_union(ZVal v, const char *r, size_t n) {
  int depth = 0;
  size_t start = 0;
  for (size_t i = 0; i <= n; i++) {
    char c = i < n ? r[i] : '|';
    if (c == '(') depth++;
    else if (c == ')') depth--;
    if ((c == '|' && depth == 0) || i == n) {
      size_t used;
      if (z_sat_rec(v, r + start, i - start, &used)) return 1;
      start = i + 1;
    }
  }
  return 0;
}

static int z_sat_rec(ZVal v, const char *r, size_t n, size_t *used) {
  size_t i = 0;
  sat_ws(r, n, &i);
  if (i >= n) { *used = i; return 0; }
  /* optional pad / tuple: ( ... ) */
  if (r[i] == '(') {
    size_t j = i + 1, depth = 1;
    while (j < n && depth) { if (r[j] == '(') depth++; else if (r[j] == ')') depth--; j++; }
    if (j > n) j = n;
    /* top-level commas inside */
    int commas = 0; depth = 0;
    for (size_t k = i + 1; k + 1 < j; k++) {
      if (r[k] == '(') depth++;
      else if (r[k] == ')') depth--;
      else if (r[k] == ',' && depth == 0) commas++;
    }
    if (commas >= 1) {
      /* tuple type — also covers record repr "(name: ty, ...)" loosely */
      v = z_unwrap(v);
      int is_tuple_val = (v.tag == Z_TUPLE || v.tag == Z_LIST || v.tag == Z_REC);
      if (!is_tuple_val) { *used = j; return 0; }
      /* optional pad: last element "null" and exactly 2 elems */
      /* split elements */
      size_t parts[16]; size_t plen[16]; int np = 0;
      depth = 0; size_t s = i + 1;
      for (size_t k = i + 1; k + 1 < j && np < 16; k++) {
        if (r[k] == '(') depth++;
        else if (r[k] == ')') depth--;
        else if (r[k] == ',' && depth == 0) { parts[np] = s; plen[np] = k - s; np++; s = k + 1; }
      }
      if (np < 16) { parts[np] = s; plen[np] = (j - 1) - s; np++; }
      /* check last elem is "null" */
      {
        size_t ls = parts[np-1], ll = plen[np-1];
        while (ll && isspace((unsigned char)r[ls])) { ls++; ll--; }
        while (ll && isspace((unsigned char)r[ls+ll-1])) ll--;
        if (ll == 4 && !strncmp(r + ls, "null", 4) && np == 2) {
          v = z_unwrap(v);
          if (v.tag == Z_NOTHING) { *used = j; return 1; }
          size_t u2;
          int ok = z_sat_rec(v, r + parts[0], plen[0], &u2);
          *used = j; return ok;
        }
      }
      size_t vn;
      if (v.tag == Z_TUPLE || v.tag == Z_LIST) vn = v.u.vec->n;
      else if (v.tag == Z_REC) vn = v.u.rec->n;
      else { *used = j; return 0; }
      if (vn != (size_t)np) { *used = j; return 0; }
      for (int e = 0; e < np; e++) {
        ZVal ev = (v.tag == Z_REC) ? v.u.rec->vals[e] : v.u.vec->v[e];
        size_t u3;
        if (!z_sat_rec(ev, r + parts[e], plen[e], &u3)) { *used = j; return 0; }
      }
      *used = j; return 1;
    }
    /* parenthesized single type */
    size_t u;
    int ok = z_sat_rec(v, r + i + 1, (j - 1) - (i + 1), &u);
    *used = j;
    return ok;
  }
  /* &T */
  if (r[i] == '&') {
    size_t u;
    int ok = z_sat_rec(v, r + i + 1, n - i - 1, &u);
    *used = i + 1 + u;
    return ok;
  }
  /* ident [ (args) ] */
  size_t start = i;
  while (i < n && (isalnum((unsigned char)r[i]) || r[i] == '_')) i++;
  size_t nl = i - start;
  char name[64];
  if (nl >= sizeof name) nl = sizeof name - 1;
  memcpy(name, r + start, nl); name[nl] = 0;
  sat_ws(r, n, &i);
  if (i < n && r[i] == '(') {
    size_t j = i + 1, d = 1;
    while (j < n && d) { if (r[j] == '(') d++; else if (r[j] == ')') d--; j++; }
    if (j > n) j = n;
    /* split args */
    size_t parts[8]; size_t plen[8]; int np = 0;
    size_t s = i + 1; d = 0;
    for (size_t k = i + 1; k + 1 < j && np < 8; k++) {
      if (r[k] == '(') d++;
      else if (r[k] == ')') d--;
      else if (r[k] == ',' && d == 0) { parts[np] = s; plen[np] = k - s; np++; s = k + 1; }
    }
    if (np < 8 && j > i + 1) { parts[np] = s; plen[np] = (j - 1) - s; np++; }
    v = z_unwrap(v);
    int ok = 0;
    if (!strcmp(name, "List")) {
      ok = (v.tag == Z_LIST || v.tag == Z_TUPLE) && v.u.vec != NULL;
      if (ok && np >= 1)
        for (size_t e = 0; e < v.u.vec->n && ok; e++) {
          size_t u3;
          ok = z_sat_rec(v.u.vec->v[e], r + parts[0], plen[0], &u3);
        }
    } else if (!strcmp(name, "Map")) {
      ok = v.tag == Z_MAP && v.u.map != NULL;
      if (ok && np >= 2)
        for (size_t e = 0; e < v.u.map->n && ok; e++) {
          size_t u3, u4;
          ok = z_sat_rec(v.u.map->p[e].k, r + parts[0], plen[0], &u3)
            && z_sat_rec(v.u.map->p[e].v, r + parts[1], plen[1], &u4);
        }
    } else if (!strcmp(name, "channel")) {
      ok = v.tag == Z_CHAN;
    } else {
      ok = 0;
    }
    *used = j;
    return ok;
  }
  /* bare atom */
  v = z_unwrap(v);
  int ok = 0;
  if (!strcmp(name, "int")) ok = v.tag == Z_INT;
  else if (!strcmp(name, "float")) ok = v.tag == Z_INT || v.tag == Z_FLOAT;
  else if (!strcmp(name, "str")) ok = v.tag == Z_STR;
  else if (!strcmp(name, "bool")) ok = v.tag == Z_BOOL;
  else if (!strcmp(name, "bytes")) ok = v.tag == Z_BYTES;
  else if (!strcmp(name, "null")) ok = v.tag == Z_NOTHING;
  else if (!strcmp(name, "any")) ok = 1;
  else if (!strcmp(name, "Err")) ok = v.tag == Z_ERR;
  else if (!strcmp(name, "poison")) ok = v.tag == Z_POISON;
  else if (!strcmp(name, "fn")) ok = v.tag == Z_FUNC;
  else if (!strcmp(name, "List")) ok = v.tag == Z_LIST || v.tag == Z_TUPLE;
  else if (!strcmp(name, "Map")) ok = v.tag == Z_MAP;
  else if (!strcmp(name, "tuple")) ok = v.tag == Z_TUPLE || v.tag == Z_LIST;
  else if (v.tag == Z_REC) ok = !strcmp(v.u.rec->ty, name) || !*v.u.rec->ty || !strcmp(name, v.u.rec->ty);
  else if (v.tag == Z_TYPE) ok = !strcmp(v.u.tname, name);
  *used = i;
  return ok;
}

static int z_satisfies(ZVal v, ZVal t) {
  t = z_unwrap(t);
  if (t.tag != Z_TYPE) return 0;
  /* record type repr "(name: ty, ...)" — contains ': ' inside parens */
  const char *r = t.u.tname;
  size_t rn = strlen(r);
  if (rn > 2 && r[0] == '(' && r[rn-1] == ')' && memchr(r, ':', rn)) {
    v = z_unwrap(v);
    if (v.tag != Z_REC && v.tag != Z_TUPLE && v.tag != Z_LIST) return 0;
    /* extract field names */
    char names[32][64]; int nn = 0;
    int depth = 0;
    const char *s = r + 1;
    for (const char *p = r + 1; p < r + rn && nn < 32; p++) {
      char c = *p;
      if (c == '(') depth++;
      else if (c == ')') depth--;
      if ((c == ',' && depth == 0) || (c == ')' && depth == -1) || p == r + rn - 1) {
        const char *colon = memchr(s, ':', p - s);
        if (colon) {
          size_t nl = colon - s;
          while (nl && isspace((unsigned char)s[nl-1])) nl--;
          size_t ns = 0; while (ns < nl && isspace((unsigned char)s[ns])) ns++;
          if (nl - ns < 64) { memcpy(names[nn], s + ns, nl - ns); names[nn][nl-ns] = 0; nn++; }
        }
        s = p + 1;
        if (c == ')') break;
      }
    }
    for (int i = 0; i < nn; i++) {
      int found = 0;
      if (v.tag == Z_REC) {
        for (size_t j = 0; j < v.u.rec->n; j++)
          if (!strcmp(v.u.rec->names[j], names[i])) { found = 1; break; }
      } else {
        found = 1; /* positional approx */
      }
      if (!found) return 0;
    }
    return 1;
  }
  return z_sat_union(v, r, rn);
}

static int z_match(ZVal scrut, ZVal ty, ZVal *out) {
  scrut = z_unwrap(scrut);
  ty = z_unwrap(ty);
  if (scrut.tag == Z_VARIANT) {
    if (ty.tag == Z_TYPE && !strcmp(ty.u.tname, scrut.u.var->name)) {
      *out = scrut.u.var->inner;
      return 1;
    }
    if (z_satisfies(scrut.u.var->inner, ty)) { *out = scrut.u.var->inner; return 1; }
    return 0;
  }
  if (z_satisfies(scrut, ty)) { *out = scrut; return 1; }
  return 0;
}

static ZVal z_cast(ZVal v, ZVal t) {
  v = z_unwrap(v); t = z_unwrap(t);
  if (t.tag == Z_TYPE && z_satisfies(v, t)) return v;
  if (t.tag == Z_TYPE && !strcmp(t.u.tname, "float") && v.tag == Z_INT)
    return z_float((double)v.u.i);
  if (t.tag == Z_TYPE && !strcmp(t.u.tname, "int") && v.tag == Z_FLOAT)
    return z_int((long long)v.u.f);
  return z_poisonv("wrongReturnType", "cast", z_val_str(t), z_val_str(v));
}

/* ---------- call machinery ---------- */
static ZFp z_find_fn(const char *name) {
  for (ZFnEnt *e = z_fns; e->name; e++)
    if (!strcmp(e->name, name)) return e->fn;
  return NULL;
}
static const char **z_find_params(const char *name, int *n) {
  for (ZFnEnt *e = z_fns; e->name; e++)
    if (!strcmp(e->name, name)) { *n = e->nparams; return e->params; }
  *n = 0; return NULL;
}
static ZVal z_poison_missing(const char *param, const char *fn) {
  return z_poisonv("missingArg", fn, param, "<missing>");
}

static ZVal z_call_type_named(ZVal t, ZVal *pos, int npos, ZVal named);
static ZVal z_builtin_named(const char *name, ZVal *pos, int npos, ZVal named, ZEnv *env);

static ZVal z_invoke(ZVal f, ZVal *pos, int npos, ZVal named, ZEnv *caller_env) {
  f = z_unwrap(f);
  if (f.tag == Z_FUNC) {
    const char *name = f.u.fn->name;
    ZFp fp = z_find_fn(name);
    if (fp) {
      int supplied = npos + (named.tag == Z_MAP && named.u.map ? (int)named.u.map->n : 0);
      if (named.tag == Z_MAP && named.u.map && named.u.map->n > 0) {
        int np = 0; const char **params = z_find_params(name, &np);
        if (params && np > 0) {
          int surplus = npos > np ? npos - np : 0;
          int total = np + surplus;
          ZVal *argv = z_alloc(sizeof(ZVal) * (total + 1));
          for (int i = 0; i < np; i++) {
            if (i < npos) argv[i] = pos[i];
            else {
              ZVal got = ZNOTHING; int found = 0;
              for (size_t j = 0; j < named.u.map->n; j++)
                { ZVal _kk = z_str(params[i]); if (z_eqv_raw(&named.u.map->p[j].k, &_kk)) { got = named.u.map->p[j].v; found = 1; break; } }
              argv[i] = found ? got : z_poison_missing(params[i], name);
            }
          }
          for (int i = 0; i < surplus; i++) argv[np + i] = pos[np + i];
          ZVal r = fp(f.u.fn->env, argv, total, supplied);
          /* fnType return check (§8.1) */
          if (f.u.fn->want.tag != Z_NOTHING && r.tag != Z_NOTHING && !z_satisfies(r, f.u.fn->want))
            return z_poisonv("wrongReturnType", name, z_val_str(f.u.fn->want), z_val_str(r));
          return r;
        }
      }
      ZVal r = fp(f.u.fn->env, pos, npos, supplied);
      if (f.u.fn->want.tag != Z_NOTHING && r.tag != Z_NOTHING && !z_satisfies(r, f.u.fn->want))
        return z_poisonv("wrongReturnType", name, z_val_str(f.u.fn->want), z_val_str(r));
      return r;
    }
    return z_builtin_named(name, pos, npos, named, caller_env);
  }
  if (f.tag == Z_TYPE) return z_call_type_named(f, pos, npos, named);
  if (f.tag == Z_ERR || f.tag == Z_POISON) return f;
  {
    char buf[256];
    snprintf(buf, sizeof buf, "not callable: %s", z_tag_name(f.tag));
    return z_errv(buf, "call");
  }
}
static ZVal z_call_qname(const char *name, ZVal *pos, int npos, ZEnv *env) {
  return z_invoke(z_fnval(name, NULL), pos, npos, ZNOTHING, env);
}
static ZVal z_call_val(ZVal f, ZVal *pos, int npos) {
  return z_invoke(f, pos, npos, ZNOTHING, NULL);
}
static ZVal z_named(ZPair *pairs, int n) {
  ZVal v = z_map_new();
  for (int i = 0; i < n; i++) z_map_setp(v.u.map, pairs[i].k, pairs[i].v);
  return v;
}

/* ---------- type construction / casts ---------- */
static ZVal z_call_type_named(ZVal t, ZVal *pos, int npos, ZVal named) {
  t = z_unwrap(t);
  if (t.tag != Z_TYPE) return z_errv("call on non-type", "typecall");
  const char *r = t.u.tname;
  if (!strncmp(r, "List(", 5)) {
    ZVal v = z_listv(NULL, 0);
    for (int i = 0; i < npos; i++) z_vec_push(v.u.vec, pos[i]);
    return v;
  }
  if (!strncmp(r, "Map(", 4)) {
    ZVal v = z_map_new();
    for (int i = 0; i + 1 < npos; i += 2) z_map_setp(v.u.map, pos[i], pos[i + 1]);
    return v;
  }
  if (r[0] == '(' && strchr(r, ':')) {
    /* record ctor: fields in repr order, positional then named */
    ZVal rec = z_rec_new("");
    size_t n = strlen(r);
    const char *s = r + 1;
    int fi = 0;
    for (const char *p = r + 1; p <= r + n; p++) {
      char c = p < r + n ? *p : ')';
      if (c == ',' || c == ')' || p == r + n) {
        const char *colon = memchr(s, ':', p - s);
        if (colon) {
          size_t nl = colon - s;
          while (nl && isspace((unsigned char)s[nl-1])) nl--;
          size_t ns = 0; while (ns < nl && isspace((unsigned char)s[ns])) ns++;
          char fname[64];
          if (nl - ns < 64) { memcpy(fname, s + ns, nl - ns); fname[nl - ns] = 0; } else fname[0] = 0;
          ZVal fv = fi < npos ? pos[fi] : ZNOTHING;
          if (named.tag == Z_MAP && named.u.map)
            for (size_t j = 0; j < named.u.map->n; j++)
              { ZVal _kk = z_str(fname); if (z_eqv_raw(&named.u.map->p[j].k, &_kk)) { fv = named.u.map->p[j].v; break; } }
          z_rec_add(rec.u.rec, fname, "any", fv);
          fi++;
        }
        s = p + 1;
        if (c == ')') break;
      }
    }
    return rec;
  }
  if (!strcmp(r, "int")) {
    ZVal a = npos ? z_unwrap(pos[0]) : ZNOTHING;
    if (a.tag == Z_INT) return a;
    if (a.tag == Z_FLOAT) return z_int((long long)a.u.f);
    if (a.tag == Z_STR) return z_int(strtoll(a.u.s, NULL, 10));
    if (a.tag == Z_BOOL) return z_int(a.u.b);
    return z_errv("cannot cast to int", "cast");
  }
  if (!strcmp(r, "float")) {
    ZVal a = npos ? z_unwrap(pos[0]) : ZNOTHING;
    if (a.tag == Z_INT || a.tag == Z_FLOAT) return z_float(z_num(a));
    if (a.tag == Z_STR) return z_float(strtod(a.u.s, NULL));
    return z_errv("cannot cast to float", "cast");
  }
  if (!strcmp(r, "str")) {
    ZVal a = npos ? pos[0] : ZNOTHING;
    if (a.tag == Z_STR) return a;
    return z_str_take(z_val_str(a));
  }
  if (!strcmp(r, "bool")) return z_bool(z_truthy(npos ? pos[0] : ZNOTHING));
  if (!strcmp(r, "Err")) return z_errv(npos ? z_val_str(pos[0]) : "Err", "Err");
  if (!strcmp(r, "bytes")) {
    ZVal a = npos ? z_unwrap(pos[0]) : ZNOTHING;
    if (a.tag == Z_STR) {
      ZVal v = {0}; v.tag = Z_BYTES;
      v.u.by.p = (unsigned char *)a.u.s; v.u.by.n = strlen(a.u.s); return v;
    }
    return z_errv("cannot cast to bytes", "cast");
  }
  {
    char buf[256];
    snprintf(buf, sizeof buf, "cannot construct type %s", r);
    return z_errv(buf, "typecall");
  }
}

/* ---------- methods ---------- */
static ZVal z_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env);
static void z_ui_render(ZUi *u, int depth);

static ZVal z_list_method(ZVal obj, const char *m, ZVal *args, int n, ZEnv *env) {
  ZVec *v = obj.u.vec;
  if (!strcmp(m, "push")) { if (n) z_vec_push(v, args[0]); return ZNOTHING; }
  if (!strcmp(m, "pop")) { if (v->n) return v->v[--v->n]; return ZNOTHING; }
  if (!strcmp(m, "len")) return z_int((long long)v->n);
  if (!strcmp(m, "first")) return v->n ? v->v[0] : ZNOTHING;
  if (!strcmp(m, "last")) return v->n ? v->v[v->n-1] : ZNOTHING;
  if (!strcmp(m, "at")) return n ? z_index(obj, (long long)z_num(args[0])) : ZNOTHING;
  if (!strcmp(m, "map") && n && args[0].tag == Z_FUNC) {
    ZVal r = z_listv(NULL, 0);
    for (size_t i = 0; i < v->n; i++) {
      ZVal out = z_invoke(args[0], &v->v[i], 1, ZNOTHING, env);
      z_vec_push(r.u.vec, out);
    }
    return r;
  }
  if (!strcmp(m, "filter") && n && args[0].tag == Z_FUNC) {
    ZVal r = z_listv(NULL, 0);
    for (size_t i = 0; i < v->n; i++) {
      ZVal keep = z_invoke(args[0], &v->v[i], 1, ZNOTHING, env);
      if (z_truthy(keep)) z_vec_push(r.u.vec, v->v[i]);
    }
    return r;
  }
  if (!strcmp(m, "contains") && n) {
    for (size_t i = 0; i < v->n; i++) if (z_eqv_raw(&v->v[i], &args[0])) return z_bool(1);
    return z_bool(0);
  }
  if (!strcmp(m, "join") && n) {
    const char *sep = args[0].tag == Z_STR ? args[0].u.s : "";
    ZSb b = {0};
    for (size_t i = 0; i < v->n; i++) {
      if (i) sb_add(&b, sep);
      char *x = z_val_str(v->v[i]); sb_add(&b, x);
    }
    ZVal r = {0}; r.tag = Z_STR; r.u.s = b.p ? b.p : z_strdup(""); return r;
  }
  if (!strcmp(m, "reverse")) {
    for (size_t i = 0; i < v->n / 2; i++) { ZVal t = v->v[i]; v->v[i] = v->v[v->n-1-i]; v->v[v->n-1-i] = t; }
    return obj;
  }
  {
    char buf[128]; snprintf(buf, sizeof buf, "unknown list method `%s`", m);
    return z_errv(buf, "method");
  }
}

static ZVal z_map_method(ZVal obj, const char *m, ZVal *args, int n) {
  ZZMap *mp = obj.u.map;
  if (!strcmp(m, "get") && n) {
    for (size_t i = 0; i < mp->n; i++) if (z_eqv_raw(&mp->p[i].k, &args[0])) return mp->p[i].v;
    return ZNOTHING;
  }
  if (!strcmp(m, "set") && n >= 2) { z_map_setp(mp, args[0], args[1]); return ZNOTHING; }
  if (!strcmp(m, "has") && n) {
    for (size_t i = 0; i < mp->n; i++) if (z_eqv_raw(&mp->p[i].k, &args[0])) return z_bool(1);
    return z_bool(0);
  }
  if (!strcmp(m, "len")) return z_int((long long)mp->n);
  if (!strcmp(m, "keys") || !strcmp(m, "values")) {
    ZVal r = z_listv(NULL, 0);
    for (size_t i = 0; i < mp->n; i++) z_vec_push(r.u.vec, !strcmp(m, "keys") ? mp->p[i].k : mp->p[i].v);
    return r;
  }
  {
    char buf[128]; snprintf(buf, sizeof buf, "unknown map method `%s`", m);
    return z_errv(buf, "method");
  }
}

static ZVal z_str_method(ZVal obj, const char *m, ZVal *args, int n) {
  const char *s = obj.u.s;
  if (!strcmp(m, "len")) return z_int((long long)strlen(s));
  if (!strcmp(m, "upper") || !strcmp(m, "lower")) {
    int up = m[0] == 'u';
    char *p = z_alloc(strlen(s) + 1);
    for (size_t i = 0; s[i]; i++) p[i] = up ? toupper((unsigned char)s[i]) : tolower((unsigned char)s[i]);
    p[strlen(s)] = 0;
    ZVal v = {0}; v.tag = Z_STR; v.u.s = p; return v;
  }
  if (!strcmp(m, "contains") && n) {
    const char *sub = args[0].tag == Z_STR ? args[0].u.s : z_val_str(args[0]);
    return z_bool(strstr(s, sub) != NULL);
  }
  if (!strcmp(m, "trim")) {
    while (isspace((unsigned char)*s)) s++;
    size_t l = strlen(s);
    while (l && isspace((unsigned char)s[l-1])) l--;
    return z_strn(s, l);
  }
  if (!strcmp(m, "split") && n) {
    const char *sep = args[0].tag == Z_STR ? args[0].u.s : ",";
    ZVal r = z_listv(NULL, 0);
    size_t sl = strlen(sep);
    const char *p = s;
    for (;;) {
      const char *q = strstr(p, sep);
      if (!q) { z_vec_push(r.u.vec, z_str(p)); break; }
      z_vec_push(r.u.vec, z_strn(p, q - p));
      p = q + sl;
    }
    return r;
  }
  {
    char buf[128]; snprintf(buf, sizeof buf, "unknown str method `%s`", m);
    return z_errv(buf, "method");
  }
}

static ZVal z_chan_method(ZVal obj, const char *m, ZVal *args, int n) {
  if (!strcmp(m, "send") && n) { z_chan_send(obj.u.chan, args[0]); return ZNOTHING; }
  if (!strcmp(m, "recv")) return z_chan_recv(obj);
  if (!strcmp(m, "close")) return z_chan_close(obj);
  return z_errv("unknown channel method", m);
}

static ZVal z_ui_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env);

static ZVal z_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env) {
  obj = z_unwrap(obj);
  /* dotted path: split first segment */
  char seg[128];
  const char *dot = strchr(path, '.');
  if (dot) {
    size_t l = dot - path; if (l >= sizeof seg) l = sizeof seg - 1;
    memcpy(seg, path, l); seg[l] = 0;
    /* only "add.X" style two-level on UI; otherwise take whole path as method name */
    if (!strncmp(path, "add.", 4)) {
      return z_ui_method(obj, path, args, n, env);
    }
    /* recurse: obj.seg → then rest as method on result? For v1 treat
       whole path minus handled prefixes as single method name */
  }
  strncpy(seg, path, sizeof seg - 1); seg[sizeof seg - 1] = 0;
  /* fnType on fn values (§8.1) */
  if (obj.tag == Z_FUNC && !strcmp(seg, "fnType") && n) {
    ZFuncV *f = z_alloc(sizeof(ZFuncV));
    *f = *obj.u.fn;
    f->want = args[0];
    ZVal v = {0}; v.tag = Z_FUNC; v.u.fn = f; return v;
  }
  if (obj.tag == Z_TYPE) {
    /* method on type → treat as constructor-ish call */
    return z_call_type_named(obj, args, n, ZNOTHING);
  }
  if (obj.tag == Z_LIST || obj.tag == Z_TUPLE) return z_list_method(obj, seg, args, n, env);
  if (obj.tag == Z_MAP) return z_map_method(obj, seg, args, n);
  if (obj.tag == Z_STR) return z_str_method(obj, seg, args, n);
  if (obj.tag == Z_CHAN) return z_chan_method(obj, seg, args, n);
  if (obj.tag == Z_UI) return z_ui_method(obj, seg, args, n, env);
  if (obj.tag == Z_ERR || obj.tag == Z_POISON) return obj;
  if (obj.tag == Z_BYTES) {
    if (!strcmp(seg, "len")) return z_int((long long)obj.u.by.n);
  }
  if (obj.tag == Z_REC) {
    /* method = field that's callable? try field then call */
    ZVal fv = z_field(obj, seg);
    if (fv.tag == Z_FUNC) return z_call_val(fv, args, n);
    return fv;
  }
  {
    char buf[256];
    snprintf(buf, sizeof buf, "no method `%s` on %s", seg, z_tag_name(obj.tag));
    return z_errv(buf, "method");
  }
}

/* ui.add.<kind>(args) → child element; ui element methods */
static ZVal z_ui_method(ZVal obj, const char *path, ZVal *args, int n, ZEnv *env) {
  (void)env;
  obj = z_unwrap(obj);
  if (obj.tag != Z_UI || !obj.u.ui) return z_errv("ui method on non-element", path);
  ZUi *u = obj.u.ui;
  /* add.<kind> */
  if (!strncmp(path, "add.", 4)) {
    const char *kind = path + 4;
    ZVal child = z_ui_new(kind);
    /* first string arg → main text/value prop */
    if (n > 0) {
      const char *pn = (!strcmp(kind, "text") || !strcmp(kind, "label") ||
                        !strcmp(kind, "button") || !strcmp(kind, "heading")) ? "text" :
                       (!strcmp(kind, "input") ? "placeholder" : "value");
      char *vs = z_val_str(args[0]);
      z_ui_set(child.u.ui, pn, z_str(vs));
    }
    /* named-ish extra args become props p0..pn */
    for (int i = 1; i < n; i++) {
      char pn[32]; snprintf(pn, sizeof pn, "arg%d", i);
      z_ui_set(child.u.ui, pn, args[i]);
    }
    if (u->cn == u->ccap) { u->ccap = u->ccap ? u->ccap * 2 : 8; u->ch = realloc(u->ch, u->ccap * sizeof(ZUi *)); }
    u->ch[u->cn++] = child.u.ui;
    return child;
  }
  /* run / show etc on window */
  if (!strcmp(path, "run") || !strcmp(path, "show")) {
    z_ui_render(u, 0);
    return ZNOTHING;
  }
  {
    char buf[128]; snprintf(buf, sizeof buf, "unknown ui method `%s`", path);
    return z_errv(buf, "ui");
  }
}

/* ---------- env / args ---------- */

/* ---------- read ---------- */
static ZVal z_read_file(ZVal pathv) {
  pathv = z_unwrap(pathv);
  const char *p = pathv.tag == Z_STR ? pathv.u.s : z_val_str(pathv);
  FILE *f = fopen(p, "rb");
  if (!f) return z_errv("cannot open file", "read");
  fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
  char *buf = z_alloc(n + 1);
  fread(buf, 1, n, f); buf[n] = 0; fclose(f);
  return z_str_take(buf);
}

/* ---------- spawn ---------- */
typedef struct { ZVal fn; ZVal *args; int n; } ZSpawnJob;
static void *z_spawn_run(void *p) {
  ZSpawnJob *j = p;
  z_call_val(j->fn, j->args, j->n);
  free(j->args); free(j);
  return NULL;
}
static ZVal z_spawn(ZVal fn, ZVal *args, int n, ZEnv *env) {
  (void)env;
  ZSpawnJob *j = z_alloc(sizeof(ZSpawnJob));
  j->fn = fn; j->n = n;
  j->args = z_alloc(sizeof(ZVal) * (n + 1));
  memcpy(j->args, args, sizeof(ZVal) * n);
  pthread_t th;
  if (pthread_create(&th, NULL, z_spawn_run, j) != 0) {
    /* fallback: run sequentially */
    z_call_val(fn, args, n);
    free(j->args); free(j);
    return ZNOTHING;
  }
  pthread_detach(th);
  return ZNOTHING;
}

/* ---------- json ---------- */
static char *z_json_escape(const char *s) {
  ZSb b = {0};
  sb_ch(&b, '"');
  for (; s && *s; s++) {
    unsigned char c = (unsigned char)*s;
    switch (c) {
      case '"': sb_add(&b, "\\\""); break;
      case '\\': sb_add(&b, "\\\\"); break;
      case '\n': sb_add(&b, "\\n"); break;
      case '\t': sb_add(&b, "\\t"); break;
      case '\r': sb_add(&b, "\\r"); break;
      default:
        if (c < 0x20) { char t[8]; snprintf(t, sizeof t, "\\u%04x", c); sb_add(&b, t); }
        else sb_ch(&b, (char)c);
    }
  }
  sb_ch(&b, '"');
  return b.p ? b.p : z_strdup("\"\"");
}
static void z_json_str(ZSb *b, ZVal v);
static void z_json_val(ZSb *b, ZVal v) {
  v = z_unwrap(v);
  switch (v.tag) {
    case Z_INT: { char t[32]; snprintf(t, sizeof t, "%lld", v.u.i); sb_add(b, t); break; }
    case Z_FLOAT: { char t[64]; snprintf(t, sizeof t, "%.17g", v.u.f); sb_add(b, t); break; }
    case Z_BOOL: sb_add(b, v.u.b ? "true" : "false"); break;
    case Z_NOTHING: sb_add(b, "null"); break;
    case Z_STR: z_json_str(b, v); break;
    case Z_BYTES: {
      sb_add(b, "\"");
      for (size_t i = 0; i < v.u.by.n; i++) { char t[8]; snprintf(t, sizeof t, "\\u%04x", v.u.by.p[i]); sb_add(b, t); }
      sb_add(b, "\""); break;
    }
    case Z_LIST: case Z_TUPLE: {
      sb_add(b, "[");
      for (size_t i = 0; i < v.u.vec->n; i++) {
        if (i) sb_add(b, ",");
        z_json_val(b, v.u.vec->v[i]);
      }
      sb_add(b, "]"); break;
    }
    case Z_MAP: {
      sb_add(b, "{");
      for (size_t i = 0; i < v.u.map->n; i++) {
        if (i) sb_add(b, ",");
        z_json_str(b, v.u.map->p[i].k);
        sb_add(b, ":");
        z_json_val(b, v.u.map->p[i].v);
      }
      sb_add(b, "}"); break;
    }
    case Z_REC: {
      sb_add(b, "{");
      for (size_t i = 0; i < v.u.rec->n; i++) {
        if (i) sb_add(b, ",");
        char *q = z_json_escape(v.u.rec->names[i]);
        sb_add(b, q);
        sb_add(b, ":");
        z_json_val(b, v.u.rec->vals[i]);
      }
      sb_add(b, "}"); break;
    }
    case Z_ERR: case Z_POISON: {
      sb_add(b, "{\"error\":");
      char *q = z_json_escape(z_val_str(v));
      sb_add(b, q);
      sb_add(b, "}");
      break;
    }
    default: z_json_str(b, z_str(z_val_str(v))); break;
  }
}
static void z_json_str(ZSb *b, ZVal v) {
  char *q = z_json_escape(z_val_str(v));
  sb_add(b, q);
}
static ZVal z_json_encode(ZVal v) {
  ZSb b = {0};
  z_json_val(&b, v);
  ZVal r = {0}; r.tag = Z_STR; r.u.s = b.p ? b.p : z_strdup("null");
  return r;
}

/* minimal json decode */
typedef struct { const char *p; } ZJ;
static void jws(ZJ *j) { while (*j->p && isspace((unsigned char)*j->p)) j->p++; }
static ZVal jval(ZJ *j);
static ZVal jstr(ZJ *j) {
  if (*j->p != '"') return z_errv("json: expected string", "json");
  j->p++;
  ZSb b = {0};
  while (*j->p && *j->p != '"') {
    if (*j->p == '\\') {
      j->p++;
      switch (*j->p) {
        case 'n': sb_add(&b, "\n"); break;
        case 't': sb_add(&b, "\t"); break;
        case 'r': sb_add(&b, "\r"); break;
        case 'u': j->p += 4; sb_add(&b, "?"); break;
        default: sb_add(&b, (char[]){*j->p, 0}); break;
      }
      j->p++;
    } else {
      char c = *j->p++;
      sb_add(&b, (char[]){c, 0});
    }
  }
  if (*j->p == '"') j->p++;
  ZVal v = {0}; v.tag = Z_STR; v.u.s = b.p ? b.p : z_strdup(""); return v;
}
static ZVal jnum(ZJ *j) {
  char *end;
  if (strchr(j->p, '.') || strchr(j->p, 'e') || strchr(j->p, 'E')) {
    double d = strtod(j->p, &end);
    j->p = end;
    return z_float(d);
  }
  long long x = strtoll(j->p, &end, 10);
  j->p = end;
  return z_int(x);
}
static ZVal jarr(ZJ *j) {
  j->p++; /* [ */
  ZVal v = z_listv(NULL, 0);
  jws(j);
  if (*j->p == ']') { j->p++; return v; }
  for (;;) {
    z_vec_push(v.u.vec, jval(j));
    jws(j);
    if (*j->p == ',') { j->p++; continue; }
    if (*j->p == ']') { j->p++; break; }
    break;
  }
  return v;
}
static ZVal jobj(ZJ *j) {
  j->p++; /* { */
  ZVal v = z_map_new();
  jws(j);
  if (*j->p == '}') { j->p++; return v; }
  for (;;) {
    jws(j);
    ZVal k = jstr(j);
    jws(j);
    if (*j->p == ':') j->p++;
    ZVal val = jval(j);
    z_map_setp(v.u.map, k, val);
    jws(j);
    if (*j->p == ',') { j->p++; continue; }
    if (*j->p == '}') { j->p++; break; }
    break;
  }
  return v;
}
static ZVal jval(ZJ *j) {
  jws(j);
  char c = *j->p;
  if (c == '"') return jstr(j);
  if (c == '[') return jarr(j);
  if (c == '{') return jobj(j);
  if (c == 't') { j->p += 4; return z_bool(1); }
  if (c == 'f') { j->p += 5; return z_bool(0); }
  if (c == 'n') { j->p += 4; return ZNOTHING; }
  return jnum(j);
}
static ZVal z_json_decode(ZVal s) {
  s = z_unwrap(s);
  if (s.tag != Z_STR) return z_errv("json.decode needs str", "json");
  ZJ j = { s.u.s };
  return jval(&j);
}

/* ---------- route / nav ---------- */
static ZVal z_route(ZVal *args, int n) {
  /* route("/path", args...) → Err in headless mode */
  (void)args; (void)n;
  return z_errv("navigation unavailable in this build", "route");
}

/* ---------- ui render ---------- */
static void z_ui_render(ZUi *u, int depth) {
  for (int i = 0; i < depth; i++) fputs("  ", stdout);
  const char *kind = u->kind[0] ? u->kind : "div";
  /* text-ish content */
  const char *txt = NULL;
  for (size_t i = 0; i < u->pn; i++) {
    ZVal k = z_unwrap(u->props[i].k);
    if (k.tag == Z_STR && (!strcmp(k.u.s, "text") || !strcmp(k.u.s, "value") ||
        !strcmp(k.u.s, "placeholder"))) {
      ZVal v = z_unwrap(u->props[i].v);
      if (v.tag == Z_STR) txt = v.u.s;
      else { static char tmp[256]; snprintf(tmp, sizeof tmp, "%s", z_val_str(v)); txt = tmp; }
      break;
    }
  }
  printf("[%s%s%s]\n", kind, txt ? " " : "", txt ? txt : "");
  for (size_t i = 0; i < u->cn; i++) z_ui_render(u->ch[i], depth + 1);
}

/* ---------- shell ---------- */
static ZVal z_shell(ZVal cmdv) {
  cmdv = z_unwrap(cmdv);
  const char *c = cmdv.tag == Z_STR ? cmdv.u.s : z_val_str(cmdv);
  FILE *p = popen(c, "r");
  if (!p) return z_errv("shell failed", "shell");
  ZSb b = {0}; char tmp[4096];
  size_t r;
  while ((r = fread(tmp, 1, sizeof tmp - 1, p)) > 0) { tmp[r] = 0; sb_add(&b, tmp); }
  pclose(p);
  ZVal v = {0}; v.tag = Z_STR; v.u.s = b.p ? b.p : z_strdup(""); return v;
}

/* ---------- clock ---------- */
static ZVal z_clock(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return z_float(ts.tv_sec + ts.tv_nsec / 1e9);
}

/* ---------- stdin / time helpers ---------- */
static ZVal z_read_token(void) {
  char buf[4096];
  if (!fgets(buf, sizeof buf, stdin)) return ZNOTHING;
  char *tok = strtok(buf, " \t\r\n");
  if (!tok) return z_str("");
  /* int → float → str, mirroring interp */
  char *end;
  long long i = strtoll(tok, &end, 10);
  if (*end == 0 && end != tok) return z_int(i);
  double d = strtod(tok, &end);
  if (*end == 0 && end != tok) return z_float(d);
  return z_str(tok);
}
static char *now_iso(void) {
  time_t t = time(NULL);
  struct tm tmv;
  gmtime_r(&t, &tmv);
  char *b = z_alloc(32);
  strftime(b, 32, "%Y-%m-%dT%H:%M:%SZ", &tmv);
  return b;
}

/* ---------- print.* ---------- */
static void z_print_args(ZVal *pos, int n) {
  for (int i = 0; i < n; i++) {
    if (i) putchar(' ');
    char *s = z_val_str(pos[i]);
    fputs(s, stdout);
  }
}

/* ---------- thisShell ---------- */
static ZVal z_shell_value(void) {
  ZVal rec = z_rec_new("thisShell");
  /* shellOut as a builtin-marker fn value */
  ZFuncV *f = z_alloc(sizeof(ZFuncV));
  memset(f, 0, sizeof *f);
  f->name = "thisShell.shellOut"; f->want = z_type_prim("str");
  ZVal fv = {0}; fv.tag = Z_FUNC; fv.u.fn = f;
  z_rec_add(rec.u.rec, "shellOut", "fn", fv);
  return rec;
}

/* ---------- builtin dispatch ---------- */
static ZVal z_builtin_named(const char *name, ZVal *pos, int npos, ZVal named, ZEnv *env) {
  (void)named;
  /* dotted builtins */
  if (!strcmp(name, "print.nl") || !strcmp(name, "print.fmt")) {
    z_print_args(pos, npos); putchar('\n'); return ZNOTHING;
  }
  if (!strcmp(name, "print.raw")) { z_print_args(pos, npos); return ZNOTHING; }
  if (!strcmp(name, "print.err") || !strcmp(name, "print.errNl")) {
    for (int i = 0; i < npos; i++) {
      if (i) fputc(' ', stderr);
      char *s = z_val_str(pos[i]); fputs(s, stderr);
    }
    if (name[strlen(name)-2] == 'N') fputc('\n', stderr);
    return ZNOTHING;
  }
  if (!strcmp(name, "env.get")) {
    ZVal k = npos ? z_unwrap(pos[0]) : ZNOTHING;
    const char *ks = k.tag == Z_STR ? k.u.s : z_val_str(k);
    const char *v = getenv(ks);
    return v ? z_str(v) : ZNOTHING;
  }
  if (!strcmp(name, "env.set")) {
    if (npos >= 2) {
      ZVal k = z_unwrap(pos[0]), v = z_unwrap(pos[1]);
      const char *ks = k.tag == Z_STR ? k.u.s : z_val_str(k);
      const char *vs = v.tag == Z_STR ? v.u.s : z_val_str(v);
      setenv(ks, vs, 1);
    }
    return ZNOTHING;
  }
  if (!strcmp(name, "clock.now")) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return z_float(ts.tv_sec + ts.tv_nsec / 1e9);
  }
  if (!strcmp(name, "clock.nowIso")) return z_str_take(now_iso());
  if (!strcmp(name, "ui.window") || !strcmp(name, "window")) {
    /* ui.window(title?, w?, h?) → root Z_UI element */
    const char *kind = "window";
    ZVal v = z_ui_new(kind);
    if (npos > 0) {
      char *t = z_val_str(pos[0]);
      z_ui_set(v.u.ui, "title", z_str(t));
    }
    return v;
  }
  if (!strcmp(name, "ui.run")) return ZNOTHING;
  if (!strcmp(name, "nav.go")) return z_errv("navigation unavailable", "nav.go");
  if (!strcmp(name, "math.abs")) {
    if (!npos) return ZNOTHING;
    ZVal a = z_unwrap(pos[0]);
    if (a.tag == Z_INT) return z_int(a.u.i < 0 ? -a.u.i : a.u.i);
    if (a.tag == Z_FLOAT) return z_float(fabs(a.u.f));
    return a;
  }
  if (!strcmp(name, "math.min") || !strcmp(name, "math.max")) {
    int ismin = name[5] == 'i';
    double best = ismin ? 1e300 : -1e300; int any = 0;
    for (int i = 0; i < npos; i++) {
      ZVal a = z_unwrap(pos[i]);
      if (a.tag != Z_INT && a.tag != Z_FLOAT) continue;
      double d = z_num(a); any = 1;
      if (ismin ? d < best : d > best) best = d;
    }
    return any ? z_float(best) : z_int(0);
  }
  if (!strcmp(name, "json.parse")) {
    ZVal s = npos ? pos[0] : z_str("");
    return z_json_decode(s);
  }
  if (!strcmp(name, "json.stringify")) {
    return z_json_encode(npos ? pos[0] : ZNOTHING);
  }
  if (!strcmp(name, "thisShell.shellOut")) return npos ? z_shell(pos[0]) : z_errv("shellOut needs a command", "shell");
  /* bare builtins */
  if (!strcmp(name, "read")) return z_read_token();
  if (!strcmp(name, "readInto") || !strcmp(name, "readDialogue")) {
    if (!strcmp(name, "readDialogue") && npos) {
      char *p = z_val_str(pos[0]);
      printf(">: %s", p); fflush(stdout);
    }
    ZVal t = z_read_token();
    if (t.tag == Z_NOTHING && !strcmp(name, "readDialogue")) {
      /* read whole line instead */
      char buf[4096];
      if (fgets(buf, sizeof buf, stdin)) {
        size_t l = strlen(buf);
        while (l && (buf[l-1] == '\n' || buf[l-1] == '\r')) buf[--l] = 0;
        return z_str(buf);
      }
      return z_str("");
    }
    return t;
  }
  if (!strcmp(name, "assert")) {
    int ok = npos && z_truthy(pos[0]);
    if (!ok) {
      char *m = npos > 1 ? z_val_str(pos[1]) : z_strdup("assertion failed");
      return z_errv(m, "assert");
    }
    return ZNOTHING;
  }
  if (!strcmp(name, "typeof")) {
    if (!npos) return z_str("nothing");
    ZVal v = z_unwrap(pos[0]);
    if (v.tag == Z_TYPE) return z_str(v.u.tname);
    return z_str(z_tag_name(v.tag));
  }
  if (!strcmp(name, "len")) {
    if (!npos) return z_int(0);
    ZVal v = z_unwrap(pos[0]);
    if (v.tag == Z_STR) return z_int((long long)strlen(v.u.s));
    if (v.tag == Z_LIST || v.tag == Z_TUPLE) return z_int((long long)v.u.vec->n);
    if (v.tag == Z_MAP) return z_int((long long)v.u.map->n);
    if (v.tag == Z_REC) return z_int((long long)v.u.rec->n);
    return z_int((long long)strlen(z_val_str(v)));
  }
  if (!strcmp(name, "str")) {
    return npos ? z_str_take(z_val_str(pos[0])) : z_str("");
  }
  if (!strcmp(name, "int")) {
    ZVal a = npos ? z_unwrap(pos[0]) : ZNOTHING;
    if (a.tag == Z_INT) return a;
    if (a.tag == Z_FLOAT) return z_int((long long)a.u.f);
    if (a.tag == Z_STR) return z_int(strtoll(a.u.s, NULL, 10));
    if (a.tag == Z_BOOL) return z_int(a.u.b);
    return z_int(0);
  }
  if (!strcmp(name, "float")) {
    ZVal a = npos ? z_unwrap(pos[0]) : ZNOTHING;
    if (a.tag == Z_INT || a.tag == Z_FLOAT) return z_float(z_num(a));
    if (a.tag == Z_STR) return z_float(strtod(a.u.s, NULL));
    return z_float(0);
  }
  if (!strcmp(name, "List") || !strcmp(name, "Map")) {
    /* List(int), Map(str, V) — type values constructed by call (§7.4) */
    if (!strcmp(name, "List")) {
      ZVal t = z_type_generic("List", pos, npos);
      return t;
    }
    return z_type_generic("Map", pos, npos);
  }
  if (!strcmp(name, "weak")) return npos ? z_weak(pos[0]) : ZNOTHING;
  if (!strcmp(name, "channel")) return z_chan_pair();
  if (!strcmp(name, "spawn")) {
    /* spawn(fnCall) — cbackend extracts fn+args and calls z_spawn directly;
       reaching here means dynamic callee: run sequentially */
    if (npos) return z_call_val(pos[0], NULL, 0);
    return ZNOTHING;
  }
  if (!strcmp(name, "exit")) {
    int code = 0;
    if (npos) { ZVal c = z_unwrap(pos[0]); if (c.tag == Z_INT) code = (int)c.u.i; }
    fflush(stdout); exit(code);
  }
  if (!strcmp(name, "route")) return z_route(pos, npos);
  if (!strcmp(name, "onClick")) {
    /* onClick(el, fn) — record handler on element */
    ZVal el = ZNOTHING, fn = ZNOTHING;
    for (int i = 0; i < npos; i++) {
      ZVal a = z_unwrap(pos[i]);
      if (a.tag == Z_UI && el.tag != Z_UI) el = a;
      if (a.tag == Z_FUNC) fn = a;
    }
    if (el.tag == Z_UI && fn.tag == Z_FUNC && el.u.ui) {
      z_vec_push(el.u.ui->handlers.u.vec, fn);
    }
    return ZNOTHING;
  }
  if (!strcmp(name, "parallel")) return ZNOTHING;
  if (!strcmp(name, "readFile") || !strcmp(name, "read.file")) {
    return npos ? z_read_file(pos[0]) : z_errv("read needs path", "read");
  }
  {
    char buf[256];
    snprintf(buf, sizeof buf, "unknown builtin `%s`", name);
    return z_errv(buf, "builtin");
  }
}
