/* Private-X-server regression for retry after a checked resource-creation failure. */
#include "../native/x11.c"
#include <assert.h>
#include <unistd.h>

int main(void) {
    char name[64], error[256];
    snprintf(name, sizeof(name), "zinput-popup-rollback-%ld", (long)getpid());
    ZixServer *server = zix_open(name, error, sizeof(error));
    if (!server) { fprintf(stderr, "%s\n", error); return 1; }
    assert(server->popup_available);
    uint8_t depth = server->popup_depth;
    server->popup_depth = 1; /* Valid colormap, incompatible window depth/visual. */
    assert(!create_popup(server));
    assert(!server->popup_colormap && !server->popup_window && !server->popup_gc);
    server->popup_depth = depth;
    assert(create_popup(server));
    assert(server->popup_colormap && server->popup_window && server->popup_gc);
    zix_close(server);
    puts("POPUP_ROLLBACK");
    return 0;
}
