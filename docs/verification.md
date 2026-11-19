# Verification for 0.1.3

## Contracts and evidence

| Claim | Evidence | Scope |
|---|---|---|
| Existing terminal encoding behavior is retained | Original codec implementation and its 23 tests are unchanged from the source commit recorded in provenance.md | Existing public codec API |
| Composition failures preserve state and caller output | tests/ime.rs plus the sequence-exhaustion unit test | Focus, stale candidates, UTF-8 boundaries, capacity, cancel and commit |
| Linux events match the host UAPI layout | tests/linux_input_fixture.c is compiled against linux/input.h and decoded by tests/linux_evdev.rs | Native 64-bit Linux; explicit layouts and byte orders also have portable tests |
| Separately compiled engines use the shared typed ABI | tools/test_native_plugin.py builds the cdylib and executes tests/plugin_native.rs through zplugin | Native Linux locally; the CI workflow includes Linux, Windows and macOS |
| Portable modules compile without an OS | Bare-metal x86_64-unknown-none checks with default features and plugins | Library compilation, not native desktop protocol integration |
| Published dependency sources are used | Cargo.lock and locked validation use registry zplugin 0.3.1, zplugin-macros 0.3.1 and zsys 0.1.1 | No local dependency patches in the release checkout |

The Linux run passed 24 library unit tests, six composition integration tests,
four Linux decoder integration tests and the separate native plugin test. Four
README examples compiled or ran as documentation tests. Clippy passed for all
targets and features with warnings denied. The Windows GNU library target also
compiled with plugins enabled. The tagged commit's Actions run supplies the
native platform matrix results.

## Measured hot paths

One local release build on a 12th Gen Intel Core i9-12900HX with Rust 1.100.0
ran the bundled benchmark at nice priority 19. Each row used one million
iterations in one process; black_box keeps the buffers, events and state
observable to the optimizer.

| Workload | Bounds | Nanoseconds per operation | Allocator calls inside measured loop |
|---|---|---:|---:|
| Begin, update and commit one composition | 10 UTF-8 bytes, 128-byte caller buffers | 17.05 | 0 |
| Decode and dispatch one complete evdev record | 24-byte little-endian record | 4.42 | 0 |

These are single-run microbenchmarks of the core, not whole-application typing
latency or a comparison with another framework. The benchmark prints its sizes,
iteration count, elapsed time and allocation count on every run. Complete
evdev records are decoded directly from the caller slice; only fragmented
records use the decoder's fixed scratch storage.

## Review questions

1. What changed? The existing codec gained reusable composition state, an evdev
   decoder and a shared engine interface; source provenance is retained.
2. Are the contracts explicit? Context identity, revision tokens, UTF-8 byte
   offsets, short-buffer behavior and pending-commit draining are documented.
3. Does evidence exercise a real boundary? A native C event fixture and an
   independently built dynamic library cross the actual tested boundaries.
4. What remains outside this package? Desktop registration, keyboard layouts,
   language dictionaries and target-application delivery belong to frontends.
5. Are performance claims bounded? Allocation checks cover the measured core
   paths; timing names the machine and workload and makes no ranking claim.
