//! One private connection to the system bus, driven by the manager's own
//! `poll()`: libdbus hands us its descriptors and timeouts, we hand it the
//! events. Nothing libdbus calls back into reaches the manager's state:
//! replies, agent calls and signals land in an inbox the manager drains
//! after each dispatch, so a reply never runs manager code from inside a
//! libdbus call.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::marker::PhantomData;
use std::os::fd::RawFd;

use lehu_core::clock;

use crate::ffi::{self, Lib};

pub const IFACE_DBUS: &str = "org.freedesktop.DBus";
pub const IFACE_LOCAL: &str = "org.freedesktop.DBus.Local";
pub const IFACE_OM: &str = "org.freedesktop.DBus.ObjectManager";
pub const IFACE_PROPS: &str = "org.freedesktop.DBus.Properties";

fn cstr(s: &str) -> Option<CString> {
    CString::new(s).ok()
}

// ── Messages ──────────────────────────────────────────────────────────────

/// A message we hold a reference on.
pub struct Message {
    lib: &'static Lib,
    ptr: *mut ffi::DBusMessage,
}

impl Message {
    /// SAFETY: `ptr` is a live message whose reference the caller passes
    /// on (the Message unrefs it on drop).
    unsafe fn owned(lib: &'static Lib, ptr: *mut ffi::DBusMessage) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { lib, ptr })
        }
    }

    /// SAFETY: `ptr` is a live message; this takes its own reference.
    unsafe fn shared(lib: &'static Lib, ptr: *mut ffi::DBusMessage) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: as above.
        unsafe { (lib.dbus_message_ref)(ptr) };
        Some(Self { lib, ptr })
    }

    pub fn method_call(
        lib: &'static Lib,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
    ) -> Option<Self> {
        let (d, p, i, m) = (
            cstr(destination)?,
            cstr(path)?,
            cstr(interface)?,
            cstr(member)?,
        );
        // SAFETY: four NUL-terminated strings; the result is checked.
        let ptr = unsafe {
            (lib.dbus_message_new_method_call)(d.as_ptr(), p.as_ptr(), i.as_ptr(), m.as_ptr())
        };
        // SAFETY: a fresh reference.
        unsafe { Self::owned(lib, ptr) }
    }

    /// An empty reply to `of`; `None` when none is wanted or none could be
    /// made.
    pub fn reply_to(of: &Message) -> Option<Self> {
        if of.no_reply_wanted() {
            return None;
        }
        // SAFETY: `of` is live.
        let ptr = unsafe { (of.lib.dbus_message_new_method_return)(of.ptr) };
        // SAFETY: a fresh reference.
        unsafe { Self::owned(of.lib, ptr) }
    }

    pub fn error_to(of: &Message, name: &str, text: &str) -> Option<Self> {
        if of.no_reply_wanted() {
            return None;
        }
        let (n, t) = (cstr(name)?, cstr(text)?);
        // SAFETY: `of` is live, the strings NUL-terminated.
        let ptr = unsafe { (of.lib.dbus_message_new_error)(of.ptr, n.as_ptr(), t.as_ptr()) };
        // SAFETY: a fresh reference.
        unsafe { Self::owned(of.lib, ptr) }
    }

    pub fn kind(&self) -> c_int {
        // SAFETY: live message.
        unsafe { (self.lib.dbus_message_get_type)(self.ptr) }
    }

    pub fn is_error(&self) -> bool {
        self.kind() == ffi::MESSAGE_ERROR
    }

    pub fn is_method_call(&self) -> bool {
        self.kind() == ffi::MESSAGE_METHOD_CALL
    }

    pub fn is_signal(&self, interface: &str, member: &str) -> bool {
        self.kind() == ffi::MESSAGE_SIGNAL
            && self.interface() == interface
            && self.member() == member
    }

    fn string_of(&self, f: unsafe extern "C" fn(*mut ffi::DBusMessage) -> *const c_char) -> &str {
        // SAFETY: live message; the string lives as long as it.
        unsafe { ffi::text(f(self.ptr)) }
    }

    pub fn member(&self) -> &str {
        self.string_of(self.lib.dbus_message_get_member)
    }

    pub fn interface(&self) -> &str {
        self.string_of(self.lib.dbus_message_get_interface)
    }

    pub fn path(&self) -> &str {
        self.string_of(self.lib.dbus_message_get_path)
    }

    pub fn sender(&self) -> &str {
        self.string_of(self.lib.dbus_message_get_sender)
    }

    pub fn error_name(&self) -> &str {
        if self.is_error() {
            self.string_of(self.lib.dbus_message_get_error_name)
        } else {
            ""
        }
    }

    /// The error's name and, when it has one, its text: for the log.
    pub fn error_text(&self) -> String {
        let mut s = self.error_name().to_string();
        if let Some(mut a) = self.args()
            && let Some(detail) = a.str()
            && !detail.is_empty()
        {
            s.push_str(" (");
            s.push_str(detail);
            s.push(')');
        }
        s
    }

    pub fn no_reply_wanted(&self) -> bool {
        // SAFETY: live message.
        unsafe { (self.lib.dbus_message_get_no_reply)(self.ptr) != ffi::FALSE }
    }

    pub fn set_no_reply(&self) {
        // SAFETY: live message.
        unsafe { (self.lib.dbus_message_set_no_reply)(self.ptr, ffi::TRUE) };
    }

    /// The arguments, when there are any.
    pub fn args(&self) -> Option<Iter<'_>> {
        let mut it = ffi::DBusMessageIter::zeroed();
        // SAFETY: live message, an iterator of libdbus's own size.
        if unsafe { (self.lib.dbus_message_iter_init)(self.ptr, &mut it) } == ffi::FALSE {
            return None;
        }
        Some(Iter {
            lib: self.lib,
            it,
            _m: PhantomData,
        })
    }

    /// Append arguments at the end.
    pub fn append(&mut self) -> Append<'_> {
        let mut it = ffi::DBusMessageIter::zeroed();
        // SAFETY: live message.
        unsafe { (self.lib.dbus_message_iter_init_append)(self.ptr, &mut it) };
        Append {
            lib: self.lib,
            it,
            ok: true,
            _m: PhantomData,
        }
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        // SAFETY: our own reference.
        unsafe { (self.lib.dbus_message_unref)(self.ptr) };
    }
}

