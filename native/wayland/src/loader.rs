use crate::{Error, Result};
use std::ffi::{CStr, c_void};

/// A scoped owner of the shared Project Z loader, closed after native objects.
pub(crate) struct Library(zsys::dylib::Library);
impl Library {
    pub(crate) fn open(name: &CStr) -> Result<Self> {
        // SAFETY: callers request only the installed Wayland and XKB libraries.
        unsafe { zsys::dylib::Library::open(name) }
            .map(Self)
            .map_err(|e| Error::new(format!("cannot load {}: {e}", name.to_string_lossy())))
    }
    pub(crate) fn address(&self, name: &CStr) -> Result<*mut c_void> {
        self.0
            .symbol(name)
            .map_err(|e| Error::new(format!("missing {}: {e}", name.to_string_lossy())))
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: owner fields drop native handles before their library owner.
        let _ = unsafe { self.0.close() };
    }
}

macro_rules! symbol {
    ($lib:expr, $name:expr, $ty:ty) => {{
        // SAFETY: each invocation provides the public native C ABI signature.
        unsafe { std::mem::transmute::<*mut std::ffi::c_void, $ty>($lib.address($name)?) }
    }};
}
pub(crate) use symbol;
