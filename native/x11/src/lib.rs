//! Linux XIM server for X11 and XWayland clients.
//!
//! Uses the installed xcb-imdkit for the XIM protocol and COMPOUND_TEXT encoding.
//! Each returned key belongs to an immutable input-context snapshot. A host
//! drains events and decides whether to consume or forward each key before
//! dispatching again. Candidate pixels are supplied by the host; this adapter
//! creates only an input-transparent, override-redirect window.
//!
//! The advertised styles are PreeditNothing with StatusNothing/StatusNone.
//! Composition is rendered by the host's candidate popup. XIM does not provide
//! password-purpose hints or surrounding-text generations.

#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fmt,
    marker::PhantomData,
    ptr::NonNull,
    rc::Rc,
    time::Duration,
};

pub const MAX_TEXT_BYTES: usize = 4000;
pub const MAX_POPUP_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_POPUP_DIMENSION: u32 = 2048;

#[derive(Debug)]
pub struct Error(String);
impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

/// Identity of the last delivered focus/reset state. Repeating SET_IC_FOCUS on
/// the same XIC preserves it; changing XIC, changing its focus window, unsetting
/// focus or resetting composition creates a new generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Context {
    pub generation: u64,
    pub serial: u32,
    pub active: bool,
    /// Always false: XIM supplies no standardized password-purpose metadata.
    pub sensitive: bool,
}

/// One X11 key and its original input-context snapshot. The private token
/// expires at the next next_event/dispatch call and prevents duplicate forward.
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub context: Context,
    pub generation: u64,
    pub serial: u32,
    /// X11 keycode, including the XKB offset of eight.
    pub keycode: u32,
    pub time: u32,
    pub pressed: bool,
    /// A repeated press from the X server, including detectable autorepeat.
    /// Release events remain visible; this adapter creates no repeat events.
    pub repeat: bool,
    pub keysym: u32,
    pub text: String,
    /// Matches zinput::Modifiers (Shift=1, Alt=2, Control=4, Super=8).
    pub modifiers: u32,
    token: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    State(Context),
    Key(KeyEvent),
}

pub mod keysym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const DELETE: u32 = 0xffff;
    pub const ISO_LEFT_TAB: u32 = 0xfe20;
    pub const SPACE: u32 = 0x20;
}

/// Tightly packed premultiplied ARGB8888 bytes in B,G,R,A order. X11 coordinates
/// are physical pixels; this adapter currently accepts scale=1.
pub struct PopupFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub pixels: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopupPresentation {
    Presented,
    /// Reserved for presenters that queue frames; XCB copies requests immediately.
    Queued,
    Unavailable,
}

#[repr(C)]
struct RawEvent {
    kind: u32,
    active: u32,
    generation: u64,
    serial: u32,
    keycode: u32,
    token: u64,
    time: u32,
    pressed: u32,
    keysym: u32,
    modifiers: u32,
    text_length: u32,
    text: [u8; 64],
}
impl RawEvent {
    fn empty() -> Self {
        Self {
            kind: 0,
            active: 0,
            generation: 0,
            serial: 0,
            keycode: 0,
            token: 0,
            time: 0,
            pressed: 0,
            keysym: 0,
            modifiers: 0,
            text_length: 0,
            text: [0; 64],
        }
    }
    fn context(&self) -> Context {
        Context {
            generation: self.generation,
            serial: self.serial,
            active: self.active != 0,
            sensitive: false,
        }
    }
}

unsafe extern "C" {
    fn zix_open(name: *const c_char, error: *mut c_char, capacity: usize) -> *mut c_void;
    fn zix_close(server: *mut c_void);
    fn zix_error(server: *mut c_void) -> *const c_char;
    fn zix_dispatch(server: *mut c_void, timeout_ms: c_int) -> c_int;
    fn zix_next(server: *mut c_void, event: *mut RawEvent) -> c_int;
    fn zix_forward(server: *mut c_void, token: u64, generation: u64, serial: u32) -> c_int;
    fn zix_update(
        server: *mut c_void,
        generation: u64,
        serial: u32,
        commit: *const u8,
        length: usize,
        clear: c_int,
    ) -> c_int;
    fn zix_popup_supported(server: *mut c_void) -> c_int;
    fn zix_popup(
        server: *mut c_void,
        generation: u64,
        serial: u32,
        pixels: *const u8,
        width: u32,
        height: u32,
    ) -> c_int;
    fn zix_hide_popup(server: *mut c_void) -> c_int;
}