/// Reading arguments in order. Every getter checks the type first, so a
/// message shaped unlike its interface says yields `None`, never a bad read.
pub struct Iter<'m> {
    lib: &'static Lib,
    it: ffi::DBusMessageIter,
    _m: PhantomData<&'m Message>,
}

impl<'m> Iter<'m> {
    pub fn arg_type(&mut self) -> c_int {
        // SAFETY: an iterator libdbus initialised.
        unsafe { (self.lib.dbus_message_iter_get_arg_type)(&mut self.it) }
    }

    /// Step to the next argument; false at the end.
    pub fn next(&mut self) -> bool {
        // SAFETY: as above.
        unsafe { (self.lib.dbus_message_iter_next)(&mut self.it) != ffi::FALSE }
    }

    /// Into a container (array, variant, dict entry).
    pub fn recurse(&mut self) -> Iter<'m> {
        let mut sub = ffi::DBusMessageIter::zeroed();
        // SAFETY: as above; `sub` has libdbus's size.
        unsafe { (self.lib.dbus_message_iter_recurse)(&mut self.it, &mut sub) };
        Iter {
            lib: self.lib,
            it: sub,
            _m: PhantomData,
        }
    }

    fn basic<T: Copy + Default>(&mut self, ty: c_int) -> Option<T> {
        if self.arg_type() != ty {
            return None;
        }
        let mut v = T::default();
        // SAFETY: the type was checked, so libdbus writes a T here.
        unsafe { (self.lib.dbus_message_iter_get_basic)(&mut self.it, (&mut v as *mut T).cast()) };
        Some(v)
    }

    fn string_typed(&mut self, ty: c_int) -> Option<&'m str> {
        if self.arg_type() != ty {
            return None;
        }
        let mut p: *const c_char = std::ptr::null();
        // SAFETY: the type was checked, so libdbus writes a char pointer.
        unsafe {
            (self.lib.dbus_message_iter_get_basic)(
                &mut self.it,
                (&mut p as *mut *const c_char).cast(),
            )
        };
        // SAFETY: the string belongs to the message, which outlives 'm.
        Some(unsafe { ffi::text(p) })
    }

    pub fn str(&mut self) -> Option<&'m str> {
        self.string_typed(ffi::TYPE_STRING)
    }

    pub fn path(&mut self) -> Option<&'m str> {
        self.string_typed(ffi::TYPE_OBJECT_PATH)
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.basic(ffi::TYPE_UINT32)
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.basic(ffi::TYPE_UINT16)
    }

    pub fn i16(&mut self) -> Option<i16> {
        self.basic(ffi::TYPE_INT16)
    }

    pub fn byte(&mut self) -> Option<u8> {
        self.basic(ffi::TYPE_BYTE)
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.basic::<u32>(ffi::TYPE_BOOLEAN).map(|b| b != 0)
    }

    /// A unix descriptor, duplicated into the message for us: ours to own.
    pub fn fd(&mut self) -> Option<RawFd> {
        self.basic::<c_int>(ffi::TYPE_UNIX_FD).filter(|fd| *fd >= 0)
    }

    /// Walk an `a{sv}`: `f(key, value)` with the value's iterator inside
    /// the variant. Anything else shaped is skipped.
    pub fn for_each_variant(&mut self, mut f: impl FnMut(&str, &mut Iter<'m>)) {
        self.for_each_entry(|key, value| {
            if value.arg_type() == ffi::TYPE_VARIANT {
                let mut inner = value.recurse();
                f(key, &mut inner);
            }
        });
    }

    /// Walk an `a{sX}` or an `a{oX}` (GetManagedObjects keys its outer
    /// dictionary by object path): `f(key, value)` with the value's
    /// iterator.
    pub fn for_each_entry(&mut self, mut f: impl FnMut(&str, &mut Iter<'m>)) {
        if self.arg_type() != ffi::TYPE_ARRAY {
            return;
        }
        let mut entries = self.recurse();
        while entries.arg_type() == ffi::TYPE_DICT_ENTRY {
            let mut entry = entries.recurse();
            if let Some(key) = entry.str().or_else(|| entry.path()) {
                entry.next();
                f(key, &mut entry);
            }
            entries.next();
        }
    }

    /// Walk an `as`.
    pub fn for_each_string(&mut self, mut f: impl FnMut(&str)) {
        if self.arg_type() != ffi::TYPE_ARRAY {
            return;
        }
        let mut items = self.recurse();
        while let Some(s) = items.str() {
            f(s);
            items.next();
        }
    }
}

/// Appending arguments. A failure (out of memory, a string with a NUL)
/// sticks: `ok()` says whether everything landed.
pub struct Append<'m> {
    lib: &'static Lib,
    it: ffi::DBusMessageIter,
    ok: bool,
    _m: PhantomData<&'m mut Message>,
}

