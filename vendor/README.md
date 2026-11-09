# Vendored crates

`fontique` 0.11.1 (Apache-2.0 OR MIT), as published, except for two
things. Three lines in `src/collection/mod.rs`: the collection's version
counter is a `u64`, and the ARM926 (ARMv5TE) has no 64-bit atomics, so on
such targets fontique's own `AtomicCounter` is an `AtomicU32` and the
comparisons cast. The fix is LEHU's author's, made first for LIVI's
F1C200s branch (`native/vendor/fontique`) and for the Buildroot
`slint-cpp` package, which carries the same patch.
And in `src/backend/fontconfig.rs`, a generic family fontconfig cannot
match is left empty instead of unwrapping `NoMatch`: a board with
fontconfig but no fonts panicked at start, and LEHU draws with the glyphs
embedded at build time anyway. `Cargo.toml` points `[patch.crates-io]
fontique` here. Drop the directory once upstream builds for targets
without 64-bit atomics and handles a fontconfig with no fonts.

`parley` 0.11.1 (Apache-2.0 OR MIT), as published, except for
`src/analysis/mod.rs`: Slint turns on parley's `complex-scripts` feature,
which selects the dictionary word and line segmenters, and their tables
(`icu_segmenter_data`: Thai, Khmer, Lao, Burmese and CJK dictionaries) are
3 MB of read-only data in a binary that is RAM on this board. The vendored
copy always uses the non-complex segmenters, so text in those scripts
breaks between characters rather than words; the screens show Latin text
and phone names. Drop the directory if Slint ever leaves the feature to
the application. (`#![allow(deprecated)]` at the top of its lib.rs keeps
an icu4x deprecation out of every build's output.)
