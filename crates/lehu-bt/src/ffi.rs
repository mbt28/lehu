//! The part of libdbus this crate uses, bound by hand and loaded with
//! `dlopen` at run time rather than linked: the host that runs the tests has
//! the runtime library but no development package, and an image built
//! without Bluetooth has neither, where LEHU then runs with Bluetooth shown
//! as off instead of failing to start. The signatures follow dbus/dbus.h of
//! libdbus 1.14 (the image has 1.14.x); the API has not changed in years.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

pub type DBusConnection = c_void;
pub type DBusMessage = c_void;
pub type DBusPendingCall = c_void;
pub type DBusWatch = c_void;
pub type DBusTimeout = c_void;
pub type DbusBool = u32;

pub const TRUE: DbusBool = 1;
pub const FALSE: DbusBool = 0;

pub const BUS_SYSTEM: c_int = 1;

pub const TYPE_BYTE: c_int = b'y' as c_int;
pub const TYPE_BOOLEAN: c_int = b'b' as c_int;
pub const TYPE_INT16: c_int = b'n' as c_int;
pub const TYPE_UINT16: c_int = b'q' as c_int;
pub const TYPE_UINT32: c_int = b'u' as c_int;
pub const TYPE_STRING: c_int = b's' as c_int;
pub const TYPE_OBJECT_PATH: c_int = b'o' as c_int;
pub const TYPE_UNIX_FD: c_int = b'h' as c_int;
pub const TYPE_ARRAY: c_int = b'a' as c_int;
pub const TYPE_VARIANT: c_int = b'v' as c_int;
pub const TYPE_DICT_ENTRY: c_int = b'e' as c_int;

pub const MESSAGE_METHOD_CALL: c_int = 1;
pub const MESSAGE_ERROR: c_int = 3;
pub const MESSAGE_SIGNAL: c_int = 4;

pub const DISPATCH_DATA_REMAINS: c_int = 0;

pub const HANDLER_RESULT_HANDLED: c_int = 0;
pub const HANDLER_RESULT_NOT_YET_HANDLED: c_int = 1;

pub const WATCH_READABLE: c_uint = 1;
pub const WATCH_WRITABLE: c_uint = 2;
pub const WATCH_ERROR: c_uint = 4;
pub const WATCH_HANGUP: c_uint = 8;

/// `DBusError`: two strings, a word of flag bits and a padding pointer.
#[repr(C)]
pub struct DBusError {
    pub name: *const c_char,
    pub message: *const c_char,
    dummy: c_uint,
    padding1: *mut c_void,
}

impl DBusError {
    pub fn zeroed() -> Self {
        Self {
            name: std::ptr::null(),
            message: std::ptr::null(),
            dummy: 0,
            padding1: std::ptr::null_mut(),
        }
    }
}

/// `DBusMessageIter` is an opaque value type of this exact size; libdbus
/// fills it in place.
#[repr(C)]
pub struct DBusMessageIter {
    dummy1: *mut c_void,
    dummy2: *mut c_void,
    dummy3: u32,
    dummy4: c_int,
    dummy5: c_int,
    dummy6: c_int,
    dummy7: c_int,
    dummy8: c_int,
    dummy9: c_int,
    dummy10: c_int,
    dummy11: c_int,
    pad1: c_int,
    pad2: *mut c_void,
    pad3: *mut c_void,
}

impl DBusMessageIter {
    pub fn zeroed() -> Self {
        Self {
            dummy1: std::ptr::null_mut(),
            dummy2: std::ptr::null_mut(),
            dummy3: 0,
            dummy4: 0,
            dummy5: 0,
            dummy6: 0,
            dummy7: 0,
            dummy8: 0,
            dummy9: 0,
            dummy10: 0,
            dummy11: 0,
            pad1: 0,
            pad2: std::ptr::null_mut(),
            pad3: std::ptr::null_mut(),
        }
    }
}

pub type MessageFn =
    unsafe extern "C" fn(*mut DBusConnection, *mut DBusMessage, *mut c_void) -> c_int;
pub type UnregisterFn = unsafe extern "C" fn(*mut DBusConnection, *mut c_void);
pub type FreeFn = unsafe extern "C" fn(*mut c_void);
pub type AddWatchFn = unsafe extern "C" fn(*mut DBusWatch, *mut c_void) -> DbusBool;
pub type WatchFn = unsafe extern "C" fn(*mut DBusWatch, *mut c_void);
pub type AddTimeoutFn = unsafe extern "C" fn(*mut DBusTimeout, *mut c_void) -> DbusBool;
pub type TimeoutFn = unsafe extern "C" fn(*mut DBusTimeout, *mut c_void);
pub type PendingNotifyFn = unsafe extern "C" fn(*mut DBusPendingCall, *mut c_void);

