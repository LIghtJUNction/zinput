//! Small fixed-capacity reference engine, not a complete language dictionary.
//!
//! Compose arbitrary UTF-8, convert "ni" to either "你" or "呢", and commit with
//! Enter/Space. Backspace removes one Unicode scalar. This exercises the same
//! generated interface used by separately compiled language engines.

use zinput::engine::zplugin::{self, Error};
use zinput::engine::{EngineSelection, InputEngine, InputEngineExport, change, key, phase};

const CAPACITY: usize = 128;

struct ReferenceEngine {
    context: u64,
    text: [u8; CAPACITY],
    len: usize,
    pending: [u8; CAPACITY],
    pending_len: usize,
}

impl ReferenceEngine {
    fn preedit_text(&self) -> &str {
        core::str::from_utf8(&self.text[..self.len]).expect("only UTF-8 is appended")
    }

    fn candidate_text(&self, index: u32) -> Result<&str, Error> {
        match (self.preedit_text(), index) {
            ("ni", 0) => Ok("你"),
            ("ni", 1) => Ok("呢"),
            (text, 0) if !text.is_empty() => Ok(text),
            _ => Err(Error::InvalidArgument),
        }
    }

    fn clear_preedit(&mut self) {
        self.text[..self.len].fill(0);
        self.len = 0;
    }

    fn clear_pending(&mut self) {
        self.pending[..self.pending_len].fill(0);
        self.pending_len = 0;
    }
}

fn copy_text(text: &[u8], output: &mut [u8]) -> Result<u64, Error> {
    if output.len() < text.len() {
        return Err(Error::BufferTooSmall);
    }
    output[..text.len()].copy_from_slice(text);
    Ok(text.len() as u64)
}

impl InputEngine for ReferenceEngine {
    fn reset(&mut self, context: u64) -> Result<(), Error> {
        self.clear_preedit();
        self.clear_pending();
        self.context = context;
        Ok(())
    }

    fn process(
        &mut self,
        transition: u32,
        key: u32,
        modifiers: u32,
        text: &str,
    ) -> Result<u32, Error> {
        if !matches!(transition, phase::PRESS | phase::REPEAT | phase::RELEASE)
            || modifiers > u8::MAX as u32
        {
            return Err(Error::InvalidArgument);
        }
        if self.context == 0 {
            return Err(Error::InvalidArgument);
        }
        if transition == phase::RELEASE {
            return Ok(0);
        }
        if self.pending_len != 0 {
            return Err(Error::Busy);
        }
        let shortcuts = zinput::Modifiers::CTRL
            | zinput::Modifiers::ALT
            | zinput::Modifiers::SUPER
            | zinput::Modifiers::HYPER
            | zinput::Modifiers::META;
        if modifiers & u32::from(shortcuts) != 0 {
            return Ok(0);
        }
        match key {
            key::TEXT if !text.is_empty() => {
                let new_len = self
                    .len
                    .checked_add(text.len())
                    .ok_or(Error::BufferTooSmall)?;
                if new_len > self.text.len() {
                    return Err(Error::BufferTooSmall);
                }
                self.text[self.len..new_len].copy_from_slice(text.as_bytes());
                self.len = new_len;
            }
            key::BACKSPACE if self.len != 0 => {
                let new_len = self.preedit_text().char_indices().next_back().unwrap().0;
                self.text[new_len..self.len].fill(0);
                self.len = new_len;
            }
            key::ESCAPE if self.len != 0 => self.clear_preedit(),
            key::ENTER | key::SPACE if self.len != 0 => return self.select(0),
            _ => return Ok(0),
        }
        Ok(change::HANDLED | change::PREEDIT | change::CANDIDATES)
    }

    fn preedit(&self, output: &mut [u8]) -> Result<u64, Error> {
        copy_text(&self.text[..self.len], output)
    }

    fn selection(&self) -> Result<EngineSelection, Error> {
        Ok(EngineSelection {
            start: self.len as u64,
            end: self.len as u64,
        })
    }

    fn candidate_count(&self) -> Result<u32, Error> {
        Ok(match self.preedit_text() {
            "" => 0,
            "ni" => 2,
            _ => 1,
        })
    }

    fn candidate(&self, index: u32, output: &mut [u8]) -> Result<u64, Error> {
        copy_text(self.candidate_text(index)?.as_bytes(), output)
    }

    fn select(&mut self, index: u32) -> Result<u32, Error> {
        if self.pending_len != 0 {
            return Err(Error::Busy);
        }
        // Copy into a local bounded buffer before mutating the source preedit.
        let mut selected = [0; CAPACITY];
        let len = copy_text(self.candidate_text(index)?.as_bytes(), &mut selected)? as usize;
        self.pending[..len].copy_from_slice(&selected[..len]);
        self.pending_len = len;
        self.clear_preedit();
        Ok(change::HANDLED | change::PREEDIT | change::CANDIDATES | change::COMMIT)
    }

    fn commit(&mut self, output: &mut [u8]) -> Result<u64, Error> {
        let written = copy_text(&self.pending[..self.pending_len], output)?;
        self.clear_pending();
        Ok(written)
    }
}

impl Drop for ReferenceEngine {
    fn drop(&mut self) {
        self.clear_preedit();
        self.clear_pending();
    }
}

fn create() -> Result<ReferenceEngine, Error> {
    Ok(ReferenceEngine {
        context: 0,
        text: [0; CAPACITY],
        len: 0,
        pending: [0; CAPACITY],
        pending_len: 0,
    })
}

zplugin::export!(
    InputEngineExport<ReferenceEngine>,
    constructor = create,
    version = (0, 1, 0),
    unload = pinned
);
