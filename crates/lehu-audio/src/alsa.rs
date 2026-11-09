// SPDX-License-Identifier: GPL-3.0-or-later
//! alsa-lib behind a safe face: the PCM for playback and capture, and the
//! simple mixer for the volume control. Declared here rather than through
//! a binding crate: the library is on the image, the handful of calls is
//! fixed, and a host without alsa-lib's development package then still
//! builds. Without the library at all the stub below takes over and every
//! open fails.

use std::fmt;

/// An alsa-lib error: the call and its code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub what: &'static str,
    pub code: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, strerror(self.code))
    }
}

impl std::error::Error for Error {}

/// A playback device as it was opened: the period and buffer the driver
/// settled on, in frames.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    pub rate: u32,
    pub channels: u32,
    pub period: usize,
    pub buffer: usize,
}

#[cfg(not(lehu_audio_no_alsa))]
mod real {
    use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
    use std::ptr::{self, NonNull};

    use super::{Error, Opened};

    #[repr(C)]
    pub struct snd_pcm_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct snd_pcm_hw_params_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct snd_pcm_sw_params_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct snd_mixer_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct snd_mixer_elem_t {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct snd_mixer_selem_id_t {
        _p: [u8; 0],
    }

    const SND_PCM_STREAM_PLAYBACK: c_int = 0;
    const SND_PCM_STREAM_CAPTURE: c_int = 1;
    const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;
    const SND_PCM_FORMAT_S16_LE: c_int = 2;
    const SND_MIXER_SCHN_FRONT_LEFT: c_int = 0;