/// Recognize both X11 repeat encodings without delaying or suppressing events.
/// Core autorepeat emits release/press pairs with the same timestamp; clients
/// using detectable autorepeat send another press while the key remains held.
struct RepeatState {
    held: [bool; 256],
    last_release: Option<(u32, u32)>,
}
impl Default for RepeatState {
    fn default() -> Self {
        Self {
            held: [false; 256],
            last_release: None,
        }
    }
}
impl RepeatState {
    fn observe(&mut self, code: u32, time: u32, pressed: bool) -> Result<bool> {
        let held = self
            .held
            .get_mut(code as usize)
            .ok_or_else(|| Error::new("X11 keycode exceeds the protocol range"))?;
        let repeat = pressed && (*held || self.last_release == Some((code, time)));
        let was_held = *held;
        *held = pressed;
        self.last_release = if !pressed && was_held {
            Some((code, time))
        } else {
            None
        };
        Ok(repeat)
    }
}

/// One owned XIM server connection. It must remain on its constructing thread.
/// An existing owner of the configured name causes connect to fail; the adapter
/// never claims the fcitx name by default and does not grab the global keyboard.
pub struct InputMethod {
    raw: NonNull<c_void>,
    current: Context,
    repeat: RepeatState,
    _thread: PhantomData<Rc<()>>,
}

impl InputMethod {
    pub fn connect() -> Result<Self> {
        Self::connect_named("ztype")
    }

