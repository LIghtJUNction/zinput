//! C ABI descriptors for input-method-v2 and virtual-keyboard-v1.
//! Opcode order and signatures follow the wlroots protocol XML definitions.
use std::ffi::{c_char, c_int};
use std::ptr;

#[repr(C)]
pub(crate) struct Message {
    pub name: *const c_char,
    pub signature: *const c_char,
    pub types: *const *const Interface,
}
#[repr(C)]
pub(crate) struct Interface {
    pub name: *const c_char,
    pub version: c_int,
    pub method_count: c_int,
    pub methods: *const Message,
    pub event_count: c_int,
    pub events: *const Message,
}
// SAFETY: protocol tables are immutable for the entire process lifetime.
unsafe impl Sync for Message {}
unsafe impl Sync for Interface {}
struct Types<const N: usize>([*const Interface; N]);
// SAFETY: entries only refer to immutable protocol tables or null.
unsafe impl<const N: usize> Sync for Types<N> {}
static NONE: Types<5> = Types([ptr::null(); 5]);

const fn message(name: &'static std::ffi::CStr, sig: &'static std::ffi::CStr) -> Message {
    Message {
        name: name.as_ptr(),
        signature: sig.as_ptr(),
        types: NONE.0.as_ptr(),
    }
}
static POPUP_METHODS: [Message; 1] = [message(c"destroy", c"")];
static POPUP_EVENTS: [Message; 1] = [message(c"text_input_rectangle", c"iiii")];
pub(crate) static POPUP: Interface = Interface {
    name: c"zwp_input_popup_surface_v2".as_ptr(),
    version: 1,
    method_count: 1,
    methods: POPUP_METHODS.as_ptr(),
    event_count: 1,
    events: POPUP_EVENTS.as_ptr(),
};
static GRAB_METHODS: [Message; 1] = [message(c"release", c"")];
static GRAB_EVENTS: [Message; 4] = [
    message(c"keymap", c"uhu"),
    message(c"key", c"uuuu"),
    message(c"modifiers", c"uuuuu"),
    message(c"repeat_info", c"ii"),
];
pub(crate) static GRAB: Interface = Interface {
    name: c"zwp_input_method_keyboard_grab_v2".as_ptr(),
    version: 1,
    method_count: 1,
    methods: GRAB_METHODS.as_ptr(),
    event_count: 4,
    events: GRAB_EVENTS.as_ptr(),
};
static POPUP_TYPES: Types<2> = Types([&POPUP, ptr::null()]);
static GRAB_TYPES: Types<1> = Types([&GRAB]);
static IM_METHODS: [Message; 7] = [
    message(c"commit_string", c"s"),
    message(c"set_preedit_string", c"sii"),
    message(c"delete_surrounding_text", c"uu"),
    message(c"commit", c"u"),
    Message {
        name: c"get_input_popup_surface".as_ptr(),
        signature: c"no".as_ptr(),
        types: POPUP_TYPES.0.as_ptr(),
    },
    Message {
        name: c"grab_keyboard".as_ptr(),
        signature: c"n".as_ptr(),
        types: GRAB_TYPES.0.as_ptr(),
    },
    message(c"destroy", c""),
];
static IM_EVENTS: [Message; 7] = [
    message(c"activate", c""),
    message(c"deactivate", c""),
    message(c"surrounding_text", c"suu"),
    message(c"text_change_cause", c"u"),
    message(c"content_type", c"uu"),
    message(c"done", c""),
    message(c"unavailable", c""),
];
pub(crate) static INPUT_METHOD: Interface = Interface {
    name: c"zwp_input_method_v2".as_ptr(),
    version: 1,
    method_count: 7,
    methods: IM_METHODS.as_ptr(),
    event_count: 7,
    events: IM_EVENTS.as_ptr(),
};
static IM_CREATE_TYPES: Types<2> = Types([ptr::null(), &INPUT_METHOD]);
static IM_MANAGER_METHODS: [Message; 2] = [
    Message {
        name: c"get_input_method".as_ptr(),
        signature: c"on".as_ptr(),
        types: IM_CREATE_TYPES.0.as_ptr(),
    },
    message(c"destroy", c""),
];
pub(crate) static IM_MANAGER: Interface = Interface {
    name: c"zwp_input_method_manager_v2".as_ptr(),
    version: 1,
    method_count: 2,
    methods: IM_MANAGER_METHODS.as_ptr(),
    event_count: 0,
    events: ptr::null(),
};
static VK_METHODS: [Message; 4] = [
    message(c"keymap", c"uhu"),
    message(c"key", c"uuu"),
    message(c"modifiers", c"uuuu"),
    message(c"destroy", c""),
];
pub(crate) static VIRTUAL_KEYBOARD: Interface = Interface {
    name: c"zwp_virtual_keyboard_v1".as_ptr(),
    version: 1,
    method_count: 4,
    methods: VK_METHODS.as_ptr(),
    event_count: 0,
    events: ptr::null(),
};
static VK_CREATE_TYPES: Types<2> = Types([ptr::null(), &VIRTUAL_KEYBOARD]);
static VK_MANAGER_METHODS: [Message; 1] = [Message {
    name: c"create_virtual_keyboard".as_ptr(),
    signature: c"on".as_ptr(),
    types: VK_CREATE_TYPES.0.as_ptr(),
}];
pub(crate) static VK_MANAGER: Interface = Interface {
    name: c"zwp_virtual_keyboard_manager_v1".as_ptr(),
    version: 1,
    method_count: 1,
    methods: VK_MANAGER_METHODS.as_ptr(),
    event_count: 0,
    events: ptr::null(),
};