    unsafe extern "C" {
        fn snd_strerror(err: c_int) -> *const c_char;

        fn snd_pcm_open(
            pcm: *mut *mut snd_pcm_t,
            name: *const c_char,
            stream: c_int,
            mode: c_int,
        ) -> c_int;
        fn snd_pcm_close(pcm: *mut snd_pcm_t) -> c_int;
        fn snd_pcm_drop(pcm: *mut snd_pcm_t) -> c_int;
        fn snd_pcm_writei(pcm: *mut snd_pcm_t, buf: *const c_void, frames: c_ulong) -> c_long;
        fn snd_pcm_readi(pcm: *mut snd_pcm_t, buf: *mut c_void, frames: c_ulong) -> c_long;
        fn snd_pcm_recover(pcm: *mut snd_pcm_t, err: c_int, silent: c_int) -> c_int;

        fn snd_pcm_hw_params_sizeof() -> usize;
        fn snd_pcm_hw_params_any(pcm: *mut snd_pcm_t, p: *mut snd_pcm_hw_params_t) -> c_int;
        fn snd_pcm_hw_params_set_access(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            access: c_int,
        ) -> c_int;
        fn snd_pcm_hw_params_set_format(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            format: c_int,
        ) -> c_int;
        fn snd_pcm_hw_params_set_channels(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            val: c_uint,
        ) -> c_int;
        fn snd_pcm_hw_params_set_rate_near(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            val: *mut c_uint,
            dir: *mut c_int,
        ) -> c_int;
        fn snd_pcm_hw_params_set_period_size_near(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            val: *mut c_ulong,
            dir: *mut c_int,
        ) -> c_int;
        fn snd_pcm_hw_params_set_buffer_size_near(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_hw_params_t,
            val: *mut c_ulong,
        ) -> c_int;
        fn snd_pcm_hw_params(pcm: *mut snd_pcm_t, p: *mut snd_pcm_hw_params_t) -> c_int;

        fn snd_pcm_sw_params_sizeof() -> usize;
        fn snd_pcm_sw_params_current(pcm: *mut snd_pcm_t, p: *mut snd_pcm_sw_params_t) -> c_int;
        fn snd_pcm_sw_params_set_start_threshold(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_sw_params_t,
            val: c_ulong,
        ) -> c_int;
        fn snd_pcm_sw_params_set_avail_min(
            pcm: *mut snd_pcm_t,
            p: *mut snd_pcm_sw_params_t,
            val: c_ulong,
        ) -> c_int;
        fn snd_pcm_sw_params(pcm: *mut snd_pcm_t, p: *mut snd_pcm_sw_params_t) -> c_int;

        fn snd_mixer_open(mixer: *mut *mut snd_mixer_t, mode: c_int) -> c_int;
        fn snd_mixer_attach(mixer: *mut snd_mixer_t, name: *const c_char) -> c_int;
        fn snd_mixer_selem_register(
            mixer: *mut snd_mixer_t,
            options: *mut c_void,
            classp: *mut c_void,
        ) -> c_int;
        fn snd_mixer_load(mixer: *mut snd_mixer_t) -> c_int;
        fn snd_mixer_close(mixer: *mut snd_mixer_t) -> c_int;
        fn snd_mixer_handle_events(mixer: *mut snd_mixer_t) -> c_int;
        fn snd_mixer_selem_id_sizeof() -> usize;
        fn snd_mixer_selem_id_set_index(id: *mut snd_mixer_selem_id_t, val: c_uint);
        fn snd_mixer_selem_id_set_name(id: *mut snd_mixer_selem_id_t, val: *const c_char);
        fn snd_mixer_find_selem(
            mixer: *mut snd_mixer_t,
            id: *const snd_mixer_selem_id_t,
        ) -> *mut snd_mixer_elem_t;
        fn snd_mixer_selem_get_name(elem: *mut snd_mixer_elem_t) -> *const c_char;
        fn snd_mixer_selem_is_active(elem: *mut snd_mixer_elem_t) -> c_int;
        fn snd_mixer_selem_has_playback_volume(elem: *mut snd_mixer_elem_t) -> c_int;
        fn snd_mixer_selem_has_playback_switch(elem: *mut snd_mixer_elem_t) -> c_int;
        fn snd_mixer_selem_get_playback_volume_range(
            elem: *mut snd_mixer_elem_t,
            min: *mut c_long,
            max: *mut c_long,
        ) -> c_int;
        fn snd_mixer_selem_get_playback_dB_range(
            elem: *mut snd_mixer_elem_t,
            min: *mut c_long,
            max: *mut c_long,
        ) -> c_int;
        fn snd_mixer_selem_get_playback_volume(
            elem: *mut snd_mixer_elem_t,
            channel: c_int,
            value: *mut c_long,
        ) -> c_int;
        fn snd_mixer_selem_get_playback_dB(
            elem: *mut snd_mixer_elem_t,
            channel: c_int,
            value: *mut c_long,
        ) -> c_int;
        fn snd_mixer_selem_set_playback_volume_all(
            elem: *mut snd_mixer_elem_t,
            value: c_long,
        ) -> c_int;
        fn snd_mixer_selem_set_playback_dB_all(
            elem: *mut snd_mixer_elem_t,
            value: c_long,
            dir: c_int,
        ) -> c_int;
        fn snd_mixer_selem_set_playback_switch_all(
            elem: *mut snd_mixer_elem_t,
            value: c_int,
        ) -> c_int;
        fn snd_mixer_selem_ask_playback_vol_dB(
            elem: *mut snd_mixer_elem_t,
            value: c_long,
            db: *mut c_long,
        ) -> c_int;
    }

    /// A C long (32 bits on the board, 64 on a host) as i64.
    fn widen<T: Into<i64>>(v: T) -> i64 {
        v.into()
    }

    pub fn strerror(err: i32) -> String {
        // SAFETY: a static string for any code.
        let s = unsafe { snd_strerror(err) };
        if s.is_null() {
            return format!("error {err}");
        }
        // SAFETY: non-null and terminated, as alsa-lib promises.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    fn check(what: &'static str, code: c_int) -> Result<(), Error> {
        if code < 0 {
            Err(Error { what, code })
        } else {
            Ok(())
        }
    }

    fn cstring(what: &'static str, s: &str) -> Result<CString, Error> {
        CString::new(s).map_err(|_| Error {
            what,
            code: -libc::EINVAL,
        })
    }

    /// An open PCM, used from the one thread that owns it.
    pub struct Pcm {
        pcm: NonNull<snd_pcm_t>,
        pub channels: u32,
    }

    // SAFETY: alsa-lib's handle may be used from any one thread at a time;
    // `Pcm` is not `Sync`.
    unsafe impl Send for Pcm {}

