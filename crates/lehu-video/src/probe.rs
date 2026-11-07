//! The system's video decoders, probed once at start. Each V4L2
//! memory-to-memory node is asked which coded formats its OUTPUT queue
//! takes and how large a picture it decodes; libavcodec is asked whether it
//! has the decoder, the parser and the v4l2-request hwaccel for each. That
//! tells boards apart (the F1C200s' cedrus takes H.264 slices only, a
//! Pi 5's decoder HEVC only), which ffmpeg's tables alone cannot: they say
//! what ffmpeg was built with, not what the kernel drives.
//!
//! A format counts only when LEHU's own path decodes it end to end: a
//! stateless decoder, ffmpeg with all three pieces, and a codec the
//! decoder takes (H.264, HEVC). There is no software path: the pictures go
//! to the video plane as the decoder's dma-bufs, and a picture in system
//! memory would need a copy per frame that this CPU cannot afford. VP9 and
//! AV1 hardware is reported, never offered.
//!
//! The probe opens each node read-only and closes it again. It allocates
//! no buffers and starts no stream: the F1C200s' CMA holds one decoder
//! pool, and that belongs to the session.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

/// The codecs LEHU's decoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }

    pub fn is_hevc(self) -> bool {
        self == Codec::Hevc
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A coded format the phones send, as a V4L2 decoder names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    H264,
    Hevc,
    Vp9,
    Av1,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::H264, Format::Hevc, Format::Vp9, Format::Av1];

    pub fn name(self) -> &'static str {
        match self {
            Format::H264 => "h264",
            Format::Hevc => "hevc",
            Format::Vp9 => "vp9",
            Format::Av1 => "av1",
        }
    }

    /// The codec LEHU's decoder takes it as; None for those it does not.
    pub fn codec(self) -> Option<Codec> {
        match self {
            Format::H264 => Some(Codec::H264),
            Format::Hevc => Some(Codec::Hevc),
            Format::Vp9 | Format::Av1 => None,
        }
    }

    /// Its place in `ALL`, which is also the helper's index.
    fn index(self) -> usize {
        match self {
            Format::H264 => 0,
            Format::Hevc => 1,
            Format::Vp9 => 2,
            Format::Av1 => 3,
        }
    }

    /// The format behind an OUTPUT-queue fourcc, and whether the decoder
    /// takes it parsed (stateless) rather than as the bitstream.
    fn of_fourcc(fcc: u32) -> Option<(Format, bool)> {
        Some(match fcc {
            FCC_H264_SLICE => (Format::H264, true),
            FCC_HEVC_SLICE => (Format::Hevc, true),
            FCC_VP9_FRAME => (Format::Vp9, true),
            FCC_AV1_FRAME => (Format::Av1, true),
            FCC_H264 => (Format::H264, false),
            FCC_HEVC => (Format::Hevc, false),
            FCC_VP9 => (Format::Vp9, false),
            _ => return None,
        })
    }
}

impl From<Codec> for Format {
    fn from(c: Codec) -> Format {
        match c {
            Codec::H264 => Format::H264,
            Codec::Hevc => Format::Hevc,
        }
    }
}

/// One V4L2 decoder's word on one format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hardware {
    pub node: String,
    pub driver: String,
    /// Takes parsed slices or frames (the request API) rather than the
    /// bitstream: what ffmpeg's v4l2-request hwaccel drives.
    pub stateless: bool,
    /// The largest coded size, None when the driver does not say.
    pub max: Option<(u32, u32)>,
}

/// What libavcodec has for a format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ffmpeg {
    pub decoder: bool,
    /// The hwaccel takes the slice parameters from the parser.
    pub parser: bool,
    /// A DRM-typed hwaccel on the decoder: v4l2-request.
    pub hwaccel: bool,
}

impl Ffmpeg {
    pub fn all() -> Ffmpeg {
        Ffmpeg {
            decoder: true,
            parser: true,
            hwaccel: true,
        }
    }
}

/// Everything known about one format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Support {
    pub format: Format,
    /// Every decoder node that takes it, in /dev order.
    pub hardware: Vec<Hardware>,
    pub ffmpeg: Ffmpeg,
}

