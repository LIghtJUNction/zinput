# zinput-wayland

A Linux input-method-v2 adapter for engines built on Project Z. The separate
`zinput` core keeps its default `no_std` build and existing APIs. This package
owns the Wayland connection, compositor keyboard grab, XKB layout mapping and
paired forwarding through virtual-keyboard-v1. It dynamically loads the system
Wayland/XKB libraries through `zsys::dylib` and has no other Cargo dependencies.

The compositor must expose `zwp_input_method_manager_v2`,
`zwp_virtual_keyboard_manager_v1` and a seat. `probe()` discovers these globals
without claiming the input-method slot. `InputMethod::connect()` reports an
occupied slot instead of taking over an existing input method.

```rust,no_run
use std::time::Duration;
use zinput_wayland::{Event, InputMethod};

let mut input = InputMethod::connect()?;
loop {
    input.dispatch(Duration::from_millis(100))?;
    while let Some(event) = input.next_event()? {
        match event {
            Event::State(context) => {
                // Reset the engine when context.generation changes.
                // Do not compose when context is inactive or sensitive.
                let _ = context;
            }
            Event::Key(key) => input.forward_key(&key)?,
        }
    }
}
# Ok::<(), zinput_wayland::Error>(())
```

An engine host consumes selected key presses and calls `update()` with the
current context, provisional text, UTF-8 cursor offset and optional commit.
Call `forward_key()` for unhandled presses and every release. Releases are
forwarded only when the matching press was forwarded; repeated events are
generated for the engine and are not duplicated into the virtual keyboard.
Deactivation and drop release outstanding forwarded keys and modifier state.

Activation, content type and deactivation apply together at each `done` event.
Password/PIN purposes and hidden/sensitive hints release the keyboard grab;
those keys go directly through the compositor. Surrounding application text is
not copied or retained. An explicit protocol `text_change_cause=other` after
activation invalidates the current composition generation and clears owned
preedit. Normal input-method echoes do not change the generation. Each output validates the current context, UTF-8 cursor
boundary, NUL exclusion and the protocol's 4000-byte limit before sending any
request. No typing history or network transport is created.

The adapter serves the first advertised seat. Its event queue holds at most
1024 protocol events; keymaps are limited to 2 MiB. Applications need a
compatible Wayland text-input frontend. XIM/XWayland support is separate
integration work.

## Native candidate popup

`InputMethod::present_popup(&context, frame)` presents a full image through
`zwp_input_popup_surface_v2` on the existing input-method connection. The host
owns text shaping, candidate layout and selection. The adapter owns the surface,
shared-memory buffers and Wayland object lifetime.

```rust,no_run
use zinput_wayland::{InputMethod, PopupFrame, PopupPresentation};

let mut input = InputMethod::connect()?;
// After dispatching an active, non-sensitive Event::State:
let context = input.context();
if context.active && !context.sensitive && input.popup_supported() {
    // Four opaque BGRA pixels; replace with the host's rendered candidate image.
    let pixels = [32, 32, 32, 255, 32, 32, 32, 255,
                  32, 32, 32, 255, 32, 32, 32, 255];
    let status = input.present_popup(&context, PopupFrame {
        width: 2,
        height: 2,
        scale: 1,
        pixels: &pixels,
    })?;
    match status {
        PopupPresentation::Presented => {}
        PopupPresentation::Queued => {} // latest image waits for release/context state
        PopupPresentation::Unavailable => {} // keep application preedit
    }
}
input.hide_popup()?;
# Ok::<(), zinput_wayland::Error>(())
```

Pixels are tightly packed, premultiplied ARGB8888: little-endian bytes B, G, R,
A, with each color channel at most alpha. Width and height are physical pixels;
both must be divisible by the positive integer buffer scale. An image is limited
to `MAX_POPUP_BYTES` (8 MiB), with each dimension at most
`MAX_POPUP_DIMENSION` (2048). The input slice is borrowed only for the call.

The surface is created lazily and has an empty input region, so it does not
intercept pointer/touch input or take keyboard focus. The compositor places it
near the active text cursor. `popup_rectangle()` exposes the text-input bounds
relative to the popup as a hint; they are neither desktop coordinates nor a
configure handshake. The first image is attached without waiting for this hint.

There are at most two shared-memory buffers and one owned pending image. A
buffer is written again only after its `wl_buffer.release`. Further submissions
replace the pending image, retaining only the latest one. Release handling waits
until queued input-context changes are applied before displaying a pending
image. If a read ends before a known context update's `done` event, presentation
waits for that state batch to complete. Hiding, committing/clearing preedit, focus changes, sensitive input and
explicit external edits discard pending candidates and remove surface content.
An expired context cannot submit a new image. Buffer release cannot restore
candidates from an expired context. No image is stored in a persistent file.

Popup support requires `wl_compositor`, `wl_shm` and ARGB8888. It is optional:
`popup_supported()` reports negotiation, and `present_popup()` returns
`Unavailable` if those capabilities are absent. Malformed frames and stale
contexts return an error before issuing surface requests. The normal
input-method service remains usable without a popup.

Protocol definitions:

- [Wayland core: surfaces, regions, shared memory and buffer release](https://wayland.freedesktop.org/docs/html/apa.html)
- [Input method v2](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/protocol/input-method-unstable-v2.xml)
- [Virtual keyboard v1](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/protocol/virtual-keyboard-unstable-v1.xml)
- [Text input v3](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/text-input/text-input-unstable-v3.xml)

Run `cargo test -p zinput-wayland`, `cargo clippy -p zinput-wayland --all-targets
-- -D warnings`, and `cargo run -p zinput-wayland --example probe`. The probe is
read-only; application acceptance must check stored text in actual receivers.
