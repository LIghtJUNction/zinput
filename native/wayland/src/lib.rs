//! Linux input-method-v2 connection, layout mapping and paired key forwarding.
//!
//! The adapter owns a single Wayland connection. Its virtual keyboard uses the
//! same connection as its input method, so compositor IME routing can exclude
//! the adapter's forwarded events from its own grab. Native input protocols and
//! keymaps are supplied by the desktop; no keyboard device is opened.
#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]

mod loader;
mod popup;
mod protocol;
pub use popup::{
    MAX_POPUP_BYTES, MAX_POPUP_DIMENSION, PopupFrame, PopupPresentation, PopupRectangle,
};
mod xkb;

use loader::{Library, symbol};
use protocol::Interface;
use std::{
    collections::VecDeque,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fmt,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    ptr,
    time::{Duration, Instant},
};

const MAX_EVENTS: usize = 1024;
const MAX_KEYMAP: usize = 2 * 1024 * 1024;
const MAX_KEYS: usize = 768;
/// Maximum UTF-8 byte length of each input-method-v2 string.
pub const MAX_TEXT_BYTES: usize = 4000;
type Proxy = *mut c_void;

/// A native connection, protocol or input-contract error. Text never includes
/// the user's preedit, committed text, surrounding text or keymap.
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

/// Advertised globals; probing does not register an IME or grab the keyboard.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub input_method: bool,
    pub virtual_keyboard: bool,
    pub seat: bool,
}
impl Capabilities {
    pub fn supported(self) -> bool {
        self.input_method && self.virtual_keyboard && self.seat
    }
}

/// The last atomically applied text-input state. A generation changes whenever
/// activation changes or an explicit external edit invalidates composition.
/// Serial counts `done` events, as required by the protocol.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Context {
    pub generation: u64,
    pub serial: u32,
    pub active: bool,
    /// Password/PIN purpose or hidden/sensitive hint. No grab is held here.
    pub sensitive: bool,
}

/// A logical key in the compositor's current layout. `keycode` is evdev, while
/// `keysym` uses XKB symbols. Modifier bits match `zinput::Modifiers`.
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub generation: u64,
    pub serial: u32,
    pub keycode: u32,
    pub time: u32,
    pub pressed: bool,
    /// Client-side repeat for a consumed key; forwarding it is a no-op because
    /// the receiving application repeats its forwarded physical press itself.
    pub repeat: bool,
    pub keysym: u32,
    pub text: String,
    pub modifiers: u32,
}

#[derive(Clone, Debug)]
pub enum Event {
    State(Context),
    Key(KeyEvent),
}

/// Common symbols useful to an engine host. All other symbols remain exposed.
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

#[derive(Default)]
struct ProtocolState {
    current: Context,
    pending: Context,
    focus_transition: bool,
    pending_other: bool,
}
impl ProtocolState {
    fn activate(&mut self) -> Result<()> {
        self.focus_transition = true;
        self.pending_other = false;
        self.pending = Context {
            generation: self
                .pending
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::new("input context generation exhausted"))?,
            serial: self.current.serial,
            active: true,
            sensitive: false,
        };
        Ok(())
    }
    fn deactivate(&mut self) -> Result<()> {
        self.focus_transition = true;
        self.pending.generation = self
            .pending
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::new("input context generation exhausted"))?;
        self.pending.active = false;
        self.pending.sensitive = false;
        Ok(())
    }
    fn content(&mut self, hints: u32, purpose: u32) {
        self.pending.sensitive = hints & 0xc0 != 0 || purpose == 8 || purpose == 9;
    }
    fn external_change(&self) -> bool {
        self.pending_other && !self.focus_transition && self.current.active && self.pending.active
    }
    fn popup_ready(&self) -> bool {
        // A read can end between a context update and its done event. Holding
        // pixels here preserves double-buffered state without redrawing a
        // previous context while a known transition is still incomplete.
        !self.focus_transition
            && !self.pending_other
            && self.current.active
            && !self.current.sensitive
            && self.pending.active
            && !self.pending.sensitive
    }
    fn done(&mut self) -> Result<Context> {
        if self.external_change() {
            self.pending.generation = self
                .pending
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::new("input context generation exhausted"))?;
        }
        self.pending.serial = self.current.serial.wrapping_add(1);
        self.current = self.pending;
        self.focus_transition = false;
        self.pending_other = false;
        Ok(self.current)
    }
}