impl<'m> Append<'m> {
    fn basic<T>(&mut self, ty: c_int, v: &T) -> &mut Self {
        // SAFETY: `v` is a live T matching `ty`.
        let r = unsafe {
            (self.lib.dbus_message_iter_append_basic)(&mut self.it, ty, (v as *const T).cast())
        };
        self.ok &= r != ffi::FALSE;
        self
    }

    fn string(&mut self, ty: c_int, s: &str) -> &mut Self {
        match cstr(s) {
            Some(c) => {
                let p = c.as_ptr();
                self.basic(ty, &p)
            }
            None => {
                self.ok = false;
                self
            }
        }
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.string(ffi::TYPE_STRING, s)
    }

    pub fn path(&mut self, s: &str) -> &mut Self {
        self.string(ffi::TYPE_OBJECT_PATH, s)
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.basic(ffi::TYPE_UINT16, &v)
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.basic(ffi::TYPE_UINT32, &v)
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.basic(ffi::TYPE_BOOLEAN, &u32::from(v))
    }

    /// Open a container; `signature` is the contents' (for arrays and
    /// variants; dict entries take none).
    pub fn open(&mut self, ty: c_int, signature: Option<&str>) -> Append<'m> {
        let mut sub = ffi::DBusMessageIter::zeroed();
        let sig = signature.and_then(cstr);
        let sig_ptr = sig.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        // SAFETY: an initialised parent iterator and a fresh child.
        let r = unsafe {
            (self.lib.dbus_message_iter_open_container)(&mut self.it, ty, sig_ptr, &mut sub)
        };
        self.ok &= r != ffi::FALSE;
        Append {
            lib: self.lib,
            it: sub,
            ok: self.ok,
            _m: PhantomData,
        }
    }

    pub fn close(&mut self, mut child: Append<'m>) -> &mut Self {
        // SAFETY: `child` was opened on this iterator.
        let r =
            unsafe { (self.lib.dbus_message_iter_close_container)(&mut self.it, &mut child.it) };
        self.ok &= child.ok && r != ffi::FALSE;
        self
    }

    /// One `{sv}` entry with a string value.
    pub fn variant_str(&mut self, key: &str, value: &str) -> &mut Self {
        self.variant(key, "s", |v| {
            v.str(value);
        })
    }

    pub fn variant_bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.variant(key, "b", |v| {
            v.bool(value);
        })
    }

    pub fn variant_u16(&mut self, key: &str, value: u16) -> &mut Self {
        self.variant(key, "q", |v| {
            v.u16(value);
        })
    }

    fn variant(&mut self, key: &str, sig: &str, fill: impl FnOnce(&mut Append<'m>)) -> &mut Self {
        let mut entry = self.open(ffi::TYPE_DICT_ENTRY, None);
        entry.str(key);
        let mut var = entry.open(ffi::TYPE_VARIANT, Some(sig));
        fill(&mut var);
        entry.close(var);
        self.close(entry)
    }
}

