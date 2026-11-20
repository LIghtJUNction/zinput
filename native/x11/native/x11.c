/* XIM wire protocol and encoding are handled by the installed xcb-imdkit.
 * All window, keyboard-map and buffer FFI stays in this Project Z adapter. */
#include "x11.h"
#include <xcb-imdkit/imdkit.h>
#include <xcb-imdkit/encoding.h>
#include <xcb/shape.h>
#include <xcb/xcbext.h>
#include <xkbcommon/xkbcommon.h>
#include <xkbcommon/xkbcommon-x11.h>
#include <poll.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

#define MAX_EVENTS 1024
#define MAX_CONTEXTS 256
#define MAX_TEXT 4000
#define MAX_POPUP 2048
#define MAX_POPUP_BYTES (8u * 1024u * 1024u)

typedef struct IcData {
    struct IcData *next;
    struct ZixServer *owner;
    xcb_im_input_context_t *ic;
    uint64_t id;
    xcb_window_t focus;
} IcData;

typedef struct {
    ZixEvent public;
    uint64_t ic_id;
    xcb_key_press_event_t key;
    uint16_t sequence;
} Entry;

struct ZixServer {
    xcb_connection_t *connection;
    xcb_screen_t *screen;
    int screen_number;
    xcb_im_t *im;
    xcb_window_t server_window;
    xcb_atom_t selection;
    int opened, closing, failed;
    char error[256];
    struct xkb_context *xkb_context;
    struct xkb_keymap *keymap;
    struct xkb_state *keyboard;
    int32_t device;
    IcData *contexts;
    size_t context_count;
    uint64_t next_ic, next_token, active_id, generation;
    uint32_t serial;
    Entry queue[MAX_EVENTS], pending;
    size_t head, count;
    int pending_valid;
    uint64_t forwarded[256];
    int popup_available, popup_mapped;
    uint8_t popup_depth;
    xcb_visualid_t popup_visual;
    xcb_window_t popup_window;
    xcb_colormap_t popup_colormap;
    xcb_pixmap_t popup_pixmap;
    xcb_gcontext_t popup_gc;
    uint32_t popup_width, popup_height;
};

static int error(ZixServer *s, const char *message) {
    snprintf(s->error, sizeof(s->error), "%s", message);
    return 0;
}
static int fatal(ZixServer *s, const char *message) {
    s->failed = 1;
    return error(s, message);
}
const char *zix_error(ZixServer *s) { return s->error; }

