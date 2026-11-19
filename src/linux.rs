//! Linux evdev record decoding with no device access or allocation.
//!
//! Hosts read their authorized input device through the platform layer and feed
//! bytes here. After SYN_DROPPED, records are suppressed until SYN_REPORT and an
//! explicit acknowledgement that the host queried the current device state.

use crate::KeyEvent;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    /// Two 32-bit timestamp fields followed by type, code and signed value.
    Time32,
    /// Two 64-bit timestamp fields followed by type, code and signed value.
    Time64,
}

impl Layout {
    pub const fn record_bytes(self) -> usize {
        match self {
            Self::Time32 => 16,
            Self::Time64 => 24,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub const fn native() -> Self {
        if cfg!(target_endian = "little") {
            Self::Little
        } else {
            Self::Big
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    RecordSize,
    IncompleteRecord,
    ResyncNotReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

pub fn decode_record(bytes: &[u8], layout: Layout, endian: Endian) -> Result<InputEvent, Error> {
    if bytes.len() != layout.record_bytes() {
        return Err(Error::RecordSize);
    }
    let start = bytes.len() - 8;
    let kind = [bytes[start], bytes[start + 1]];
    let code = [bytes[start + 2], bytes[start + 3]];
    let value = [
        bytes[start + 4],
        bytes[start + 5],
        bytes[start + 6],
        bytes[start + 7],
    ];
    let (kind, code, value) = match endian {
        Endian::Little => (
            u16::from_le_bytes(kind),
            u16::from_le_bytes(code),
            i32::from_le_bytes(value),
        ),
        Endian::Big => (
            u16::from_be_bytes(kind),
            u16::from_be_bytes(code),
            i32::from_be_bytes(value),
        ),
    };
    Ok(InputEvent { kind, code, value })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key { code: u16, transition: KeyEvent },
    Synchronize,
    ResyncRequired,
    ReadyToResync,
    InvalidKey { code: u16, value: i32 },
    Other(InputEvent),
}

pub struct EvdevDecoder {
    layout: Layout,
    endian: Endian,
    record: [u8; 24],
    filled: usize,
    dropped: bool,
    report_seen: bool,
}

impl EvdevDecoder {
    pub const fn new(layout: Layout, endian: Endian) -> Self {
        Self {
            layout,
            endian,
            record: [0; 24],
            filled: 0,
            dropped: false,
            report_seen: false,
        }
    }

    /// Native 64-bit Linux input_event layout. Timestamp fields are ignored.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    pub const fn native() -> Self {
        Self::new(Layout::Time64, Endian::native())
    }

    pub const fn is_desynchronized(&self) -> bool {
        self.dropped
    }

    /// Feed arbitrary byte fragments. Returns the number of completed records.
    /// The callback executes synchronously and no event queue is allocated.
    pub fn feed(&mut self, mut input: &[u8], mut emit: impl FnMut(Event)) -> usize {
        let size = self.layout.record_bytes();
        let mut records = 0;
        while !input.is_empty() {
            // Complete records can be decoded in place; the scratch record is
            // needed only when a read splits the native event across chunks.
            if self.filled == 0 && input.len() >= size {
                let event = decode_record(&input[..size], self.layout, self.endian)
                    .expect("record has the configured layout size");
                input = &input[size..];
                records += 1;
                self.dispatch(event, &mut emit);
                continue;
            }
            let count = (size - self.filled).min(input.len());
            self.record[self.filled..self.filled + count].copy_from_slice(&input[..count]);
            self.filled += count;
            input = &input[count..];
            if self.filled == size {
                let event = decode_record(&self.record[..size], self.layout, self.endian)
                    .expect("record has the configured layout size");
                self.filled = 0;
                records += 1;
                self.dispatch(event, &mut emit);
            }
        }
        records
    }

    pub fn finish(&self) -> Result<(), Error> {
        if self.filled == 0 {
            Ok(())
        } else {
            Err(Error::IncompleteRecord)
        }
    }

    /// Call after querying device state with EVIOCG* through the system layer.
    pub fn resynchronized(&mut self) -> Result<(), Error> {
        if !self.dropped || !self.report_seen {
            return Err(Error::ResyncNotReady);
        }
        self.dropped = false;
        self.report_seen = false;
        Ok(())
    }

    fn dispatch(&mut self, event: InputEvent, emit: &mut impl FnMut(Event)) {
        if event.kind == EV_SYN && event.code == SYN_DROPPED {
            self.dropped = true;
            self.report_seen = false;
            emit(Event::ResyncRequired);
            return;
        }
        if self.dropped {
            if event.kind == EV_SYN && event.code == SYN_REPORT && !self.report_seen {
                self.report_seen = true;
                emit(Event::ReadyToResync);
            }
            return;
        }
        match (event.kind, event.code, event.value) {
            (EV_SYN, SYN_REPORT, _) => emit(Event::Synchronize),
            (EV_KEY, code, value @ 0..=2) => emit(Event::Key {
                code,
                transition: match value {
                    0 => KeyEvent::Release,
                    1 => KeyEvent::Press,
                    _ => KeyEvent::Repeat,
                },
            }),
            (EV_KEY, code, value) => emit(Event::InvalidKey { code, value }),
            _ => emit(Event::Other(event)),
        }
    }
}
