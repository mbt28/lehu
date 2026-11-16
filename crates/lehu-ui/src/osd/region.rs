// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Settings > Map: the region the map shows, stepped through the regions on
//! the card like Output's cards and taken with Enter, and the voice the
//! spoken prompts speak with, stepped through the voices on the card the
//! same way. A region damaged or written for a newer
//! LEHU says so as it is stepped past, and Enter leaves the region shown as
//! it was: nothing to choose wrong while driving.

use toml::Value;

use super::{Bridge, Host, MAP_SETTINGS, SETTINGS, State, clean};

/// Settings' Map row and the Region row when there is no region.
const NO_REGION: &str = "None";
/// The Voice row when the card has none.
const NO_VOICE: &str = "Not installed";

impl<H: Host> State<H> {
    /// The regions read again, and the labels after them.
    pub(super) fn refresh_regions(&mut self, bridge: &Bridge<'_>) {
        self.regions = self.host.map_regions();
        self.regions_gen = Some(self.host.map_regions_generation());
        let current = self.regions.iter().find(|r| r.current);
        let label = current.map_or_else(|| NO_REGION.to_string(), crate::host::MapRegion::label);
        if self.p_region_label.update(&label) {
            bridge.set_region_label(clean(&mut self.scratch, &label));
        }
        self.voices = self.host.map_voices();
        let voice = !self.voices.is_empty();
        let voice_label = self
            .voices
            .iter()
            .find(|v| v.current)
            .or(self.voices.first())
            .map_or(NO_VOICE, |v| v.name.as_str())
            .to_string();
        if self.p_voice_label.update(&voice_label) {
            bridge.set_voice_label(clean(&mut self.scratch, &voice_label));
        }
        let hint = if self.regions.is_empty() {
            "No map on the card: copy a region folder to its LEHU-MAPS drive"
        } else if !voice {
            "Spoken prompts: copy the voice folder to the LEHU-MAPS drive"
        } else {
            ""
        };
        if self.p_map_hint.update(hint) {
            bridge.set_map_hint(clean(&mut self.scratch, hint));
        }
    }

    /// Each pump: read again when Settings or its Map screen opens, and
    /// when a region's state may have changed.
    pub(super) fn pump_regions(&mut self, bridge: &Bridge<'_>) {
        let screen = bridge.get_screen();
        let shown = screen == SETTINGS || screen == MAP_SETTINGS;
        let opened = shown && self.regions_screen != screen;
        self.regions_screen = screen;
        let changed = self.regions_gen != Some(self.host.map_regions_generation());
        if self.regions_gen.is_none() || opened || (shown && changed) {
            self.refresh_regions(bridge);
        }
    }

    /// The Region row starts on the region shown; nothing to step through
    /// without one.
    pub(super) fn region_start(&mut self) -> Option<i32> {
        if self.regions.is_empty() {
            return None;
        }
        self.pick_list.clear();
        self.pick_list
            .extend(self.regions.iter().map(crate::host::MapRegion::label));
        Some(self.regions.iter().position(|r| r.current).unwrap_or(0) as i32)
    }

    /// The Voice row starts on the voice spoken with; nothing to step
    /// through without one.
    pub(super) fn voice_start(&mut self) -> Option<i32> {
        if self.voices.is_empty() {
            return None;
        }
        self.pick_list.clear();
        self.pick_list
            .extend(self.voices.iter().map(|v| v.name.clone()));
        Some(self.voices.iter().position(|v| v.current).unwrap_or(0) as i32)
    }

    /// Enter on a voice: the prompts speak with it from the next one.
    pub(super) fn voice_take(&mut self, bridge: &Bridge<'_>, taken: i32) {
        let Some(voice) = usize::try_from(taken)
            .ok()
            .and_then(|i| self.voices.get(i))
            .cloned()
        else {
            return;
        };
        if voice.current {
            return;
        }
        self.persist("map.voice-dir", Value::String(voice.dir.clone()));
        self.refresh_regions(bridge);
    }

    /// Enter on a region: shown from now (the map opens on its own start),
    /// unless it cannot be: then the region shown stays.
    pub(super) fn region_take(&mut self, bridge: &Bridge<'_>, taken: i32) {
        let Some(region) = usize::try_from(taken)
            .ok()
            .and_then(|i| self.regions.get(i))
            .cloned()
        else {
            return;
        };
        if region.current || !region.state.usable() {
            return;
        }
        self.persist("map.dir", Value::String(region.dir.clone()));
        self.persist("map.zoom", Value::Integer(0));
        self.refresh_regions(bridge);
    }
}