// ── The inbox ─────────────────────────────────────────────────────────────

/// What libdbus delivered while dispatching, for the manager to handle
/// afterwards.
pub enum Incoming<T> {
    /// A pending call finished: its reply, or `None` when it never got one
    /// (the connection went, or the call could not even be sent).
    Reply {
        tag: T,
        reply: Option<Message>,
    },
    /// A method call to one of our objects (the agent, a profile).
    Call(Message),
    Signal(Message),
}

struct Inbox<T> {
    lib: &'static Lib,
    items: RefCell<VecDeque<Incoming<T>>>,
}

impl<T> Inbox<T> {
    fn push(&self, item: Incoming<T>) {
        self.items.borrow_mut().push_back(item);
    }
}

/// libdbus's descriptors and timers, kept where its callbacks can reach
/// them.
struct Glue {
    lib: &'static Lib,
    watches: RefCell<Vec<*mut ffi::DBusWatch>>,
    /// Each timeout with its deadline in `clock::now_ms` time.
    timeouts: RefCell<Vec<(*mut ffi::DBusTimeout, u64)>>,
    pending: RefCell<Vec<*mut ffi::DBusPendingCall>>,
}

unsafe extern "C" fn add_watch(w: *mut ffi::DBusWatch, data: *mut c_void) -> ffi::DbusBool {
    // SAFETY: `data` is the Glue the connection was given.
    let glue = unsafe { &*(data as *const Glue) };
    glue.watches.borrow_mut().push(w);
    ffi::TRUE
}

unsafe extern "C" fn remove_watch(w: *mut ffi::DBusWatch, data: *mut c_void) {
    // SAFETY: as above.
    let glue = unsafe { &*(data as *const Glue) };
    glue.watches.borrow_mut().retain(|x| *x != w);
}

unsafe extern "C" fn toggle_watch(_w: *mut ffi::DBusWatch, _data: *mut c_void) {
    // The enabled flag is read when the poll set is built.
}

