use crate::{Api, Connection, Context, Error, Proxy, RawEvent, Result, State, protocol};
use std::{
    ffi::{c_char, c_int, c_void},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    ptr,
};

/// Maximum number of bytes in one tightly packed popup image.
pub const MAX_POPUP_BYTES: usize = 8 * 1024 * 1024;
/// Maximum physical width or height, independently of buffer scale.
pub const MAX_POPUP_DIMENSION: u32 = 2048;

/// Borrowed, tightly packed premultiplied ARGB8888 image. Width and height are
/// physical pixels. Bytes are B, G, R, A (the protocol's little-endian format).
/// Each dimension must be divisible by the positive integer buffer scale.
pub struct PopupFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
    pub pixels: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopupPresentation {
    Presented,
    Queued,
    Unavailable,
}

/// Text input bounds relative to the popup, as a compositor-provided hint.
/// These are not desktop coordinates and are not a configure handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PopupRectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub(crate) fn validate_frame(frame: &PopupFrame<'_>) -> Result<()> {
    let length = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|n| n.checked_mul(4));
    if frame.width == 0
        || frame.height == 0
        || frame.width > MAX_POPUP_DIMENSION
        || frame.height > MAX_POPUP_DIMENSION
        || frame.scale == 0
        || frame.scale > MAX_POPUP_DIMENSION
        || !frame.width.is_multiple_of(frame.scale)
        || !frame.height.is_multiple_of(frame.scale)
        || length.is_none_or(|n| n > MAX_POPUP_BYTES || n != frame.pixels.len())
    {
        return Err(Error::new("invalid or oversized popup image"));
    }
    // In premultiplied ARGB, each color channel must be at most alpha.
    if frame
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[..3].iter().any(|v| *v > p[3]))
    {
        return Err(Error::new("popup image is not premultiplied ARGB8888"));
    }
    Ok(())
}

struct OwnedFrame {
    generation: u64,
    revision: u64,
    width: u32,
    height: u32,
    scale: u32,
    pixels: Vec<u8>,
}
impl OwnedFrame {
    fn borrowed(&self) -> PopupFrame<'_> {
        PopupFrame {
            width: self.width,
            height: self.height,
            scale: self.scale,
            pixels: &self.pixels,
        }
    }
}
impl Drop for OwnedFrame {
    fn drop(&mut self) {
        self.pixels.fill(0);
    }
}

#[derive(Default)]
struct Schedule {
    generation: Option<u64>,
    revision: u64,
    pending: Option<OwnedFrame>,
}
impl Schedule {
    fn request(&mut self, generation: u64) -> Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::new("popup render revision exhausted"))?;
        self.pending = None;
        self.generation = Some(generation);
        Ok(())
    }
    fn defer(&mut self, frame: &PopupFrame<'_>) -> Result<()> {
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(frame.pixels.len())
            .map_err(|_| Error::new("cannot allocate pending popup image"))?;
        pixels.extend_from_slice(frame.pixels);
        self.pending = Some(OwnedFrame {
            generation: self.generation.expect("a popup request precedes deferral"),
            revision: self.revision,
            width: frame.width,
            height: frame.height,
            scale: frame.scale,
            pixels,
        });
        Ok(())
    }
    fn cancel(&mut self) {
        self.generation = None;
        self.pending = None;
    }
    fn take(&mut self, context: Context) -> Option<OwnedFrame> {
        self.pending.take().filter(|frame| {
            context.active
                && !context.sensitive
                && self.generation == Some(context.generation)
                && frame.generation == context.generation
                && frame.revision == self.revision
        })
    }
}

unsafe extern "C" {
    fn memfd_create(name: *const c_char, flags: u32) -> c_int;
}

