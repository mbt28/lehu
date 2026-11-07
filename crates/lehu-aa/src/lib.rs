//! Android Auto, wired and wireless: the accessory switch and the GAL
//! session over the accessory's bulk pipe, or the Bluetooth handoff and the
//! same session over the phone's TCP connection, shown to the main loop as
//! a [`lehu_session::Session`].
//!
//! The protocol is FastCarPlay's native backend ported as it is: the
//! version handshake, TLS 1.2 inside frames (OpenSSL over memory, ChaCha20
//! when forced), service discovery with the head unit's capabilities, the
//! channels for H.264 video, PCM audio (media, guidance, system), the
//! microphone, touch, keys and the rotary, the sensors (driving status,
//! night mode), the three focuses, ping and ByeBye. Threads and poll, no
//! runtime; nothing allocated per frame on the receive path.
//!
//! # What the main loop does
//!
//! - **Attach.** For a USB device that appeared and is neither a hub nor
//!   Apple's (`lehu_usb::Role::Other`), and that `Config::accepts`, call
//!   [`switch_to_accessory`]. `Switched` means the phone is re-enumerating:
//!   nothing more to do with that device. `Skipped` and `NotSupported` mean
//!   it is not a phone. Never switch while a wireless Android Auto session
//!   runs: the phone runs one session, and the switch would fight it (the
//!   combined-mode plan ends the wireless session first).
//! - **Start.** When a device of `Role::Accessory` appears, call
//!   [`Session::start`] with it and adopt the session. Its phases: Spawned
//!   at start; Identified at the phone's discovery request (its name);
//!   `VideoCodec` then Ready at the video setup, which is when the decoder
//!   is started; Live at the video start; Backgrounded while the phone's
//!   video is stopped; Closing on either side's goodbye; Closed when the
//!   link is down. After Closed the USB transport has reset the device,
//!   so it re-enumerates: the Detached of the accessory and the Attached
//!   of the plain phone that follow are ours. A phone that ended the
//!   session itself (`Fault::PhoneLeft`) or lost the link is switched
//!   again when it reappears; after a `Cmd::Close` of the loop's own, do
//!   not switch it until it is unplugged and plugged again.
//! - **Left-over accessory.** An accessory present at program start was
//!   left by an earlier run and may hold a half-open pipe: reset it once
//!   (`lehu_usb::reset`) and start on its re-enumeration; never reset one
//!   that appeared after start.
//! - **Audio.** `Sinks::audio_pcm` gets the PCM as it comes, per kind;
//!   `Event::Audio(kind, on)` brackets each stream.
//! - **Microphone.** `Sinks::mic(Some(..))` asks for 16 kHz mono PCM in
//!   20 ms frames in `ports.mic`; `Sinks::mic(None)` stops it.
//! - **Screen.** `Event::HostUi` is the phone's exit button;
//!   `Event::VideoFocus(true)` the phone wanting the screen back.
//!   `Cmd::VideoActive(false)` tells the phone the head unit took the
//!   screen (its video stops), `Cmd::VideoActive(true)` and `Cmd::Resume`
//!   give it back: the phone restarts its stream on the parameter sets
//!   and a keyframe, which is the only keyframe request Android Auto has.
//!   `Cmd::Keyframe` does nothing unless `Config::keyframe_by_focus`,
//!   which restarts the stream the same way; with it off the loop should
//!   give the decoder no keyframe request for this session.
//! - **Sensors.** `Cmd::Night` sends the night mode; Auto is day, as at
//!   connect, since there is no light sensor. The driving status is
//!   always unrestricted.
//! - **Input.** `Cmd::Touch` is scaled from the panel's `TOUCH_SCALE`
//!   units to the phone's resolution (800x480 here); `Cmd::Key` sends a
//!   full press on its down; `Cmd::Rotary` turns the rotary controller.
//!   Up and Down turn it as well; Left and Right nudge between areas.
//! - **Calls** are not carried: this head unit offers no telephony stream
//!   (calls stay on Bluetooth), so `Event::Call` never comes.
//! - **Now playing and turn-by-turn.** The media playback status and the
//!   navigation status channels land in `ports.now_playing` and
//!   `ports.guidance`, each change named by `Event::NowPlaying` or
//!   `Event::Guidance` (`status`). Transport keys are `Cmd::Key`.
//!
//! # Wireless
//!
//! [`Session::start_wireless`] is the session that waits for phones: it
//! registers the Android Auto service with the Bluetooth manager (channel
//! 8, the UUID phones look for), binds TCP 5277 and hands every phone that
//! connects the service the access point's credentials and that endpoint
//! (`aaw`, `handoff`). The phone joins the access point and connects; from
//! there the GAL session runs over the TCP connection exactly as over
//! USB, with the same phases. The loop keeps one such session waiting
//! whenever no phone is on the cable, starts it again after it closed
//! (it never starts over by itself), and ends it when a cable comes: a
//! cable always comes first, and a phone that runs a wireless session is
//! never switched to accessory mode under it. The session ends with
//! `Fault::LinkLost` when the TCP link breaks (or `Silent` when the pings
//! stop) and when the access point interface loses carrier once a phone
//! was in the session; it needs `ap.valid()` (`Error::NoAccessPoint`), the
//! Bluetooth manager running (`Error::Bluetooth`) and the port free
//! (`Error::Bind`).

pub mod aaw;
pub mod aoap;
pub mod cert;
pub mod consts;
pub mod frame;
pub mod handoff;
pub mod machine;
pub mod proto;
pub mod ring;
pub mod session;
pub mod status;
pub mod tcp;
pub mod tls;
pub mod transport;
mod wireless;

pub use aoap::{Switch, switch_to_accessory};
pub use machine::{Config, NativeAnswer};
pub use session::{Error, Session};
pub use transport::{StreamTransport, Transport, UsbTransport, UsbTuning};
