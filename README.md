# zinput

Reusable input-method state, native Linux event decoding, and terminal input
encoding in Rust. The portable library remains `#![no_std]`; its default
feature set has no external dependencies. The composition and evdev paths use
caller-owned or fixed storage without allocating during event processing.
Existing terminal encoding APIs use `alloc` for returned or reusable byte buffers.

The optional `plugins` feature adds a shared input-engine trait and a generated
native ABI through `zplugin`. System library loading is centralized in `zsys`.
Desktop input-method registration, keyboard-layout mapping and text delivery are
provided by application frontends. The separate [zinput-wayland](native/wayland)
package supplies a Linux input-method-v2 adapter with compositor keyboard grabs,
XKB layout mapping, preedit/commit delivery and paired virtual-keyboard forwarding.
It is optional and does not change the core library's default feature set.

With `plugins` enabled, the runtime dependency chain is `zinput → zplugin →
zsys`. Generating the typed interface also builds `zplugin-macros`, which uses
`proc-macro2`, `quote`, `syn` and `unicode-ident`. Those build dependencies are
not part of the default feature set; this is not a claim that the plugin build
has no third-party packages.

## Install

```toml
[dependencies]
zinput = "0.1.3"
```

## Composition lifecycle

The `ime` module keeps one focused input context and one composition in a caller
buffer. A composition is the text being edited before the user accepts it;
`preedit` exposes that provisional text. An accepted commit copies complete
UTF-8 to a caller buffer and ends the composition.

```rust
use zinput::ime::{Composition, ContextId, Selection};

let mut preedit = [0; 128];
let mut committed = [0; 128];
let mut state = Composition::new(&mut preedit);
state.set_focus(Some(ContextId::new(7).unwrap()));

let token = state.begin().unwrap();
let token = state.set_preedit(token, "你好", Selection::caret(6)).unwrap();
assert_eq!(state.preedit(), "你好");
let result = state.commit_preedit(token, &mut committed).unwrap();
assert_eq!(&committed[..result.bytes_written], "你好".as_bytes());
assert_eq!(result.context.get(), 7);
assert_eq!(state.preedit(), "");
```

Every accepted preedit update returns a new revision token. `CandidatePage`
borrows a slice of candidate strings and carries the token that produced them;
`commit_candidate` rejects an older page after another update or a focus change.
Tokens belong to the same `Composition` instance and must not be exchanged
between instances. Changing focus or blurring cancels the old composition;
reporting the same focus again preserves it.

Buffer and selection failures leave state and output unchanged. Selection
endpoints are UTF-8 byte offsets at character boundaries; grapheme navigation
belongs to the engine. Commit, cancel, a shorter preedit, focus change and drop
clear the retained text bytes that are no longer used. This ordinary memory
clearing is not a guarantee against compiler elimination or process-memory
inspection. The revision counter returns an error at exhaustion and never wraps.

`TextTarget` is the delivery boundary for applications. Its `commit_text` method
receives an explicit context and a complete string. The frontend must confirm
that context still identifies the intended target before delivery. A delivery
error can follow partial platform output, so the platform adapter owns retry
semantics; the composition core cannot roll back external application input.

## Input engine plugins

Enable `plugins` to use one shared Rust trait and its generated native ABI:

```toml
[dependencies]
zinput = { version = "0.1.3", features = ["plugins"] }
```

Built-in engines implement `engine::InputEngine` directly. Separately compiled
engines export `InputEngineExport<T>` using `zplugin::export!`; hosts use
`InputEngineApi` and its generated proxy. The service ID is
`dev.project-z.zinput.engine`, version 1. `EngineSelection` has its own explicit
ABI type ID and uses 64-bit byte offsets, independent of the host pointer width.

| Engine call | Contract |
|---|---|
| `reset(context)` | Establish one context; zero means blur. Discard old preedit and pending commit. |
| `process(phase, key, modifiers, text)` | Consume a logical key after keyboard-layout mapping. Return `change` flags; zero leaves the event to the frontend. |
| `preedit(output)`, `selection()` | Read complete provisional text and valid UTF-8 selection offsets. |
| `candidate_count()`, `candidate(index, output)` | Read a bounded candidate page without returning pointers into plugin memory. |
| `select(index)` | Accept a candidate and report the resulting change flags. |
| `commit(output)` | Drain a pending commit only after complete copying succeeds. |

Phases and logical keys are constants in `engine::phase` and `engine::key`.
Modifier bits match `Modifiers`. Calls that write strings return their UTF-8
byte length. A short buffer returns `zplugin::Error::BufferTooSmall` without
partial output or a consumed commit. An engine must return `Busy` rather than
overwrite undrained committed text. Hosts drain commits before processing the
next producing event and fetch changed preedit/candidates according to the flags.

