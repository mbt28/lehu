//! Which codec each phone is asked for: the probe's answer, with
//! `display.codec` as an override the hardware must be able to honour.

use lehu_core::settings::VideoCodec;

use crate::probe::{Caps, Codec};

/// The forced codec when the hardware decodes it; None for auto, and for
/// a forced codec it cannot decode, which is refused with a warning.
fn forced(caps: &Caps, wanted: VideoCodec, phone: &str) -> Option<Codec> {
    let codec = match wanted {
        VideoCodec::Auto => return None,
        VideoCodec::H264 => Codec::H264,
        VideoCodec::Hevc => Codec::Hevc,
    };
    if caps.has(codec) {
        return Some(codec);
    }
    log::warn!(
        "video: display.codec = {codec}, but nothing here decodes {codec}; {phone} uses auto"
    );
    None
}

/// CarPlay streams one codec: HEVC where it decodes, else H.264, also
/// when nothing decodes (the session then runs without its picture).
pub fn carplay_codec(caps: &Caps, wanted: VideoCodec) -> Codec {
    if let Some(c) = forced(caps, wanted, "carplay") {
        return c;
    }
    if caps.has(Codec::Hevc) {
        Codec::Hevc
    } else {
        Codec::H264
    }
}

/// Android Auto is offered a list, one video configuration each, and the
/// phone picks: the codecs that decode, H.264 first. Where H.264 does not
/// decode it still goes last, as every phone encodes it: a phone without
/// the other codec then keeps its session, without the picture, as when
/// nothing decodes at all.
pub fn android_codecs(caps: &Caps, wanted: VideoCodec) -> Vec<Codec> {
    if let Some(c) = forced(caps, wanted, "android auto") {
        return vec![c];
    }
    let mut list: Vec<Codec> = [Codec::H264, Codec::Hevc]
        .into_iter()
        .filter(|&c| caps.has(c))
        .collect();
    if !list.contains(&Codec::H264) {
        list.push(Codec::H264);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Ffmpeg, Format, Hardware};

    /// A stateless decoder: its format, driver and largest size.
    type Decoder<'a> = (Format, &'a str, Option<(u32, u32)>);

    /// A board with these decoders and a complete ffmpeg.
    fn board(decoders: &[Decoder<'_>]) -> Caps {
        let mut caps = Caps::none();
        for &(format, driver, max) in decoders {
            caps.add_hardware(
                format,
                Hardware {
                    node: "/dev/video0".into(),
                    driver: driver.into(),
                    stateless: true,
                    max,
                },
            );
        }
        for format in Format::ALL {
            caps.set_ffmpeg(format, Ffmpeg::all());
        }
        caps
    }

    #[test]
    fn the_f1c_gets_h264_everywhere() {
        let f1c = board(&[(Format::H264, "cedrus", Some((1920, 1088)))]);
        assert_eq!(carplay_codec(&f1c, VideoCodec::Auto), Codec::H264);
        assert_eq!(android_codecs(&f1c, VideoCodec::Auto), [Codec::H264]);
        // HEVC forced: refused, auto instead.
        assert_eq!(carplay_codec(&f1c, VideoCodec::Hevc), Codec::H264);
        assert_eq!(android_codecs(&f1c, VideoCodec::Hevc), [Codec::H264]);
        assert_eq!(android_codecs(&f1c, VideoCodec::H264), [Codec::H264]);
    }

    #[test]
    fn an_hevc_only_board_offers_hevc_and_keeps_h264_last() {
        let pi5 = board(&[(Format::Hevc, "rpi-hevc-dec", Some((4096, 4096)))]);
        assert_eq!(carplay_codec(&pi5, VideoCodec::Auto), Codec::Hevc);
        assert_eq!(
            android_codecs(&pi5, VideoCodec::Auto),
            [Codec::Hevc, Codec::H264]
        );
        // H.264 forced cannot decode: auto.
        assert_eq!(carplay_codec(&pi5, VideoCodec::H264), Codec::Hevc);
        assert_eq!(android_codecs(&pi5, VideoCodec::Hevc), [Codec::Hevc]);
    }

    #[test]
    fn both_decode_h264_leads_for_android_and_hevc_for_carplay() {
        let both = board(&[
            (Format::H264, "hantro-vpu", None),
            (Format::Hevc, "rkvdec", None),
            (Format::Vp9, "rkvdec", None),
        ]);
        assert_eq!(carplay_codec(&both, VideoCodec::Auto), Codec::Hevc);
        assert_eq!(carplay_codec(&both, VideoCodec::H264), Codec::H264);
        assert_eq!(
            android_codecs(&both, VideoCodec::Auto),
            [Codec::H264, Codec::Hevc]
        );
        assert_eq!(android_codecs(&both, VideoCodec::H264), [Codec::H264]);
    }

    #[test]
    fn nothing_decodes_h264_is_still_asked_for() {
        let none = Caps::none();
        assert_eq!(carplay_codec(&none, VideoCodec::Auto), Codec::H264);
        assert_eq!(carplay_codec(&none, VideoCodec::Hevc), Codec::H264);
        assert_eq!(android_codecs(&none, VideoCodec::Auto), [Codec::H264]);
    }
}
