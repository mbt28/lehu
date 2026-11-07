//! The C helper (`csrc/decoder.c`) behind a safe face. Without ffmpeg's
//! headers on the host the stub below takes its place and every open fails.

/// Picture slots the helper keeps referenced: the three held and the
/// pending one. The Rust side decides what goes where.
pub const SLOTS: usize = 4;

/// What a picture's owner can do with the slots while the picture is in
/// hand, and after.
pub trait Slots {
    /// Keep the picture of the current callback in `slot`, releasing what
    /// the slot held.
    fn hold(&mut self, slot: usize);
    /// Give the slot's buffer back to the decoder's pool.
    fn release(&mut self, slot: usize);
}

#[cfg(not(lehu_video_stub))]
mod real {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::ptr::NonNull;

    use lehu_drm::{Frame, MAX_PLANES, Plane};

    use super::Slots;

    /// A picture as the helper describes it.
    #[repr(C)]
    pub struct Picture {
        fourcc: u32,
        width: i32,
        height: i32,
        planes: i32,
        fds: [i32; 4],
        pitches: [u32; 4],
        offsets: [u32; 4],
        modifier: u64,
    }

    impl Picture {
        /// The display's view of it; sizes and counts are checked again by
        /// `Frame::check` before any import.
        fn frame(&self) -> Frame {
            let count = usize::try_from(self.planes).unwrap_or(0).min(MAX_PLANES);
            let mut planes = [Plane::default(); MAX_PLANES];
            for (i, p) in planes.iter_mut().enumerate().take(count) {
                *p = Plane {
                    fd: self.fds[i],
                    pitch: self.pitches[i],
                    offset: self.offsets[i],
                };
            }
            Frame {
                fourcc: self.fourcc,
                width: u32::try_from(self.width).unwrap_or(0),
                height: u32::try_from(self.height).unwrap_or(0),
                planes,
                plane_count: count,
                modifier: self.modifier,
            }
        }
    }

    #[repr(C)]
    struct Dec {
        _private: [u8; 0],
    }

    type PictureCb = extern "C" fn(*mut c_void, *const Picture);

    unsafe extern "C" {
        fn lehu_dec_support(which: c_int) -> c_int;
        fn lehu_dec_open(
            hevc: c_int,
            low_delay: c_int,
            fast: c_int,
            err: *mut c_char,
            err_len: usize,
        ) -> *mut Dec;
        fn lehu_dec_name(d: *const Dec) -> *const c_char;
        fn lehu_dec_push(
            d: *mut Dec,
            data: *const u8,
            len: usize,
            cb: PictureCb,
            ctx: *mut c_void,
        ) -> c_int;
        fn lehu_dec_hold(d: *mut Dec, slot: c_int);
        fn lehu_dec_release(d: *mut Dec, slot: c_int);
        fn lehu_dec_close(d: *mut Dec);
        fn lehu_dec_strerror(err: c_int, buf: *mut c_char, len: usize);
    }

    /// What ffmpeg has for a coded format (`Format`'s index).
    pub fn support(which: usize) -> crate::probe::Ffmpeg {
        let Ok(which) = c_int::try_from(which) else {
            return crate::probe::Ffmpeg::default();
        };
        // SAFETY: a pure query of libavcodec's tables.
        let bits = unsafe { lehu_dec_support(which) };
        crate::probe::Ffmpeg {
            decoder: bits & 1 != 0,
            parser: bits & 2 != 0,
            hwaccel: bits & 4 != 0,
        }
    }

    /// An open decoder. Used from one thread at a time; moved to the decode
    /// thread after `open`.
    pub struct Raw(NonNull<Dec>);

    // SAFETY: the helper keeps no thread-local state; the context is used by
    // one thread at a time and `Raw` is not `Sync`.
    unsafe impl Send for Raw {}

    /// The slots, usable while a picture is in hand (inside the callback).
    pub struct Scratch(NonNull<Dec>);

    impl Slots for Scratch {
        fn hold(&mut self, slot: usize) {
            // SAFETY: the context is open for the length of the callback.
            unsafe { lehu_dec_hold(self.0.as_ptr(), c_int::try_from(slot).unwrap_or(-1)) }
        }
        fn release(&mut self, slot: usize) {
            // SAFETY: as above.
            unsafe { lehu_dec_release(self.0.as_ptr(), c_int::try_from(slot).unwrap_or(-1)) }
        }
    }