static int request_ok(ZixServer *s, xcb_void_cookie_t cookie, const char *message) {
    xcb_generic_error_t *failure = xcb_request_check(s->connection, cookie);
    if (!failure) return 1;
    free(failure);
    return error(s, message);
}
static int flush(ZixServer *s) {
    xcb_flush(s->connection);
    if (xcb_connection_has_error(s->connection))
        return fatal(s, "the X11 connection closed");
    return 1;
}
static IcData *find_ic(ZixServer *s, uint64_t id) {
    for (IcData *data = s->contexts; data; data = data->next)
        if (data->id == id) return data;
    return NULL;
}
static int current(ZixServer *s, uint64_t generation, uint32_t serial) {
    return !s->failed && !s->closing && s->active_id &&
           generation == s->generation && serial == s->serial;
}
void zix_context(ZixServer *s, ZixEvent *event) {
    memset(event, 0, sizeof(*event));
    event->kind = 1;
    event->active = s->active_id != 0;
    event->generation = s->generation;
    event->serial = s->serial;
}
static void push(ZixServer *s, const Entry *entry) {
    if (s->closing || s->failed) return;
    if (s->count == MAX_EVENTS) {
        fatal(s, "the XIM event queue exceeded its bound");
        return;
    }
    s->queue[(s->head + s->count) % MAX_EVENTS] = *entry;
    ++s->count;
}
int zix_hide_popup(ZixServer *s) {
    if (s->popup_window && s->popup_mapped) {
        xcb_unmap_window(s->connection, s->popup_window);
        s->popup_mapped = 0;
    }
    return flush(s);
}
static void change_context(ZixServer *s, uint64_t id) {
    if (s->generation == UINT64_MAX) {
        fatal(s, "input context generation exhausted");
        return;
    }
    s->active_id = id;
    ++s->generation;
    ++s->serial;
    zix_hide_popup(s);
    Entry entry = {0};
    zix_context(s, &entry.public);
    push(s, &entry);
}
static void free_ic(void *pointer) {
    IcData *data = pointer;
    ZixServer *s = data->owner;
    IcData **item = &s->contexts;
    while (*item && *item != data) item = &(*item)->next;
    if (*item) *item = data->next;
    if (s->context_count) --s->context_count;
    for (size_t i = 0; i < 256; ++i)
        if (s->forwarded[i] == data->id) s->forwarded[i] = 0;
    if (s->pending_valid && s->pending.ic_id == data->id) s->pending_valid = 0;
    free(data);
}
static void forward_entry(ZixServer *s, const Entry *entry) {
    IcData *data = find_ic(s, entry->ic_id);
    if (!data) return;
    uint8_t code = entry->key.detail;
    if (entry->public.pressed) {
        s->forwarded[code] = data->id;
    } else {
        int paired = s->forwarded[code] == data->id;
        s->forwarded[code] = 0;
        if (!paired) return;
    }
    xcb_key_press_event_t key = entry->key;
    xcb_im_forward_event_full(s->im, data->ic, entry->sequence, &key);
}
static void finish_pending(ZixServer *s) {
    if (s->pending_valid && !s->pending.public.pressed)
        s->forwarded[s->pending.key.detail] = 0;
    s->pending_valid = 0;
}
static int map_keyboard(ZixServer *s) {
    struct xkb_keymap *keymap = xkb_x11_keymap_new_from_device(
        s->xkb_context, s->connection, s->device, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!keymap) return fatal(s, "cannot obtain the X11 keyboard layout");
    struct xkb_state *state = xkb_x11_state_new_from_device(keymap, s->connection, s->device);
    if (!state) {
        xkb_keymap_unref(keymap);
        return fatal(s, "cannot obtain the X11 keyboard state");
    }
    if (s->keyboard) xkb_state_unref(s->keyboard);
    if (s->keymap) xkb_keymap_unref(s->keymap);
    s->keymap = keymap;
    s->keyboard = state;
    return 1;
}
static void map_key(ZixServer *s, Entry *entry) {
    xcb_key_press_event_t *key = &entry->key;
    /* X11 events carry authoritative core modifiers and the XKB group. This
       updates only our local lookup state, never the server's physical keys. */
    xkb_state_update_mask(s->keyboard, key->state & 0xffu, 0, 0,
                          0, 0, (key->state >> 13) & 3u);
    entry->public.keysym = xkb_state_key_get_one_sym(s->keyboard, key->detail);
    int length = xkb_state_key_get_utf8(s->keyboard, key->detail,
        (char *)entry->public.text, sizeof(entry->public.text));
    if (length < 0 || (size_t)length >= sizeof(entry->public.text)) {
        fatal(s, "XKB key text exceeded its bound");
        return;
    }
    entry->public.text_length = (uint32_t)length;
    static const char *names[] = {"Shift", "Mod1", "Control", "Mod4", "Lock", "Mod2"};
    static const uint32_t bits[] = {1, 2, 4, 8, 64, 128};
    for (size_t i = 0; i < sizeof(bits) / sizeof(bits[0]); ++i)
        if (xkb_state_mod_name_is_active(s->keyboard, names[i], XKB_STATE_MODS_EFFECTIVE) > 0)
            entry->public.modifiers |= bits[i];
}
static void callback(xcb_im_t *im, xcb_im_client_t *client, xcb_im_input_context_t *ic,
                     const xcb_im_packet_header_fr_t *header, void *frame,
                     void *argument, void *user_data) {
    (void)im; (void)client;
    ZixServer *s = user_data;
    if (!ic || s->closing || s->failed) return;
    IcData *data = xcb_im_input_context_get_data(ic);
    switch (header->major_opcode) {
    case XCB_XIM_CREATE_IC:
        if (s->context_count == MAX_CONTEXTS || s->next_ic == UINT64_MAX) {
            fatal(s, "the XIM context limit was reached");
            return;
        }
        data = calloc(1, sizeof(*data));
        if (!data) { fatal(s, "cannot allocate an XIM context"); return; }
        data->owner = s; data->ic = ic; data->id = ++s->next_ic;
        data->focus = xcb_im_input_context_get_focus_window(ic);
        data->next = s->contexts; s->contexts = data; ++s->context_count;
        xcb_im_input_context_set_data(ic, data, free_ic);
        break;
    case XCB_XIM_SET_IC_FOCUS:
        if (data && s->active_id != data->id) change_context(s, data->id);
        break;
    case XCB_XIM_UNSET_IC_FOCUS:
    case XCB_XIM_DESTROY_IC:
        if (data && s->active_id == data->id) change_context(s, 0);
        break;
    case XCB_XIM_RESET_IC:
        if (data && s->active_id == data->id) change_context(s, data->id);
        break;
    case XCB_XIM_SET_IC_VALUES:
        if (data) {
            xcb_window_t focus = xcb_im_input_context_get_focus_window(ic);
            if (focus != data->focus) {
                data->focus = focus;
                if (s->active_id == data->id) change_context(s, data->id);
            }
        }
        break;
    case XCB_XIM_FORWARD_EVENT: {
        if (!data || !frame || !argument) return;
        xcb_key_press_event_t *key = argument;
        uint8_t kind = key->response_type & 0x7fu;
        if (kind != XCB_KEY_PRESS && kind != XCB_KEY_RELEASE) return;
        if (s->next_token == UINT64_MAX) {
            fatal(s, "input event token exhausted"); return;
        }
        Entry entry = {0};
        zix_context(s, &entry.public);
        entry.public.kind = 2;
        entry.public.token = ++s->next_token;
        entry.public.keycode = key->detail;
        entry.public.time = key->time;
        entry.public.pressed = kind == XCB_KEY_PRESS;
        entry.ic_id = data->id;
        entry.key = *key;
        entry.sequence = ((xcb_im_forward_event_fr_t *)frame)->sequence_number;
        if (s->active_id != data->id) {
            /* Preserve an old client's unprocessed key in that same client;
               it must never enter the newly focused engine session. */
            forward_entry(s, &entry);
            return;
        }
        map_key(s, &entry);
        push(s, &entry);
        break;
    }
    default: break;
    }
}