impl Support {
    /// The decoder LEHU drives for this format, or why there is none.
    pub fn decoder(&self) -> Result<&Hardware, &'static str> {
        if self.hardware.is_empty() {
            return Err("no hardware decoder");
        }
        let Some(hw) = self.hardware.iter().find(|h| h.stateless) else {
            return Err("a stateful decoder only; LEHU drives stateless ones");
        };
        if self.format.codec().is_none() {
            return Err("LEHU's decoder does not take it");
        }
        if !self.ffmpeg.decoder {
            return Err("ffmpeg was built without its decoder");
        }
        if !self.ffmpeg.parser {
            return Err("ffmpeg was built without its parser");
        }
        if !self.ffmpeg.hwaccel {
            return Err("ffmpeg has no v4l2-request hwaccel for it");
        }
        Ok(hw)
    }
}

/// What the system decodes, per format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    formats: [Support; 4],
}

impl Default for Caps {
    fn default() -> Self {
        Self::none()
    }
}

/// Decoders work on whole macroblocks: 1080 lines are 1088 coded.
fn coded(px: u32) -> u32 {
    px.saturating_add(15) & !15
}

fn size(max: Option<(u32, u32)>) -> String {
    max.map_or("any size".into(), |(w, h)| format!("up to {w}x{h}"))
}

impl Caps {
    /// Nothing decodes: where a probe starts, and a host without V4L2.
    pub fn none() -> Self {
        Self {
            formats: Format::ALL.map(|format| Support {
                format,
                hardware: Vec::new(),
                ffmpeg: Ffmpeg::default(),
            }),
        }
    }

    pub fn support(&self, format: Format) -> &Support {
        &self.formats[format.index()]
    }

    pub fn add_hardware(&mut self, format: Format, hw: Hardware) {
        self.formats[format.index()].hardware.push(hw);
    }

    pub fn set_ffmpeg(&mut self, format: Format, ffmpeg: Ffmpeg) {
        self.formats[format.index()].ffmpeg = ffmpeg;
    }

    /// LEHU decodes this codec in hardware.
    pub fn has(&self, codec: Codec) -> bool {
        self.support(codec.into()).decoder().is_ok()
    }

    /// The largest coded size the codec's decoder takes; None when there
    /// is no decoder or it does not say.
    pub fn max(&self, codec: Codec) -> Option<(u32, u32)> {
        self.support(codec.into()).decoder().ok()?.max
    }

    /// The codec's decoder takes a picture this size.
    pub fn fits(&self, codec: Codec, width: u32, height: u32) -> bool {
        match self.support(codec.into()).decoder() {
            Ok(hw) => hw
                .max
                .is_none_or(|(w, h)| coded(width) <= w && coded(height) <= h),
            Err(_) => false,
        }
    }

    /// The driver behind the codec's decoder, for the log.
    pub fn driver(&self, codec: Codec) -> Option<&str> {
        let hw = self.support(codec.into()).decoder().ok()?;
        Some(&hw.driver)
    }

    /// One line for the log: the codecs LEHU decodes with their sizes,
    /// and hardware it leaves unused.
    pub fn summary(&self) -> String {
        let mut line = String::from("video: decoders:");
        for s in &self.formats {
            let used = s.decoder();
            let sep = if line.ends_with(':') { " " } else { ", " };
            match (s.format.codec(), used) {
                (Some(_), Ok(hw)) => {
                    let _ = write!(
                        line,
                        "{sep}{} {} ({})",
                        s.format.name(),
                        size(hw.max),
                        hw.driver
                    );
                }
                (Some(_), Err(_)) if s.hardware.is_empty() => {
                    let _ = write!(line, "{sep}{} none", s.format.name());
                }
                (_, Err(why)) if !s.hardware.is_empty() => {
                    let _ = write!(
                        line,
                        "{sep}{} unused on {} ({why})",
                        s.format.name(),
                        s.hardware[0].driver
                    );
                }
                _ => {}
            }
        }
        line
    }