    struct Call<'a> {
        dec: NonNull<Dec>,
        f: &'a mut dyn FnMut(&Frame, &mut dyn Slots),
    }

    extern "C" fn on_picture(ctx: *mut c_void, p: *const Picture) {
        if ctx.is_null() || p.is_null() {
            return;
        }
        // SAFETY: ctx is the `Call` that `push` passed, alive for the push;
        // p is the helper's picture, valid for this call.
        let (call, p) = unsafe { (&mut *ctx.cast::<Call<'_>>(), &*p) };
        let mut scratch = Scratch(call.dec);
        (call.f)(&p.frame(), &mut scratch);
    }

    fn message(buf: &[c_char]) -> String {
        // SAFETY: the helper always terminates what it writes into `buf`.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    impl Raw {
        pub fn open(hevc: bool, low_delay: bool, fast: bool) -> Result<Self, String> {
            let mut err = [0 as c_char; 128];
            // SAFETY: err is a writable buffer of the length given.
            let d = unsafe {
                lehu_dec_open(
                    c_int::from(hevc),
                    c_int::from(low_delay),
                    c_int::from(fast),
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            match NonNull::new(d) {
                Some(d) => Ok(Raw(d)),
                None => Err(message(&err)),
            }
        }

        /// ffmpeg's name for the decoder in use.
        pub fn name(&self) -> String {
            // SAFETY: the context is open; the name is a static string.
            unsafe { CStr::from_ptr(lehu_dec_name(self.0.as_ptr())) }
                .to_string_lossy()
                .into_owned()
        }

        /// Decode one access unit (`PADDING` readable bytes must follow
        /// `data`; `AccessUnit::padded` guarantees it). `f` sees each picture
        /// while it is in hand. Ok with the picture count, or the decoder's
        /// error code when it refused a packet and no picture came.
        pub fn push(
            &mut self,
            data: &[u8],
            f: &mut dyn FnMut(&Frame, &mut dyn Slots),
        ) -> Result<u32, i32> {
            let mut call = Call { dec: self.0, f };
            // SAFETY: the context is open; data is readable with its padding;
            // the callback only runs during this call with `call` alive.
            let ret = unsafe {
                lehu_dec_push(
                    self.0.as_ptr(),
                    data.as_ptr(),
                    data.len(),
                    on_picture,
                    (&raw mut call).cast(),
                )
            };
            u32::try_from(ret).map_err(|_| ret)
        }

        pub fn release(&mut self, slot: usize) {
            Scratch(self.0).release(slot);
        }

        /// Close the decoder: its pool goes back to CMA.
        pub fn close(self) {}

        pub fn error_text(err: i32) -> String {
            let mut buf = [0 as c_char; 128];
            // SAFETY: buf is a writable buffer of the length given.
            unsafe { lehu_dec_strerror(err, buf.as_mut_ptr(), buf.len()) };
            message(&buf)
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            // SAFETY: open, closed once, here.
            unsafe { lehu_dec_close(self.0.as_ptr()) }
        }
    }
}

#[cfg(lehu_video_stub)]
mod stub {
    use lehu_drm::Frame;

    use super::Slots;

    pub fn support(_which: usize) -> crate::probe::Ffmpeg {
        crate::probe::Ffmpeg::default()
    }

    /// Never opens: there is no ffmpeg on this host.
    pub struct Raw(());

    impl Raw {
        pub fn open(_hevc: bool, _low_delay: bool, _fast: bool) -> Result<Self, String> {
            Err("built without ffmpeg".to_string())
        }

        pub fn name(&self) -> String {
            String::new()
        }

        pub fn push(
            &mut self,
            _data: &[u8],
            _f: &mut dyn FnMut(&Frame, &mut dyn Slots),
        ) -> Result<u32, i32> {
            Ok(0)
        }

        pub fn release(&mut self, _slot: usize) {}

        pub fn close(self) {}

        pub fn error_text(err: i32) -> String {
            format!("error {err}")
        }
    }
}

#[cfg(not(lehu_video_stub))]
pub use real::{Raw, support};
#[cfg(lehu_video_stub)]
pub use stub::{Raw, support};