unsafe extern "C" fn add_timeout(t: *mut ffi::DBusTimeout, data: *mut c_void) -> ffi::DbusBool {
    // SAFETY: as above.
    let glue = unsafe { &*(data as *const Glue) };
    // SAFETY: a live timeout.
    let interval = unsafe { (glue.lib.dbus_timeout_get_interval)(t) }.max(0) as u64;
    glue.timeouts
        .borrow_mut()
        .push((t, clock::now_ms() + interval));
    ffi::TRUE
}

unsafe extern "C" fn remove_timeout(t: *mut ffi::DBusTimeout, data: *mut c_void) {
    // SAFETY: as above.
    let glue = unsafe { &*(data as *const Glue) };
    glue.timeouts.borrow_mut().retain(|(x, _)| *x != t);
}

unsafe extern "C" fn toggle_timeout(t: *mut ffi::DBusTimeout, data: *mut c_void) {
    // SAFETY: as above.
    let glue = unsafe { &*(data as *const Glue) };
    // SAFETY: a live timeout.
    let interval = unsafe { (glue.lib.dbus_timeout_get_interval)(t) }.max(0) as u64;
    for slot in glue.timeouts.borrow_mut().iter_mut() {
        if slot.0 == t {
            slot.1 = clock::now_ms() + interval;
        }
    }
}

struct PendingCtx<T> {
    inbox: *const Inbox<T>,
    glue: *const Glue,
    tag: Option<T>,
}

unsafe extern "C" fn on_pending<T>(pc: *mut ffi::DBusPendingCall, data: *mut c_void) {
    // SAFETY: `data` is the PendingCtx<T> given with the notify; the pending
    // call owns it until its unref below frees it, so everything is taken
    // out first.
    let ctx = unsafe { &mut *(data as *mut PendingCtx<T>) };
    let (inbox_ptr, glue_ptr, tag) = (ctx.inbox, ctx.glue, ctx.tag.take());
    // SAFETY: the inbox and glue outlive every pending call (the bus cancels
    // them before it goes).
    let (inbox, glue) = unsafe { (&*inbox_ptr, &*glue_ptr) };
    // SAFETY: a finished call hands over its reply's reference.
    let reply = unsafe { Message::owned(inbox.lib, (inbox.lib.dbus_pending_call_steal_reply)(pc)) };
    glue.pending.borrow_mut().retain(|x| *x != pc);
    // SAFETY: our reference on the call; this frees `ctx`.
    unsafe { (inbox.lib.dbus_pending_call_unref)(pc) };
    if let Some(tag) = tag {
        inbox.push(Incoming::Reply { tag, reply });
    }
}

unsafe extern "C" fn free_ctx<T>(data: *mut c_void) {
    // SAFETY: a Box<PendingCtx<T>> handed to libdbus as its user data.
    drop(unsafe { Box::from_raw(data as *mut PendingCtx<T>) });
}

unsafe extern "C" fn object_message<T>(
    _c: *mut ffi::DBusConnection,
    msg: *mut ffi::DBusMessage,
    data: *mut c_void,
) -> c_int {
    // SAFETY: `data` is the Inbox<T> the object path was registered with.
    let inbox = unsafe { &*(data as *const Inbox<T>) };
    // SAFETY: a live message; we take our own reference.
    if let Some(m) = unsafe { Message::shared(inbox.lib, msg) }
        && m.is_method_call()
    {
        inbox.push(Incoming::Call(m));
        return ffi::HANDLER_RESULT_HANDLED;
    }
    ffi::HANDLER_RESULT_NOT_YET_HANDLED
}

unsafe extern "C" fn filter<T>(
    _c: *mut ffi::DBusConnection,
    msg: *mut ffi::DBusMessage,
    data: *mut c_void,
) -> c_int {
    // SAFETY: as in `object_message`.
    let inbox = unsafe { &*(data as *const Inbox<T>) };
    // SAFETY: a live message; our own reference.
    if let Some(m) = unsafe { Message::shared(inbox.lib, msg) }
        && m.kind() == ffi::MESSAGE_SIGNAL
    {
        inbox.push(Incoming::Signal(m));
    }
    ffi::HANDLER_RESULT_NOT_YET_HANDLED
}

// ── The bus ───────────────────────────────────────────────────────────────

