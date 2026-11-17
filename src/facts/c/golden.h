/* A header the extractor's facts are fixed for. Not compiled. */
#ifndef GOLDEN_H
#define GOLDEN_H

#include <stddef.h>

#define MAX(a, b) ((a) > (b) ? (a) : (b))

enum color { RED, GREEN, BLUE };

struct point {
    int x;
    int y;
};

typedef int (*compare)(const void *, const void *);

int area(int width, int height);
const char *describe(enum color c);

static inline int point_sum(struct point p) {
    return p.x + p.y;
}

static inline int sign(int n) {
    return n > 0 ? 1 : n < 0 ? -1 : 0;
}

#endif
