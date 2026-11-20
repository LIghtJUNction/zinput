//! Shared Rust trait and generated C ABI for input-method engine plugins.
//!
//! Built-in engines implement InputEngine directly. Native hosts use the
//! generated InputEngineApi and InputEngineProxy from the same declaration.
//! System library loading is provided by zplugin through the shared zsys layer.

pub use zplugin;

pub mod phase {
    pub const PRESS: u32 = 1;
    pub const REPEAT: u32 = 2;
    pub const RELEASE: u32 = 3;
}

/// Logical keys after the frontend applies its keyboard layout.
pub mod key {
    pub const TEXT: u32 = 0;
    pub const BACKSPACE: u32 = 1;
    pub const ENTER: u32 = 2;
    pub const ESCAPE: u32 = 3;
    pub const UP: u32 = 4;
    pub const DOWN: u32 = 5;
    pub const LEFT: u32 = 6;
    pub const RIGHT: u32 = 7;
    pub const SPACE: u32 = 8;
    pub const TAB: u32 = 9;
    pub const DELETE: u32 = 10;
    pub const PAGE_UP: u32 = 11;
    pub const PAGE_DOWN: u32 = 12;
    pub const HOME: u32 = 13;
    pub const END: u32 = 14;
}

/// Bit flags returned from process/select. Zero leaves the key to the frontend.
pub mod change {
    pub const PREEDIT: u32 = 1;
    pub const CANDIDATES: u32 = 2;
    pub const COMMIT: u32 = 4;
    pub const HANDLED: u32 = 8;
}

#[zplugin::abi_type(id = "dev.project-z.zinput.selection", version = 1)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EngineSelection {
    /// UTF-8 byte offsets in the string returned by preedit.
    pub start: u64,
    pub end: u64,
}

/// One engine object serves one focused context at a time.
///
/// Hosts reset on focus changes, then process logical key events. They fetch
/// preedit/candidates only when the corresponding change flag is returned.
/// Getter calls write complete valid UTF-8 and return the byte count. A short
/// output slice returns BufferTooSmall without changing output or engine state.
/// A pending commit must be drained before accepting another producing event;
/// engines return Busy rather than overwrite it. Reset deliberately discards
/// old-context composition and pending text. Native plugins are trusted code.
#[zplugin::interface(id = "dev.project-z.zinput.engine", version = 1)]
pub trait InputEngine {
    /// Zero blurs the engine; nonzero values identify an input context.
    fn reset(&mut self, context: u64) -> Result<(), zplugin::Error>;

    /// Modifier bits follow zinput::Modifiers. Unknown phases are invalid.
    fn process(
        &mut self,
        phase: u32,
        key: u32,
        modifiers: u32,
        text: &str,
    ) -> Result<u32, zplugin::Error>;

    fn preedit(&self, output: &mut [u8]) -> Result<u64, zplugin::Error>;
    fn selection(&self) -> Result<EngineSelection, zplugin::Error>;
    fn candidate_count(&self) -> Result<u32, zplugin::Error>;
    fn candidate(&self, index: u32, output: &mut [u8]) -> Result<u64, zplugin::Error>;
    fn select(&mut self, index: u32) -> Result<u32, zplugin::Error>;

    /// Drain pending committed text after successful complete copying.
    fn commit(&mut self, output: &mut [u8]) -> Result<u64, zplugin::Error>;
}