enum RawEvent {
    Activate,
    Deactivate,
    Content(u32, u32),
    ChangeCause(u32),
    PopupReleased(u64),
    PopupRectangle(PopupRectangle),
    PopupUnavailable,
    Done,
    Keymap(File, Vec<u8>),
    Key { time: u32, code: u32, pressed: bool },
    Modifiers([u32; 4]),
    Repeat { rate: i32, delay: i32 },
}
#[derive(Default)]
struct State {
    seat: Option<(u32, u32)>,
    compositor: Option<(u32, u32)>,
    shm: Option<u32>,
    shm_argb8888: bool,
    im_manager: Option<u32>,
    vk_manager: Option<u32>,
    events: VecDeque<RawEvent>,
    failure: Option<Error>,
    unavailable: bool,
    synced: bool,
}
impl State {
    fn push(&mut self, event: RawEvent) {
        if self.failure.is_some() {
            return;
        }
        if self.events.len() == MAX_EVENTS {
            self.failure = Some(Error::new("Wayland input event queue exceeded its bound"));
        } else {
            self.events.push_back(event);
        }
    }
    fn check(&mut self) -> Result<()> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if self.unavailable {
            return Err(Error::new(
                "Wayland input method unavailable: another IME owns this seat or the seat was removed",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Api {
    connect: unsafe extern "C" fn(*const c_char) -> Proxy,
    disconnect: unsafe extern "C" fn(Proxy),
    get_fd: unsafe extern "C" fn(Proxy) -> c_int,
    flush: unsafe extern "C" fn(Proxy) -> c_int,
    dispatch_pending: unsafe extern "C" fn(Proxy) -> c_int,
    prepare_read: unsafe extern "C" fn(Proxy) -> c_int,
    read_events: unsafe extern "C" fn(Proxy) -> c_int,
    cancel_read: unsafe extern "C" fn(Proxy),
    destroy: unsafe extern "C" fn(Proxy),
    add_listener: unsafe extern "C" fn(Proxy, *const *const c_void, Proxy) -> c_int,
    marshal: unsafe extern "C" fn(Proxy, u32, *const Interface, u32, u32, ...) -> Proxy,
    registry: *const Interface,
    seat: *const Interface,
    callback: *const Interface,
    compositor: *const Interface,
    surface: *const Interface,
    region: *const Interface,
    shm: *const Interface,
    shm_pool: *const Interface,
    buffer: *const Interface,
}
impl Api {
    fn load(lib: &Library) -> Result<Self> {
        Ok(Self {
            connect: symbol!(
                lib,
                c"wl_display_connect",
                unsafe extern "C" fn(*const c_char) -> Proxy
            ),
            disconnect: symbol!(lib, c"wl_display_disconnect", unsafe extern "C" fn(Proxy)),
            get_fd: symbol!(
                lib,
                c"wl_display_get_fd",
                unsafe extern "C" fn(Proxy) -> c_int
            ),
            flush: symbol!(
                lib,
                c"wl_display_flush",
                unsafe extern "C" fn(Proxy) -> c_int
            ),
            dispatch_pending: symbol!(
                lib,
                c"wl_display_dispatch_pending",
                unsafe extern "C" fn(Proxy) -> c_int
            ),
            prepare_read: symbol!(
                lib,
                c"wl_display_prepare_read",
                unsafe extern "C" fn(Proxy) -> c_int
            ),
            read_events: symbol!(
                lib,
                c"wl_display_read_events",
                unsafe extern "C" fn(Proxy) -> c_int
            ),
            cancel_read: symbol!(lib, c"wl_display_cancel_read", unsafe extern "C" fn(Proxy)),
            destroy: symbol!(lib, c"wl_proxy_destroy", unsafe extern "C" fn(Proxy)),
            add_listener: symbol!(
                lib,
                c"wl_proxy_add_listener",
                unsafe extern "C" fn(Proxy, *const *const c_void, Proxy) -> c_int
            ),
            marshal: symbol!(
                lib,
                c"wl_proxy_marshal_flags",
                unsafe extern "C" fn(Proxy, u32, *const Interface, u32, u32, ...) -> Proxy
            ),
            registry: lib.address(c"wl_registry_interface")?.cast(),
            seat: lib.address(c"wl_seat_interface")?.cast(),
            callback: lib.address(c"wl_callback_interface")?.cast(),
            compositor: lib.address(c"wl_compositor_interface")?.cast(),
            surface: lib.address(c"wl_surface_interface")?.cast(),
            region: lib.address(c"wl_region_interface")?.cast(),
            shm: lib.address(c"wl_shm_interface")?.cast(),
            shm_pool: lib.address(c"wl_shm_pool_interface")?.cast(),
            buffer: lib.address(c"wl_buffer_interface")?.cast(),
        })
    }
}
#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: usize, timeout: c_int) -> c_int;
}

struct Connection {
    api: Api,
    display: Proxy,
    registry: Proxy,
    compositor: Proxy,
    shm: Proxy,
    popup_version: u32,
    seat: Proxy,
    im_manager: Proxy,
    vk_manager: Proxy,
    state: Box<State>,
    _library: Library,
}
impl Connection {
    fn new() -> Result<Self> {
        let lib = Library::open(c"libwayland-client.so.0")?;
        let api = Api::load(&lib)?;
        let mut this = Self {
            api,
            display: ptr::null_mut(),
            registry: ptr::null_mut(),
            compositor: ptr::null_mut(),
            shm: ptr::null_mut(),
            popup_version: 0,
            seat: ptr::null_mut(),
            im_manager: ptr::null_mut(),
            vk_manager: ptr::null_mut(),
            state: Box::default(),
            _library: lib,
        };
        // SAFETY: null selects WAYLAND_DISPLAY/WAYLAND_SOCKET according to libwayland.
        unsafe {
            this.display = (this.api.connect)(ptr::null());
            if this.display.is_null() {
                return Err(Error::new("cannot connect to the current Wayland display"));
            }
            this.registry = (this.api.marshal)(
                this.display,
                1,
                this.api.registry,
                1,
                0,
                ptr::null_mut::<c_void>(),
            );
        }
        if this.registry.is_null() {
            return Err(Error::new("cannot obtain the Wayland registry"));
        }
        this.listen(
            this.registry,
            (&REGISTRY_LISTENER as *const RegistryListener).cast(),
        )?;
        this.sync()?;
        Ok(this)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            input_method: self.state.im_manager.is_some(),
            virtual_keyboard: self.state.vk_manager.is_some(),
            seat: self.state.seat.is_some(),
        }
    }
    fn bind(&self, name: u32, interface: &Interface, version: u32) -> Result<Proxy> {
        // SAFETY: registry.bind signature is name, interface string, version, new_id.
        let object = unsafe {
            (self.api.marshal)(
                self.registry,
                0,
                interface,
                version,
                0,
                name,
                interface.name,
                version,
                ptr::null_mut::<c_void>(),
            )
        };
        if object.is_null() {
            Err(Error::new("cannot bind a Wayland input interface"))
        } else {
            Ok(object)
        }
    }
    fn bind_inputs(&mut self) -> Result<()> {
        let (seat, version) = self
            .state
            .seat
            .ok_or_else(|| Error::new("Wayland compositor has no seat"))?;
        let im = self
            .state
            .im_manager
            .ok_or_else(|| Error::new("Wayland compositor does not expose input-method-v2"))?;
        let vk = self
            .state
            .vk_manager
            .ok_or_else(|| Error::new("Wayland compositor does not expose virtual-keyboard-v1"))?;
        // SAFETY: api.seat points to the library's static interface descriptor.
        self.seat = self.bind(seat, unsafe { &*self.api.seat }, version.min(7))?;
        self.listen(self.seat, (&SEAT_LISTENER as *const SeatListener).cast())?;
        self.im_manager = self.bind(im, &protocol::IM_MANAGER, 1)?;
        self.vk_manager = self.bind(vk, &protocol::VK_MANAGER, 1)?;
        self.bind_popup_globals()
    }
    fn bind_popup_globals(&mut self) -> Result<()> {
        let (Some((name, version)), Some(shm)) = (self.state.compositor, self.state.shm) else {
            return Ok(());
        };
        self.popup_version = version.min(4);
        // Descriptors are static exports from the live Wayland library.
        self.compositor = self.bind(name, unsafe { &*self.api.compositor }, self.popup_version)?;
        self.shm = self.bind(shm, unsafe { &*self.api.shm }, 1)?;
        self.listen(self.shm, (&SHM_LISTENER as *const ShmListener).cast())?;
        // Negotiate globals/formats before registering the input method. The
        // surface stays lazy, without nested dispatch inside present_popup.
        self.sync()
    }
    fn listen(&mut self, object: Proxy, listener: *const *const c_void) -> Result<()> {
        let data = (&mut *self.state as *mut State).cast();
        // SAFETY: listeners are static C tables; Box keeps callback data stable.
        if unsafe { (self.api.add_listener)(object, listener, data) } != 0 {
            return Err(Error::new("cannot attach a Wayland listener"));
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        self.state.synced = false;
        // SAFETY: wl_display.sync creates the callback described by this ABI.
        let callback = unsafe {
            (self.api.marshal)(
                self.display,
                0,
                self.api.callback,
                1,
                0,
                ptr::null_mut::<c_void>(),
            )
        };
        if callback.is_null() {
            return Err(Error::new("cannot synchronize the Wayland display"));
        }
        let result = (|| {
            self.listen(
                callback,
                (&CALLBACK_LISTENER as *const CallbackListener).cast(),
            )?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while !self.state.synced {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::new("Wayland display synchronization timed out"));
                }
                self.pump(remaining)?;
            }
            self.state.check()
        })();
        // SAFETY: a sync callback has no client destructor request.
        unsafe {
            (self.api.destroy)(callback);
        }
        result
    }
    fn flush(&self) -> Result<bool> {
        // SAFETY: display is a live connection owned here.
        if unsafe { (self.api.flush)(self.display) } >= 0 {
            return Ok(true);
        }
        if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        Err(Error::new("Wayland connection flush failed"))
    }
    fn pump(&mut self, timeout: Duration) -> Result<()> {
        self.state.check()?;
        // SAFETY: all display reads occur on this thread, paired with read/cancel.
        unsafe {
            while (self.api.prepare_read)(self.display) != 0 {
                if (self.api.dispatch_pending)(self.display) < 0 {
                    return Err(Error::new("Wayland event dispatch failed"));
                }
                self.state.check()?;
            }
            let flushed = match self.flush() {
                Ok(value) => value,
                Err(e) => {
                    (self.api.cancel_read)(self.display);
                    return Err(e);
                }
            };
            let mut fd = PollFd {
                fd: (self.api.get_fd)(self.display),
                events: 1 | if flushed { 0 } else { 4 },
                revents: 0,
            };
            let timeout = timeout
                .as_nanos()
                .div_ceil(1_000_000)
                .min(c_int::MAX as u128) as c_int;
            let ready = poll(&mut fd, 1, timeout);
            if ready < 0 {
                (self.api.cancel_read)(self.display);
                return if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    Ok(())
                } else {
                    Err(Error::new("Wayland display poll failed"))
                };
            }
            if fd.revents & (8 | 16 | 32) != 0 {
                (self.api.cancel_read)(self.display);
                return Err(Error::new("Wayland compositor disconnected"));
            }
            if fd.revents & 1 != 0 {
                if (self.api.read_events)(self.display) < 0 {
                    return Err(Error::new("Wayland event read failed"));
                }
            } else {
                (self.api.cancel_read)(self.display);
            }
            if fd.revents & 4 != 0 {
                self.flush()?;
            }
            if (self.api.dispatch_pending)(self.display) < 0 {
                return Err(Error::new("Wayland event dispatch failed"));
            }
        }
        self.state.check()
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: local proxies are destroyed before their display and library.
        unsafe {
            if !self.display.is_null() {
                for proxy in [
                    self.shm,
                    self.compositor,
                    self.vk_manager,
                    self.im_manager,
                    self.seat,
                    self.registry,
                ] {
                    if !proxy.is_null() {
                        (self.api.destroy)(proxy);
                    }
                }
                (self.api.disconnect)(self.display);
            }
        }
    }
}

