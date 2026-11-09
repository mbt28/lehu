// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Text from outside the UI -- a phone's name in the status line, a Wi-Fi
//! name, what BlueZ reports -- reduced to the glyphs the build embedded.
//!
//! The software renderer has only the glyphs rasterised at build time:
//! printable ASCII plus every character in a `.slint` literal. Anything else
//! draws as nothing (or falls back to a runtime font the board does not
//! have). So before such text is shown, letters with accents lose them
//! (Zoë's iPhone reads Zoe's iPhone, not Zo's), the typographic quotes and
//! dashes phones use become the plain ones, and the rest -- emoji, other
//! scripts, control characters -- is dropped, with the spaces around it
//! collapsed. Everything the UI composes itself is ASCII already.

enum Fold {
    Keep(char),
    Text(&'static str),
    Space,
    Drop,
}

/// `text` reduced to the embedded glyphs, written into `out` (which is
/// cleared first), so a caller that runs every tick allocates nothing.
pub fn sanitize_into(out: &mut String, text: &str) {
    out.clear();
    let mut pending_space = false;
    for c in text.chars() {
        match fold(c) {
            Fold::Drop => {}
            Fold::Space => pending_space = !out.is_empty(),
            Fold::Keep(c) => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(c);
            }
            Fold::Text(s) => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push_str(s);
            }
        }
    }
}

/// `text` reduced to the embedded glyphs, as a new string.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    sanitize_into(&mut out, text);
    out
}

fn fold(c: char) -> Fold {
    match c {
        ' '
        | '\t'
        | '\n'
        | '\r'
        | '\u{A0}'
        | '\u{2000}'..='\u{200A}'
        | '\u{202F}'
        | '\u{205F}'
        | '\u{3000}' => Fold::Space,
        '!'..='~' => Fold::Keep(c),
        // Quotes and dashes as phones write them.
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => Fold::Keep('\''),
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' | '\u{AB}' | '\u{BB}' => Fold::Keep('"'),
        '\u{2010}'..='\u{2015}' | '\u{2212}' => Fold::Keep('-'),
        '\u{2026}' => Fold::Text("..."),
        '\u{D7}' => Fold::Keep('x'),
        // Latin letters with marks, to their base letter.
        'À'..='Å' | 'Ā' | 'Ă' | 'Ą' => Fold::Keep('A'),
        'à'..='å' | 'ā' | 'ă' | 'ą' => Fold::Keep('a'),
        'Æ' => Fold::Text("AE"),
        'æ' => Fold::Text("ae"),
        'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => Fold::Keep('C'),
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => Fold::Keep('c'),
        'Ð' | 'Ď' | 'Đ' => Fold::Keep('D'),
        'ð' | 'ď' | 'đ' => Fold::Keep('d'),
        'È'..='Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => Fold::Keep('E'),
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => Fold::Keep('e'),
        'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => Fold::Keep('G'),
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => Fold::Keep('g'),
        'Ĥ' | 'Ħ' => Fold::Keep('H'),
        'ĥ' | 'ħ' => Fold::Keep('h'),
        'Ì'..='Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' => Fold::Keep('I'),
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => Fold::Keep('i'),
        'Ĵ' => Fold::Keep('J'),
        'ĵ' => Fold::Keep('j'),
        'Ķ' => Fold::Keep('K'),
        'ķ' => Fold::Keep('k'),
        'Ĺ' | 'Ļ' | 'Ľ' | 'Ŀ' | 'Ł' => Fold::Keep('L'),
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => Fold::Keep('l'),
        'Ñ' | 'Ń' | 'Ņ' | 'Ň' => Fold::Keep('N'),
        'ñ' | 'ń' | 'ņ' | 'ň' => Fold::Keep('n'),
        'Ò'..='Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' => Fold::Keep('O'),
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => Fold::Keep('o'),
        'Œ' => Fold::Text("OE"),
        'œ' => Fold::Text("oe"),
        'Ŕ' | 'Ŗ' | 'Ř' => Fold::Keep('R'),
        'ŕ' | 'ŗ' | 'ř' => Fold::Keep('r'),
        'Ś' | 'Ŝ' | 'Ş' | 'Š' | 'Ș' => Fold::Keep('S'),
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => Fold::Keep('s'),
        'ß' => Fold::Text("ss"),
        'Ţ' | 'Ť' | 'Ŧ' | 'Ț' => Fold::Keep('T'),
        'ţ' | 'ť' | 'ŧ' | 'ț' => Fold::Keep('t'),
        'Þ' => Fold::Text("Th"),
        'þ' => Fold::Text("th"),
        'Ù'..='Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => Fold::Keep('U'),
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => Fold::Keep('u'),
        'Ŵ' => Fold::Keep('W'),
        'ŵ' => Fold::Keep('w'),
        'Ý' | 'Ŷ' | 'Ÿ' => Fold::Keep('Y'),
        'ý' | 'ÿ' | 'ŷ' => Fold::Keep('y'),
        'Ź' | 'Ż' | 'Ž' => Fold::Keep('Z'),
        'ź' | 'ż' | 'ž' => Fold::Keep('z'),
        _ => Fold::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_untouched() {
        assert_eq!(sanitize("Alex's iPhone 15"), "Alex's iPhone 15");
        assert_eq!(sanitize("a`b~c"), "a`b~c");
    }

    #[test]
    fn phones_quotes_and_dashes_become_plain() {
        assert_eq!(sanitize("Alex\u{2019}s iPhone"), "Alex's iPhone");
        assert_eq!(sanitize("\u{201C}Car\u{201D} \u{2014} AP"), "\"Car\" - AP");
        assert_eq!(sanitize("wait\u{2026}"), "wait...");
    }

    #[test]
    fn accents_fold_to_base_letters() {
        assert_eq!(sanitize("Zoë"), "Zoe");
        assert_eq!(sanitize("Şebnem Çağrı"), "Sebnem Cagri");
        assert_eq!(sanitize("İstanbul ışık"), "Istanbul isik");
        assert_eq!(sanitize("Straße Œuvre"), "Strasse OEuvre");
    }

    #[test]
    fn the_rest_is_dropped_and_spaces_collapse() {
        assert_eq!(sanitize("\u{1F4F1} iPhone"), "iPhone");
        assert_eq!(sanitize("Pixel \u{1F600} 8"), "Pixel 8");
        assert_eq!(sanitize("  a\t\tb  "), "a b");
        assert_eq!(sanitize("\u{0}\u{7}ctl"), "ctl");
        assert_eq!(sanitize("日本語"), "");
    }

    #[test]
    fn into_reuses_the_buffer() {
        let mut out = String::with_capacity(32);
        let ptr = out.as_ptr();
        sanitize_into(&mut out, "one");
        sanitize_into(&mut out, "two words");
        assert_eq!(out, "two words");
        assert_eq!(out.as_ptr(), ptr);
    }
}