    impl Pcm {
        fn open(
            device: &str,
            stream: c_int,
            rate: u32,
            channels: u32,
            fallback_channels: Option<u32>,
            period: usize,
            buffer: usize,
        ) -> Result<(Pcm, Opened), Error> {
            let name = cstring("device name", device)?;
            let mut raw: *mut snd_pcm_t = ptr::null_mut();
            // SAFETY: name is terminated; raw receives the handle. Mode 0 is
            // blocking, which is the point: the writes pace the thread.
            check("open", unsafe {
                snd_pcm_open(&raw mut raw, name.as_ptr(), stream, 0)
            })?;
            let Some(handle) = NonNull::new(raw) else {
                return Err(Error {
                    what: "open",
                    code: -libc::EINVAL,
                });
            };
            let mut pcm = Pcm {
                pcm: handle,
                channels,
            };
            let got = pcm.set_hw(rate, channels, fallback_channels, period, buffer)?;
            pcm.channels = got.channels;
            if stream == SND_PCM_STREAM_PLAYBACK {
                pcm.set_sw(got.period)?;
            }
            Ok((pcm, got))
        }

        fn set_hw(
            &self,
            rate: u32,
            channels: u32,
            fallback_channels: Option<u32>,
            period: usize,
            buffer: usize,
        ) -> Result<Opened, Error> {
            // SAFETY: the opaque params live in a buffer of the size alsa-lib
            // states, zeroed, as snd_pcm_hw_params_alloca does.
            let mut params = vec![0u8; unsafe { snd_pcm_hw_params_sizeof() }];
            let hw = params.as_mut_ptr().cast::<snd_pcm_hw_params_t>();
            let p = self.pcm.as_ptr();
            let mut got_channels = channels;
            // SAFETY: p is open, hw is a valid params buffer for every call.
            unsafe {
                check("hw params any", snd_pcm_hw_params_any(p, hw))?;
                check(
                    "set access",
                    snd_pcm_hw_params_set_access(p, hw, SND_PCM_ACCESS_RW_INTERLEAVED),
                )?;
                check(
                    "set format",
                    snd_pcm_hw_params_set_format(p, hw, SND_PCM_FORMAT_S16_LE),
                )?;
                if snd_pcm_hw_params_set_channels(p, hw, channels) < 0 {
                    let Some(other) = fallback_channels else {
                        return Err(Error {
                            what: "set channels",
                            code: -libc::EINVAL,
                        });
                    };
                    check("set channels", snd_pcm_hw_params_set_channels(p, hw, other))?;
                    got_channels = other;
                }
            }
            let mut got_rate: c_uint = rate;
            let mut got_period: c_ulong = period as c_ulong;
            let mut got_buffer: c_ulong = buffer as c_ulong;
            // SAFETY: as above; the out-parameters are locals.
            unsafe {
                check(
                    "set rate",
                    snd_pcm_hw_params_set_rate_near(p, hw, &raw mut got_rate, ptr::null_mut()),
                )?;
                check(
                    "set period",
                    snd_pcm_hw_params_set_period_size_near(
                        p,
                        hw,
                        &raw mut got_period,
                        ptr::null_mut(),
                    ),
                )?;
                check(
                    "set buffer",
                    snd_pcm_hw_params_set_buffer_size_near(p, hw, &raw mut got_buffer),
                )?;
                check("hw params", snd_pcm_hw_params(p, hw))?;
            }
            if got_rate != rate {
                return Err(Error {
                    what: "rate not available",
                    code: -libc::EINVAL,
                });
            }
            Ok(Opened {
                rate: got_rate,
                channels: got_channels,
                period: got_period as usize,
                buffer: got_buffer as usize,
            })
        }