static xcb_atom_t atom(ZixServer *s, const char *name) {
    xcb_intern_atom_reply_t *reply = xcb_intern_atom_reply(
        s->connection, xcb_intern_atom(s->connection, 0, (uint16_t)strlen(name), name), NULL);
    if (!reply) return XCB_ATOM_NONE;
    xcb_atom_t value = reply->atom;
    free(reply);
    return value;
}
static int popup_visual(ZixServer *s) {
    const xcb_setup_t *setup = xcb_get_setup(s->connection);
    if (setup->image_byte_order != XCB_IMAGE_ORDER_LSB_FIRST) return 0;
    const xcb_query_extension_reply_t *shape = xcb_get_extension_data(s->connection, &xcb_shape_id);
    if (!shape || !shape->present) return 0;
    xcb_shape_query_version_reply_t *version = xcb_shape_query_version_reply(
        s->connection, xcb_shape_query_version(s->connection), NULL);
    int supported = version && (version->major_version > 1 ||
        (version->major_version == 1 && version->minor_version >= 1));
    free(version);
    if (!supported) return 0;
    for (int preference = 0; preference < 2; ++preference) {
        uint8_t desired = preference == 0 ? 32 : s->screen->root_depth;
        int format_ok = 0;
        for (xcb_format_iterator_t f = xcb_setup_pixmap_formats_iterator(setup); f.rem; xcb_format_next(&f))
            if (f.data->depth == desired && f.data->bits_per_pixel == 32) format_ok = 1;
        if (!format_ok) continue;
        for (xcb_depth_iterator_t d = xcb_screen_allowed_depths_iterator(s->screen); d.rem; xcb_depth_next(&d)) {
            if (d.data->depth != desired) continue;
            for (xcb_visualtype_iterator_t v = xcb_depth_visuals_iterator(d.data); v.rem; xcb_visualtype_next(&v)) {
                if (v.data->_class == XCB_VISUAL_CLASS_TRUE_COLOR &&
                    v.data->red_mask == 0xff0000 && v.data->green_mask == 0xff00 && v.data->blue_mask == 0xff) {
                    s->popup_visual = v.data->visual_id;
                    s->popup_depth = desired;
                    return 1;
                }
            }
        }
    }
    return 0;
}
static int create_popup(ZixServer *s) {
    if (s->popup_window) return 1;
    xcb_colormap_t colormap = xcb_generate_id(s->connection);
    if (!request_ok(s, xcb_create_colormap_checked(s->connection, XCB_COLORMAP_ALLOC_NONE,
        colormap, s->screen->root, s->popup_visual), "cannot create the candidate colormap")) return 0;
    xcb_window_t window = xcb_generate_id(s->connection);
    const uint32_t values[] = {0, 0, 1, XCB_EVENT_MASK_EXPOSURE, colormap};
    if (!request_ok(s, xcb_create_window_checked(s->connection, s->popup_depth, window,
        s->screen->root, 0, 0, 1, 1, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT, s->popup_visual,
        XCB_CW_BACK_PIXEL | XCB_CW_BORDER_PIXEL | XCB_CW_OVERRIDE_REDIRECT |
        XCB_CW_EVENT_MASK | XCB_CW_COLORMAP, values), "cannot create the candidate window")) goto fail_colormap;
    if (!request_ok(s, xcb_shape_rectangles_checked(s->connection, XCB_SHAPE_SO_SET,
        XCB_SHAPE_SK_INPUT, XCB_CLIP_ORDERING_UNSORTED, window, 0, 0, 0, NULL),
        "cannot make the candidate window input-transparent")) goto fail_window;
    const char title[] = "zinput-x11 candidate popup";
    xcb_change_property(s->connection, XCB_PROP_MODE_REPLACE, window,
        XCB_ATOM_WM_NAME, XCB_ATOM_STRING, 8, sizeof(title)-1, title);
    xcb_atom_t type = atom(s, "_NET_WM_WINDOW_TYPE_TOOLTIP");
    xcb_atom_t property = atom(s, "_NET_WM_WINDOW_TYPE");
    if (type && property)
        xcb_change_property(s->connection, XCB_PROP_MODE_REPLACE, window,
            property, XCB_ATOM_ATOM, 32, 1, &type);
    xcb_gcontext_t gc = xcb_generate_id(s->connection);
    if (!request_ok(s, xcb_create_gc_checked(s->connection, gc, window, 0, NULL),
        "cannot create the candidate drawing context")) goto fail_window;
    s->popup_colormap = colormap;
    s->popup_window = window;
    s->popup_gc = gc;
    return 1;
fail_window:
    xcb_destroy_window(s->connection, window);
fail_colormap:
    xcb_free_colormap(s->connection, colormap);
    xcb_flush(s->connection);
    return 0;
}
int zix_popup_supported(ZixServer *s) { return s->popup_available; }

