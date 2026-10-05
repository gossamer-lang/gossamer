/* A small C library exercising every shape the Gossamer FFI binds: opaque
 * handles, nullable results, out-parameters, caller- and callee-owned
 * buffers, structs with pointer fields, callbacks run during a call and from
 * a later call, a callback from a thread the library starts, and function
 * pointers handed back, and error codes set or left alone. The tests build it
 * as a shared library. */

#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#define EXPORT __declspec(dllexport)
#else
#include <pthread.h>
#define EXPORT __attribute__((visibility("default")))
#endif

typedef struct Counter {
    int value;
    int resets;
} Counter;

EXPORT Counter *counter_new(int start) {
    Counter *c = (Counter *)malloc(sizeof(Counter));
    if (c == NULL) {
        return NULL;
    }
    c->value = start;
    c->resets = 0;
    return c;
}

EXPORT void counter_add(Counter *c, int amount) { c->value += amount; }

EXPORT int counter_get(const Counter *c) { return c->value; }

EXPORT void counter_reset(Counter *c) {
    c->value = 0;
    c->resets += 1;
}

EXPORT int counter_resets(const Counter *c) { return c->resets; }

EXPORT void counter_free(Counter *c) { free(c); }

/* NULL for a key that has no counter. */
static Counter shared_counter = {42, 0};
EXPORT Counter *counter_find(int key) { return key == 1 ? &shared_counter : NULL; }

/* `T **`: a counter through an out-parameter; returns 0 on success. */
EXPORT int counter_open(int start, Counter **out) {
    *out = counter_new(start);
    return *out == NULL ? -1 : 0;
}

/* `int *` out-parameters. */
EXPORT int split_pair(int64_t value, int32_t *high, int32_t *low) {
    *high = (int32_t)(value >> 32);
    *low = (int32_t)(value & 0xffffffff);
    return 2;
}

/* A double out-parameter. */
EXPORT void scale(double factor, double *value) { *value *= factor; }

/* Callee-owned buffer: valid until the next call that changes the counter. */
static uint8_t borrowed[8];
EXPORT void counter_bytes(const Counter *c, const uint8_t **data, size_t *len) {
    for (int i = 0; i < 4; i++) {
        borrowed[i] = (uint8_t)((c->value >> (8 * i)) & 0xff);
    }
    *data = borrowed;
    *len = 4;
}

/* Caller-owned buffer the library allocates; free with `buffer_free`. */
EXPORT uint8_t *buffer_make(size_t len, uint8_t seed) {
    uint8_t *data = (uint8_t *)malloc(len);
    for (size_t i = 0; i < len; i++) {
        data[i] = (uint8_t)(seed + i);
    }
    return data;
}

EXPORT void buffer_free(uint8_t *data) { free(data); }

/* Transferred ownership: the library takes the buffer and frees it. */
EXPORT int buffer_take_sum(uint8_t *data, size_t len) {
    int sum = 0;
    for (size_t i = 0; i < len; i++) {
        sum += data[i];
    }
    free(data);
    return sum;
}

/* Fills memory the caller owns. */
EXPORT void buffer_fill(uint8_t *data, size_t len, uint8_t seed) {
    for (size_t i = 0; i < len; i++) {
        data[i] = (uint8_t)(seed * (i + 1));
    }
}

/* A NUL-terminated string the library owns. */
EXPORT const char *greeting(void) { return "hello from C"; }

/* A struct with a pointer field, read and written through pointers. */
typedef struct Slice {
    const uint8_t *data;
    size_t len;
    int32_t tag;
} Slice;

static const uint8_t slice_bytes[] = {10, 20, 30, 40};

/* By value: 24 bytes, which every convention passes in memory or by pointer. */
EXPORT int slice_sum(Slice s) {
    int sum = 0;
    for (size_t i = 0; i < s.len; i++) {
        sum += s.data[i];
    }
    return sum + s.tag;
}

EXPORT void slice_fill(Slice *out) {
    out->data = slice_bytes;
    out->len = 4;
    out->tag = 7;
}

/* Callbacks run during the call. */
EXPORT int apply(int (*f)(int, void *), int x, void *ctx) { return f(x, ctx) + f(x + 1, ctx); }

EXPORT void each(const int32_t *xs, size_t n, void (*f)(int32_t, void *), void *ctx) {
    for (size_t i = 0; i < n; i++) {
        f(xs[i], ctx);
    }
}

EXPORT double map_double(double (*f)(double), double x) { return f(x) * 2.0; }

EXPORT float map_float(float (*f)(float), float x) { return f(x) + 0.5f; }

EXPORT int64_t map_wide(int64_t (*f)(int8_t, uint16_t, int64_t, uint8_t), int64_t x) {
    return f(-3, 65000, x, 1);
}

EXPORT void *map_pointer(void *(*f)(void *), void *p) { return f(p); }

/* `unsigned char` has the ABI of a C `_Bool`, which MSVC's default C mode lacks. */
EXPORT int map_flag(int (*f)(unsigned char), int flag) { return f(flag != 0); }

