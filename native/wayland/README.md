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
not copied or retained. Each output validates the current context, UTF-8 cursor
boundary, NUL exclusion and the protocol's 4000-byte limit before sending any
request. No typing history or network transport is created.

The adapter serves the first advertised seat. Its event queue holds at most
1024 protocol events; keymaps are limited to 2 MiB. It does not implement a
candidate popup or XIM. Applications need a compatible Wayland text-input
frontend; XWayland and compositor-specific protocol support are separate
integration work.

Protocol definitions:

- [Input method v2](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/protocol/input-method-unstable-v2.xml)
- [Virtual keyboard v1](https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/protocol/virtual-keyboard-unstable-v1.xml)
- [Text input v3](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/text-input/text-input-unstable-v3.xml)

Run `cargo test -p zinput-wayland`, `cargo clippy -p zinput-wayland --all-targets
-- -D warnings`, and `cargo run -p zinput-wayland --example probe`. The probe is
read-only; application acceptance must check stored text in actual receivers.