static int popup_position(ZixServer *s, int32_t *x, int32_t *y, uint32_t width, uint32_t height) {
    IcData *data = find_ic(s, s->active_id);
    if (!data) return 0;
    xcb_window_t focus = xcb_im_input_context_get_focus_window(data->ic);
    if (!focus) focus = xcb_im_input_context_get_client_window(data->ic);
    if (!focus) return 0;
    int16_t sx = 8, sy = 8;
    uint32_t mask = xcb_im_input_context_get_preedit_attr_mask(data->ic);
    const xcb_im_preedit_attr_t *attributes = xcb_im_input_context_get_preedit_attr(data->ic);
    if (attributes && (mask & XCB_XIM_XNSpotLocation_MASK)) {
        sx = attributes->spot_location.x;
        sy = attributes->spot_location.y;
    } else {
        xcb_get_geometry_reply_t *geometry = xcb_get_geometry_reply(
            s->connection, xcb_get_geometry(s->connection, focus), NULL);
        if (!geometry) return 0;
        sy = (int16_t)(geometry->height > INT16_MAX ? INT16_MAX : geometry->height);
        free(geometry);
    }
    xcb_translate_coordinates_reply_t *position = xcb_translate_coordinates_reply(
        s->connection, xcb_translate_coordinates(s->connection, focus, s->screen->root, sx, sy), NULL);
    if (!position) return 0;
    *x = position->dst_x; *y = position->dst_y + 4;
    free(position);
    int32_t max_x = s->screen->width_in_pixels > width ? (int32_t)(s->screen->width_in_pixels - width) : 0;
    int32_t max_y = s->screen->height_in_pixels > height ? (int32_t)(s->screen->height_in_pixels - height) : 0;
    if (*x < 0) *x = 0;
    if (*x > max_x) *x = max_x;
    if (*y < 0) *y = 0;
    if (*y > max_y) *y = max_y;
    return 1;
}
static void repaint(ZixServer *s) {
    if (s->popup_mapped && s->popup_pixmap)
        xcb_copy_area(s->connection, s->popup_pixmap, s->popup_window, s->popup_gc,
            0, 0, 0, 0, (uint16_t)s->popup_width, (uint16_t)s->popup_height);
}
int zix_popup(ZixServer *s, uint64_t generation, uint32_t serial,
              const unsigned char *pixels, uint32_t width, uint32_t height) {
    if (!current(s, generation, serial)) return error(s, "candidate context is no longer active");
    if (!pixels || !width || !height || width > MAX_POPUP || height > MAX_POPUP ||
        (size_t)width * height > MAX_POPUP_BYTES / 4)
        return error(s, "invalid candidate image dimensions");
    if (!s->popup_available) return 2;
    if (s->popup_depth != 32)
        for (size_t i = 3; i < (size_t)width * height * 4; i += 4)
            if (pixels[i] != 255) return 2;
    int32_t x, y;
    if (!popup_position(s, &x, &y, width, height)) {
        zix_hide_popup(s);
        return 2;
    }
    if (!create_popup(s)) return 0;
    xcb_pixmap_t pixmap = xcb_generate_id(s->connection);
    if (!request_ok(s, xcb_create_pixmap_checked(s->connection, s->popup_depth,
        pixmap, s->popup_window, (uint16_t)width, (uint16_t)height),
        "cannot allocate the candidate pixmap")) return 0;
    uint64_t request_bytes = (uint64_t)xcb_get_maximum_request_length(s->connection) * 4;
    uint32_t stride = width * 4;
    uint64_t available = request_bytes > 64 ? request_bytes - 64 : 0;
    uint32_t rows = available / stride > height ? height : (uint32_t)(available / stride);
    if (!rows) { xcb_free_pixmap(s->connection, pixmap); return error(s, "X11 image request limit is too small"); }
    for (uint32_t row = 0; row < height; row += rows) {
        uint32_t count = height - row < rows ? height - row : rows;
        if (!request_ok(s, xcb_put_image_checked(s->connection, XCB_IMAGE_FORMAT_Z_PIXMAP,
            pixmap, s->popup_gc, (uint16_t)width, (uint16_t)count, 0, (int16_t)row,
            0, s->popup_depth, count * stride, pixels + (size_t)row * stride),
            "cannot upload the candidate pixels")) {
            xcb_free_pixmap(s->connection, pixmap);
            return 0;
        }
    }
    uint32_t geometry[] = {(uint32_t)x, (uint32_t)y, width, height, XCB_STACK_MODE_ABOVE};
    xcb_configure_window(s->connection, s->popup_window,
        XCB_CONFIG_WINDOW_X | XCB_CONFIG_WINDOW_Y | XCB_CONFIG_WINDOW_WIDTH |
        XCB_CONFIG_WINDOW_HEIGHT | XCB_CONFIG_WINDOW_STACK_MODE, geometry);
    if (s->popup_pixmap) xcb_free_pixmap(s->connection, s->popup_pixmap);
    s->popup_pixmap = pixmap; s->popup_width = width; s->popup_height = height;
    if (!s->popup_mapped) {
        xcb_map_window(s->connection, s->popup_window);
        s->popup_mapped = 1;
    }
    repaint(s);
    return flush(s);
}