/// Inspect protocol availability without taking the seat's input-method slot.
pub fn probe() -> Result<Capabilities> {
    Ok(Connection::new()?.capabilities())
}

struct Repeat {
    code: u32,
    time: u32,
    started: Instant,
    next: Instant,
}

/// Native frontend for one seat. Keep it on the constructing thread and call
/// `dispatch` followed by `next_event` until the queue is drained. Composition
/// policy and candidate UI remain in the engine/application.
pub struct InputMethod {
    // Drop native popup objects before the display and library.
    popup: Option<popup::Popup>,
    connection: Connection,
    input: Proxy,
    keyboard: Proxy,
    grab: Proxy,
    state: ProtocolState,
    keymap: Option<xkb::Keymap>,
    raw_modifiers: [u32; 4],
    forwarded: [bool; MAX_KEYS],
    held: [bool; MAX_KEYS],
    repeat: Option<Repeat>,
    repeat_rate: i32,
    repeat_delay: i32,
    last_time: u32,
    preedit_present: bool,
}
impl InputMethod {
    pub fn connect() -> Result<Self> {
        let mut connection = Connection::new()?;
        connection.bind_inputs()?;
        let mut this = Self {
            popup: None,
            connection,
            input: ptr::null_mut(),
            keyboard: ptr::null_mut(),
            grab: ptr::null_mut(),
            state: ProtocolState::default(),
            keymap: None,
            raw_modifiers: [0; 4],
            forwarded: [false; MAX_KEYS],
            held: [false; MAX_KEYS],
            repeat: None,
            repeat_rate: 0,
            repeat_delay: 0,
            last_time: 0,
            preedit_present: false,
        };
        // SAFETY: manager.get_input_method arguments follow the protocol table.
        this.input = unsafe {
            (this.connection.api.marshal)(
                this.connection.im_manager,
                0,
                &protocol::INPUT_METHOD,
                1,
                0,
                this.connection.seat,
                ptr::null_mut::<c_void>(),
            )
        };
        if this.input.is_null() {
            return Err(Error::new("cannot create Wayland input method"));
        }
        this.connection
            .listen(this.input, (&IM_LISTENER as *const ImListener).cast())?;
        this.connection.sync()?;
        // Both input method and virtual keyboard intentionally use one wl_client.
        // Compositors distinguish this forwarding keyboard from physical input.
        this.keyboard = unsafe {
            (this.connection.api.marshal)(
                this.connection.vk_manager,
                0,
                &protocol::VIRTUAL_KEYBOARD,
                1,
                0,
                this.connection.seat,
                ptr::null_mut::<c_void>(),
            )
        };
        if this.keyboard.is_null() {
            return Err(Error::new("cannot create the IME forwarding keyboard"));
        }
        this.connection.flush()?;
        Ok(this)
    }
    pub fn context(&self) -> Context {
        self.state.current
    }
    pub fn dispatch(&mut self, timeout: Duration) -> Result<()> {
        if !self.connection.state.events.is_empty() {
            return self.connection.state.check();
        }
        let timeout = self.repeat.as_ref().map_or(timeout, |repeat| {
            timeout.min(repeat.next.saturating_duration_since(Instant::now()))
        });
        self.connection.pump(timeout)
    }
    /// Apply protocol events in order; a returned State is already current.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        self.connection.state.check()?;
        while let Some(event) = self.connection.state.events.pop_front() {
            match event {
                RawEvent::Activate => self.state.activate()?,
                RawEvent::Deactivate => self.state.deactivate()?,
                RawEvent::Content(hints, purpose) => self.state.content(hints, purpose),
                RawEvent::ChangeCause(cause) => self.state.pending_other = cause != 0,
                RawEvent::PopupReleased(token) => {
                    if let Some(popup) = self.popup.as_mut() {
                        popup.released(token);
                    }
                }
                RawEvent::PopupRectangle(rectangle) => {
                    if let Some(popup) = self.popup.as_mut() {
                        popup.rectangle = Some(rectangle);
                    }
                }
                RawEvent::PopupUnavailable => {
                    if let Some(popup) = self.popup.as_mut() {
                        popup.hide();
                    }
                    self.connection.flush()?;
                }
                RawEvent::Done => {
                    let focus_transition = self.state.focus_transition;
                    let external_change = self.state.external_change();
                    let current = self.state.done()?;
                    if (focus_transition || external_change || !current.active || current.sensitive)
                        && let Some(popup) = self.popup.as_mut()
                    {
                        popup.hide();
                    }
                    if focus_transition {
                        // Activation resets the protocol preedit for this context.
                        self.preedit_present = false;
                    }
                    if focus_transition || !current.active || current.sensitive {
                        self.release_forwarded();
                        self.held.fill(false);
                        self.repeat = None;
                    }
                    if external_change {
                        // External edits invalidate composition, not physical key
                        // ownership. Preserve forwarded Ctrl/Shift press pairs.
                        self.repeat = None;
                        if current.active && !current.sensitive && self.preedit_present {
                            self.send_text("", 0, None)?;
                        }
                    }
                    if current.active && !current.sensitive {
                        self.ensure_grab()?;
                    } else {
                        if current.active && self.preedit_present {
                            self.send_text("", 0, None)?;
                        }
                        self.release_grab();
                    }
                    self.connection.flush()?;
                    return Ok(Some(Event::State(current)));
                }
                RawEvent::Keymap(file, bytes) => {
                    if self.grab.is_null() {
                        continue;
                    }
                    let mut keymap = xkb::Keymap::new(&bytes)?;
                    self.release_forwarded();
                    keymap.modifiers(self.raw_modifiers);
                    // SAFETY: libwayland duplicates the fd while marshalling it;
                    // the owned File can close after the request is queued.
                    unsafe {
                        (self.connection.api.marshal)(
                            self.keyboard,
                            0,
                            ptr::null(),
                            1,
                            0,
                            1u32,
                            file.as_raw_fd(),
                            bytes.len() as u32,
                        );
                    }
                    self.keymap = Some(keymap);
                    self.send_modifiers(self.raw_modifiers);
                    self.connection.flush()?;
                }
                RawEvent::Modifiers(mask) => {
                    self.raw_modifiers = mask;
                    if let Some(keymap) = self.keymap.as_mut() {
                        keymap.modifiers(mask);
                        self.send_modifiers(mask);
                        self.connection.flush()?;
                    }
                }
                RawEvent::Repeat { rate, delay } => {
                    self.repeat_rate = rate;
                    self.repeat_delay = delay;
                    if rate == 0 {
                        self.repeat = None;
                    }
                }
                RawEvent::Key {
                    time,
                    code,
                    pressed,
                } => {
                    if !self.state.current.active
                        || self.state.current.sensitive
                        || self.grab.is_null()
                    {
                        continue;
                    }
                    let index = key_index(code)?;
                    self.last_time = time;
                    let repeated = pressed && self.held[index];
                    self.held[index] = pressed;
                    let (event, repeats) = self.logical_key(time, code, pressed, repeated)?;
                    if pressed && !repeated && repeats && self.repeat_rate > 0 {
                        let now = Instant::now();
                        self.repeat = Some(Repeat {
                            code,
                            time,
                            started: now,
                            next: now + Duration::from_millis(self.repeat_delay as u64),
                        });
                    } else if !pressed && self.repeat.as_ref().is_some_and(|r| r.code == code) {
                        self.repeat = None;
                    }
                    return Ok(Some(Event::Key(event)));
                }
            }
        }
        // Apply queued context changes before release can display deferred pixels.
        if self.state.popup_ready()
            && let Some(popup) = self.popup.as_mut()
        {
            popup.flush_pending(self.state.current)?;
            self.connection.flush()?;
        }
        if let Some(repeat) = self.repeat.as_mut().filter(|r| r.next <= Instant::now()) {
            let code = repeat.code;
            let time = repeat
                .time
                .wrapping_add(repeat.started.elapsed().as_millis() as u32);
            repeat.next = Instant::now()
                + Duration::from_nanos(1_000_000_000 / self.repeat_rate.max(1) as u64);
            let (event, _) = self.logical_key(time, code, true, true)?;
            return Ok(Some(Event::Key(event)));
        }
        Ok(None)
    }
    fn logical_key(
        &self,
        time: u32,
        code: u32,
        pressed: bool,
        repeat: bool,
    ) -> Result<(KeyEvent, bool)> {
        let keymap = self
            .keymap
            .as_ref()
            .ok_or_else(|| Error::new("Wayland key arrived before a valid keymap"))?;
        let (keysym, text, modifiers, repeats) = keymap.logical(code)?;
        Ok((
            KeyEvent {
                generation: self.state.current.generation,
                serial: self.state.current.serial,
                keycode: code,
                time,
                pressed,
                repeat,
                keysym,
                text,
                modifiers,
            },
            repeats,
        ))
    }
    fn ensure_grab(&mut self) -> Result<()> {
        if !self.grab.is_null() {
            return Ok(());
        }
        // SAFETY: input_method.grab_keyboard creates the declared grab interface.
        self.grab = unsafe {
            (self.connection.api.marshal)(
                self.input,
                5,
                &protocol::GRAB,
                1,
                0,
                ptr::null_mut::<c_void>(),
            )
        };
        if self.grab.is_null() {
            return Err(Error::new("cannot grab the active input-method keyboard"));
        }
        self.connection
            .listen(self.grab, (&GRAB_LISTENER as *const GrabListener).cast())?;
        Ok(())
    }
    fn release_grab(&mut self) {
        if !self.grab.is_null() {
            // SAFETY: release is the grab's destructor (WL_MARSHAL_FLAG_DESTROY).
            unsafe {
                (self.connection.api.marshal)(self.grab, 0, ptr::null(), 1, 1);
            }
            self.grab = ptr::null_mut();
        }
        self.keymap = None;
        self.raw_modifiers = [0; 4];
        self.repeat = None;
    }
    fn send_modifiers(&self, mask: [u32; 4]) {
        if self.keyboard.is_null() || self.keymap.is_none() {
            return;
        }
        // SAFETY: virtual keyboard uses the same keymap, so masks keep meaning.
        unsafe {
            (self.connection.api.marshal)(
                self.keyboard,
                2,
                ptr::null(),
                1,
                0,
                mask[0],
                mask[1],
                mask[2],
                mask[3],
            );
        }
    }
    /// Forward an unhandled press. Call this for every release: releases for
    /// consumed keys are suppressed, and each forwarded press is released once.
    pub fn forward_key(&mut self, event: &KeyEvent) -> Result<()> {
        let context = self.state.current;
        if event.generation != context.generation || !context.active || context.sensitive {
            return Err(Error::new(
                "key belongs to an inactive or replaced input context",
            ));
        }
        let index = key_index(event.keycode)?;
        if event.repeat {
            return Ok(());
        }
        if event.pressed {
            if self.forwarded[index] {
                return Ok(());
            }
            if self.keymap.is_none() {
                return Err(Error::new(
                    "cannot forward a key before the compositor keymap",
                ));
            }
        } else if !self.forwarded[index] {
            return Ok(());
        }
        self.send_modifiers(self.raw_modifiers);
        // SAFETY: key code/time came from the grab, and the keymap is installed.
        unsafe {
            (self.connection.api.marshal)(
                self.keyboard,
                1,
                ptr::null(),
                1,
                0,
                event.time,
                event.keycode,
                u32::from(event.pressed),
            );
        }
        self.forwarded[index] = event.pressed;
        self.last_time = event.time;
        self.connection.flush()?;
        Ok(())
    }
    fn release_forwarded(&mut self) {
        if self.keyboard.is_null() {
            return;
        }
        for (code, held) in self.forwarded.iter_mut().enumerate() {
            if *held {
                // SAFETY: release pairs with a previously queued virtual press.
                unsafe {
                    (self.connection.api.marshal)(
                        self.keyboard,
                        1,
                        ptr::null(),
                        1,
                        0,
                        self.last_time,
                        code as u32,
                        0u32,
                    );
                }
                *held = false;
            }
        }
        self.send_modifiers([0; 4]);
    }
    /// Whether optional popup globals and ARGB8888 were negotiated. This
    /// does not create a surface, take focus, or change the core IME capability.
    pub fn popup_supported(&self) -> bool {
        !self.connection.compositor.is_null()
            && !self.connection.shm.is_null()
            && self.connection.state.compositor.is_some()
            && self.connection.state.shm.is_some()
            && self.connection.state.shm_argb8888
    }
    /// Submit a borrowed image. At most two buffers are in flight, plus the
    /// latest pending frame. Positioning belongs to the compositor, and the
    /// popup does not receive pointer/touch input or take keyboard focus.
    pub fn present_popup(
        &mut self,
        context: &Context,
        frame: PopupFrame<'_>,
    ) -> Result<PopupPresentation> {
        if *context != self.state.current || !context.active || context.sensitive {
            return Err(Error::new(
                "popup belongs to an inactive, sensitive or replaced input context",
            ));
        }
        popup::validate_frame(&frame)?;
        if !self.popup_supported() {
            return Ok(PopupPresentation::Unavailable);
        }
        if self.popup.is_none() {
            self.popup = Some(popup::Popup::new(&mut self.connection, self.input)?);
        }
        let status = self
            .popup
            .as_mut()
            .expect("popup was initialized")
            .present(*context, frame, self.state.popup_ready())?;
        self.connection.flush()?;
        Ok(status)
    }
    /// Clear visible and pending candidates even if an earlier context expired.
    pub fn hide_popup(&mut self) -> Result<()> {
        if let Some(popup) = self.popup.as_mut() {
            popup.hide();
        }
        self.connection.flush().map(|_| ())
    }
    /// Compositor hint in popup-local coordinates; not a configure handshake.
    pub fn popup_rectangle(&self) -> Option<PopupRectangle> {
        self.popup.as_ref().and_then(|popup| popup.rectangle)
    }

    /// Atomically replace preedit and optionally insert a complete commit. The
    /// context must still match; an empty preedit clears the application's mark.
    /// Cursor is a UTF-8 byte offset or -1 to hide it. Text is limited to 4000
    /// bytes by input-method-v2, and errors reject the whole update before I/O.
    pub fn update(
        &mut self,
        context: &Context,
        preedit: &str,
        cursor: i32,
        commit: Option<&str>,
    ) -> Result<()> {
        if *context != self.state.current || !context.active || context.sensitive {
            return Err(Error::new(
                "composition belongs to an inactive, sensitive or replaced input context",
            ));
        }
        self.send_text(preedit, cursor, commit)?;
        if preedit.is_empty()
            && let Some(popup) = self.popup.as_mut()
        {
            popup.hide();
        }
        self.connection.flush()?;
        Ok(())
    }
    fn send_text(&mut self, preedit: &str, cursor: i32, commit: Option<&str>) -> Result<()> {
        validate_text(preedit, cursor)?;
        if let Some(text) = commit {
            validate_text(text, text.len() as i32)?;
        }
        let preedit = CString::new(preedit).map_err(|_| Error::new("preedit contains a NUL"))?;
        let commit = commit
            .map(CString::new)
            .transpose()
            .map_err(|_| Error::new("commit contains a NUL"))?;
        // SAFETY: all strings and indices are validated before any request, and
        // libwayland copies their bytes during marshalling. Serial is current.
        unsafe {
            if let Some(text) = commit.as_ref() {
                (self.connection.api.marshal)(self.input, 0, ptr::null(), 1, 0, text.as_ptr());
            }
            (self.connection.api.marshal)(
                self.input,
                1,
                ptr::null(),
                1,
                0,
                preedit.as_ptr(),
                cursor,
                cursor,
            );
            (self.connection.api.marshal)(
                self.input,
                3,
                ptr::null(),
                1,
                0,
                self.state.current.serial,
            );
        }
        self.preedit_present = !preedit.is_empty();
        Ok(())
    }
}
impl Drop for InputMethod {
    fn drop(&mut self) {
        drop(self.popup.take());
        self.release_forwarded();
        if !self.input.is_null()
            && self.state.current.active
            && !self.state.current.sensitive
            && self.preedit_present
            && !self.connection.state.unavailable
        {
            let _ = self.send_text("", 0, None);
        }
        self.release_grab();
        // SAFETY: send destructors while the display and library remain live.
        unsafe {
            if !self.keyboard.is_null() {
                (self.connection.api.marshal)(self.keyboard, 3, ptr::null(), 1, 1);
            }
            if !self.input.is_null() {
                (self.connection.api.marshal)(self.input, 6, ptr::null(), 1, 1);
            }
        }
        let _ = self.connection.flush();
    }
}