    /// The full report for `lehu --decoders`.
    pub fn report(&self) -> String {
        let yn = |b: bool| if b { "yes" } else { "no" };
        let mut out = String::new();
        for s in &self.formats {
            let _ = writeln!(
                out,
                "{}: {}",
                s.format.name(),
                match s.decoder() {
                    Ok(hw) => format!("decodes {} on {} ({})", size(hw.max), hw.driver, hw.node),
                    Err(why) => format!("not used: {why}"),
                }
            );
            for hw in &s.hardware {
                let _ = writeln!(
                    out,
                    "  {} {}: {}, {}",
                    hw.node,
                    hw.driver,
                    if hw.stateless {
                        "stateless"
                    } else {
                        "stateful"
                    },
                    size(hw.max)
                );
            }
            let _ = writeln!(
                out,
                "  ffmpeg: decoder {}, parser {}, v4l2-request {}",
                yn(s.ffmpeg.decoder),
                yn(s.ffmpeg.parser),
                yn(s.ffmpeg.hwaccel)
            );
        }
        out
    }
}

// Spelled out: the libc crate has no V4L2. The ioctl numbers are built
// from the structs' sizes (the asm-generic encoding every architecture
// this runs on uses), so v4l2_format's pointer-sized alignment comes out
// right on 32 and 64 bits alike.
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}
const FCC_H264_SLICE: u32 = fourcc(b'S', b'2', b'6', b'4');
const FCC_HEVC_SLICE: u32 = fourcc(b'S', b'2', b'6', b'5');
const FCC_VP9_FRAME: u32 = fourcc(b'V', b'P', b'9', b'F');
const FCC_AV1_FRAME: u32 = fourcc(b'A', b'V', b'1', b'F');
const FCC_H264: u32 = fourcc(b'H', b'2', b'6', b'4');
const FCC_HEVC: u32 = fourcc(b'H', b'E', b'V', b'C');
const FCC_VP9: u32 = fourcc(b'V', b'P', b'9', b'0');

const fn iowr(nr: u32, size: usize) -> libc::c_ulong {
    (3 << 30 | (size as u32) << 16 | (b'V' as u32) << 8 | nr) as libc::c_ulong
}
const VIDIOC_QUERYCAP: libc::c_ulong =
    (2 << 30 | (size_of::<Capability>() as u32) << 16 | (b'V' as u32) << 8) as libc::c_ulong;
const VIDIOC_ENUM_FMT: libc::c_ulong = iowr(2, size_of::<FmtDesc>());
const VIDIOC_TRY_FMT: libc::c_ulong = iowr(64, size_of::<V4l2Format>());
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = iowr(74, size_of::<FrmSizeEnum>());
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_8000;
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
/// What TRY_FMT is asked for: a driver clamps it to its largest.
const ASK: u32 = 16384;

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// v4l2_frmsizeenum; the union holds the discrete size in its first two
/// words or the stepwise range in all six.
#[repr(C)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    /// min_width, max_width, step_width, min_height, max_height,
    /// step_height; a discrete size is width, height.
    size: [u32; 6],
    reserved: [u32; 2],
}

/// v4l2_format: the queue type, then a 200-byte union whose members hold
/// pointers, so it is pointer-aligned. Width, height and the fourcc come
/// first in both the single- and the multi-planar format.
#[repr(C)]
struct V4l2Format {
    kind: u32,
    fmt: [usize; 200 / size_of::<usize>()],
}

