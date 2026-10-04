/* A small C library exercising every shape the Gossamer FFI binds: opaque
 * handles, nullable results, out-parameters, caller- and callee-owned
 * buffers, structs with pointer fields, callbacks run during a call and from
 * a later call, a callback from a thread the library starts, and function
 * pointers handed back. The tests build it as a shared library. */

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

EXPORT int slice_sum(const Slice *s) {
    int sum = 0;
    for (size_t i = 0; i < s->len; i++) {
        sum += s->data[i];
    }
    return sum + s->tag;
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
