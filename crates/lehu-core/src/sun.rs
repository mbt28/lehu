// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Where the sun is, for Night set to Auto: day or night by the sun at the
//! head unit's position, from the clock.
//!
//! The sun's elevation is the low-precision formula of the Astronomical
//! Almanac (about a hundredth of a degree from 1950 to 2050, a minute or
//! so of sunrise and sunset), in f64: it runs once a minute, so the board's
//! soft floating point costs nothing that matters.
//!
//! [`Dusk`] says night once the sun is [`NIGHT_BELOW_DEG`] below the
//! horizon, with a little hysteresis so that it does not flap. Sunset
//! itself (the sun's top at the horizon, its centre 0.83 degrees below)
//! is still broad daylight; civil twilight ends at 6 degrees below, when
//! it is dark. Two degrees below is about ten minutes after sunset in
//! Luxembourg (a quarter of an hour in midsummer), when the street lights
//! come on and a bright screen starts to dazzle: the night palette then,
//! and the day palette again as long after sunrise.

/// Night below this solar elevation, degrees; day above it, each with
/// `HYSTERESIS_DEG` to cross.
pub const NIGHT_BELOW_DEG: f64 = -2.0;
/// Half the band the elevation must cross to change day to night or back:
/// night at -2.5 degrees, day at -1.5 (about four minutes each way in
/// Luxembourg).
pub const HYSTERESIS_DEG: f64 = 0.5;

/// The sun's elevation above the horizon, degrees (refraction left out),
/// at `unix_s` seconds since 1970 UTC, seen from `lat`, `lon` (degrees,
/// north and east positive).
pub fn elevation(unix_s: i64, lat: f64, lon: f64) -> f64 {
    // Days from J2000.0 (2000-01-01 12:00 TT, taken as UTC: a minute off).
    let n = unix_s as f64 / 86_400.0 - 10_957.5;
    let mean_lon = (280.460 + 0.985_647_4 * n).rem_euclid(360.0);
    let anomaly = (357.528 + 0.985_600_3 * n).rem_euclid(360.0).to_radians();
    let ecliptic = (mean_lon + 1.915 * anomaly.sin() + 0.020 * (2.0 * anomaly).sin()).to_radians();
    let obliquity = (23.439 - 0.000_000_4 * n).to_radians();
    let ra = (obliquity.cos() * ecliptic.sin()).atan2(ecliptic.cos());
    let dec = (obliquity.sin() * ecliptic.sin()).asin();
    // Greenwich mean sidereal time, degrees, then the hour angle here.
    let gmst = (280.460_618_37 + 360.985_647_366_29 * n).rem_euclid(360.0);
    let hour = (gmst + lon).to_radians() - ra;
    let lat = lat.to_radians();
    (lat.sin() * dec.sin() + lat.cos() * dec.cos() * hour.cos())
        .asin()
        .to_degrees()
}

/// Day or night by the sun, as it goes down and comes up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dusk {
    night: Option<bool>,
}

impl Dusk {
    pub fn new() -> Self {
        Self::default()
    }

    /// Night or not with the sun at `elevation` degrees: the first time
    /// by `NIGHT_BELOW_DEG` alone, then changing only past it by
    /// `HYSTERESIS_DEG`.
    pub fn update(&mut self, elevation: f64) -> bool {
        let night = match self.night {
            None => elevation < NIGHT_BELOW_DEG,
            Some(true) => elevation < NIGHT_BELOW_DEG + HYSTERESIS_DEG,
            Some(false) => elevation < NIGHT_BELOW_DEG - HYSTERESIS_DEG,
        };
        self.night = Some(night);
        night
    }

    /// What it last said; None before the first `update`.
    pub fn night(&self) -> Option<bool> {
        self.night
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Luxembourg City.
    const LAT: f64 = 49.6116;
    const LON: f64 = 6.1319;

    /// Unix seconds at a UTC date and time.
    fn utc(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> i64 {
        // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
        let y = if mo <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60
    }

    /// The minute (UTC, from midnight) the sun's centre crosses `deg`
    /// going up (`rising`) or down, on a day.
    fn crossing(day: i64, deg: f64, rising: bool) -> i64 {
        let above = |m: i64| elevation(day + m * 60, LAT, LON) > deg;
        let (mut lo, mut hi) = if rising {
            (0, 12 * 60)
        } else {
            (12 * 60, 24 * 60)
        };
        assert_ne!(above(lo), above(hi));
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if above(mid) == above(lo) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        hi
    }

    fn near(minute: i64, h: i64, m: i64) -> bool {
        (minute - (h * 60 + m)).abs() <= 3
    }

    #[test]
    fn sunrise_and_sunset_in_luxembourg_are_on_time() {
        // Sunrise and sunset: the sun's top at the horizon, its centre
        // 0.833 degrees below. The sunrise equation's times, in UTC (the
        // published ones agree within a minute): 21 June 03:28 and 19:46
        // (05:28 and 21:46 summer time), solar noon 11:37; 21 December
        // 07:30 and 15:38 (08:30 and 16:38 winter time).
        let june = utc(2026, 6, 21, 0, 0);
        let rise = crossing(june, -0.833, true);
        let set = crossing(june, -0.833, false);
        assert!(near(rise, 3, 28), "{}:{:02}", rise / 60, rise % 60);
        assert!(near(set, 19, 46), "{}:{:02}", set / 60, set % 60);
        let december = utc(2026, 12, 21, 0, 0);
        let rise = crossing(december, -0.833, true);
        let set = crossing(december, -0.833, false);
        assert!(near(rise, 7, 30), "{}:{:02}", rise / 60, rise % 60);
        assert!(near(set, 15, 38), "{}:{:02}", set / 60, set % 60);
        // Noon in June: 90 - 49.6 + 23.4 degrees up, at 13:37 summer time.
        let noon = elevation(utc(2026, 6, 21, 11, 37), LAT, LON);
        assert!((noon - 63.8).abs() < 0.2, "{noon}");
    }

    #[test]
    fn night_comes_after_sunset_and_does_not_flap() {
        let december = utc(2026, 12, 21, 0, 0);
        let set = crossing(december, -0.833, false);
        let dark = crossing(december, NIGHT_BELOW_DEG, false);
        // Ten minutes or so after sunset, well before the end of civil
        // twilight.
        let civil = crossing(december, -6.0, false);
        assert!((8..=15).contains(&(dark - set)), "{}", dark - set);
        assert!(dark < civil);
        let mut dusk = Dusk::new();
        assert_eq!(dusk.night(), None);
        assert!(!dusk.update(-1.9));
        // Within the band: still day; past it, night.
        assert!(!dusk.update(-2.4));
        assert!(dusk.update(-2.6));
        // And back: night until -1.5.
        assert!(dusk.update(-1.6));
        assert!(!dusk.update(-1.4));
        // The first answer is by the line itself.
        assert!(Dusk::new().update(-2.1));
        // A minute at a time across a December evening: one change.
        let mut dusk = Dusk::new();
        let changes = (14 * 60..18 * 60)
            .map(|m| dusk.update(elevation(december + m * 60, LAT, LON)))
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0] != w[1])
            .count();
        assert_eq!(changes, 1);
    }
}
