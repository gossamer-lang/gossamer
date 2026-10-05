/* A header covering what `gos bindgen --c` writes declarations for. */
#ifndef SHAPES_H
#define SHAPES_H

#include <stddef.h>
#include <stdint.h>

#define SHAPES_VERSION 3
#define SHAPES_NAME "shapes"
#define SHAPES_FLAG (0x10u)
#define SHAPES_SCALE 1.5

typedef struct shapes_registry shapes_registry;

typedef struct shapes_point {
    int32_t x;
    int32_t y;
} shapes_point;

typedef struct {
    shapes_point min;
    shapes_point max;
} shapes_box;

struct shapes_label {
    uint8_t tag[4];
    const char *text;
    int (*measure)(const char *text, int scale);
    union {
        int32_t whole;
        float part;
    } weight;
};

typedef union shapes_value {
    int64_t integer;
    double real;
} shapes_value;

enum shapes_kind { SHAPES_CIRCLE, SHAPES_SQUARE = 4, SHAPES_TRIANGLE };

typedef int (*shapes_visit)(void *context, int32_t index);

extern int shapes_created;

int32_t shapes_area(shapes_box box);
shapes_box shapes_grow(shapes_box box, int32_t by);
shapes_registry *shapes_open(const char *name, size_t capacity);
size_t shapes_name_len(const shapes_registry *registry);
int shapes_each(shapes_registry *registry, shapes_visit visit, void *context);
double shapes_value_real(shapes_value value);
void shapes_close(shapes_registry *registry);
int shapes_printf(const char *format, ...);

#endif