/* A callback stored now and run by a later call on the same thread. */
static void (*stored_handler)(int, void *) = NULL;
static void *stored_context = NULL;

EXPORT void set_handler(void (*f)(int, void *), void *ctx) {
    stored_handler = f;
    stored_context = ctx;
}

EXPORT void fire(int value) {
    if (stored_handler != NULL) {
        stored_handler(value, stored_context);
    }
}

/* Runs the stored handler on a thread the library starts. */
#ifdef _WIN32
static DWORD WINAPI fire_thread(LPVOID arg) {
    fire((int)(intptr_t)arg);
    return 0;
}

EXPORT void fire_on_thread(int value) {
    HANDLE thread = CreateThread(NULL, 0, fire_thread, (LPVOID)(intptr_t)value, 0, NULL);
    WaitForSingleObject(thread, INFINITE);
    CloseHandle(thread);
}
#else
static void *fire_thread(void *arg) {
    fire((int)(intptr_t)arg);
    return NULL;
}

EXPORT void fire_on_thread(int value) {
    pthread_t thread;
    pthread_create(&thread, NULL, fire_thread, (void *)(intptr_t)value);
    pthread_join(thread, NULL);
}
#endif

/* Function pointers handed back. */
static int doubled(int x) { return x * 2; }
static double halved(double x) { return x / 2.0; }

EXPORT int (*get_doubler(void))(int) { return doubled; }

EXPORT void *get_halver(void) { return (void *)halved; }

/* Error codes: what a call finds on entry, and calls that set or leave them. */
EXPORT int errno_on_entry(void) { return errno; }

EXPORT int fail_with_errno(int code) {
    errno = code;
    return -1;
}

EXPORT int succeed_quietly(void) { return 7; }

#ifdef _WIN32
EXPORT int last_error_on_entry(void) { return (int)GetLastError(); }

EXPORT int fail_with_last_error(int code) {
    SetLastError((DWORD)code);
    return -1;
}
#endif

/* A tagged union, its layout, and globals the program reaches by address. */
#include <stddef.h>

typedef struct Pair {
    float x;
    float y;
} Pair;

typedef struct Tagged {
    uint8_t tag;
    union {
        int32_t i;
        double d;
        Pair v;
    } value;
    int16_t after;
} Tagged;

EXPORT size_t tagged_size(void) { return sizeof(Tagged); }

EXPORT size_t tagged_value_offset(void) { return offsetof(Tagged, value); }

EXPORT size_t tagged_after_offset(void) { return offsetof(Tagged, after); }

EXPORT double tagged_read(const Tagged *t) {
    switch (t->tag) {
    case 0:
        return (double)t->value.i;
    case 1:
        return t->value.d;
    default:
        return (double)(t->value.v.x + t->value.v.y);
    }
}

EXPORT void tagged_store_double(Tagged *t, double d) {
    t->tag = 1;
    t->value.d = d;
    t->after = -7;
}

EXPORT int32_t gosffi_counter = 41;

EXPORT int32_t gosffi_bump(void) { return ++gosffi_counter; }

/* Arrays of structs. */
typedef struct Item {
    int32_t id;
    double weight;
} Item;

EXPORT double items_total(const Item *items, size_t n) {
    double total = 0.0;
    for (size_t i = 0; i < n; i++) {
        total += items[i].weight * (double)items[i].id;
    }
    return total;
}

EXPORT void items_scale(Item *items, size_t n, double by) {
    for (size_t i = 0; i < n; i++) {
        items[i].weight *= by;
        items[i].id += 100;
    }
}

/* Structs by value: a matrix of shapes each calling convention classifies
 * differently, and arguments that run the registers out. */
typedef struct V2 {
    float x;
    float y;
} V2;

typedef struct V3 {
    float x;
    float y;
    float z;
} V3;

typedef struct D4 {
    double a;
    double b;
    double c;
    double d;
} D4;

typedef struct IF {
    int32_t i;
    float f;
} IF;

typedef struct ID {
    int32_t i;
    double d;
} ID;

typedef struct BigI {
    int64_t a;
    int64_t b;
    int64_t c;
} BigI;

typedef struct Small {
    uint8_t a;
} Small;

typedef struct Arr3 {
    uint8_t b[3];
} Arr3;

typedef struct Nested {
    V2 v;
    int32_t n;
} Nested;

typedef struct PairI {
    int64_t a;
    int64_t b;
} PairI;

EXPORT V2 v2_scale(V2 v, float k) {
    V2 out = {v.x * k, v.y * k};
    return out;
}

EXPORT float v2_dot(V2 a, V2 b) { return a.x * b.x + a.y * b.y; }

EXPORT V3 v3_cross(V3 a, V3 b) {
    V3 out = {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x};
    return out;
}

EXPORT D4 d4_add(D4 a, D4 b) {
    D4 out = {a.a + b.a, a.b + b.b, a.c + b.c, a.d + b.d};
    return out;
}

EXPORT double id_sum(ID x) { return (double)x.i + x.d; }