struct BufferTag {
    state: *mut State,
    token: u64,
}
struct Slot {
    api: Api,
    buffer: Proxy,
    tag: Box<BufferTag>,
    file: File,
    width: u32,
    height: u32,
    busy: bool,
}
impl Slot {
    fn new(
        api: Api,
        shm: Proxy,
        state: *mut State,
        token: u64,
        frame: &PopupFrame<'_>,
    ) -> Result<Self> {
        // SAFETY: a fixed NUL-terminated name and MFD_CLOEXEC; ownership of a
        // successful descriptor is immediately transferred to File.
        let fd = unsafe { memfd_create(c"zinput-popup".as_ptr(), 1) };
        if fd < 0 {
            return Err(Error::new("cannot create popup shared memory"));
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(frame.pixels.len() as u64)
            .map_err(|_| Error::new("cannot size popup shared memory"))?;
        // SAFETY: dimensions and byte length were validated before allocation.
        // Libwayland duplicates this owned fd while marshalling create_pool.
        let pool = unsafe {
            (api.marshal)(
                shm,
                0,
                api.shm_pool,
                1,
                0,
                ptr::null_mut::<c_void>(),
                file.as_raw_fd(),
                frame.pixels.len() as i32,
            )
        };
        if pool.is_null() {
            return Err(Error::new("cannot create popup memory pool"));
        }
        let buffer = unsafe {
            (api.marshal)(
                pool,
                0,
                api.buffer,
                1,
                0,
                ptr::null_mut::<c_void>(),
                0i32,
                frame.width as i32,
                frame.height as i32,
                (frame.width * 4) as i32,
                0u32,
            )
        };
        // A created wl_buffer retains the server-side pool mapping.
        unsafe {
            (api.marshal)(pool, 1, ptr::null(), 1, 1);
        }
        if buffer.is_null() {
            return Err(Error::new("cannot create popup buffer"));
        }
        let mut this = Self {
            api,
            buffer,
            tag: Box::new(BufferTag { state, token }),
            file,
            width: frame.width,
            height: frame.height,
            busy: false,
        };
        // SAFETY: tag remains stable and alive until its proxy is destroyed.
        if unsafe {
            (api.add_listener)(
                buffer,
                (&BUFFER_LISTENER as *const BufferListener).cast(),
                (&mut *this.tag as *mut BufferTag).cast(),
            )
        } != 0
        {
            return Err(Error::new("cannot install popup buffer listener"));
        }
        Ok(this)
    }
    fn write(&self, frame: &PopupFrame<'_>) -> Result<()> {
        if self.busy {
            return Err(Error::new("popup buffer is still in use"));
        }
        self.file
            .write_all_at(frame.pixels, 0)
            .map_err(|_| Error::new("cannot write popup shared memory"))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        // Destroying a busy buffer is valid if its backing storage is not
        // changed/reused. Closing our fd leaves the compositor's reference live.
        unsafe {
            (self.api.marshal)(self.buffer, 0, ptr::null(), 1, 1);
        }
    }
}

pub(crate) struct Popup {
    api: Api,
    version: u32,
    shm: Proxy,
    state: *mut State,
    role: Proxy,
    surface: Proxy,
    slots: [Option<Slot>; 2],
    next_token: u64,
    schedule: Schedule,
    visible: bool,
    pub(crate) rectangle: Option<PopupRectangle>,
}
impl Popup {
    pub(crate) fn new(connection: &mut Connection, input: Proxy) -> Result<Self> {
        let version = connection.popup_version;
        let api = connection.api;
        let mut this = Self {
            api,
            version,
            shm: connection.shm,
            state: (&mut *connection.state) as *mut State,
            role: ptr::null_mut(),
            surface: ptr::null_mut(),
            slots: [None, None],
            next_token: 0,
            schedule: Schedule::default(),
            visible: false,
            rectangle: None,
        };
        // SAFETY: all objects belong to this connection; the fresh surface has
        // no previous role. Constructor new_id is represented by a null slot.
        this.surface = unsafe {
            (api.marshal)(
                connection.compositor,
                0,
                api.surface,
                version,
                0,
                ptr::null_mut::<c_void>(),
            )
        };
        if this.surface.is_null() {
            return Err(Error::new("cannot create popup surface"));
        }
        connection.listen(
            this.surface,
            (&SURFACE_LISTENER as *const SurfaceListener).cast(),
        )?;
        this.role = unsafe {
            (api.marshal)(
                input,
                4,
                &protocol::POPUP,
                1,
                0,
                ptr::null_mut::<c_void>(),
                this.surface,
            )
        };
        if this.role.is_null() {
            return Err(Error::new("cannot assign popup role"));
        }
        connection.listen(this.role, (&POPUP_LISTENER as *const PopupListener).cast())?;
        let region = unsafe {
            (api.marshal)(
                connection.compositor,
                1,
                api.region,
                version,
                0,
                ptr::null_mut::<c_void>(),
            )
        };
        if region.is_null() {
            return Err(Error::new("cannot create popup input region"));
        }
        // A newly created region is empty. NULL would instead mean infinite.
        unsafe {
            (api.marshal)(this.surface, 5, ptr::null(), version, 0, region);
            (api.marshal)(region, 0, ptr::null(), version, 1);
        }
        Ok(this)
    }
    pub(crate) fn present(
        &mut self,
        context: Context,
        frame: PopupFrame<'_>,
    ) -> Result<PopupPresentation> {
        if self.version < 3 && frame.scale != 1 {
            return Err(Error::new(
                "compositor does not support popup buffer scaling",
            ));
        }
        self.schedule.request(context.generation)?;
        if let Some(index) = self.free_slot() {
            self.draw(index, &frame)?;
            Ok(PopupPresentation::Presented)
        } else {
            self.schedule.defer(&frame)?;
            Ok(PopupPresentation::Queued)
        }
    }
    fn free_slot(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_none_or(|slot| !slot.busy))
    }
    fn draw(&mut self, index: usize, frame: &PopupFrame<'_>) -> Result<()> {
        if self.slots[index]
            .as_ref()
            .is_none_or(|slot| slot.width != frame.width || slot.height != frame.height)
        {
            let token = self
                .next_token
                .checked_add(1)
                .ok_or_else(|| Error::new("popup buffer identity exhausted"))?;
            // Retire only a free slot, before allocating its replacement.
            self.slots[index] = None;
            self.slots[index] = Some(Slot::new(self.api, self.shm, self.state, token, frame)?);
            self.next_token = token;
        }
        let slot = self.slots[index]
            .as_mut()
            .expect("a free popup slot was initialized");
        slot.write(frame)?;
        // SAFETY: the surface and released buffer are live and on this display.
        // There is no xdg configure handshake; attach the first image directly.
        unsafe {
            if self.version >= 3 {
                (self.api.marshal)(
                    self.surface,
                    8,
                    ptr::null(),
                    self.version,
                    0,
                    frame.scale as i32,
                );
            }
            (self.api.marshal)(
                self.surface,
                1,
                ptr::null(),
                self.version,
                0,
                slot.buffer,
                0i32,
                0i32,
            );
            let (opcode, width, height) = if self.version >= 4 {
                (9, frame.width, frame.height)
            } else {
                (2, frame.width / frame.scale, frame.height / frame.scale)
            };
            (self.api.marshal)(
                self.surface,
                opcode,
                ptr::null(),
                self.version,
                0,
                0i32,
                0i32,
                width as i32,
                height as i32,
            );
            (self.api.marshal)(self.surface, 6, ptr::null(), self.version, 0);
        }
        slot.busy = true;
        self.visible = true;
        Ok(())
    }
    pub(crate) fn released(&mut self, token: u64) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.tag.token == token)
        {
            slot.busy = false;
        }
    }
    pub(crate) fn flush_pending(&mut self, context: Context) -> Result<()> {
        if let Some(index) = self.free_slot()
            && let Some(frame) = self.schedule.take(context)
        {
            self.draw(index, &frame.borrowed())?;
        }
        Ok(())
    }
    pub(crate) fn hide(&mut self) {
        self.schedule.cancel();
        self.rectangle = None;
        if self.visible {
            // NULL attach followed by commit explicitly removes surface content.
            unsafe {
                (self.api.marshal)(
                    self.surface,
                    1,
                    ptr::null(),
                    self.version,
                    0,
                    ptr::null_mut::<c_void>(),
                    0i32,
                    0i32,
                );
                (self.api.marshal)(self.surface, 6, ptr::null(), self.version, 0);
            }
            self.visible = false;
        }
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        self.hide();
        // The popup role must be destroyed before its underlying wl_surface.
        unsafe {
            if !self.role.is_null() {
                (self.api.marshal)(self.role, 0, ptr::null(), 1, 1);
            }
            if !self.surface.is_null() {
                (self.api.marshal)(self.surface, 0, ptr::null(), self.version, 1);
            }
        }
        // Slot destructors run while InputMethod still owns the Wayland library.
    }
}