        /// Start on the second period so the first write never races the
        /// DAC; wake the thread only when a full period fits.
        fn set_sw(&self, period: usize) -> Result<(), Error> {
            // SAFETY: as for the hardware params.
            let mut params = vec![0u8; unsafe { snd_pcm_sw_params_sizeof() }];
            let sw = params.as_mut_ptr().cast::<snd_pcm_sw_params_t>();
            let p = self.pcm.as_ptr();
            let period = period as c_ulong;
            // SAFETY: p is open, sw a valid params buffer.
            unsafe {
                check("sw params current", snd_pcm_sw_params_current(p, sw))?;
                check(
                    "start threshold",
                    snd_pcm_sw_params_set_start_threshold(p, sw, 2 * period),
                )?;
                check("avail min", snd_pcm_sw_params_set_avail_min(p, sw, period))?;
                check("sw params", snd_pcm_sw_params(p, sw))?;
            }
            Ok(())
        }

        /// Blocking playback: S16, `channels` interleaved, `period` frames,
        /// `buffer` frames of ring.
        pub fn open_playback(
            device: &str,
            rate: u32,
            channels: u32,
            period: usize,
            buffer: usize,
        ) -> Result<(Pcm, Opened), Error> {
            Self::open(
                device,
                SND_PCM_STREAM_PLAYBACK,
                rate,
                channels,
                None,
                period,
                buffer,
            )
        }

        /// Blocking capture: mono S16 when the hardware allows it, else
        /// stereo (`Opened::channels` says which).
        pub fn open_capture(
            device: &str,
            rate: u32,
            period: usize,
            buffer: usize,
        ) -> Result<(Pcm, Opened), Error> {
            Self::open(
                device,
                SND_PCM_STREAM_CAPTURE,
                rate,
                1,
                Some(2),
                period,
                buffer,
            )
        }

        /// Write `frames` frames from `buf` (which holds at least
        /// `frames * channels` samples). Frames written, or the error code.
        pub fn write(&self, buf: &[i16], frames: usize) -> Result<usize, i32> {
            let frames = frames.min(buf.len() / self.channels.max(1) as usize);
            // SAFETY: buf holds frames * channels samples; the stream is
            // interleaved S16 as set at open.
            let n = unsafe {
                snd_pcm_writei(self.pcm.as_ptr(), buf.as_ptr().cast(), frames as c_ulong)
            };
            usize::try_from(n).map_err(|_| n as i32)
        }

        /// Read up to `frames` frames into `buf`. Frames read, or the code.
        pub fn read(&self, buf: &mut [i16], frames: usize) -> Result<usize, i32> {
            let frames = frames.min(buf.len() / self.channels.max(1) as usize);
            // SAFETY: as for write, with buf writable.
            let n = unsafe {
                snd_pcm_readi(
                    self.pcm.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    frames as c_ulong,
                )
            };
            usize::try_from(n).map_err(|_| n as i32)
        }

        /// Try to bring the stream back from `err` (an underrun or a
        /// suspend). The code when alsa-lib could not.
        pub fn recover(&self, err: i32) -> Result<(), i32> {
            // SAFETY: the handle is open.
            let r = unsafe { snd_pcm_recover(self.pcm.as_ptr(), err, 1) };
            if r < 0 { Err(r) } else { Ok(()) }
        }
    }

    impl Drop for Pcm {
        fn drop(&mut self) {
            // SAFETY: open, closed once, here. Drop first: what is queued is
            // not played out, the device is released at once.
            unsafe {
                snd_pcm_drop(self.pcm.as_ptr());
                snd_pcm_close(self.pcm.as_ptr());
            }
        }
    }

    /// The simple-mixer view of a control device.
    pub struct SimpleMixer {
        mixer: NonNull<snd_mixer_t>,
        id: Vec<u8>,
    }

    // SAFETY: used under the volume's mutex, one thread at a time.
    unsafe impl Send for SimpleMixer {}

    impl SimpleMixer {
        pub fn open(device: &str) -> Result<SimpleMixer, Error> {
            let name = cstring("control device", device)?;
            let mut raw: *mut snd_mixer_t = ptr::null_mut();
            // SAFETY: raw receives the handle.
            check("mixer open", unsafe { snd_mixer_open(&raw mut raw, 0) })?;
            let Some(mixer) = NonNull::new(raw) else {
                return Err(Error {
                    what: "mixer open",
                    code: -libc::EINVAL,
                });
            };
            // SAFETY: the id buffer is of the size alsa-lib states.
            let id = vec![0u8; unsafe { snd_mixer_selem_id_sizeof() }];
            let m = SimpleMixer { mixer, id };
            let p = m.mixer.as_ptr();
            // SAFETY: p is open; name terminated. On failure `m` is dropped
            // and closed.
            unsafe {
                check("mixer attach", snd_mixer_attach(p, name.as_ptr()))?;
                check(
                    "mixer register",
                    snd_mixer_selem_register(p, ptr::null_mut(), ptr::null_mut()),
                )?;
                check("mixer load", snd_mixer_load(p))?;
            }
            Ok(m)
        }

