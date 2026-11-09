# Third-party software in the lehu binary

Written by `scripts/third-party.py` from Cargo.lock; do not edit by hand.

LEHU itself is Copyright (C) 2026 Mehmet Tekbas, GPL-3.0-or-later (LICENSE;
NOTICE names the projects its code descends from). The binary links Slint,
which LEHU takes under the GNU General Public License version 3 only, so a
binary as a whole is distributed under version 3 of that licence. Every other
crate below is under a licence compatible with it; where a crate offers a
choice, the one in the "Taken under" column is the one LEHU takes, and its
text is in the section "Licence texts" with the crate's copyright notice
above, in the table. Procedural macros and build dependencies run at compile
time only and are not listed.

Targets: armv5te-unknown-linux-gnueabi, aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu. Crates: 150.

## Crates

| Crate | Version | Taken under | Offered | Copyright |
|---|---|---|---|---|
| adler2 | 2.0.1 | MIT | 0BSD OR MIT OR Apache-2.0 | Copyright (C) Jonas Schievink <jonasschievink@gmail.com> |
| aead | 0.5.2 | MIT | MIT OR Apache-2.0 | Copyright (c) 2019 The RustCrypto Project Developers; Copyright (c) 2019 MobileCoin, LLC |
| aes | 0.8.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018 Artyom Pavlov |
| allocator-api2 | 0.2.21 | MIT | MIT OR Apache-2.0 | the authors of allocator-api2: Zakarum (no copyright line published) |
| arrayvec | 0.7.8 | MIT | MIT OR Apache-2.0 | Copyright (c) Ulrik Sverdrup "bluss" 2015-2023 |
| bitflags | 2.13.2 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers |
| block-buffer | 0.10.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018-2019 The RustCrypto Project Developers |
| bytemuck | 1.25.2 | MIT | Zlib OR Apache-2.0 OR MIT | Copyright (c) 2019 Daniel "Lokathor" Gee. |
| bytes | 1.12.1 | MIT | MIT | Copyright (c) 2018 Carl Lerche |
| cfg-if | 1.0.5 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 Alex Crichton |
| chacha20 | 0.9.1 | MIT | Apache-2.0 OR MIT | Copyright (c) 2019-2023 The RustCrypto Project Developers |
| chacha20poly1305 | 0.10.1 | MIT | Apache-2.0 OR MIT | Copyright (c) 2019 The RustCrypto Project Developers |
| chrono | 0.4.45 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014--2026, Kang Seonghoon and; Copyright (c) 2014, Kang Seonghoon. |
| cipher | 0.4.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016-2020 RustCrypto Developers |
| clru | 0.6.3 | MIT | MIT | Copyright (c) 2020 Élie ROUDNINSKI (marmeladema) <xademax@gmail.com> |
| const-field-offset | 0.2.1 | MIT | MIT OR Apache-2.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| cpufeatures | 0.2.17 | MIT | MIT OR Apache-2.0 | Copyright (c) 2020-2025 The RustCrypto Project Developers |
| critical-section | 1.2.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2022 The critical-section authors |
| crypto-common | 0.1.7 | MIT | MIT OR Apache-2.0 | Copyright (c) 2021 RustCrypto Developers |
| ctr | 0.9.2 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018-2022 RustCrypto Developers; Copyright (c) 2018 Artyom Pavlov |
| curve25519-dalek | 4.1.3 | BSD-3-Clause | BSD-3-Clause | Copyright (c) 2016-2021 isis agora lovecruft. All rights reserved.; Copyright (c) 2016-2021 Henry de Valence. All rights reserved.; Copyright (c) 2012 The Go Authors. All rights reserved. |
| derive_more | 2.1.1 | MIT | MIT | Copyright (c) 2016 Jelte Fennema |
| digest | 0.10.7 | MIT | MIT OR Apache-2.0 | Copyright (c) 2017 Artyom Pavlov |
| dlib | 0.5.3 | MIT | MIT | Copyright (c) 2015 Victor Berger |
| drm | 0.14.1 | MIT | MIT | the authors of drm: Tyler Slabinski, Victoria Brekenfeld (no copyright line published) |
| drm-ffi | 0.9.1 | MIT | MIT | the authors of drm-ffi: Tyler Slabinski (no copyright line published) |
| drm-fourcc | 2.2.0 | MIT | MIT | the authors of drm-fourcc: Daniel Franklin (no copyright line published) |
| drm-sys | 0.8.1 | MIT | MIT | the authors of drm-sys: Tyler Slabinski (no copyright line published) |
| ed25519 | 2.2.3 | MIT | Apache-2.0 OR MIT | Copyright (c) 2018-2023 RustCrypto Developers |
| ed25519-dalek | 2.2.0 | BSD-3-Clause | BSD-3-Clause | Copyright (c) 2017-2019 isis agora lovecruft. All rights reserved. |
| equivalent | 1.0.3 | MIT | Apache-2.0 OR MIT | Copyright (c) 2016--2023 |
| euclid | 0.22.14 | MIT | MIT OR Apache-2.0 | Copyright (c) 2012-2013 Mozilla Foundation |
| field-offset | 0.3.6 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016-2021 Diggory Blake, and other contributors. |
| fixed_decimal | 0.7.3 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| foldhash | 0.2.0 | Zlib | Zlib | Copyright (c) 2024 Orson Peters |
| font-types | 0.12.7 | MIT | MIT OR Apache-2.0 | Copyright (c) 2019 Fontations Developers |
| fontique (vendor/) | 0.11.1 | MIT | Apache-2.0 OR MIT | Copyright 2024 the Parley Authors |
| foreign-types | 0.3.2 | MIT | MIT/Apache-2.0 | Copyright (c) 2017 The foreign-types Developers |
| foreign-types-shared | 0.1.1 | MIT | MIT/Apache-2.0 | Copyright (c) 2017 The foreign-types Developers |
| futures-core | 0.3.34 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016 Alex Crichton; Copyright (c) 2017 The Tokio Authors |
| generic-array | 0.14.7 | MIT | MIT | Copyright (c) 2015 Bartłomiej Kamiński |
| getrandom | 0.3.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018-2025 The rust-random Project Developers; Copyright (c) 2014 The Rust Project Developers |
| harfrust | 0.12.0 | MIT | MIT | Copyright (c) HarfBuzz developers; Copyright (c) 2020 Yevhenii Reizner |
| hashbrown | 0.16.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016 Amanieu d'Antras |
| hashbrown | 0.17.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016 Amanieu d'Antras |
| hkdf | 0.12.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2015-2018 Vlad Filippov; Copyright (c) 2018-2021 RustCrypto Developers |
| hmac | 0.12.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2017 Artyom Pavlov |
| htmlparser | 0.2.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018 Reizner Evgeniy |
| i-slint-backend-selector | 1.18.1 | GPL-3.0-only | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| i-slint-common | 1.18.1 | GPL-3.0-only | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| i-slint-core | 1.18.1 | GPL-3.0-only | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| i-slint-renderer-software | 1.18.1 | GPL-3.0-only | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| iana-time-zone | 0.1.65 | MIT | MIT OR Apache-2.0 | Copyright (c) 2020 Andrew D. Straw |
| icu_collections | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_decimal | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_decimal_data | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_locale_core | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_locale_fallback | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_locale_fallback_data | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_normalizer | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_normalizer_data | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_properties | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_properties_data | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_provider | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_segmenter | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| icu_segmenter_data | 2.4.0 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| inout | 0.1.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2022 The RustCrypto Project Developers; Copyright (c) 2022 Artyom Pavlov |
| libc | 0.2.191 | MIT | MIT OR Apache-2.0 | Copyright (c) The Rust Project Developers |
| libloading | 0.8.9 | ISC | ISC | Copyright © 2015, Simonas Kazlauskas |
| libm | 0.2.16 | MIT | MIT | Copyright (c) 2018 Jorge Aparicio; Copyright © 2005-2020 Rich Felker, et al.; Copyright © 1993,2004 Sun Microsystems or; Copyright © 2003-2011 David Schultz or; Copyright © 2003-2009 Steven G. Kargl or; Copyright © 2003-2009 Bruce D. Evans or |
| linebender_resource_handle | 0.1.1 | MIT | Apache-2.0 OR MIT | Copyright 2024 the Raw Resource Handle Authors |
| linux-raw-sys | 0.12.1 | MIT | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Copyrights in the `linux-raw-sys` project are retained by their contributors.; copyright assignment is required to contribute to the `linux-raw-sys` |
| linux-raw-sys | 0.4.15 | MIT | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Copyrights in the `linux-raw-sys` project are retained by their contributors.; copyright assignment is required to contribute to the `linux-raw-sys` |
| linux-raw-sys | 0.9.4 | MIT | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Copyrights in the `linux-raw-sys` project are retained by their contributors.; copyright assignment is required to contribute to the `linux-raw-sys` |
| litemap | 0.8.3 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| log | 0.4.34 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers |
| lyon_algorithms | 1.0.22 | MIT | MIT OR Apache-2.0 | Copyright (c) 2013 Nicolas Silva (github.com/nical/lyon, LICENSE-MIT) |
| lyon_extra | 1.1.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2013 Nicolas Silva (github.com/nical/lyon, LICENSE-MIT) |
| lyon_geom | 1.0.20 | MIT | MIT OR Apache-2.0 | Copyright (c) 2013 Nicolas Silva (github.com/nical/lyon, LICENSE-MIT) |
| lyon_path | 1.0.19 | MIT | MIT OR Apache-2.0 | Copyright (c) 2013 Nicolas Silva (github.com/nical/lyon, LICENSE-MIT) |
| memchr | 2.8.3 | MIT | Unlicense OR MIT | Copyright (c) 2015 Andrew Gallant |
| memmap2 | 0.9.12 | MIT | MIT OR Apache-2.0 | Copyright (c) 2020 Yevhenii Reizner; Copyright (c) 2015 Dan Burkert |
| memoffset | 0.9.1 | MIT | MIT | Copyright (c) 2017 Gilad Naaman |
| miniz_oxide | 0.8.9 | MIT | MIT OR Zlib OR Apache-2.0 | Copyright 2013-2014 RAD Game Tools and Valve Software; Copyright 2010-2014 Rich Geldreich and Tenacious Software LLC; Copyright (c) 2017 Frommi; Copyright (c) 2017-2024 oyvindln |
| num-bigint | 0.4.8 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers |
| num-integer | 0.1.47 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers |
| num-traits | 0.2.19 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 The Rust Project Developers |
| nusb | 0.2.8 | MIT | Apache-2.0 OR MIT | the authors of nusb: Kevin Mehall (no copyright line published) |
| once_cell | 1.21.4 | MIT | MIT OR Apache-2.0 | the authors of once_cell: Aleksey Kladov (no copyright line published) |
| opaque-debug | 0.3.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018-2024 The RustCrypto Project Developers |
| openssl | 0.10.81 | Apache-2.0 | Apache-2.0 | Copyright 2011-2017 Google Inc. |
| openssl-sys | 0.9.117 | MIT | MIT | Copyright (c) 2014 Alex Crichton |
| parlance | 0.1.1 | MIT | Apache-2.0 OR MIT | Copyright 2020 the Parley Authors |
| parley (vendor/) | 0.11.1 | MIT | Apache-2.0 OR MIT | Copyright 2020 the Parley Authors |
| parley_data | 0.11.1 | MIT | Apache-2.0 OR MIT | Copyright 2020 the Parley Authors |
| pin-project | 1.1.13 | MIT | Apache-2.0 OR MIT | the authors of pin-project, see https://github.com/taiki-e/pin-project (no copyright line published) |
| pin-weak | 1.1.0 | MIT | MIT | Copyright (c) 2020 Olivier Goffart <ogoffart@sixtyfps.io> |
| poly1305 | 0.8.0 | MIT | Apache-2.0 OR MIT | Copyright (c) 2015-2019 RustCrypto Developers |
| portable-atomic | 1.15.0 | MIT | Apache-2.0 OR MIT | the authors of portable-atomic, see https://github.com/taiki-e/portable-atomic (no copyright line published) |
| potential_utf | 0.1.6 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| prost | 0.14.4 | Apache-2.0 | Apache-2.0 | the authors of prost: Dan Burkert, Lucio Franco, Casper Meijn, Tokio Contributors (no copyright line published) |
| pulldown-cmark | 0.13.5 | MIT | MIT | Copyright 2015 Google Inc. All rights reserved. |
| qrcodegen | 1.8.0 | MIT | MIT | Copyright (c) Project Nayuki. (MIT License) |
| rand_core | 0.6.4 | MIT | MIT OR Apache-2.0 | Copyright 2018 Developers of the Rand project; Copyright (c) 2014 The Rust Project Developers |
| read-fonts | 0.41.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2019 Fontations Developers |
| rgb | 0.8.53 | MIT | MIT | Copyright (c) 2019 Kornel |
| rustix | 0.38.44 | MIT | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Copyrights in the `rustix` project are retained by their contributors.; copyright assignment is required to contribute to the `rustix` |
| rustix | 1.1.5 | MIT | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Copyrights in the `rustix` project are retained by their contributors.; copyright assignment is required to contribute to the `rustix` |
| scopeguard | 1.2.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2016-2019 Ulrik Sverdrup "bluss" and scopeguard developers |
| serde | 1.0.229 | MIT | MIT OR Apache-2.0 | the authors of serde: Erick Tryzelaar, David Tolnay (no copyright line published) |
| serde_core | 1.0.229 | MIT | MIT OR Apache-2.0 | the authors of serde_core: Erick Tryzelaar, David Tolnay (no copyright line published) |
| serde_spanned | 1.1.2 | MIT | MIT OR Apache-2.0 | Copyright (c) Individual contributors |
| sha1 | 0.10.7 | MIT | MIT OR Apache-2.0 | Copyright (c) 2006-2009 Graydon Hoare; Copyright (c) 2009-2013 Mozilla Foundation; Copyright (c) 2016 Artyom Pavlov |
| sha2 | 0.10.9 | MIT | MIT OR Apache-2.0 | Copyright (c) 2006-2009 Graydon Hoare; Copyright (c) 2009-2013 Mozilla Foundation; Copyright (c) 2016 Artyom Pavlov |
| signature | 2.2.0 | MIT | Apache-2.0 OR MIT | Copyright (c) 2018-2023 RustCrypto Developers |
| skrifa | 0.44.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2019 Fontations Developers |
| slab | 0.4.12 | MIT | MIT | Copyright (c) 2019 Carl Lerche |
| slint | 1.18.1 | GPL-3.0-only | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| slotmap | 1.1.1 | Zlib | Zlib | Copyright (c) 2021 Orson Peters <orsonpeters@gmail.com> |
| smallvec | 1.16.4 | MIT | MIT OR Apache-2.0 | Copyright (c) 2018 The Servo Project Developers |
| stable_deref_trait | 1.2.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2017 Robert Grosse |
| strum | 0.28.0 | MIT | MIT | Copyright (c) 2019 Peter Glotfelty |
| subtle | 2.6.1 | BSD-3-Clause | BSD-3-Clause | Copyright (c) 2016-2017 Isis Agora Lovecruft, Henry de Valence. All rights reserved.; Copyright (c) 2016-2024 Isis Agora Lovecruft. All rights reserved. |
| swash | 0.2.10 | MIT | Apache-2.0 OR MIT | Copyright (c) 2020 Chad Brokaw |
| sys-locale | 0.3.2 | MIT | MIT OR Apache-2.0 | Copyright (c) 2021 1Password |
| taffy | 0.10.1 | MIT | MIT | Copyright (c) 2018 Visly Inc.; Copyright (c) 2026 Taffy Authors (github.com/DioxusLabs/taffy, LICENSE) |
| thiserror | 2.0.21 | MIT | MIT OR Apache-2.0 | the authors of thiserror: David Tolnay (no copyright line published) |
| tinystr | 0.8.5 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| toml | 0.9.12+spec-1.1.0 | MIT | MIT OR Apache-2.0 | Copyright (c) Individual contributors |
| toml_datetime | 0.7.5+spec-1.1.0 | MIT | MIT OR Apache-2.0 | Copyright (c) Individual contributors |
| toml_parser | 1.1.5+spec-1.1.0 | MIT | MIT OR Apache-2.0 | Copyright (c) Individual contributors |
| toml_writer | 1.1.3+spec-1.1.0 | MIT | MIT OR Apache-2.0 | Copyright (c) Individual contributors |
| typenum | 1.20.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014 Paho Lurie-Gregg |
| unicase | 2.10.0 | MIT | MIT OR Apache-2.0 | Copyright (c) 2014-2026 Sean McArthur |
| unicode-segmentation | 1.13.3 | MIT | MIT OR Apache-2.0 | Copyright (c) 2015 The Rust Project Developers |
| universal-hash | 0.5.1 | MIT | MIT OR Apache-2.0 | Copyright (c) 2019-2020 RustCrypto Developers |
| utf8_iter | 1.0.4 | MIT | Apache-2.0 OR MIT | Copyright Mozilla Foundation |
| vtable | 0.5.0 | MIT | MIT OR Apache-2.0 | Copyright © SixtyFPS GmbH <info@slint.dev> |
| winnow | 0.7.15 | MIT | MIT | the authors of winnow, see https://github.com/winnow-rs/winnow (no copyright line published) |
| winnow | 1.0.4 | MIT | MIT | the authors of winnow, see https://github.com/winnow-rs/winnow (no copyright line published) |
| writeable | 0.6.4 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| x25519-dalek | 2.0.1 | BSD-3-Clause | BSD-3-Clause | Copyright (c) 2017-2021 isis agora lovecruft. All rights reserved.; Copyright (c) 2019-2021 DebugSteven. All rights reserved. |
| yazi | 0.2.1 | MIT | Apache-2.0 OR MIT | Copyright (c) 2020 Chad Brokaw |
| yeslogic-fontconfig-sys | 6.0.1 | MIT | MIT | Copyright 2019 YesLogic Pty. Ltd.; Copyright 2016 Manuel Reinhardt; Copyright 2014 Austin Bonander; Copyright 2013 The Servo Project Developers. |
| yoke | 0.8.4 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| zeno | 0.3.3 | MIT | Apache-2.0 OR MIT | Copyright (c) 2020 Chad Brokaw |
| zerofrom | 0.1.8 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| zeroize | 1.9.1 | MIT | Apache-2.0 OR MIT | Copyright (c) 2018-2026 The RustCrypto Project Developers |
| zerotrie | 0.2.5 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |
| zerovec | 0.11.9 | Unicode-3.0 | Unicode-3.0 | Copyright © 2020-2024 Unicode, Inc.; © 1995-2016 International Business Machines Corporation and others. |

