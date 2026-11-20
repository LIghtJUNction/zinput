# zinput-x11

A Linux XIM server for X11 and XWayland clients. The application supplies its
input engine and candidate pixels. The adapter owns the protocol connection,
keyboard translation, text encoding and an input-transparent candidate window.

The default server name is `ztype`; clients select it through
`XMODIFIERS=@im=ztype` before opening their XIM connection. A running owner of
the same name causes connection to fail. Existing applications retain their
own XIM connection until they reconnect.

## System requirements

Install xcb-imdkit, XCB, XCB Shape, and xkbcommon-x11 development libraries,
plus a C compiler, ar and pkg-config. This crate has no Cargo dependencies.
All native calls are implemented within this companion.

## Host contract

Use `InputMethod::connect()` or `connect_named(name)`, then call
`dispatch(Duration)` and drain `next_event()`. `Event::State` advances the
visible context; each `KeyEvent` includes its own context snapshot.
Process a key before asking for the next event. Forward an unconsumed key
once with `forward_key`; forwarded presses are paired with releases in the
same XIC. Input generation checks prevent an old input context from receiving
a new composition update.

`update(context, preedit, cursor, commit)` validates complete UTF-8 strings
and cursor boundaries before I/O. Commits are converted to COMPOUND_TEXT and
round-tripped before delivery. Each input string is limited to 4000 bytes.
The advertised XIM styles are `PreeditNothing|StatusNothing` and
`PreeditNothing|StatusNone`: the host presents preedit in the candidate
popup. XIM provides no standardized password-purpose or surrounding-text
metadata; `Context::sensitive` is always false.

`present_popup` accepts tightly packed premultiplied ARGB8888 bytes in
B,G,R,A order, with physical width/height and scale 1. Frames are limited to
2048 pixels in each dimension and 8 MiB. A 32-bit visual is preferred;
a 24-bit fallback accepts opaque frames. The override-redirect window has
an empty input shape and does not take keyboard focus. Position uses the
XIC spot when available, otherwise the focus window's lower-left edge.
XCB copies pixels to an X server pixmap; the caller can reuse its byte slice
after the call. Deactivation and reset hide the candidate window.

The server stays on its constructing thread. It does not read keyboard
devices, grab the global keyboard, or use the clipboard to insert text.

## Development checks

```sh
cargo test -p zinput-x11 --offline
cargo clippy -p zinput-x11 --all-targets --offline -- -D warnings
cargo build -p zinput-x11 --example protocol_probe --offline
```

The `protocol_probe` example is a private-display test host. Application
engines and desktop installation remain with the consuming application.