        /// Take in changes made by others (alsa-lib skips writing a value
        /// it believes is set already) and controls that came or went.
        pub fn handle_events(&self) -> Result<(), Error> {
            // SAFETY: open.
            check("mixer events", unsafe {
                snd_mixer_handle_events(self.mixer.as_ptr())
            })
        }

        /// The active playback-volume control of that name, if any. Valid
        /// until the next `handle_events`, which may free it.
        pub fn find(&mut self, name: &str) -> Option<Elem<'_>> {
            let cname = cstring("control name", name).ok()?;
            let id = self.id.as_mut_ptr().cast::<snd_mixer_selem_id_t>();
            // SAFETY: id is a valid buffer; the mixer is open.
            let e = unsafe {
                snd_mixer_selem_id_set_index(id, 0);
                snd_mixer_selem_id_set_name(id, cname.as_ptr());
                snd_mixer_find_selem(self.mixer.as_ptr(), id)
            };
            let e = NonNull::new(e)?;
            // SAFETY: e is a live element.
            let usable = unsafe {
                snd_mixer_selem_is_active(e.as_ptr()) != 0
                    && snd_mixer_selem_has_playback_volume(e.as_ptr()) != 0
            };
            usable.then_some(Elem {
                e,
                _m: std::marker::PhantomData,
            })
        }
    }

    impl Drop for SimpleMixer {
        fn drop(&mut self) {
            // SAFETY: open, closed once.
            unsafe { snd_mixer_close(self.mixer.as_ptr()) };
        }
    }

    /// One simple-mixer control, borrowed from its mixer.
    pub struct Elem<'m> {
        e: NonNull<snd_mixer_elem_t>,
        _m: std::marker::PhantomData<&'m mut SimpleMixer>,
    }

    impl Elem<'_> {
        pub fn name(&self) -> String {
            // SAFETY: a live element; the name is its own storage.
            let s = unsafe { snd_mixer_selem_get_name(self.e.as_ptr()) };
            if s.is_null() {
                return String::new();
            }
            // SAFETY: non-null and terminated.
            unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
        }

        pub fn has_switch(&self) -> bool {
            // SAFETY: a live element.
            unsafe { snd_mixer_selem_has_playback_switch(self.e.as_ptr()) != 0 }
        }

        fn range(
            &self,
            f: unsafe extern "C" fn(*mut snd_mixer_elem_t, *mut c_long, *mut c_long) -> c_int,
        ) -> Option<(i64, i64)> {
            let (mut min, mut max): (c_long, c_long) = (0, 0);
            // SAFETY: a live element; min and max are locals.
            let r = unsafe { f(self.e.as_ptr(), &raw mut min, &raw mut max) };
            (r >= 0).then_some((widen(min), widen(max)))
        }

        fn get(
            &self,
            f: unsafe extern "C" fn(*mut snd_mixer_elem_t, c_int, *mut c_long) -> c_int,
        ) -> Option<i64> {
            let mut v: c_long = 0;
            // SAFETY: as above.
            let r = unsafe { f(self.e.as_ptr(), SND_MIXER_SCHN_FRONT_LEFT, &raw mut v) };
            (r >= 0).then_some(widen(v))
        }

        pub fn volume_range(&self) -> Option<(i64, i64)> {
            self.range(snd_mixer_selem_get_playback_volume_range)
        }

        /// In 0.01 dB.
        pub fn db_range(&self) -> Option<(i64, i64)> {
            self.range(snd_mixer_selem_get_playback_dB_range)
        }

        pub fn volume(&self) -> Option<i64> {
            self.get(snd_mixer_selem_get_playback_volume)
        }

        pub fn db(&self) -> Option<i64> {
            self.get(snd_mixer_selem_get_playback_dB)
        }

        /// The dB value of a raw step.
        pub fn db_of_volume(&self, value: i64) -> Option<i64> {
            let mut db: c_long = 0;
            // SAFETY: as above.
            let r = unsafe {
                snd_mixer_selem_ask_playback_vol_dB(self.e.as_ptr(), value as c_long, &raw mut db)
            };
            (r >= 0).then_some(widen(db))
        }

        pub fn set_volume(&self, value: i64) -> bool {
            // SAFETY: a live element.
            unsafe {
                snd_mixer_selem_set_playback_volume_all(self.e.as_ptr(), value as c_long) >= 0
            }
        }

        /// Set a dB value, rounding up to the next step the control has.
        pub fn set_db(&self, value: i64) -> bool {
            // SAFETY: a live element.
            unsafe { snd_mixer_selem_set_playback_dB_all(self.e.as_ptr(), value as c_long, 1) >= 0 }
        }

        pub fn set_switch(&self, on: bool) -> bool {
            // SAFETY: a live element.
            unsafe {
                snd_mixer_selem_set_playback_switch_all(self.e.as_ptr(), c_int::from(on)) >= 0
            }
        }
    }
}