## Licence texts

### MIT

Crates: adler2, aead, aes, allocator-api2, arrayvec, bitflags, block-buffer, bytemuck, bytes, cfg-if, chacha20, chacha20poly1305, chrono, cipher, clru, const-field-offset, cpufeatures, critical-section, crypto-common, ctr, derive_more, digest, dlib, drm, drm-ffi, drm-fourcc, drm-sys, ed25519, equivalent, euclid, field-offset, font-types, fontique, foreign-types, foreign-types-shared, futures-core, generic-array, getrandom, harfrust, hashbrown, hkdf, hmac, htmlparser, iana-time-zone, inout, libc, libm, linebender_resource_handle, linux-raw-sys, log, lyon_algorithms, lyon_extra, lyon_geom, lyon_path, memchr, memmap2, memoffset, miniz_oxide, num-bigint, num-integer, num-traits, nusb, once_cell, opaque-debug, openssl-sys, parlance, parley, parley_data, pin-project, pin-weak, poly1305, portable-atomic, pulldown-cmark, qrcodegen, rand_core, read-fonts, rgb, rustix, scopeguard, serde, serde_core, serde_spanned, sha1, sha2, signature, skrifa, slab, smallvec, stable_deref_trait, strum, swash, sys-locale, taffy, thiserror, toml, toml_datetime, toml_parser, toml_writer, typenum, unicase, unicode-segmentation, universal-hash, utf8_iter, vtable, winnow, yazi, yeslogic-fontconfig-sys, zeno, zeroize.

