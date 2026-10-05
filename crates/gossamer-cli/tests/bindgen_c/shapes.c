#include <stdlib.h>
#include <string.h>
#include "shapes.h"

struct shapes_registry {
    char name[64];
    size_t capacity;
};

int shapes_created = 0;

int32_t shapes_area(shapes_box box) {
    return (box.max.x - box.min.x) * (box.max.y - box.min.y);
}

shapes_box shapes_grow(shapes_box box, int32_t by) {
    box.min.x -= by;
    box.min.y -= by;
    box.max.x += by;
    box.max.y += by;
    return box;
}

shapes_registry *shapes_open(const char *name, size_t capacity) {
    shapes_registry *registry = calloc(1, sizeof *registry);
    strncpy(registry->name, name, sizeof registry->name - 1);
    registry->capacity = capacity;
    shapes_created++;
    return registry;
}

size_t shapes_name_len(const shapes_registry *registry) {
    return strlen(registry->name);
}

int shapes_each(shapes_registry *registry, shapes_visit visit, void *context) {
    int total = 0;
    for (size_t i = 0; i < registry->capacity; i++) {
        total += visit(context, (int32_t)i);
    }
    return total;
}

double shapes_value_real(shapes_value value) {
    return value.real;
}

void shapes_close(shapes_registry *registry) {
    free(registry);
}

int shapes_printf(const char *format, ...) {
    (void)format;
    return 0;
}

/* Layout facts the bindings must agree with. */
size_t shapes_label_size(void) { return sizeof(struct shapes_label); }
size_t shapes_label_weight_offset(void) { return offsetof(struct shapes_label, weight); }
size_t shapes_box_size(void) { return sizeof(shapes_box); }