#[repr(C)]
pub struct DBusObjectPathVTable {
    pub unregister_function: Option<UnregisterFn>,
    pub message_function: Option<MessageFn>,
    pad1: Option<unsafe extern "C" fn(*mut c_void)>,
    pad2: Option<unsafe extern "C" fn(*mut c_void)>,
    pad3: Option<unsafe extern "C" fn(*mut c_void)>,
    pad4: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl DBusObjectPathVTable {
    pub const fn new(message: MessageFn) -> Self {
        Self {
            unregister_function: None,
            message_function: Some(message),
            pad1: None,
            pad2: None,
            pad3: None,
            pad4: None,
        }
    }
}

macro_rules! library {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// The functions, resolved once from `libdbus-1.so.3`.
        #[allow(non_snake_case)]
        pub struct Lib {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Lib {
            /// SAFETY: every symbol is cast to the signature libdbus
            /// declares for it; `handle` is a live dlopen handle.
            unsafe fn resolve(handle: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $($name: {
                        let name = concat!(stringify!($name), "\0");
                        // SAFETY: a NUL-terminated literal and a live handle.
                        let sym = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
                        if sym.is_null() {
                            return Err(format!("libdbus has no {}", stringify!($name)));
                        }
                        // SAFETY: the symbol is a function of this signature.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(sym) }
                    },)*
                })
            }
        }
    };
}