    pub fn connect_named(name: &str) -> Result<Self> {
        validate_name(name)?;
        let name = CString::new(name).map_err(|_| Error::new("invalid XIM server name"))?;
        let mut error = [0 as c_char; 256];
        // SAFETY: name and error storage stay valid for the synchronous call.
        // The bridge owns and returns one independent server or null.
        let raw = unsafe { zix_open(name.as_ptr(), error.as_mut_ptr(), error.len()) };
        let raw = NonNull::new(raw).ok_or_else(|| {
            // SAFETY: the bridge writes a bounded NUL-terminated error string.
            Error::new(
                unsafe { CStr::from_ptr(error.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            )
        })?;
        Ok(Self {
            raw,
            current: Context::default(),
            repeat: RepeatState::default(),
            _thread: PhantomData,
        })
    }

    pub fn context(&self) -> Context {
        self.current
    }

    /// Dispatch protocol events, waiting at most 60 seconds. If raw events are
    /// already queued this returns immediately. Call next_event until empty.
    pub fn dispatch(&mut self, timeout: Duration) -> Result<()> {
        let millis = timeout.as_millis().min(60_000) as c_int;
        // SAFETY: the server is exclusively owned on this thread.
        if unsafe { zix_dispatch(self.raw.as_ptr(), millis) } == 0 {
            return Err(self.native_error());
        }
        Ok(())
    }

    /// Return the next current context/key. Obsolete queued keys are forwarded
    /// unchanged to their original live XIC, never to the new engine session.
    /// Visible current state advances only when this returns Event::State.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        let mut raw = RawEvent::empty();
        // SAFETY: RawEvent is the matching C ABI layout and is writable.
        match unsafe { zix_next(self.raw.as_ptr(), &mut raw) } {
            0 => return Ok(None),
            -1 => return Err(self.native_error()),
            _ => {}
        }
        let context = raw.context();
        match raw.kind {
            1 => {
                if self.current != context {
                    self.repeat = RepeatState::default();
                }
                self.current = context;
                Ok(Some(Event::State(context)))
            }
            2 => {
                if context != self.current {
                    return Err(Error::new("XIM key arrived before its focus state"));
                }
                let length = raw.text_length as usize;
                if length >= raw.text.len() {
                    return Err(Error::new("XKB key text exceeded its bound"));
                }
                let text = std::str::from_utf8(&raw.text[..length])
                    .map_err(|_| Error::new("XKB returned invalid UTF-8"))?
                    .to_owned();
                Ok(Some(Event::Key(KeyEvent {
                    context,
                    generation: raw.generation,
                    serial: raw.serial,
                    keycode: raw.keycode,
                    time: raw.time,
                    pressed: raw.pressed != 0,
                    repeat: self
                        .repeat
                        .observe(raw.keycode, raw.time, raw.pressed != 0)?,
                    keysym: raw.keysym,
                    text,
                    modifiers: raw.modifiers,
                    token: raw.token,
                })))
            }
            _ => Err(Error::new("unknown native XIM event")),
        }
    }

    /// Forward this event at most once. A release is forwarded only if a press
    /// for that key was previously forwarded to the same XIC. Unforwarded
    /// presses are consumed when the host asks for the next event.
    pub fn forward_key(&mut self, event: &KeyEvent) -> Result<()> {
        self.check_context(&event.context)?;
        // SAFETY: the opaque token is checked against the bridge's current
        // pending event and live XIC before it accesses any protocol object.
        if unsafe {
            zix_forward(
                self.raw.as_ptr(),
                event.token,
                event.generation,
                event.serial,
            )
        } == 0
        {
            return Err(self.native_error());
        }
        Ok(())
    }

    /// Validate preedit/cursor and optionally commit a complete UTF-8 string.
    /// The host draws preedit in its popup for the advertised PreeditNothing
    /// style. An empty preedit hides the popup. Encoding is round-tripped before
    /// sending a commit so an unsupported character is never silently dropped.
    pub fn update(
        &mut self,
        context: &Context,
        preedit: &str,
        cursor: i32,
        commit: Option<&str>,
    ) -> Result<()> {
        self.check_context(context)?;
        validate_text(preedit, cursor)?;
        if let Some(text) = commit {
            validate_text(text, text.len() as i32)?;
        }
        let (pointer, length) =
            commit.map_or((std::ptr::null(), 0), |text| (text.as_ptr(), text.len()));
        // SAFETY: UTF-8/cursor/length have been validated; the bridge copies
        // protocol bytes synchronously and checks its private live IC generation.
        if unsafe {
            zix_update(
                self.raw.as_ptr(),
                context.generation,
                context.serial,
                pointer,
                length,
                c_int::from(preedit.is_empty()),
            )
        } == 0
        {
            return Err(self.native_error());
        }
        Ok(())
    }

    pub fn popup_supported(&self) -> bool {
        // SAFETY: immutable capability query on the owned server.
        unsafe { zix_popup_supported(self.raw.as_ptr()) != 0 }
    }

    /// Copy one complete image to an X server pixmap and present it near the IC
    /// spot; without a spot, use the focus window's lower-left edge. The window
    /// has an empty input shape and never receives focus or pointer events.
    pub fn present_popup(
        &mut self,
        context: &Context,
        frame: PopupFrame<'_>,
    ) -> Result<PopupPresentation> {
        self.check_context(context)?;
        validate_frame(&frame)?;
        // SAFETY: the checked byte slice stays alive through the synchronous
        // XCB copy. The bridge validates native dimensions and the live XIC.
        match unsafe {
            zix_popup(
                self.raw.as_ptr(),
                context.generation,
                context.serial,
                frame.pixels.as_ptr(),
                frame.width,
                frame.height,
            )
        } {
            1 => Ok(PopupPresentation::Presented),
            2 => Ok(PopupPresentation::Unavailable),
            _ => Err(self.native_error()),
        }
    }

    pub fn hide_popup(&mut self) -> Result<()> {
        // SAFETY: the server and any popup resources are exclusively owned.
        if unsafe { zix_hide_popup(self.raw.as_ptr()) } == 0 {
            return Err(self.native_error());
        }
        Ok(())
    }

    fn check_context(&self, context: &Context) -> Result<()> {
        if *context != self.current || !context.active || context.sensitive {
            Err(Error::new("input context is inactive or was replaced"))
        } else {
            Ok(())
        }
    }

    fn native_error(&self) -> Error {
        // SAFETY: the bridge retains a bounded NUL-terminated diagnostic buffer.
        let message = unsafe { CStr::from_ptr(zix_error(self.raw.as_ptr())) };
        Error::new(message.to_string_lossy().into_owned())
    }
}
impl Drop for InputMethod {
    fn drop(&mut self) {
        // SAFETY: uniquely owned server; no event callback escapes the bridge.
        unsafe { zix_close(self.raw.as_ptr()) };
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err(Error::new(
            "XIM name must contain 1 to 63 ASCII identifier characters",
        ));
    }
    Ok(())
}
fn validate_text(text: &str, cursor: i32) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES || text.as_bytes().contains(&0) {
        return Err(Error::new("input text exceeds its bound or contains NUL"));
    }
    if cursor != -1
        && (cursor < 0 || cursor as usize > text.len() || !text.is_char_boundary(cursor as usize))
    {
        return Err(Error::new("preedit cursor is not a UTF-8 boundary"));
    }
    Ok(())
}
fn validate_frame(frame: &PopupFrame<'_>) -> Result<()> {
    let length = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|n| n.checked_mul(4));
    if !matches!(frame.width, 1..=MAX_POPUP_DIMENSION)
        || !matches!(frame.height, 1..=MAX_POPUP_DIMENSION)
        || frame.scale != 1
        || length.is_none_or(|length| length > MAX_POPUP_BYTES || length != frame.pixels.len())
    {
        return Err(Error::new("invalid or oversized X11 candidate image"));
    }
    if frame
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[..3].iter().any(|&channel| channel > pixel[3]))
    {
        return Err(Error::new("candidate image is not premultiplied ARGB8888"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_names_and_utf8_cursors() {
        for valid in ["ztype", "test-1.2_a"] {
            validate_name(valid).unwrap();
        }
        for invalid in ["", "fcitx name", "@im=ztype", "输入法"] {
            assert!(validate_name(invalid).is_err());
        }
        assert!(validate_name(&"a".repeat(64)).is_err());
        validate_text("你好", 6).unwrap();
        validate_text("你好", -1).unwrap();
        for cursor in [-2, 1, 4, 7] {
            assert!(validate_text("你好", cursor).is_err());
        }
        assert!(validate_text("a\0b", 3).is_err());
        assert!(validate_text(&"a".repeat(MAX_TEXT_BYTES + 1), 0).is_err());
    }
    #[test]
    fn popup_bounds_and_alpha_are_checked_before_io() {
        let valid = [0x20, 0x30, 0x40, 0x80];
        validate_frame(&PopupFrame {
            width: 1,
            height: 1,
            scale: 1,
            pixels: &valid,
        })
        .unwrap();
        for (width, height, scale, pixels) in [
            (0, 1, 1, valid.as_slice()),
            (1, 1, 0, valid.as_slice()),
            (1, 1, 2, valid.as_slice()),
            (u32::MAX, u32::MAX, 1, valid.as_slice()),
            (1, 1, 1, &valid[..3]),
            (1, 1, 1, &[255, 0, 0, 1][..]),
        ] {
            assert!(
                validate_frame(&PopupFrame {
                    width,
                    height,
                    scale,
                    pixels
                })
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod repeat_tests {
    use super::RepeatState;

    #[test]
    fn detects_core_and_detectable_autorepeat() {
        let mut state = RepeatState::default();
        assert!(!state.observe(38, 10, true).unwrap());
        assert!(!state.observe(38, 500, false).unwrap());
        assert!(state.observe(38, 500, true).unwrap());
        assert!(state.observe(38, 530, true).unwrap());
        assert!(!state.observe(38, 600, false).unwrap());
        assert!(!state.observe(38, 700, true).unwrap());
    }

    #[test]
    fn repeat_requires_matching_key_and_timestamp() {
        let mut state = RepeatState::default();
        assert!(!state.observe(38, 10, true).unwrap());
        assert!(!state.observe(38, 500, false).unwrap());
        assert!(!state.observe(39, 500, true).unwrap());
        assert!(!state.observe(38, 500, true).unwrap());
        assert!(!state.observe(38, 600, false).unwrap());
        assert!(!state.observe(38, 601, true).unwrap());
        assert!(state.observe(256, 700, true).is_err());
    }

    #[test]
    fn unpaired_release_after_focus_does_not_create_a_repeat() {
        let mut state = RepeatState::default();
        assert!(!state.observe(38, 500, false).unwrap());
        assert!(!state.observe(38, 500, true).unwrap());
    }
}
