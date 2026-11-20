use crate::loader::{Library, symbol};
use crate::{Error, Result};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr;

type Handle = *mut c_void;
struct Api {
    context_new: unsafe extern "C" fn(c_int) -> Handle,
    context_unref: unsafe extern "C" fn(Handle),
    keymap_new: unsafe extern "C" fn(Handle, *const c_char, c_int, c_int) -> Handle,
    keymap_unref: unsafe extern "C" fn(Handle),
    state_new: unsafe extern "C" fn(Handle) -> Handle,
    state_unref: unsafe extern "C" fn(Handle),
    update_mask: unsafe extern "C" fn(Handle, u32, u32, u32, u32, u32, u32) -> u32,
    utf8: unsafe extern "C" fn(Handle, u32, *mut c_char, usize) -> c_int,
    sym: unsafe extern "C" fn(Handle, u32) -> u32,
    mod_active: unsafe extern "C" fn(Handle, *const c_char, u32) -> c_int,
    repeats: unsafe extern "C" fn(Handle, u32) -> c_int,
}

pub(crate) struct Keymap {
    api: Api,
    context: Handle,
    map: Handle,
    state: Handle,
    _library: Library,
}
impl Keymap {
    pub(crate) fn new(bytes: &[u8]) -> Result<Self> {
        let source = CStr::from_bytes_with_nul(bytes)
            .map_err(|_| Error::new("invalid XKB keymap string"))?;
        let lib = Library::open(c"libxkbcommon.so.0")?;
        let api = Api {
            context_new: symbol!(
                lib,
                c"xkb_context_new",
                unsafe extern "C" fn(c_int) -> Handle
            ),
            context_unref: symbol!(lib, c"xkb_context_unref", unsafe extern "C" fn(Handle)),
            keymap_new: symbol!(
                lib,
                c"xkb_keymap_new_from_string",
                unsafe extern "C" fn(Handle, *const c_char, c_int, c_int) -> Handle
            ),
            keymap_unref: symbol!(lib, c"xkb_keymap_unref", unsafe extern "C" fn(Handle)),
            state_new: symbol!(
                lib,
                c"xkb_state_new",
                unsafe extern "C" fn(Handle) -> Handle
            ),
            state_unref: symbol!(lib, c"xkb_state_unref", unsafe extern "C" fn(Handle)),
            update_mask: symbol!(
                lib,
                c"xkb_state_update_mask",
                unsafe extern "C" fn(Handle, u32, u32, u32, u32, u32, u32) -> u32
            ),
            utf8: symbol!(
                lib,
                c"xkb_state_key_get_utf8",
                unsafe extern "C" fn(Handle, u32, *mut c_char, usize) -> c_int
            ),
            sym: symbol!(
                lib,
                c"xkb_state_key_get_one_sym",
                unsafe extern "C" fn(Handle, u32) -> u32
            ),
            mod_active: symbol!(
                lib,
                c"xkb_state_mod_name_is_active",
                unsafe extern "C" fn(Handle, *const c_char, u32) -> c_int
            ),
            repeats: symbol!(
                lib,
                c"xkb_keymap_key_repeats",
                unsafe extern "C" fn(Handle, u32) -> c_int
            ),
        };
        let mut this = Self {
            api,
            context: ptr::null_mut(),
            map: ptr::null_mut(),
            state: ptr::null_mut(),
            _library: lib,
        };
        // SAFETY: ABI types above match libxkbcommon, and source is NUL terminated.
        unsafe {
            this.context = (this.api.context_new)(0);
            if this.context.is_null() {
                return Err(Error::new("XKB context creation failed"));
            }
            this.map = (this.api.keymap_new)(this.context, source.as_ptr(), 1, 0);
            if this.map.is_null() {
                return Err(Error::new("compositor XKB keymap is invalid"));
            }
            this.state = (this.api.state_new)(this.map);
            if this.state.is_null() {
                return Err(Error::new("XKB state creation failed"));
            }
        }
        Ok(this)
    }
    pub(crate) fn modifiers(&mut self, mask: [u32; 4]) {
        // SAFETY: state is live, and modifier masks originate from the compositor.
        unsafe {
            (self.api.update_mask)(self.state, mask[0], mask[1], mask[2], 0, 0, mask[3]);
        }
    }
    pub(crate) fn logical(&self, code: u32) -> Result<(u32, String, u32, bool)> {
        let code = code
            .checked_add(8)
            .ok_or_else(|| Error::new("invalid Wayland keycode"))?;
        let mut text = [0u8; 128];
        // SAFETY: xkb state is live, and output length describes the buffer.
        let count =
            unsafe { (self.api.utf8)(self.state, code, text.as_mut_ptr().cast(), text.len()) };
        if count < 0 || count as usize >= text.len() {
            return Err(Error::new("XKB key text exceeds the input bound"));
        }
        let text = std::str::from_utf8(&text[..count as usize])
            .map_err(|_| Error::new("XKB returned invalid UTF-8"))?
            .to_owned();
        let mut modifiers = 0;
        // Values match zinput::Modifiers; XKB_STATE_MODS_EFFECTIVE is 1 << 3.
        for (name, bit) in [
            (c"Shift", 1),
            (c"Mod1", 2),
            (c"Control", 4),
            (c"Mod4", 8),
            (c"Lock", 64),
            (c"Mod2", 128),
        ] {
            // SAFETY: constant names are NUL terminated; state is owned here.
            if unsafe { (self.api.mod_active)(self.state, name.as_ptr(), 8) } > 0 {
                modifiers |= bit;
            }
        }
        // SAFETY: both calls read the owned state or keymap.
        Ok(unsafe {
            (
                (self.api.sym)(self.state, code),
                text,
                modifiers,
                (self.api.repeats)(self.map, code) != 0,
            )
        })
    }
}
impl Drop for Keymap {
    fn drop(&mut self) {
        // SAFETY: partial construction uses null; all live handles belong to this API.
        unsafe {
            if !self.state.is_null() {
                (self.api.state_unref)(self.state);
            }
            if !self.map.is_null() {
                (self.api.keymap_unref)(self.map);
            }
            if !self.context.is_null() {
                (self.api.context_unref)(self.context);
            }
        }
    }
}
