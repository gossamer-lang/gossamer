/* Declarations that differ between targets, written without system headers
   so any host's clang reads it for any target. */
#ifndef TARGETS_H
#define TARGETS_H

struct targets_point {
    int x;
    int y;
};

int targets_common(struct targets_point p);

#ifdef _WIN32
int targets_windows_only(void);
#else
int targets_unix_only(void);
#endif

#endif