int zix_dispatch(ZixServer *s, int timeout_ms) {
    finish_pending(s);
    if (s->failed) return 0;
    if (s->count) return 1;
    if (!flush(s)) return 0;
    int waited = 0;
    for (size_t budget = 0; budget < 4096; ++budget) {
        xcb_generic_event_t *event = xcb_poll_for_event(s->connection);
        if (!event) {
            if (xcb_connection_has_error(s->connection)) return fatal(s, "the X11 connection closed");
            if (waited) return 1;
            struct pollfd fd = {xcb_get_file_descriptor(s->connection), POLLIN, 0};
            int result;
            do { result = poll(&fd, 1, timeout_ms); } while (result < 0 && errno == EINTR);
            if (result < 0 || (result > 0 && (fd.revents & (POLLERR | POLLHUP | POLLNVAL))))
                return fatal(s, "cannot poll the X11 input connection");
            if (!result) return 1;
            waited = 1;
            continue;
        }
        uint8_t kind = event->response_type & 0x7fu;
        if (kind == 0) {
            fatal(s, "an X11 input request failed");
        } else if (kind == XCB_SELECTION_CLEAR &&
                   ((xcb_selection_clear_event_t *)event)->selection == s->selection) {
            fatal(s, "the XIM server selection was replaced");
        } else if (kind == XCB_MAPPING_NOTIFY) {
            map_keyboard(s);
        } else if (kind == XCB_EXPOSE &&
                   ((xcb_expose_event_t *)event)->window == s->popup_window) {
            repaint(s);
        } else {
            xcb_im_filter_event(s->im, event);
        }
        free(event);
        if (s->failed) return 0;
        if (s->count) return flush(s);
    }
    return flush(s);
}
int zix_next(ZixServer *s, ZixEvent *event) {
    finish_pending(s);
    if (s->failed) return -1;
    while (s->count) {
        Entry entry = s->queue[s->head];
        s->head = (s->head + 1) % MAX_EVENTS;
        --s->count;
        int matches = entry.public.generation == s->generation && entry.public.serial == s->serial;
        if (entry.public.kind == 1) {
            if (!matches) continue;
        } else {
            if (!matches || entry.ic_id != s->active_id) {
                forward_entry(s, &entry);
                continue;
            }
            s->pending = entry;
            s->pending_valid = 1;
        }
        *event = entry.public;
        return 1;
    }
    return 0;
}
int zix_forward(ZixServer *s, uint64_t token, uint64_t generation, uint32_t serial) {
    if (!s->pending_valid || s->pending.public.token != token ||
        !current(s, generation, serial) || s->pending.ic_id != s->active_id)
        return error(s, "key event is no longer the current XIM event");
    forward_entry(s, &s->pending);
    s->pending_valid = 0;
    return flush(s);
}
int zix_update(ZixServer *s, uint64_t generation, uint32_t serial,
               const unsigned char *commit, size_t length, int clear) {
    if (!current(s, generation, serial)) return error(s, "composition context is no longer active");
    if (length > MAX_TEXT || (!commit && length)) return error(s, "commit exceeds the text bound");
    IcData *data = find_ic(s, s->active_id);
    if (!data) return error(s, "input context was destroyed");
    char *encoded = NULL;
    size_t encoded_length = 0;
    if (commit && length) {
        encoded = xcb_utf8_to_compound_text((const char *)commit, length, &encoded_length);
        if (!encoded || encoded_length > UINT16_MAX) {
            free(encoded); return error(s, "cannot encode the XIM commit");
        }
        size_t decoded_length = 0;
        char *decoded = xcb_compound_text_to_utf8(encoded, encoded_length, &decoded_length);
        int exact = decoded && decoded_length == length && !memcmp(decoded, commit, length);
        free(decoded);
        if (!exact) { free(encoded); return error(s, "XIM encoding cannot preserve this commit"); }
    }
    if (encoded) {
        xcb_im_commit_string(s->im, data->ic, XCB_XIM_LOOKUP_CHARS,
            encoded, (uint32_t)encoded_length, 0);
        free(encoded);
    }
    if (clear) zix_hide_popup(s);
    return flush(s);
}