```no_run
# #[cfg(feature = "plugins")]
# fn load_reference_engine() -> Result<(), zinput::engine::zplugin::Error> {
use zinput::engine::{InputEngineApi, key, phase, zplugin::Plugin};

// SAFETY: This path must identify reviewed, trusted native plugin code.
let library = unsafe { Plugin::load(c"/absolute/path/libime_engine.so")? };
let mut engine = InputEngineApi::create(&library)?;
engine.reset(7)?;
engine.process(phase::PRESS, key::TEXT, 0, "ni")?;
let mut candidate = [0; 64];
let used = engine.candidate(0, &mut candidate)?;
assert_eq!(&candidate[..used as usize], "你".as_bytes());
# Ok(())
# }
```

The example `ime_engine` implements a bounded reference engine: arbitrary UTF-8
composition, scalar backspace, and two candidates for `ni`. It is a protocol and
lifecycle example, not a full language dictionary. Its constructor allocates the
plugin object through `zplugin`; its event methods use fixed buffers. Native
plugins execute trusted code in the host process. Library loading comes from
`zplugin` through `zsys`; `zinput` adds no independent system FFI or loader.

## Native Linux event boundary

`linux::EvdevDecoder` accepts bytes from Linux `input_event` records and emits
key presses, repeats, releases and synchronization events. The decoder supports
explicit 16-byte and 24-byte timestamp layouts, little and big endian records,
and fragments split at any byte. On 64-bit Linux, `EvdevDecoder::native()` selects
the native layout. Device access and keyboard-layout translation remain in the
platform adapter; keycodes from evdev are physical codes, not Unicode text.