```text
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

### Apache-2.0

Crates: openssl, prost.

```text
Copyright 2011-2017 Google Inc.
          2013 Jack Lloyd
          2013-2014 Steven Fackler

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

### BSD-3-Clause

Crates: curve25519-dalek, ed25519-dalek, subtle, x25519-dalek.

```text
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### ISC

Crates: libloading.

```text
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY
AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM
LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR
OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

### Zlib

Crates: foldhash, slotmap.

```text
This software is provided 'as-is', without any express or implied
warranty. In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would be
   appreciated but is not required.
2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.
3. This notice may not be removed or altered from any source distribution.
```

### Unicode-3.0

Crates: fixed_decimal, icu_collections, icu_decimal, icu_decimal_data, icu_locale_core, icu_locale_fallback, icu_locale_fallback_data, icu_normalizer, icu_normalizer_data, icu_properties, icu_properties_data, icu_provider, icu_segmenter, icu_segmenter_data, litemap, potential_utf, tinystr, writeable, yoke, zerofrom, zerotrie, zerovec.

```text
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 2020-2024 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.

SPDX-License-Identifier: Unicode-3.0

—

Portions of ICU4X may have been adapted from ICU4C and/or ICU4J.
ICU 1.8.1 to ICU 57.1 © 1995-2016 International Business Machines Corporation and others.
```