#[cfg(lehu_audio_no_alsa)]
mod stub {
    use super::{Error, Opened};

    const UNAVAILABLE: Error = Error {
        what: "built without ALSA",
        code: -libc::ENOSYS,
    };

    pub fn strerror(err: i32) -> String {
        format!("error {err}")
    }

    /// Never opens: there is no alsa-lib on this host.
    pub struct Pcm {
        pub channels: u32,
    }

    impl Pcm {
        pub fn open_playback(
            _device: &str,
            _rate: u32,
            _channels: u32,
            _period: usize,
            _buffer: usize,
        ) -> Result<(Pcm, Opened), Error> {
            Err(UNAVAILABLE)
        }

        pub fn open_capture(
            _device: &str,
            _rate: u32,
            _period: usize,
            _buffer: usize,
        ) -> Result<(Pcm, Opened), Error> {
            Err(UNAVAILABLE)
        }

        pub fn write(&self, _buf: &[i16], _frames: usize) -> Result<usize, i32> {
            Err(-libc::ENOSYS)
        }

        pub fn read(&self, _buf: &mut [i16], _frames: usize) -> Result<usize, i32> {
            Err(-libc::ENOSYS)
        }

        pub fn recover(&self, err: i32) -> Result<(), i32> {
            Err(err)
        }
    }

    pub struct SimpleMixer(());

    impl SimpleMixer {
        pub fn open(_device: &str) -> Result<SimpleMixer, Error> {
            Err(UNAVAILABLE)
        }

        pub fn handle_events(&self) -> Result<(), Error> {
            Err(UNAVAILABLE)
        }

        pub fn find(&mut self, _name: &str) -> Option<Elem<'_>> {
            None
        }
    }

    pub struct Elem<'m>(std::marker::PhantomData<&'m mut SimpleMixer>);

    impl Elem<'_> {
        pub fn name(&self) -> String {
            String::new()
        }
        pub fn has_switch(&self) -> bool {
            false
        }
        pub fn volume_range(&self) -> Option<(i64, i64)> {
            None
        }
        pub fn db_range(&self) -> Option<(i64, i64)> {
            None
        }
        pub fn volume(&self) -> Option<i64> {
            None
        }
        pub fn db(&self) -> Option<i64> {
            None
        }
        pub fn db_of_volume(&self, _value: i64) -> Option<i64> {
            None
        }
        pub fn set_volume(&self, _value: i64) -> bool {
            false
        }
        pub fn set_db(&self, _value: i64) -> bool {
            false
        }
        pub fn set_switch(&self, _on: bool) -> bool {
            false
        }
    }
}

#[cfg(not(lehu_audio_no_alsa))]
pub use real::{Elem, Pcm, SimpleMixer, strerror};
#[cfg(lehu_audio_no_alsa)]
pub use stub::{Elem, Pcm, SimpleMixer, strerror};