After `SYN_DROPPED`, the decoder suppresses events until it observes `SYN_REPORT`
and the host calls `resynchronized()`. The host must first query current device
state through its system layer, as required by the [Linux input event
protocol](https://www.kernel.org/doc/html/latest/input/event-codes.html). This
explicit acknowledgement prevents records from an unsynchronized stream from
being mistaken for valid keystrokes. The callback executes synchronously without
an allocated event queue.

This library does not register a desktop input method, read a keyboard device,
install a shortcut, or select a target application. Its portable state and plugin
contract can be used by Linux, Windows and macOS frontends. A working desktop
frontend still needs its platform's input-method and text-delivery integration.
Neither the core nor the reference engine depends on Fcitx5.

## Encode keyboard input

```rust
use zinput::{encode_key, Key, KeyEvent, Modes, Modifiers};

let legacy = Modes::default();
assert_eq!(
    encode_key(Key::Up, Modifiers(0), KeyEvent::Press, "", &legacy),
    b"\x1b[A",
);

let kitty = Modes { kitty_keyboard: 1, ..Modes::default() };
assert_eq!(
    encode_key(
        Key::Character('c'), Modifiers(Modifiers::CTRL),
        KeyEvent::Press, "\u{3}", &kitty,
    ),
    b"\x1b[99;5u",
);
```

`Key::Character` identifies the **unshifted** key. The `text` argument is the
complete UTF-8 text committed by the platform. Pass `Key::Unidentified` or use
`encode_text` when an input method commits text without a physical key identity.
Multi-character commits are retained.

The modifier bit values are Shift=1, Alt=2, Ctrl=4, Super=8, Hyper=16, Meta=32,
Caps Lock=64 and Num Lock=128. The encoder adds the protocol's modifier offset.
For a modifier key's own event, supply its state after the transition, including
any other physical key that keeps the same modifier active.

## Reuse a buffer

```rust
extern crate alloc;

use alloc::vec::Vec;
use zinput::{encode_key_into, Key, KeyEvent, Modes, Modifiers};

let modes = Modes::default();
let mut bytes = Vec::with_capacity(128);
encode_key_into(Key::Enter, Modifiers(0), KeyEvent::Press, "\r", &modes, &mut bytes);
assert_eq!(bytes, b"\r");

// After the transport accepts these bytes, clear and reuse the same allocation.
bytes.clear();
encode_key_into(Key::Left, Modifiers(0), KeyEvent::Press, "", &modes, &mut bytes);
assert_eq!(bytes, b"\x1b[D");
```

The `_into` functions append to the supplied buffer. They avoid further buffer
allocation while its existing capacity is sufficient. The convenience functions
return a new `Vec<u8>`.

## Protocol coverage

| Input | Behavior |
|---|---|
| Legacy keyboard | UTF-8 text, Control/Alt combinations, distinct Ctrl+Shift character shortcuts, application cursor and keypad modes, function keys and modifiers |
| Kitty keyboard | Progressive enhancement flags 1/2/4/8/16, press/repeat/release, alternate keys, modifier keys, keypad identity and associated text |
| Mouse | Tracking modes 9/1000/1002/1003, SGR 1006 and legacy encoding, modifiers, vertical/horizontal wheel and xterm buttons 1–11 |
| Paste | Optional bracketed-paste delimiters; tabs and line endings preserved; other control characters removed |
| Focus | Focus-in/out sequences when focus reporting is enabled |

Kitty alternate-key reporting can infer a shifted character from committed text.
Use `encode_key_with_alternates` with `KeyAlternates` when the platform also knows
the standard-layout key identity. A pure input-method commit uses key 0 with
associated text when flags 8 and 16 are both enabled; otherwise it is emitted as
UTF-8 to preserve input that has no physical key event.

Enter, Tab and Backspace retain their legacy behavior until Kitty flag 8 is
enabled. Function key F3 uses `CSI 13~` in enhanced mode to avoid cursor-report
ambiguity. Modifier-key events require flag 8, and release reports require
flag 2.

In legacy mode, Ctrl+Shift character shortcuts retain the Shift distinction
using CSI-u, as in Kitty and Ghostty. Ctrl+Shift+Space keeps the legacy NUL
exception. Lock state remains absent from legacy sequences. Ctrl+tilde emits
the documented ASCII control value 30.

Mouse positions are zero-based at the API boundary. The encoder converts them
to one-based protocol coordinates. Buttons are 0=left, 1=middle, 2=right,
3/4=vertical wheel, 5/6=horizontal wheel and 7–10=extra buttons. These are
zero-based xterm/X11 button numbers; platform frontends must normalize their
native button order. Wheel releases and wheel-button motion are ignored.
Extra-button releases identify their button in SGR and use the common release
code in legacy encoding. Motion with an extra button follows mode 1002/1003.
Mode 9 reports only left/middle/right presses and omits modifiers.
SGR encoding alone does not enable tracking. Legacy coordinates beyond its
representable range produce no event. The host should suppress mouse motion
that remains inside the same character cell.

Clipboard encoding removes Escape, DEL and C0/C1 controls other than Tab, CR and
LF. Consequently clipboard contents cannot inject a bracketed-paste end marker.

## Integration contract

The host is responsible for input-mode negotiation, keyboard layout, native input-method protocols and text delivery, clipboard access, transport backpressure and delivery order. Report
only modes that are connected to the host's actual event path. The codec owns no
window or operating-system resources and performs no I/O.

## Verification

```sh
cargo test --all-features
python tools/test_native_plugin.py
cargo check --lib --no-default-features --target x86_64-unknown-none
cargo check --lib --all-features --target x86_64-unknown-none
cargo bench --bench ime
```

The native plugin driver builds a separate dynamic library, locates Cargo's
reported artifact and runs the explicit ABI integration test against that file.
Generic `cargo test` leaves this one test ignored so missing plugin artifacts
cannot become silent passes. The CI workflow is configured to run this driver on Linux, Windows and macOS.

Core tests cover multilingual commits, UTF-8 selection boundaries, caller
capacity, stale candidates and focus changes. Linux tests cover both event
layouts and byte orders, fragmented records and loss/resynchronization. On
64-bit Linux they compile a small C fixture against `<linux/input.h>` and decode
the actual native records it emits. This fixture needs a C compiler and Linux
development headers; it never opens an input device.

The benchmark performs one million bounded composition lifecycles and one
million evdev records. It reports elapsed time, buffer sizes and allocation
counts, and asserts zero allocator calls inside each measured loop. Timing
results describe that build and machine; they are not a comparison against
other input frameworks or a desktop latency guarantee.

Existing codec tests cover all 32 keyboard flag combinations for reset-key
behavior, alternate-key and release-text boundaries, multi-character commits,
xterm button ranges, mouse tracking and coordinate limits, clipboard delimiter
injection, focus reporting and buffer reuse. Test tooling uses Rust's normal
test harness; the published library remains `no_std`.

## Protocol references

- [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
- [XTerm control sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)
- [Ghostty input guide](https://lib.ghostty.org/vt/guide-input.html)
- [Ghostty keyboard encoder](https://github.com/ghostty-org/ghostty/blob/main/src/input/key_encode.zig)
- [Ghostty mouse encoder](https://github.com/ghostty-org/ghostty/blob/main/src/input/mouse_encode.zig)

## License

MIT. Part of [Project Z](https://github.com/LIghtJUNction/project-z). Source and releases: [zinput](https://github.com/LIghtJUNction/zinput).