/// The largest coded size `fcc` decodes at: the driver's frame sizes when
/// it lists them, else the size TRY_FMT clamps an oversized request to.
/// Neither allocates or changes the node's state.
fn max_size(fd: libc::c_int, kind: u32, fcc: u32) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for index in 0..64u32 {
        // SAFETY: a zeroed struct of the size the ioctl reads and writes,
        // on our own fd.
        let e = unsafe {
            let mut e: FrmSizeEnum = std::mem::zeroed();
            e.index = index;
            e.pixel_format = fcc;
            if libc::ioctl(fd, VIDIOC_ENUM_FRAMESIZES as _, &raw mut e) != 0 {
                break;
            }
            e
        };
        let (w, h) = if e.kind == V4L2_FRMSIZE_TYPE_DISCRETE {
            (e.size[0], e.size[1])
        } else {
            // Stepwise and continuous give one range: done.
            let s = (e.size[1], e.size[4]);
            if s.0 > 0 && s.1 > 0 {
                return Some(s);
            }
            break;
        };
        if w > 0
            && h > 0
            && best
                .is_none_or(|(bw, bh)| u64::from(w) * u64::from(h) > u64::from(bw) * u64::from(bh))
        {
            best = Some((w, h));
        }
    }
    if best.is_some() {
        return best;
    }
    // SAFETY: as above; TRY_FMT only negotiates, it changes nothing.
    let f = unsafe {
        let mut f: V4l2Format = std::mem::zeroed();
        f.kind = kind;
        let words = (&raw mut f.fmt).cast::<u32>();
        words.write(ASK);
        words.add(1).write(ASK);
        words.add(2).write(fcc);
        if libc::ioctl(fd, VIDIOC_TRY_FMT as _, &raw mut f) != 0 {
            return None;
        }
        f
    };
    // SAFETY: the union's first two words, written by the driver.
    let (w, h) = unsafe {
        let words = (&raw const f.fmt).cast::<u32>();
        (words.read(), words.add(1).read())
    };
    // A driver that hands the request back unclamped said nothing.
    (w > 0 && h > 0 && w < ASK && h < ASK).then_some((w, h))
}

/// The SoC's own ceiling where the driver reports a generic one: cedrus
/// clamps every Allwinner VE to 4096x2304, the F1C100s/F1C200s VE decodes
/// H.264 up to 1080p (its data sheet).
fn soc_ceiling(driver: &str, compatible: &[u8]) -> Option<(u32, u32)> {
    let suniv = compatible
        .split(|&b| b == 0)
        .any(|c| c.starts_with(b"allwinner,suniv-"));
    (driver == "cedrus" && suniv).then_some((1920, 1088))
}

/// The formats `path` decodes, empty when it is not a decoder (a camera,
/// an ISP) or cannot be opened.
fn query(path: &str, compatible: &[u8]) -> Vec<(Format, Hardware)> {
    let mut found = Vec::new();
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    else {
        return found;
    };
    let fd = file.as_raw_fd();
    // SAFETY: a zeroed struct of the size the ioctl writes, on our own fd.
    let cap = unsafe {
        let mut cap: Capability = std::mem::zeroed();
        if libc::ioctl(fd, VIDIOC_QUERYCAP as _, &raw mut cap) != 0 {
            return found;
        }
        cap
    };
    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    // Only memory-to-memory devices decode.
    if caps & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE) == 0 {
        return found;
    }
    let end = cap.driver.iter().position(|&b| b == 0).unwrap_or(16);
    let driver = String::from_utf8_lossy(&cap.driver[..end]).into_owned();
    let ceiling = soc_ceiling(&driver, compatible);
    for kind in [
        V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
        V4L2_BUF_TYPE_VIDEO_OUTPUT,
    ] {
        for index in 0..64u32 {
            // SAFETY: as above; the ioctl reads index and kind, writes the rest.
            let fmt = unsafe {
                let mut fmt: FmtDesc = std::mem::zeroed();
                fmt.index = index;
                fmt.kind = kind;
                if libc::ioctl(fd, VIDIOC_ENUM_FMT as _, &raw mut fmt) != 0 {
                    break;
                }
                fmt
            };
            let Some((format, stateless)) = Format::of_fourcc(fmt.pixelformat) else {
                continue;
            };
            let max = match (max_size(fd, kind, fmt.pixelformat), ceiling) {
                (Some((w, h)), Some((cw, ch))) => Some((w.min(cw), h.min(ch))),
                (max, None) => max,
                (None, ceiling) => ceiling,
            };
            found.push((
                format,
                Hardware {
                    node: path.to_string(),
                    driver: driver.clone(),
                    stateless,
                    max,
                },
            ));
        }
    }
    found
}