fn key_index(code: u32) -> Result<usize> {
    if code < MAX_KEYS as u32 {
        Ok(code as usize)
    } else {
        Err(Error::new("Wayland keycode exceeds Linux KEY_MAX"))
    }
}
fn validate_text(text: &str, cursor: i32) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES || text.as_bytes().contains(&0) {
        return Err(Error::new(
            "Wayland text exceeds 4000 bytes or contains a NUL",
        ));
    }
    if cursor < -1 || (cursor >= 0 && !text.is_char_boundary(cursor as usize)) {
        return Err(Error::new("preedit cursor is outside a UTF-8 boundary"));
    }
    Ok(())
}

// Listener tables use the public C callback layouts. Libwayland owns callback
// invocation, and all callbacks borrow the stable State only for their call.
#[repr(C)]
struct RegistryListener {
    global: unsafe extern "C" fn(Proxy, Proxy, u32, *const c_char, u32),
    remove: unsafe extern "C" fn(Proxy, Proxy, u32),
}
static REGISTRY_LISTENER: RegistryListener = RegistryListener {
    global,
    remove: global_remove,
};
unsafe extern "C" fn global(
    data: Proxy,
    _: Proxy,
    name: u32,
    interface: *const c_char,
    version: u32,
) {
    // SAFETY: callback contract supplies State and a terminated interface name.
    let state = unsafe { &mut *data.cast::<State>() };
    let interface = unsafe { CStr::from_ptr(interface) };
    match interface.to_bytes() {
        b"wl_seat" if state.seat.is_none() => state.seat = Some((name, version)),
        b"wl_compositor" if state.compositor.is_none() => state.compositor = Some((name, version)),
        b"wl_shm" if state.shm.is_none() => state.shm = Some(name),
        b"zwp_input_method_manager_v2" => state.im_manager = Some(name),
        b"zwp_virtual_keyboard_manager_v1" => state.vk_manager = Some(name),
        _ => {}
    }
}
unsafe extern "C" fn global_remove(data: Proxy, _: Proxy, name: u32) {
    // SAFETY: callback data points to the owned stable State.
    let state = unsafe { &mut *data.cast::<State>() };
    if state.seat.map(|s| s.0) == Some(name)
        || state.im_manager == Some(name)
        || state.vk_manager == Some(name)
    {
        state.failure = Some(Error::new("Wayland input capability was removed"));
    }
    if state.compositor.is_some_and(|global| global.0 == name) || state.shm == Some(name) {
        if state.compositor.is_some_and(|global| global.0 == name) {
            state.compositor = None;
        }
        if state.shm == Some(name) {
            state.shm = None;
        }
        state.push(RawEvent::PopupUnavailable);
    }
}
#[repr(C)]
struct ShmListener {
    format: unsafe extern "C" fn(Proxy, Proxy, u32),
}
static SHM_LISTENER: ShmListener = ShmListener { format: shm_format };
unsafe extern "C" fn shm_format(data: Proxy, _: Proxy, format: u32) {
    if format == 0 {
        unsafe {
            (*data.cast::<State>()).shm_argb8888 = true;
        }
    }
}
#[repr(C)]
struct CallbackListener {
    done: unsafe extern "C" fn(Proxy, Proxy, u32),
}
static CALLBACK_LISTENER: CallbackListener = CallbackListener { done: sync_done };
unsafe extern "C" fn sync_done(data: Proxy, _: Proxy, _: u32) {
    unsafe {
        (*data.cast::<State>()).synced = true;
    }
}
#[repr(C)]
struct SeatListener {
    capabilities: unsafe extern "C" fn(Proxy, Proxy, u32),
    name: unsafe extern "C" fn(Proxy, Proxy, *const c_char),
}
static SEAT_LISTENER: SeatListener = SeatListener {
    capabilities: seat_capabilities,
    name: seat_name,
};
unsafe extern "C" fn seat_capabilities(_: Proxy, _: Proxy, _: u32) {}
unsafe extern "C" fn seat_name(_: Proxy, _: Proxy, _: *const c_char) {}
#[repr(C)]
struct ImListener {
    activate: unsafe extern "C" fn(Proxy, Proxy),
    deactivate: unsafe extern "C" fn(Proxy, Proxy),
    surrounding: unsafe extern "C" fn(Proxy, Proxy, *const c_char, u32, u32),
    cause: unsafe extern "C" fn(Proxy, Proxy, u32),
    content: unsafe extern "C" fn(Proxy, Proxy, u32, u32),
    done: unsafe extern "C" fn(Proxy, Proxy),
    unavailable: unsafe extern "C" fn(Proxy, Proxy),
}
static IM_LISTENER: ImListener = ImListener {
    activate,
    deactivate,
    surrounding,
    cause,
    content,
    done,
    unavailable,
};
unsafe extern "C" fn activate(data: Proxy, _: Proxy) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::Activate);
    }
}
unsafe extern "C" fn deactivate(data: Proxy, _: Proxy) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::Deactivate);
    }
}
// Surrounding application text is intentionally neither copied nor retained.
unsafe extern "C" fn surrounding(_: Proxy, _: Proxy, _: *const c_char, _: u32, _: u32) {}
unsafe extern "C" fn cause(data: Proxy, _: Proxy, value: u32) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::ChangeCause(value));
    }
}
unsafe extern "C" fn content(data: Proxy, _: Proxy, hints: u32, purpose: u32) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::Content(hints, purpose));
    }
}
unsafe extern "C" fn done(data: Proxy, _: Proxy) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::Done);
    }
}
unsafe extern "C" fn unavailable(data: Proxy, _: Proxy) {
    unsafe {
        (*data.cast::<State>()).unavailable = true;
    }
}
#[repr(C)]
struct GrabListener {
    keymap: unsafe extern "C" fn(Proxy, Proxy, u32, c_int, u32),
    key: unsafe extern "C" fn(Proxy, Proxy, u32, u32, u32, u32),
    modifiers: unsafe extern "C" fn(Proxy, Proxy, u32, u32, u32, u32, u32),
    repeat: unsafe extern "C" fn(Proxy, Proxy, i32, i32),
}
static GRAB_LISTENER: GrabListener = GrabListener {
    keymap,
    key,
    modifiers,
    repeat: repeat_info,
};
unsafe extern "C" fn keymap(data: Proxy, _: Proxy, format: u32, fd: c_int, size: u32) {
    let state = unsafe { &mut *data.cast::<State>() };
    if fd < 0 {
        state.failure = Some(Error::new("Wayland keymap has an invalid descriptor"));
        return;
    }
    // SAFETY: the keymap event transfers ownership of this descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    if format != 1 || size == 0 || size as usize > MAX_KEYMAP {
        state.failure = Some(Error::new("unsupported or oversized compositor keymap"));
        return;
    }
    let mut bytes = vec![0u8; size as usize];
    if file.read_exact_at(&mut bytes, 0).is_err() {
        state.failure = Some(Error::new("cannot read the compositor keymap"));
        return;
    }
    state.push(RawEvent::Keymap(file, bytes));
}
unsafe extern "C" fn key(data: Proxy, _: Proxy, _: u32, time: u32, code: u32, pressed: u32) {
    let state = unsafe { &mut *data.cast::<State>() };
    if pressed > 1 {
        state.failure = Some(Error::new("invalid Wayland key state"));
        return;
    }
    state.push(RawEvent::Key {
        time,
        code,
        pressed: pressed == 1,
    });
}
unsafe extern "C" fn modifiers(
    data: Proxy,
    _: Proxy,
    _: u32,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::Modifiers([depressed, latched, locked, group]));
    }
}
unsafe extern "C" fn repeat_info(data: Proxy, _: Proxy, rate: i32, delay: i32) {
    let state = unsafe { &mut *data.cast::<State>() };
    if rate < 0 || delay < 0 || rate > 1000 {
        state.failure = Some(Error::new("invalid compositor repeat settings"));
        return;
    }
    state.push(RawEvent::Repeat { rate, delay });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn activation_and_password_policy_apply_only_at_done() {
        let mut state = ProtocolState::default();
        state.activate().unwrap();
        state.content(0x80, 8);
        assert_eq!(state.current, Context::default());
        let password = state.done().unwrap();
        assert!(password.active && password.sensitive);
        assert_eq!((password.generation, password.serial), (1, 1));
        state.deactivate().unwrap();
        state.activate().unwrap();
        assert_eq!(state.current, password);
        let normal = state.done().unwrap();
        assert!(normal.active && !normal.sensitive);
        assert_eq!((normal.generation, normal.serial), (3, 2));
    }
    #[test]
    fn each_secure_purpose_and_hint_bypasses_composition() {
        for (hints, purpose) in [(0x40, 0), (0x80, 0), (0, 8), (0, 9)] {
            let mut state = ProtocolState::default();
            state.activate().unwrap();
            state.content(hints, purpose);
            assert!(state.done().unwrap().sensitive);
        }
    }
    #[test]
    fn serial_is_protocol_u32_while_context_does_not_wrap() {
        let mut state = ProtocolState::default();
        state.current.serial = u32::MAX;
        assert_eq!(state.done().unwrap().serial, 0);
        state.pending.generation = u64::MAX;
        assert!(state.activate().is_err());
    }
    #[test]
    fn complete_utf8_updates_are_checked_before_marshalling() {
        assert!(validate_text("你好", 6).is_ok());
        assert!(validate_text("你好", -1).is_ok());
        assert!(validate_text("你好", 1).is_err());
        assert!(validate_text("你好", 7).is_err());
        assert!(validate_text("你好", -2).is_err());
        assert!(validate_text("a\0b", 3).is_err());
        assert!(validate_text(&"a".repeat(4000), 4000).is_ok());
        assert!(validate_text(&"a".repeat(4001), 4001).is_err());
    }
    #[test]
    fn external_edit_invalidates_once_without_a_focus_transition() {
        let mut state = ProtocolState::default();
        state.activate().unwrap();
        let initial = state.done().unwrap();
        state.pending_other = true;
        assert!(state.external_change());
        assert!(!state.focus_transition);
        let changed = state.done().unwrap();
        assert_eq!(changed.generation, initial.generation + 1);
        assert!(!state.external_change());
        assert_eq!(state.done().unwrap().generation, changed.generation);
    }

    #[test]
    fn initial_other_and_normal_echo_do_not_reset_twice() {
        let mut state = ProtocolState::default();
        state.activate().unwrap();
        state.pending_other = true;
        assert!(!state.external_change());
        let active = state.done().unwrap();
        assert_eq!(active.generation, 1);
        assert_eq!(state.done().unwrap().generation, 1);
        state.deactivate().unwrap();
        state.pending_other = true;
        assert!(!state.external_change());
        assert!(!state.done().unwrap().active);
    }

    #[test]
    fn popup_waits_for_partial_context_batches_to_complete() {
        let mut state = ProtocolState::default();
        state.activate().unwrap();
        assert!(!state.popup_ready());
        state.done().unwrap();
        assert!(state.popup_ready());

        let old = state.current;
        state.pending_other = true;
        assert!(!state.popup_ready());
        assert_eq!(state.current, old);
        state.done().unwrap();
        assert!(state.popup_ready());
        assert_ne!(state.current.generation, old.generation);

        state.content(0, 8);
        assert!(!state.popup_ready());
        state.done().unwrap();
        assert!(!state.popup_ready());
        state.content(0, 0);
        assert!(!state.popup_ready());
        state.done().unwrap();
        assert!(state.popup_ready());

        state.deactivate().unwrap();
        assert!(!state.popup_ready());
        state.done().unwrap();
        assert!(!state.popup_ready());
        state.activate().unwrap();
        assert!(!state.popup_ready());
        state.done().unwrap();
        assert!(state.popup_ready());
    }

    #[test]
    fn external_generation_exhaustion_rejects_before_applying_state() {
        let mut state = ProtocolState::default();
        state.activate().unwrap();
        state.done().unwrap();
        state.pending.generation = u64::MAX;
        state.pending_other = true;
        let current = state.current;
        assert!(state.done().is_err());
        assert_eq!(state.current, current);
    }
}