EXPORT IF if_make(int32_t i, float f) {
    IF out = {i * 2, f / 2.0f};
    return out;
}

EXPORT BigI big_rot(BigI x) {
    BigI out = {x.b, x.c, x.a};
    return out;
}

EXPORT int32_t small_arr(Small s, Arr3 a) {
    return (int32_t)s.a * 1000 + a.b[0] * 100 + a.b[1] * 10 + a.b[2];
}

EXPORT Nested nested_bump(Nested n) {
    n.v.x += 1.0f;
    n.v.y += 2.0f;
    n.n += 3;
    return n;
}

EXPORT double many_then_v2(double a, double b, double c, double d, double e, double f, double g,
                           V2 v) {
    return a + b + c + d + e + f + g + v.x * 10.0 + v.y * 100.0;
}

EXPORT double many8_then_v2(double a, double b, double c, double d, double e, double f, double g,
                            double h, V2 v, double last) {
    return a + b + c + d + e + f + g + h + v.x * 10.0 + v.y * 100.0 + last * 1000.0;
}

EXPORT int64_t ints_then_pair(int64_t a, int64_t b, int64_t c, int64_t d, int64_t e, PairI p,
                              int64_t g) {
    return a + b + c + d + e + p.a * 100 + p.b * 1000 + g * 10000;
}

/* Callbacks taking and answering structs by value. */
EXPORT V2 apply_v2(V2 (*f)(V2, float), V2 v, float k) { return f(v, k); }

EXPORT double apply_d4(double (*f)(D4), D4 d) { return f(d); }

EXPORT BigI apply_big(BigI (*f)(BigI)) {
    BigI b = {1, 2, 3};
    return f(b);
}

EXPORT void *get_v2_scale(void) { return (void *)v2_scale; }

/* Memory a program views in place, and counters it changes atomically. */
static float samples[8] = {0.5f, 1.5f, 2.5f, 3.5f, 4.5f, 5.5f, 6.5f, 7.5f};

EXPORT float *samples_buffer(void) { return samples; }

EXPORT float samples_total(void) {
    float total = 0.0f;
    for (int i = 0; i < 8; i++) {
        total += samples[i];
    }
    return total;
}

static Item items_store[3] = {{1, 0.5}, {2, 1.5}, {3, 2.5}};

EXPORT Item *items_buffer(void) { return items_store; }

static int64_t atomic_counter = 0;

EXPORT int64_t *counter_address(void) { return &atomic_counter; }

EXPORT int64_t counter_value(void) { return atomic_counter; }

/* Callbacks from several threads the library starts, at once. */
typedef struct ThreadWork {
    int (*f)(int, void *);
    void *ctx;
    int first;
    int count;
    long long total;
} ThreadWork;

#ifdef _WIN32
static DWORD WINAPI work_thread(LPVOID arg) {
    ThreadWork *work = (ThreadWork *)arg;
    for (int i = 0; i < work->count; i++) {
        work->total += work->f(work->first + i, work->ctx);
    }
    return 0;
}
#else
static void *work_thread(void *arg) {
    ThreadWork *work = (ThreadWork *)arg;
    for (int i = 0; i < work->count; i++) {
        work->total += work->f(work->first + i, work->ctx);
    }
    return NULL;
}
#endif

EXPORT long long run_threads(int (*f)(int, void *), void *ctx, int threads, int count) {
    ThreadWork work[8];
    if (threads > 8) {
        threads = 8;
    }
#ifdef _WIN32
    HANDLE handles[8];
    for (int t = 0; t < threads; t++) {
        ThreadWork w = {f, ctx, t * count, count, 0};
        work[t] = w;
        handles[t] = CreateThread(NULL, 0, work_thread, &work[t], 0, NULL);
    }
    for (int t = 0; t < threads; t++) {
        WaitForSingleObject(handles[t], INFINITE);
        CloseHandle(handles[t]);
    }
#else
    pthread_t handles[8];
    for (int t = 0; t < threads; t++) {
        ThreadWork w = {f, ctx, t * count, count, 0};
        work[t] = w;
        pthread_create(&handles[t], NULL, work_thread, &work[t]);
    }
    for (int t = 0; t < threads; t++) {
        pthread_join(handles[t], NULL);
    }
#endif
    long long total = 0;
    for (int t = 0; t < threads; t++) {
        total += work[t].total;
    }
    return total;
}

/* Function pointers held in data: an ops table and a registration array. */
typedef struct Ops {
    int (*add)(int, int);
    int (*mul)(int, int);
    const char *name;
} Ops;

EXPORT int ops_run(const Ops *ops, int a, int b) {
    return ops->add(a, b) * 100 + ops->mul(a, b);
}

typedef struct Reg {
    const char *name;
    int (*f)(int);
} Reg;

EXPORT int reg_call(const Reg *regs, const char *name, int x) {
    for (const Reg *r = regs; r->name != NULL; r++) {
        if (strcmp(r->name, name) == 0) {
            return r->f(x);
        }
    }
    return -1;
}
