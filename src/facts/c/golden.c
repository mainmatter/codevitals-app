/* A file the extractor's facts are fixed for. Not compiled. */
#include <stdio.h>
#include "golden.h"

#define SQUARE(x) ((x) * (x))
#define LOG(...) fprintf(stderr, __VA_ARGS__)

static int count;

int area(int width, int height);

int area(int width, int height) {
    return width * height;
}

static int clamp(int value, int low, int high) {
    if (value < low) {
        return low;
    } else if (value > high) {
        return high;
    }
    return value;
}

/* Same shape as `clamp`. */
static int bound(int n, int min, int max) {
    if (n < min) {
        return min;
    } else if (n > max) {
        return max;
    }
    return n;
}

const char *describe(enum color c) {
    switch (c) {
    case RED:
        return "red";
    case GREEN:
    case BLUE:
        return c == GREEN ? "green" : "blue";
    default:
        return "unknown";
    }
}

int search(const int *items, size_t n, int wanted) {
    for (size_t i = 0; i < n; i++) {
        while (items[i] < 0 && i < n || wanted == 0) {
            do {
                i++;
            } while (i < n);
        }
        if (items[i] == wanted) {
            return (int)i;
        }
    }
    return -1;
}

int (*pick(int which))(int, int) {
    return which ? area : 0;
}

void log_all(const char *format, ...) {
    LOG("%s", format);
}

int old_style(a, b)
    int a;
    char b;
{
    return a + b;
}

#ifdef FAST
int speed(void) { return 2; }
#else
int speed(void) { return 1; }
#endif

int main(void) {
    LOG("%d\n", SQUARE(area(2, 3)));
    return count;
}