### GPL-3.0-only

Crates: i-slint-backend-selector, i-slint-common, i-slint-core, i-slint-renderer-software, slint.

The GNU General Public License version 3 is the file LICENSE at the root of
LEHU's source tree (on Debian systems also /usr/share/common-licenses/GPL-3).


## Font

The screens embed Open Sans Regular 3.003, Copyright 2020 The Open Sans
Project Authors (https://github.com/googlefonts/opensans), under the SIL Open
Font License 1.1; its glyphs are rasterised into the binary at build time.

```text
Copyright 2020 The Open Sans Project Authors (https://github.com/googlefonts/opensans)

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
https://openfontlicense.org


-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

## System libraries

These are the system's own, found at run time and not shipped by LEHU; their
licences come with the system's packages.

| Library | Licence | Use |
|---|---|---|
| ffmpeg (libavcodec, libavutil) | LGPL-2.1-or-later as LEHU's images build it | the hardware decoder through v4l2-request, the AAC decoder |
| alsa-lib | LGPL-2.1-or-later | sound |
| libopus | BSD-3-Clause | the microphone and wireless audio |
| OpenSSL 3 | Apache-2.0 | TLS to the phone, hashing |
| libdbus-1 | AFL-2.1 or GPL-2.0-or-later | BlueZ, opened with dlopen |
| glibc | LGPL-2.1-or-later | the C library |