ZixServer *zix_open(const char *name, char *output_error, size_t capacity) {
    if (!name || !*name || strlen(name) > 63) {
        snprintf(output_error, capacity, "XIM name must contain 1 to 63 ASCII identifier characters");
        return NULL;
    }
    for (const unsigned char *p = (const unsigned char *)name; *p; ++p)
        if (!((*p >= 'a' && *p <= 'z') || (*p >= 'A' && *p <= 'Z') ||
              (*p >= '0' && *p <= '9') || *p == '-' || *p == '_' || *p == '.')) {
            snprintf(output_error, capacity, "invalid XIM server name"); return NULL;
        }
    ZixServer *s = calloc(1, sizeof(*s));
    if (!s) { snprintf(output_error, capacity, "cannot allocate the XIM server"); return NULL; }
    s->connection = xcb_connect(NULL, &s->screen_number);
    if (!s->connection || xcb_connection_has_error(s->connection)) {
        error(s, "cannot connect to the X11 display"); goto failure;
    }
    xcb_screen_iterator_t screen = xcb_setup_roots_iterator(xcb_get_setup(s->connection));
    for (int n = 0; n < s->screen_number && screen.rem; ++n) xcb_screen_next(&screen);
    if (!screen.rem) { error(s, "the X11 screen is unavailable"); goto failure; }
    s->screen = screen.data;
    char selection_name[80];
    snprintf(selection_name, sizeof(selection_name), "@server=%s", name);
    s->selection = atom(s, selection_name);
    if (!s->selection) { error(s, "cannot resolve the XIM selection"); goto failure; }
    /* Check ownership independently of XIM_SERVERS. xcb-imdkit's registration
       list alone does not cover an owner whose root property is missing. */
    xcb_get_selection_owner_reply_t *owner = xcb_get_selection_owner_reply(
        s->connection, xcb_get_selection_owner(s->connection, s->selection), NULL);
    if (!owner) { error(s, "cannot inspect the XIM selection owner"); goto failure; }
    int occupied = owner->owner != XCB_WINDOW_NONE;
    free(owner);
    if (occupied) { error(s, "another input method already owns this XIM name"); goto failure; }
    uint16_t major, minor;
    uint8_t event_base, error_base;
    if (!xkb_x11_setup_xkb_extension(s->connection, XKB_X11_MIN_MAJOR_XKB_VERSION,
        XKB_X11_MIN_MINOR_XKB_VERSION, XKB_X11_SETUP_XKB_EXTENSION_NO_FLAGS,
        &major, &minor, &event_base, &error_base)) {
        error(s, "the X11 XKB extension is unavailable"); goto failure;
    }
    s->xkb_context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    s->device = xkb_x11_get_core_keyboard_device_id(s->connection);
    if (!s->xkb_context || s->device < 0 || !map_keyboard(s)) {
        error(s, "cannot prepare the X11 keyboard map"); goto failure;
    }
    s->server_window = xcb_generate_id(s->connection);
    if (!request_ok(s, xcb_create_window_checked(s->connection, XCB_COPY_FROM_PARENT,
        s->server_window, s->screen->root, 0, 0, 1, 1, 0,
        XCB_WINDOW_CLASS_INPUT_OUTPUT, s->screen->root_visual, 0, NULL),
        "cannot create the XIM protocol window")) goto failure;
    uint32_t styles[] = {XCB_IM_PreeditNothing | XCB_IM_StatusNothing,
                         XCB_IM_PreeditNothing | XCB_IM_StatusNone};
    xcb_im_styles_t style_list = {2, styles};
    char *encodings[] = {"COMPOUND_TEXT"};
    xcb_im_encodings_t encoding_list = {1, encodings};
    xcb_im_trigger_keys_t triggers = {0, NULL};
    xcb_compound_text_init();
    s->im = xcb_im_create(s->connection, s->screen_number, s->server_window,
        name, XCB_IM_ALL_LOCALES, &style_list, &triggers, &triggers,
        &encoding_list, XCB_EVENT_MASK_KEY_PRESS | XCB_EVENT_MASK_KEY_RELEASE, callback, s);
    if (!s->im) { error(s, "cannot allocate the XIM protocol server"); goto failure; }
    xcb_im_set_use_sync_mode(s->im, false);
    xcb_im_set_use_sync_event(s->im, true);
    if (!xcb_im_open_im(s->im)) {
        error(s, "cannot register XIM name; another input method may own it"); goto failure;
    }
    s->opened = 1;
    s->popup_available = popup_visual(s);
    if (!flush(s)) goto failure;
    return s;
failure:
    snprintf(output_error, capacity, "%s", s->error);
    zix_close(s);
    return NULL;
}
void zix_close(ZixServer *s) {
    if (!s) return;
    s->closing = 1;
    if (s->im) {
        if (s->opened) xcb_im_close_im(s->im);
        xcb_im_destroy(s->im);
    }
    if (s->connection) {
        if (s->popup_pixmap) xcb_free_pixmap(s->connection, s->popup_pixmap);
        if (s->popup_gc) xcb_free_gc(s->connection, s->popup_gc);
        if (s->popup_window) xcb_destroy_window(s->connection, s->popup_window);
        if (s->popup_colormap) xcb_free_colormap(s->connection, s->popup_colormap);
        if (s->server_window) xcb_destroy_window(s->connection, s->server_window);
        xcb_disconnect(s->connection);
    }
    if (s->keyboard) xkb_state_unref(s->keyboard);
    if (s->keymap) xkb_keymap_unref(s->keymap);
    if (s->xkb_context) xkb_context_unref(s->xkb_context);
    free(s);
}