#[repr(C)]
struct BufferListener {
    release: unsafe extern "C" fn(Proxy, Proxy),
}
static BUFFER_LISTENER: BufferListener = BufferListener {
    release: buffer_release,
};
unsafe extern "C" fn buffer_release(data: Proxy, _: Proxy) {
    let tag = unsafe { &*data.cast::<BufferTag>() };
    unsafe {
        (*tag.state).push(RawEvent::PopupReleased(tag.token));
    }
}
#[repr(C)]
struct PopupListener {
    rectangle: unsafe extern "C" fn(Proxy, Proxy, i32, i32, i32, i32),
}
static POPUP_LISTENER: PopupListener = PopupListener {
    rectangle: rectangle_changed,
};
unsafe extern "C" fn rectangle_changed(
    data: Proxy,
    _: Proxy,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    unsafe {
        (*data.cast::<State>()).push(RawEvent::PopupRectangle(PopupRectangle {
            x,
            y,
            width,
            height,
        }));
    }
}
#[repr(C)]
struct SurfaceListener {
    enter: unsafe extern "C" fn(Proxy, Proxy, Proxy),
    leave: unsafe extern "C" fn(Proxy, Proxy, Proxy),
}
static SURFACE_LISTENER: SurfaceListener = SurfaceListener {
    enter: surface_output,
    leave: surface_output,
};
unsafe extern "C" fn surface_output(_: Proxy, _: Proxy, _: Proxy) {}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(generation: u64) -> Context {
        Context {
            generation,
            active: true,
            ..Context::default()
        }
    }
    fn frame(pixels: &[u8]) -> PopupFrame<'_> {
        PopupFrame {
            width: 1,
            height: 1,
            scale: 1,
            pixels,
        }
    }
    #[test]
    fn validates_complete_premultiplied_images_and_scale() {
        assert!(validate_frame(&frame(&[10, 20, 30, 255])).is_ok());
        assert!(validate_frame(&frame(&[1, 0, 0, 0])).is_err());
        assert!(validate_frame(&frame(&[0, 0, 0])).is_err());
        let mut image = frame(&[0, 0, 0, 255]);
        image.scale = 0;
        assert!(validate_frame(&image).is_err());
        image.scale = 2;
        assert!(validate_frame(&image).is_err());
        image.scale = 1;
        image.width = u32::MAX;
        assert!(validate_frame(&image).is_err());
        image.width = 2048;
        image.height = 2048;
        assert!(validate_frame(&image).is_err());
    }
    #[test]
    fn latest_queued_frame_replaces_previous_image() {
        let mut queue = Schedule::default();
        queue.request(3).unwrap();
        queue.defer(&frame(&[0, 0, 1, 255])).unwrap();
        queue.request(3).unwrap();
        queue.defer(&frame(&[0, 0, 2, 255])).unwrap();
        assert_eq!(queue.take(context(3)).unwrap().pixels, [0, 0, 2, 255]);
        assert!(queue.take(context(3)).is_none());
    }
    #[test]
    fn hiding_or_context_changes_cannot_resurrect_pending_candidates() {
        let mut queue = Schedule::default();
        queue.request(1).unwrap();
        queue.defer(&frame(&[0, 0, 0, 255])).unwrap();
        queue.cancel();
        assert!(queue.take(context(1)).is_none());
        queue.request(1).unwrap();
        queue.defer(&frame(&[0, 0, 0, 255])).unwrap();
        assert!(queue.take(context(2)).is_none());
        queue.request(2).unwrap();
        queue.defer(&frame(&[0, 0, 0, 255])).unwrap();
        let mut secure = context(2);
        secure.sensitive = true;
        assert!(queue.take(secure).is_none());
        queue.request(2).unwrap();
        queue.defer(&frame(&[0, 0, 0, 255])).unwrap();
        let mut inactive = context(2);
        inactive.active = false;
        assert!(queue.take(inactive).is_none());
    }
}
