#ifndef ZINPUT_X11_NATIVE_H
#define ZINPUT_X11_NATIVE_H
#include <stddef.h>
#include <stdint.h>
typedef struct ZixServer ZixServer;
typedef struct {
    uint32_t kind, active;
    uint64_t generation;
    uint32_t serial, keycode;
    uint64_t token;
    uint32_t time, pressed, keysym, modifiers, text_length;
    unsigned char text[64];
} ZixEvent;
ZixServer *zix_open(const char *name, char *error, size_t capacity);
void zix_close(ZixServer *server);
const char *zix_error(ZixServer *server);
int zix_dispatch(ZixServer *server, int timeout_ms);
int zix_next(ZixServer *server, ZixEvent *event);
void zix_context(ZixServer *server, ZixEvent *event);
int zix_forward(ZixServer *server, uint64_t token, uint64_t generation, uint32_t serial);
int zix_update(ZixServer *server, uint64_t generation, uint32_t serial,
               const unsigned char *commit, size_t length, int clear);
int zix_popup_supported(ZixServer *server);
int zix_popup(ZixServer *server, uint64_t generation, uint32_t serial,
              const unsigned char *pixels, uint32_t width, uint32_t height);
int zix_hide_popup(ZixServer *server);
#endif
