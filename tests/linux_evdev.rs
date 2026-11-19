use zinput::KeyEvent;
use zinput::linux::{Endian, Error, EvdevDecoder, Event, Layout, decode_record};

fn record(layout: Layout, endian: Endian, kind: u16, code: u16, value: i32) -> Vec<u8> {
    let mut bytes = vec![0; layout.record_bytes()];
    let at = bytes.len() - 8;
    let (kind, code, value) = match endian {
        Endian::Little => (kind.to_le_bytes(), code.to_le_bytes(), value.to_le_bytes()),
        Endian::Big => (kind.to_be_bytes(), code.to_be_bytes(), value.to_be_bytes()),
    };
    bytes[at..at + 2].copy_from_slice(&kind);
    bytes[at + 2..at + 4].copy_from_slice(&code);
    bytes[at + 4..].copy_from_slice(&value);
    bytes
}

#[test]
fn layouts_endianness_and_byte_fragments_match() {
    for layout in [Layout::Time32, Layout::Time64] {
        for endian in [Endian::Little, Endian::Big] {
            let bytes = record(layout, endian, 1, 30, 2);
            assert_eq!(decode_record(&bytes, layout, endian).unwrap().value, 2);
            assert_eq!(
                decode_record(&bytes[..bytes.len() - 1], layout, endian),
                Err(Error::RecordSize)
            );
            let mut decoder = EvdevDecoder::new(layout, endian);
            let mut events = Vec::new();
            for byte in &bytes[..bytes.len() - 1] {
                assert_eq!(decoder.feed(&[*byte], |event| events.push(event)), 0);
            }
            assert_eq!(decoder.finish(), Err(Error::IncompleteRecord));
            assert_eq!(
                decoder.feed(&bytes[bytes.len() - 1..], |event| events.push(event)),
                1
            );
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!(
                events,
                [Event::Key {
                    code: 30,
                    transition: KeyEvent::Repeat
                }]
            );
        }
    }
}

#[test]
fn dropped_stream_requires_report_and_device_resync_acknowledgement() {
    let mut decoder = EvdevDecoder::new(Layout::Time64, Endian::Little);
    let mut events = Vec::new();
    assert_eq!(decoder.resynchronized(), Err(Error::ResyncNotReady));
    decoder.feed(&record(Layout::Time64, Endian::Little, 0, 3, 0), |e| {
        events.push(e)
    });
    assert_eq!(decoder.resynchronized(), Err(Error::ResyncNotReady));
    decoder.feed(&record(Layout::Time64, Endian::Little, 1, 48, 1), |e| {
        events.push(e)
    });
    decoder.feed(&record(Layout::Time64, Endian::Little, 0, 0, 0), |e| {
        events.push(e)
    });
    decoder.feed(&record(Layout::Time64, Endian::Little, 1, 46, 1), |e| {
        events.push(e)
    });
    assert_eq!(events, [Event::ResyncRequired, Event::ReadyToResync]);
    assert!(decoder.is_desynchronized());
    decoder.resynchronized().unwrap();
    decoder.feed(&record(Layout::Time64, Endian::Little, 1, 46, 1), |e| {
        events.push(e)
    });
    assert_eq!(
        events.last(),
        Some(&Event::Key {
            code: 46,
            transition: KeyEvent::Press
        })
    );
    assert!(!decoder.is_desynchronized());
}

#[test]
fn invalid_key_values_and_other_events_remain_explicit() {
    let mut decoder = EvdevDecoder::new(Layout::Time64, Endian::Little);
    let mut events = Vec::new();
    for value in [-1, 3, i32::MAX] {
        decoder.feed(&record(Layout::Time64, Endian::Little, 1, 30, value), |e| {
            events.push(e)
        });
    }
    assert!(
        events
            .iter()
            .all(|e| matches!(e, Event::InvalidKey { code: 30, .. }))
    );
    decoder.feed(&record(Layout::Time64, Endian::Little, 2, 0, -12), |e| {
        events.push(e)
    });
    assert!(matches!(events.last(), Some(Event::Other(event)) if event.value == -12));
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[test]
fn native_linux_headers_generate_the_decoded_layout() {
    use std::process::Command;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let scratch = std::env::temp_dir().join(format!("zinput-evdev-fixture-{}", std::process::id()));
    std::fs::create_dir(&scratch).unwrap();
    let binary = scratch.join("events");
    let built = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(root.join("tests/linux_input_fixture.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = Command::new(&binary).output().unwrap();
    assert!(output.status.success());
    let mut decoder = EvdevDecoder::native();
    let mut events = Vec::new();
    for chunk in output.stdout.chunks(7) {
        decoder.feed(chunk, |event| events.push(event));
    }
    assert_eq!(decoder.finish(), Ok(()));
    assert_eq!(
        events,
        [
            Event::Key {
                code: 30,
                transition: KeyEvent::Press
            },
            Event::Key {
                code: 30,
                transition: KeyEvent::Repeat
            },
            Event::Key {
                code: 30,
                transition: KeyEvent::Release
            },
            Event::ResyncRequired,
            Event::ReadyToResync,
        ]
    );
    std::fs::remove_dir_all(scratch).unwrap();
}