/// The connection. Used from one thread.
pub struct Bus<T: 'static> {
    lib: &'static Lib,
    conn: *mut ffi::DBusConnection,
    glue: Box<Glue>,
    inbox: Box<Inbox<T>>,
    paths: Vec<CString>,
    polled: Vec<*mut ffi::DBusWatch>,
}

impl<T: 'static> Bus<T> {
    /// A private connection to the system bus; `Err` with libdbus's reason
    /// when there is none (yet).
    pub fn connect_system() -> Result<Self, String> {
        let lib = ffi::lib().map_err(str::to_string)?;
        let mut err = ffi::DBusError::zeroed();
        // SAFETY: a zeroed error for libdbus to fill.
        unsafe { (lib.dbus_error_init)(&mut err) };
        // SAFETY: plain call; the result is checked.
        let conn = unsafe { (lib.dbus_bus_get_private)(ffi::BUS_SYSTEM, &mut err) };
        if conn.is_null() {
            // SAFETY: libdbus set the error's strings.
            let why = unsafe { ffi::text(err.message) }.to_string();
            // SAFETY: an error libdbus filled.
            unsafe { (lib.dbus_error_free)(&mut err) };
            return Err(if why.is_empty() {
                "no system bus".into()
            } else {
                why
            });
        }
        let glue = Box::new(Glue {
            lib,
            watches: RefCell::new(Vec::new()),
            timeouts: RefCell::new(Vec::new()),
            pending: RefCell::new(Vec::new()),
        });
        let inbox = Box::new(Inbox {
            lib,
            items: RefCell::new(VecDeque::new()),
        });
        let glue_ptr: *const Glue = &*glue;
        let inbox_ptr: *const Inbox<T> = &*inbox;
        // SAFETY: a live connection; the glue and inbox are boxed, so their
        // addresses hold for the connection's life, and the bus drops the
        // connection before them.
        unsafe {
            (lib.dbus_connection_set_exit_on_disconnect)(conn, ffi::FALSE);
            (lib.dbus_connection_set_watch_functions)(
                conn,
                Some(add_watch),
                Some(remove_watch),
                Some(toggle_watch),
                glue_ptr as *mut c_void,
                None,
            );
            (lib.dbus_connection_set_timeout_functions)(
                conn,
                Some(add_timeout),
                Some(remove_timeout),
                Some(toggle_timeout),
                glue_ptr as *mut c_void,
                None,
            );
            (lib.dbus_connection_add_filter)(conn, filter::<T>, inbox_ptr as *mut c_void, None);
        }
        Ok(Self {
            lib,
            conn,
            glue,
            inbox,
            paths: Vec::new(),
            polled: Vec::new(),
        })
    }

    pub fn lib(&self) -> &'static Lib {
        self.lib
    }

    pub fn connected(&self) -> bool {
        // SAFETY: live connection.
        unsafe { (self.lib.dbus_connection_get_is_connected)(self.conn) != ffi::FALSE }
    }

    /// Serve method calls to `path` through the inbox.
    pub fn register_path(&mut self, path: &str) -> bool {
        let Some(p) = cstr(path) else {
            return false;
        };
        let vtable = ffi::DBusObjectPathVTable::new(object_message::<T>);
        let inbox_ptr: *const Inbox<T> = &*self.inbox;
        // SAFETY: live connection; libdbus copies the vtable.
        let r = unsafe {
            (self.lib.dbus_connection_register_object_path)(
                self.conn,
                p.as_ptr(),
                &vtable,
                inbox_ptr as *mut c_void,
            )
        };
        if r == ffi::FALSE {
            return false;
        }
        self.paths.push(p);
        true
    }

    pub fn unregister_path(&mut self, path: &str) {
        if let Some(i) = self
            .paths
            .iter()
            .position(|p| p.to_bytes() == path.as_bytes())
        {
            let p = self.paths.remove(i);
            // SAFETY: a path registered on this connection.
            unsafe { (self.lib.dbus_connection_unregister_object_path)(self.conn, p.as_ptr()) };
        }
    }

    /// Ask the daemon to send us the signals matching `rule`. Without an
    /// error argument libdbus does not wait for the reply.
    pub fn add_match(&self, rule: &str) {
        if let Some(r) = cstr(rule) {
            // SAFETY: live connection, NUL-terminated rule.
            unsafe { (self.lib.dbus_bus_add_match)(self.conn, r.as_ptr(), std::ptr::null_mut()) };
        }
    }

    /// Send without waiting for a reply.
    pub fn send(&self, msg: Message) {
        // SAFETY: live connection and message.
        unsafe { (self.lib.dbus_connection_send)(self.conn, msg.ptr, std::ptr::null_mut()) };
    }

    /// Send; the reply reaches the inbox as `Incoming::Reply { tag, .. }`,
    /// with `None` for a reply when the call could not be made.
    pub fn call(&self, msg: Message, timeout_ms: c_int, tag: T) {
        // SAFETY: live message.
        unsafe { (self.lib.dbus_message_set_auto_start)(msg.ptr, ffi::FALSE) };
        let mut pc: *mut ffi::DBusPendingCall = std::ptr::null_mut();
        // SAFETY: live connection and message; `pc` receives the call.
        let sent = unsafe {
            (self.lib.dbus_connection_send_with_reply)(self.conn, msg.ptr, &mut pc, timeout_ms)
        };
        if sent == ffi::FALSE || pc.is_null() {
            self.inbox.push(Incoming::Reply { tag, reply: None });
            return;
        }
        let ctx = Box::new(PendingCtx {
            inbox: &*self.inbox as *const Inbox<T>,
            glue: &*self.glue as *const Glue,
            tag: Some(tag),
        });
        let raw = Box::into_raw(ctx);
        // SAFETY: a live pending call; libdbus frees `raw` through free_ctx.
        let set = unsafe {
            (self.lib.dbus_pending_call_set_notify)(
                pc,
                on_pending::<T>,
                raw as *mut c_void,
                Some(free_ctx::<T>),
            )
        };
        if set == ffi::FALSE {
            // SAFETY: libdbus did not take `raw`; the call is ours to drop.
            let ctx = unsafe { Box::from_raw(raw) };
            unsafe {
                (self.lib.dbus_pending_call_cancel)(pc);
                (self.lib.dbus_pending_call_unref)(pc);
            }
            if let Some(tag) = ctx.tag {
                self.inbox.push(Incoming::Reply { tag, reply: None });
            }
            return;
        }
        self.glue.pending.borrow_mut().push(pc);
    }

    /// Hand libdbus what arrived, until it has nothing left to dispatch.
    pub fn dispatch(&self) {
        // SAFETY: live connection.
        while unsafe { (self.lib.dbus_connection_dispatch)(self.conn) }
            == ffi::DISPATCH_DATA_REMAINS
        {}
    }

    /// The next item libdbus delivered.
    pub fn take(&self) -> Option<Incoming<T>> {
        self.inbox.items.borrow_mut().pop_front()
    }

    /// Add libdbus's enabled descriptors to a poll set; `handle` wants the
    /// same slice back with the results.
    pub fn poll_fds(&mut self, out: &mut Vec<libc::pollfd>) {
        self.polled.clear();
        for &w in self.glue.watches.borrow().iter() {
            // SAFETY: a watch libdbus added and has not removed.
            if unsafe { (self.lib.dbus_watch_get_enabled)(w) } == ffi::FALSE {
                continue;
            }
            // SAFETY: as above.
            let flags = unsafe { (self.lib.dbus_watch_get_flags)(w) };
            let mut events: libc::c_short = 0;
            if flags & ffi::WATCH_READABLE != 0 {
                events |= libc::POLLIN;
            }
            if flags & ffi::WATCH_WRITABLE != 0 {
                events |= libc::POLLOUT;
            }
            out.push(libc::pollfd {
                // SAFETY: as above.
                fd: unsafe { (self.lib.dbus_watch_get_unix_fd)(w) },
                events,
                revents: 0,
            });
            self.polled.push(w);
        }
    }

    /// Tell libdbus what `poll` reported for the descriptors `poll_fds`
    /// added, in the same order.
    pub fn handle(&mut self, fds: &[libc::pollfd]) {
        let polled = std::mem::take(&mut self.polled);
        for (i, &w) in polled.iter().enumerate() {
            let Some(p) = fds.get(i) else {
                break;
            };
            if p.revents == 0 || !self.glue.watches.borrow().contains(&w) {
                continue;
            }
            let mut flags: c_uint = 0;
            if p.revents & libc::POLLIN != 0 {
                flags |= ffi::WATCH_READABLE;
            }
            if p.revents & libc::POLLOUT != 0 {
                flags |= ffi::WATCH_WRITABLE;
            }
            if p.revents & libc::POLLHUP != 0 {
                flags |= ffi::WATCH_HANGUP;
            }
            if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                flags |= ffi::WATCH_ERROR;
            }
            // SAFETY: a watch still in libdbus's list.
            unsafe { (self.lib.dbus_watch_handle)(w, flags) };
        }
        self.polled = polled;
        self.handle_timeouts();
    }

    /// The nearest libdbus timeout, in milliseconds from now.
    pub fn next_timeout_ms(&self) -> Option<u64> {
        let now = clock::now_ms();
        let mut next = None;
        for &(t, deadline) in self.glue.timeouts.borrow().iter() {
            // SAFETY: a timeout libdbus added and has not removed.
            if unsafe { (self.lib.dbus_timeout_get_enabled)(t) } == ffi::FALSE {
                continue;
            }
            let left = deadline.saturating_sub(now);
            next = Some(next.map_or(left, |n: u64| n.min(left)));
        }
        next
    }

    fn handle_timeouts(&self) {
        let now = clock::now_ms();
        let due: Vec<*mut ffi::DBusTimeout> = self
            .glue
            .timeouts
            .borrow()
            .iter()
            // SAFETY: as above.
            .filter(|(t, d)| {
                *d <= now && unsafe { (self.lib.dbus_timeout_get_enabled)(*t) } != ffi::FALSE
            })
            .map(|(t, _)| *t)
            .collect();
        for t in due {
            // An earlier handler may have removed it.
            let still = {
                let mut list = self.glue.timeouts.borrow_mut();
                match list.iter_mut().find(|(x, _)| *x == t) {
                    Some(slot) => {
                        // SAFETY: a live timeout.
                        let interval =
                            unsafe { (self.lib.dbus_timeout_get_interval)(t) }.max(0) as u64;
                        slot.1 = now + interval;
                        true
                    }
                    None => false,
                }
            };
            if still {
                // SAFETY: a live timeout; the list is not borrowed here.
                unsafe { (self.lib.dbus_timeout_handle)(t) };
            }
        }
    }

    /// Flush what is queued to the daemon (used before a polite leave).
    pub fn flush(&self) {
        // SAFETY: live connection.
        unsafe { (self.lib.dbus_connection_flush)(self.conn) };
    }
}

impl<T: 'static> Drop for Bus<T> {
    fn drop(&mut self) {
        let pending: Vec<_> = self.glue.pending.borrow_mut().drain(..).collect();
        for pc in pending {
            // SAFETY: calls this bus sent and has not seen finish; cancel
            // keeps the notify from running, unref frees the context.
            unsafe {
                (self.lib.dbus_pending_call_cancel)(pc);
                (self.lib.dbus_pending_call_unref)(pc);
            }
        }
        let inbox_ptr: *const Inbox<T> = &*self.inbox;
        // SAFETY: the filter and paths this bus registered on its own
        // connection; a private connection is closed before its last unref.
        unsafe {
            (self.lib.dbus_connection_remove_filter)(
                self.conn,
                filter::<T>,
                inbox_ptr as *mut c_void,
            );
            for p in &self.paths {
                (self.lib.dbus_connection_unregister_object_path)(self.conn, p.as_ptr());
            }
            (self.lib.dbus_connection_close)(self.conn);
            (self.lib.dbus_connection_unref)(self.conn);
        }
        self.inbox.items.borrow_mut().clear();
    }
}