fn probe() -> Caps {
    let mut caps = Caps::none();
    for format in Format::ALL {
        caps.set_ffmpeg(format, crate::ffi::support(format.index()));
    }
    let Ok(dir) = std::fs::read_dir("/dev") else {
        return caps;
    };
    let mut names: Vec<String> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("video"))
        .collect();
    // video2 before video10.
    names.sort_by_key(|n| (n.len(), n.clone()));
    let compatible = std::fs::read("/proc/device-tree/compatible").unwrap_or_default();
    for name in names {
        let path = format!("/dev/{name}");
        for (format, hw) in query(&path, &compatible) {
            log::debug!(
                "{path} ({}) takes {} {}, {}",
                hw.driver,
                format.name(),
                if hw.stateless {
                    "parsed"
                } else {
                    "as a bitstream"
                },
                size(hw.max)
            );
            caps.add_hardware(format, hw);
        }
    }
    caps
}

/// What the system decodes; probed once, on the first call, and logged.
pub fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(|| {
        let c = probe();
        if c.has(Codec::H264) || c.has(Codec::Hevc) {
            log::info!("{}", c.summary());
        } else {
            log::warn!("{}; video stays off", c.summary());
        }
        c
    })
}

/// Is there hardware decode for this codec on this board?
pub fn hw_available(codec: Codec) -> bool {
    caps().has(codec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_structs() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<FrmSizeEnum>(), 44);
        // 4 + 200 on 32 bits; the union is 8-aligned on 64.
        assert_eq!(
            size_of::<V4l2Format>(),
            if cfg!(target_pointer_width = "64") {
                208
            } else {
                204
            }
        );
        // The kernel's values on this host's word size.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        let try_fmt: libc::c_ulong = if cfg!(target_pointer_width = "64") {
            0xc0d0_5640
        } else {
            0xc0cc_5640
        };
        assert_eq!(VIDIOC_TRY_FMT, try_fmt);
    }

    #[test]
    fn fourccs_name_their_formats() {
        assert_eq!(FCC_H264_SLICE, u32::from_le_bytes(*b"S264"));
        assert_eq!(FCC_AV1_FRAME, u32::from_le_bytes(*b"AV1F"));
        assert_eq!(
            Format::of_fourcc(FCC_H264_SLICE),
            Some((Format::H264, true))
        );
        assert_eq!(
            Format::of_fourcc(FCC_HEVC_SLICE),
            Some((Format::Hevc, true))
        );
        assert_eq!(Format::of_fourcc(FCC_VP9_FRAME), Some((Format::Vp9, true)));
        assert_eq!(Format::of_fourcc(FCC_HEVC), Some((Format::Hevc, false)));
        // MPEG-2 slices (cedrus has them too) are no phone's.
        assert_eq!(Format::of_fourcc(u32::from_le_bytes(*b"MG2S")), None);
        for f in Format::ALL {
            assert_eq!(Format::ALL[f.index()], f);
        }
        assert_eq!(Codec::Hevc.name(), "hevc");
        assert!(Codec::Hevc.is_hevc() && !Codec::H264.is_hevc());
    }

    #[test]
    fn the_f1c_ceiling_is_its_data_sheets() {
        let f1c = b"widora,mangopi-r3\0allwinner,suniv-f1c200s\0allwinner,suniv-f1c100s\0";
        assert_eq!(soc_ceiling("cedrus", f1c), Some((1920, 1088)));
        assert_eq!(soc_ceiling("cedrus", b"allwinner,sun50i-h6\0"), None);
        assert_eq!(soc_ceiling("rpi-hevc-dec", f1c), None);
        assert_eq!(soc_ceiling("cedrus", b""), None);
    }

    fn hw(driver: &str, stateless: bool, max: Option<(u32, u32)>) -> Hardware {
        Hardware {
            node: "/dev/video0".into(),
            driver: driver.into(),
            stateless,
            max,
        }
    }

    /// The F1C200s as it should probe: cedrus with H.264 slices, the
    /// image's ffmpeg (h264 and hevc decoders and parsers, v4l2-request).
    fn f1c() -> Caps {
        let mut c = Caps::none();
        c.add_hardware(Format::H264, hw("cedrus", true, Some((1920, 1088))));
        c.set_ffmpeg(Format::H264, Ffmpeg::all());
        c.set_ffmpeg(Format::Hevc, Ffmpeg::all());
        c
    }

    #[test]
    fn the_f1c_decodes_h264_to_1080p_and_nothing_else() {
        let c = f1c();
        assert!(c.has(Codec::H264) && !c.has(Codec::Hevc));
        assert_eq!(c.max(Codec::H264), Some((1920, 1088)));
        assert_eq!(c.driver(Codec::H264), Some("cedrus"));
        assert!(c.fits(Codec::H264, 1920, 1080), "1080 lines are 1088 coded");
        assert!(c.fits(Codec::H264, 800, 480));
        assert!(!c.fits(Codec::H264, 2560, 1440));
        assert!(!c.fits(Codec::Hevc, 800, 480));
        assert_eq!(
            c.summary(),
            "video: decoders: h264 up to 1920x1088 (cedrus), hevc none"
        );
        assert!(c.report().contains("hevc: not used: no hardware decoder"));
    }

    #[test]
    fn what_cannot_be_driven_is_not_counted() {
        // The Pi 5: HEVC slices, but an ffmpeg without the hwaccel.
        let mut c = Caps::none();
        c.add_hardware(Format::Hevc, hw("rpi-hevc-dec", true, Some((4096, 4096))));
        c.set_ffmpeg(
            Format::Hevc,
            Ffmpeg {
                hwaccel: false,
                ..Ffmpeg::all()
            },
        );
        assert!(!c.has(Codec::Hevc));
        assert_eq!(c.max(Codec::Hevc), None);
        c.set_ffmpeg(
            Format::Hevc,
            Ffmpeg {
                parser: false,
                ..Ffmpeg::all()
            },
        );
        assert_eq!(
            c.support(Format::Hevc).decoder(),
            Err("ffmpeg was built without its parser")
        );
        c.set_ffmpeg(Format::Hevc, Ffmpeg::all());
        assert!(c.has(Codec::Hevc) && c.fits(Codec::Hevc, 1920, 1080));
        // A stateful H.264 decoder (bcm2835-codec) is not v4l2-request's.
        c.add_hardware(Format::H264, hw("bcm2835-codec", false, Some((1920, 1088))));
        c.set_ffmpeg(Format::H264, Ffmpeg::all());
        assert!(!c.has(Codec::H264));
        // VP9 hardware is reported and never used.
        c.add_hardware(Format::Vp9, hw("rkvdec", true, None));
        c.set_ffmpeg(Format::Vp9, Ffmpeg::all());
        assert_eq!(
            c.support(Format::Vp9).decoder(),
            Err("LEHU's decoder does not take it")
        );
        assert_eq!(
            c.summary(),
            "video: decoders: h264 unused on bcm2835-codec (a stateful decoder only; LEHU drives stateless ones), \
             hevc up to 4096x4096 (rpi-hevc-dec), vp9 unused on rkvdec (LEHU's decoder does not take it)"
        );
        // A size the driver does not give limits nothing.
        let mut any = Caps::none();
        any.add_hardware(Format::H264, hw("hantro-vpu", true, None));
        any.set_ffmpeg(Format::H264, Ffmpeg::all());
        assert!(any.fits(Codec::H264, 3840, 2160));
        assert_eq!(
            Caps::none().summary(),
            "video: decoders: h264 none, hevc none"
        );
    }

    /// On a host with V4L2 decoders: what the probe finds, printed.
    /// `cargo test -p lehu-video -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn the_probe_on_this_host() {
        let c = probe();
        println!("{}\n{}", c.summary(), c.report());
        // A Raspberry Pi 5 has stateless HEVC on rpi-hevc-dec.
        if std::path::Path::new("/dev/video19").exists() {
            let hevc = c.support(Format::Hevc);
            let hw = hevc.hardware.iter().find(|h| h.driver == "rpi-hevc-dec");
            let hw = hw.expect("rpi-hevc-dec takes HEVC");
            assert!(hw.stateless);
            assert!(hw.max.is_some_and(|(w, h)| w >= 1920 && h >= 1088));
        }
    }
}