library! {
    dbus_threads_init_default: fn() -> DbusBool;
    dbus_error_init: fn(*mut DBusError);
    dbus_error_free: fn(*mut DBusError);
    dbus_bus_get_private: fn(c_int, *mut DBusError) -> *mut DBusConnection;
    dbus_bus_add_match: fn(*mut DBusConnection, *const c_char, *mut DBusError);
    dbus_connection_set_exit_on_disconnect: fn(*mut DBusConnection, DbusBool);
    dbus_connection_set_watch_functions: fn(*mut DBusConnection, Option<AddWatchFn>, Option<WatchFn>, Option<WatchFn>, *mut c_void, Option<FreeFn>) -> DbusBool;
    dbus_connection_set_timeout_functions: fn(*mut DBusConnection, Option<AddTimeoutFn>, Option<TimeoutFn>, Option<TimeoutFn>, *mut c_void, Option<FreeFn>) -> DbusBool;
    dbus_connection_register_object_path: fn(*mut DBusConnection, *const c_char, *const DBusObjectPathVTable, *mut c_void) -> DbusBool;
    dbus_connection_unregister_object_path: fn(*mut DBusConnection, *const c_char) -> DbusBool;
    dbus_connection_add_filter: fn(*mut DBusConnection, MessageFn, *mut c_void, Option<FreeFn>) -> DbusBool;
    dbus_connection_remove_filter: fn(*mut DBusConnection, MessageFn, *mut c_void);
    dbus_connection_dispatch: fn(*mut DBusConnection) -> c_int;
    dbus_connection_send: fn(*mut DBusConnection, *mut DBusMessage, *mut u32) -> DbusBool;
    dbus_connection_send_with_reply: fn(*mut DBusConnection, *mut DBusMessage, *mut *mut DBusPendingCall, c_int) -> DbusBool;
    dbus_connection_flush: fn(*mut DBusConnection);
    dbus_connection_close: fn(*mut DBusConnection);
    dbus_connection_unref: fn(*mut DBusConnection);
    dbus_connection_get_is_connected: fn(*mut DBusConnection) -> DbusBool;
    dbus_watch_get_unix_fd: fn(*mut DBusWatch) -> c_int;
    dbus_watch_get_flags: fn(*mut DBusWatch) -> c_uint;
    dbus_watch_get_enabled: fn(*mut DBusWatch) -> DbusBool;
    dbus_watch_handle: fn(*mut DBusWatch, c_uint) -> DbusBool;
    dbus_timeout_get_interval: fn(*mut DBusTimeout) -> c_int;
    dbus_timeout_get_enabled: fn(*mut DBusTimeout) -> DbusBool;
    dbus_timeout_handle: fn(*mut DBusTimeout) -> DbusBool;
    dbus_pending_call_set_notify: fn(*mut DBusPendingCall, PendingNotifyFn, *mut c_void, Option<FreeFn>) -> DbusBool;
    dbus_pending_call_steal_reply: fn(*mut DBusPendingCall) -> *mut DBusMessage;
    dbus_pending_call_cancel: fn(*mut DBusPendingCall);
    dbus_pending_call_unref: fn(*mut DBusPendingCall);
    dbus_message_new_method_call: fn(*const c_char, *const c_char, *const c_char, *const c_char) -> *mut DBusMessage;
    dbus_message_new_method_return: fn(*mut DBusMessage) -> *mut DBusMessage;
    dbus_message_new_error: fn(*mut DBusMessage, *const c_char, *const c_char) -> *mut DBusMessage;
    dbus_message_ref: fn(*mut DBusMessage) -> *mut DBusMessage;
    dbus_message_unref: fn(*mut DBusMessage);
    dbus_message_get_type: fn(*mut DBusMessage) -> c_int;
    dbus_message_get_member: fn(*mut DBusMessage) -> *const c_char;
    dbus_message_get_interface: fn(*mut DBusMessage) -> *const c_char;
    dbus_message_get_path: fn(*mut DBusMessage) -> *const c_char;
    dbus_message_get_sender: fn(*mut DBusMessage) -> *const c_char;
    dbus_message_get_error_name: fn(*mut DBusMessage) -> *const c_char;
    dbus_message_get_no_reply: fn(*mut DBusMessage) -> DbusBool;
    dbus_message_set_no_reply: fn(*mut DBusMessage, DbusBool);
    dbus_message_set_auto_start: fn(*mut DBusMessage, DbusBool);
    dbus_message_iter_init: fn(*mut DBusMessage, *mut DBusMessageIter) -> DbusBool;
    dbus_message_iter_init_append: fn(*mut DBusMessage, *mut DBusMessageIter);
    dbus_message_iter_get_arg_type: fn(*mut DBusMessageIter) -> c_int;
    dbus_message_iter_next: fn(*mut DBusMessageIter) -> DbusBool;
    dbus_message_iter_recurse: fn(*mut DBusMessageIter, *mut DBusMessageIter);
    dbus_message_iter_get_basic: fn(*mut DBusMessageIter, *mut c_void);
    dbus_message_iter_append_basic: fn(*mut DBusMessageIter, c_int, *const c_void) -> DbusBool;
    dbus_message_iter_open_container: fn(*mut DBusMessageIter, c_int, *const c_char, *mut DBusMessageIter) -> DbusBool;
    dbus_message_iter_close_container: fn(*mut DBusMessageIter, *mut DBusMessageIter) -> DbusBool;
}

static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

/// The library, loaded on the first call; the error names what is missing.
pub fn lib() -> Result<&'static Lib, &'static str> {
    LIB.get_or_init(|| {
        // SAFETY: a NUL-terminated literal; dlopen has no other precondition.
        let handle = unsafe { libc::dlopen(c"libdbus-1.so.3".as_ptr(), libc::RTLD_NOW) };
        if handle.is_null() {
            return Err("libdbus-1.so.3 is not on this system".into());
        }
        // SAFETY: a live handle from dlopen.
        let lib = unsafe { Lib::resolve(handle) }?;
        // SAFETY: called once, before any other libdbus call.
        unsafe { (lib.dbus_threads_init_default)() };
        Ok(lib)
    })
    .as_ref()
    .map_err(String::as_str)
}

/// A C string from libdbus as `&str`; empty when null or not UTF-8.
///
/// SAFETY: `p` is null or points at a NUL-terminated string that outlives
/// the returned slice (libdbus strings live as long as their message).
pub unsafe fn text<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_iterator_and_error_have_libdbus_sizes() {
        // dbus-message.h fixes the iterator at 2 pointers, a u32, 9 ints and
        // 2 pointers; dbus-errors.h the error at 2 pointers, a word and a
        // pointer: 72 and 32 bytes on a 64-bit host, 56 and 16 on the board.
        let ptr = std::mem::size_of::<*mut c_void>();
        assert_eq!(
            std::mem::size_of::<DBusMessageIter>(),
            if ptr == 8 { 72 } else { 56 }
        );
        assert_eq!(
            std::mem::size_of::<DBusError>(),
            if ptr == 8 { 32 } else { 16 }
        );
    }
}
