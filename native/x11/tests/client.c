/* Real Xlib XIM client for the private-display protocol acceptance test. */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/keysym.h>
#include <X11/extensions/shape.h>
#include <locale.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    setlocale(LC_ALL, "");
    Display *display = XOpenDisplay(NULL);
    if (!display) return 2;
    if (argc == 3 && !strcmp(argv[1], "--keycode")) {
        printf("%x\n", XKeysymToKeycode(display, XStringToKeysym(argv[2])));
        XCloseDisplay(display); return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "--hide-registration")) {
        XDeleteProperty(display, DefaultRootWindow(display), XInternAtom(display, "XIM_SERVERS", False));
        XSync(display, False); XCloseDisplay(display); return 0;
    }
    if (argc == 3 && !strcmp(argv[1], "--inspect-popup")) {
        Window window = strtoul(argv[2], NULL, 0);
        XWindowAttributes attributes;
        if (!XGetWindowAttributes(display, window, &attributes) || !attributes.override_redirect) return 3;
        int count = -1, ordering = 0;
        XRectangle *rectangles = XShapeGetRectangles(display, window, ShapeInput, &count, &ordering);
        if (rectangles) XFree(rectangles);
        XCloseDisplay(display);
        if (count != 0) return 4;
        puts("INPUT_TRANSPARENT");
        return 0;
    }
    if (!XSetLocaleModifiers("")) { fputs("locale modifiers failed\n", stderr); return 3; }
    XIM im = XOpenIM(display, NULL, NULL, NULL);
    if (!im) { fputs("XOpenIM failed\n", stderr); return 4; }
    Window windows[2];
    XIC contexts[2];
    for (int i = 0; i < 2; ++i) {
        windows[i] = XCreateSimpleWindow(display, DefaultRootWindow(display),
            40 + i * 360, 40, 300, 180, 0, 0, 0xffffff);
        XStoreName(display, windows[i], i ? "zinput-x11-test-b" : "zinput-x11-test-a");
        contexts[i] = XCreateIC(im, XNInputStyle, XIMPreeditNothing | XIMStatusNothing,
            XNClientWindow, windows[i], XNFocusWindow, windows[i], NULL);
        if (!contexts[i]) { fputs("XCreateIC failed\n", stderr); return 5; }
        long filter = 0;
        XGetICValues(contexts[i], XNFilterEvents, &filter, NULL);
        XSelectInput(display, windows[i], filter | KeyPressMask | KeyReleaseMask | FocusChangeMask | StructureNotifyMask);
        XPoint spot = {20, 40};
        XVaNestedList attributes = XVaCreateNestedList(0, XNSpotLocation, &spot, NULL);
        if (XSetICValues(contexts[i], XNPreeditAttributes, attributes, NULL)) return 6;
        XFree(attributes);
        XMapWindow(display, windows[i]);
    }
    XSetInputFocus(display, windows[0], RevertToParent, CurrentTime);
    XSetICFocus(contexts[0]);
    XFlush(display);
    printf("READY %lu %lu\n", windows[0], windows[1]); fflush(stdout);
    for (;;) {
        XEvent event;
        XNextEvent(display, &event);
        if (XFilterEvent(&event, None)) continue;
        int index = event.xany.window == windows[0] ? 0 : event.xany.window == windows[1] ? 1 : -1;
        if (index < 0 || !contexts[index]) continue;
        if (event.type == FocusIn) { XSetICFocus(contexts[index]); XFlush(display); }
        else if (event.type == FocusOut) { XUnsetICFocus(contexts[index]); XFlush(display); }
        else if (event.type == KeyPress) {
            char text[8192]; KeySym symbol = 0; Status status = 0;
            int length = Xutf8LookupString(contexts[index], &event.xkey, text, sizeof(text), &symbol, &status);
            if (status == XBufferOverflow || length < 0) return 7;
            printf("KEY %d P %lx %u %d\n", index, symbol, event.xkey.state, length);
            if (length) {
                printf("TEXT %d ", index);
                for (int i = 0; i < length; ++i) printf("%02x", (unsigned char)text[i]);
                putchar('\n');
            }
            if (symbol == XK_F6) {
                char *reset = Xutf8ResetIC(contexts[index]);
                if (reset) XFree(reset);
                printf("RESET %d\n", index);
            }
            if (symbol == XK_F7) {
                XUnsetICFocus(contexts[index]);
                XDestroyIC(contexts[index]); contexts[index] = NULL;
                XDestroyWindow(display, windows[index]);
                XFlush(display);
                printf("DESTROY %d\n", index);
            }
            fflush(stdout);
        } else if (event.type == KeyRelease) {
            printf("KEY %d R %x %u 0\n", index, event.xkey.keycode, event.xkey.state);
            fflush(stdout);
        }
    }
}
