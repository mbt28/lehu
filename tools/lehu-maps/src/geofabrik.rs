// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Geofabrik's list of OpenStreetMap extracts
//! (<https://download.geofabrik.de/index-v1-nogeom.json>): every continent,
//! country and sub-region with the URL of its `.osm.pbf`, as a tree.

use serde::Deserialize;

pub const INDEX_URL: &str = "https://download.geofabrik.de/index-v1-nogeom.json";

/// One extract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Area {
    /// Geofabrik's id: `hungary`, `bayern`, `us/california`.
    pub id: String,
    pub parent: Option<String>,
    pub name: String,
    pub pbf: String,
    /// The parents' names, the outermost first: "Europe › Germany".
    pub path: String,
    /// How deep in the tree: 0 for a continent.
    pub depth: usize,
    /// ISO 3166 codes (`HU`, `US-CA`), for the search.
    pub codes: Vec<String>,
}

#[derive(Deserialize)]
struct Index {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Props,
}

#[derive(Deserialize)]
struct Props {
    id: String,
    parent: Option<String>,
    name: String,
    #[serde(default)]
    urls: Urls,
    #[serde(rename = "iso3166-1:alpha2", default)]
    iso1: Vec<String>,
    #[serde(rename = "iso3166-2", default)]
    iso2: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Urls {
    pbf: Option<String>,
}

/// A name fit to show: Geofabrik names the US states `us/california`.
fn pretty(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(char::is_uppercase) {
        return last.to_string();
    }
    last.split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or_else(String::new, |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The index's extracts in tree order: each after its parent, siblings by
/// name. Those without a pbf are left out; one whose parent is missing is
/// put at the top.
pub fn parse(json: &str) -> Result<Vec<Area>, String> {
    let index: Index =
        serde_json::from_str(json).map_err(|e| format!("Geofabrik's index does not read: {e}"))?;
    let mut flat: Vec<Area> = index
        .features
        .into_iter()
        .filter_map(|f| {
            let p = f.properties;
            Some(Area {
                pbf: p.urls.pbf?,
                name: pretty(&p.name),
                codes: p.iso1.into_iter().chain(p.iso2).collect(),
                id: p.id,
                parent: p.parent,
                path: String::new(),
                depth: 0,
            })
        })
        .collect();
    flat.sort_by_key(|a| a.name.to_lowercase());
    let known: std::collections::HashSet<String> = flat.iter().map(|a| a.id.clone()).collect();
    let mut out = Vec::with_capacity(flat.len());
    let roots: Vec<usize> = (0..flat.len())
        .filter(|&i| flat[i].parent.as_ref().is_none_or(|p| !known.contains(p)))
        .collect();
    let mut stack: Vec<(usize, usize, String)> = roots
        .into_iter()
        .rev()
        .map(|i| (i, 0, String::new()))
        .collect();
    let mut seen = vec![false; flat.len()];
    while let Some((i, depth, path)) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        let mut a = flat[i].clone();
        a.depth = depth;
        a.path.clone_from(&path);
        let child_path = if path.is_empty() {
            a.name.clone()
        } else {
            format!("{path} › {}", a.name)
        };
        let id = a.id.clone();
        out.push(a);
        for j in (0..flat.len()).rev() {
            if flat[j].parent.as_deref() == Some(id.as_str()) {
                stack.push((j, depth + 1, child_path.clone()));
            }
        }
    }
    Ok(out)
}

/// The rows to show for `query`: all when it is empty, else those whose
/// name, id or ISO code holds it (any case), as indices into `areas`.
pub fn filter(areas: &[Area], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    (0..areas.len())
        .filter(|&i| {
            let a = &areas[i];
            q.is_empty()
                || a.name.to_lowercase().contains(&q)
                || a.id.to_lowercase().contains(&q)
                || a.codes.iter().any(|c| c.to_lowercase() == q)
        })
        .collect()
}

/// The folder a region has on the card: the id, `/` made `-`
/// (`us/california` is `us-california`).
pub fn folder(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// The zoom preset map-region.py has for a region: its own for the two it
/// knows, else `auto`.
pub fn preset(id: &str) -> &'static str {
    match id {
        "luxembourg" => "luxembourg",
        "hungary" => "hungary",
        _ => "auto",
    }
}

/// The extract's date from the dated file name Geofabrik's `-latest` URL
/// leads to (`luxembourg-261110.osm.pbf` is 2026-11-10).
pub fn dated(url: &str) -> Option<String> {
    let file = url.rsplit('/').next()?;
    let stem = file.strip_suffix(".osm.pbf")?;
    let (_, d) = stem.rsplit_once('-')?;
    if d.len() != 6 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (yy, mm, dd) = (&d[0..2], &d[2..4], &d[4..6]);
    let (m, day): (u32, u32) = (mm.parse().ok()?, dd.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("20{yy}-{mm}-{dd}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"type":"FeatureCollection","features":[
      {"type":"Feature","properties":{"id":"europe","name":"Europe",
        "urls":{"pbf":"https://download.geofabrik.de/europe-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"hungary","parent":"europe","iso3166-1:alpha2":["HU"],
        "name":"Hungary","urls":{"pbf":"https://download.geofabrik.de/europe/hungary-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"germany","parent":"europe","iso3166-1:alpha2":["DE"],
        "name":"Germany","urls":{"pbf":"https://download.geofabrik.de/europe/germany-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"bayern","parent":"germany","name":"Bayern",
        "urls":{"pbf":"https://download.geofabrik.de/europe/germany/bayern-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"north-america","name":"North America",
        "urls":{"pbf":"https://download.geofabrik.de/north-america-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"us/new-york","parent":"north-america","iso3166-2":["US-NY"],
        "name":"us/new-york","urls":{"pbf":"https://download.geofabrik.de/north-america/us/new-york-latest.osm.pbf"}}},
      {"type":"Feature","properties":{"id":"nowhere","parent":"europe","name":"No file","urls":{}}}
    ]}"#;

    #[test]
    fn tree_order_and_paths() {
        let areas = parse(SAMPLE).unwrap();
        let ids: Vec<&str> = areas.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "europe",
                "germany",
                "bayern",
                "hungary",
                "north-america",
                "us/new-york"
            ]
        );
        let bayern = &areas[2];
        assert_eq!(bayern.depth, 2);
        assert_eq!(bayern.path, "Europe › Germany");
        assert_eq!(areas[5].name, "New York");
        assert_eq!(
            areas[3].pbf,
            "https://download.geofabrik.de/europe/hungary-latest.osm.pbf"
        );
    }

    #[test]
    fn search() {
        let areas = parse(SAMPLE).unwrap();
        let names = |q| -> Vec<String> {
            filter(&areas, q)
                .into_iter()
                .map(|i| areas[i].name.clone())
                .collect()
        };
        assert_eq!(names("hun"), ["Hungary"]);
        assert_eq!(names("HU"), ["Hungary"]);
        assert_eq!(names("us-ny"), ["New York"]);
        assert_eq!(filter(&areas, "  ").len(), 6);
        assert!(names("atlantis").is_empty());
    }

    #[test]
    fn folders_presets_dates() {
        assert_eq!(folder("us/california"), "us-california");
        assert_eq!(folder("Hungary"), "hungary");
        assert_eq!(preset("hungary"), "hungary");
        assert_eq!(preset("bayern"), "auto");
        assert_eq!(
            dated("https://download.geofabrik.de/europe/luxembourg-261110.osm.pbf").as_deref(),
            Some("2026-11-10")
        );
        assert_eq!(dated("https://x/luxembourg-latest.osm.pbf"), None);
        assert_eq!(dated("https://x/a-261340.osm.pbf"), None);
    }

    #[test]
    fn bad_json() {
        assert!(parse("{").is_err());
    }
}
