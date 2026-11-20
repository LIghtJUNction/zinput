# zinput-x11

A Linux XIM server for X11 and XWayland clients. The application supplies its
input engine and candidate pixels. The adapter owns the protocol connection,
keyboard translation, text encoding and an input-transparent candidate window.

The default server name is `ztype`; clients select it through
`XMODIFIERS=@im=ztype` before opening their XIM connection. A running owner of
the same name causes connection to fail. Existing applications retain their
own XIM connection until they reconnect.

## System requirements

Install xcb-imdkit, XCB, XCB Shape, XCB XKB, and xkbcommon-x11 development libraries,
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

`KeyEvent::repeat` identifies repeated presses from both ordinary X11
release/press pairs and detectable autorepeat. All server events remain visible;
the adapter does not synthesize repeat events. Hosts should ignore repeats for
mode toggles while allowing repeated text, deletion and navigation. Forwarded
repeat events must still be passed to `forward_key`.

The adapter subscribes to XKB keymap replacement and keyboard-device events.
Changing the live layout rebuilds its logical mapping and invalidates the old
composition. Key events retain their own modifier/group state, so queued events
are translated using the state attached to that event.

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

The protocol regression also requires Xvfb, xdotool, setxkbmap, and Xlib/Xext development
libraries. It starts its own XIM name and disposable clients on an explicitly
private display:

```sh
CARGO_TARGET_DIR=/var/tmp/zinput-x11-check cargo build \
  -p zinput-x11 --example protocol_probe --locked --offline
xvfb-run -a -s '-screen 0 1024x768x24 -nolisten tcp' \
  env ZINPUT_X11_TEST_PRIVATE=1 python3 native/x11/tests/check_xim.py \
  --server /var/tmp/zinput-x11-check/debug/examples/protocol_probe \
  --output /var/tmp/zinput-x11-check/acceptance.json
```

The checks cover real XOpenIM/XCreateIC, Chinese and kana COMPOUND_TEXT commits,
modifier shortcuts, release pairing, autorepeat classification, live XKB layout
replacement, owner refusal, context changes, reset and destruction, invalid
updates, and candidate placement/input transparency.
A native failure-path check forces incompatible window depth and verifies that
a subsequent candidate-window creation succeeds after rollback. These are
transport checks; they do not establish behavior in every application or a
desktop installation.


## Ztty consumer acceptance

The consumer check starts an owned private Xvfb and an isolated Ztty broker.
It opens Ztty before the input service, then verifies XIM discovery, native
Chinese and kana commits in the terminal's raw PTY, multiple windows, visible
candidate placement, cancellation, and service restart recovery. The test uses
XTEST key events and XIM text delivery. It never reads the desktop clipboard or
attaches to an existing broker.

Pass a Ztty source tree containing its shared private-display and PTY fixtures.
The supplied Ztty binary is the actual program under test; it can be an
installed release.

```sh
python3 native/x11/tests/check_ztty.py \
  --ztty /absolute/path/to/ztty \
  --ztty-source /absolute/path/to/ztty-source \
  --server target/debug/examples/protocol_probe \
  --server-mode probe --name fcitx \
  --output /var/tmp/zinput-ztty-acceptance.json
```

For a full input engine, use `--server /absolute/path/to/ztype
--server-mode ztype`, optionally `--dictionary PATH` and `--config PATH`.
That mode launches `ztype x11`, sets `ZTYPE_XIM_NAME` to the requested private
name, and verifies `nihao` and `shijie` composition. Add
`--screenshot /var/tmp/zinput-ztty-candidate.png` to capture only the test's
private display while its candidate window is visible.
