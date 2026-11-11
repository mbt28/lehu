// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The lines scripts/map-region.py prints with `--progress` and
//! `--estimate`.

#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// `@stage NAME`: graph, tiles, labels, pack, manifest.
    Stage(String),
    /// `@progress DONE TOTAL` within the stage.
    Progress(u64, u64),
    /// `@done`: the region is complete.
    Done,
    /// `@error MESSAGE`, before a non-zero exit.
    Error(String),
    /// `@estimate BYTES SECONDS`.
    Estimate(u64, u64),
    /// Anything else: for the log.
    Text(String),
}

pub fn parse(line: &str) -> Line {
    let l = line.trim_end_matches(['\r', '\n']);
    let Some(rest) = l.strip_prefix('@') else {
        return Line::Text(l.to_string());
    };
    let (word, args) = rest.split_once(' ').unwrap_or((rest, ""));
    let args = args.trim();
    let two = || {
        let mut it = args.split_whitespace().map(str::parse::<u64>);
        match (it.next(), it.next(), it.next()) {
            (Some(Ok(a)), Some(Ok(b)), None) => Some((a, b)),
            _ => None,
        }
    };
    match word {
        "stage" if !args.is_empty() => Line::Stage(args.to_string()),
        "progress" => {
            two().map_or_else(|| Line::Text(l.to_string()), |(a, b)| Line::Progress(a, b))
        }
        "estimate" => {
            two().map_or_else(|| Line::Text(l.to_string()), |(a, b)| Line::Estimate(a, b))
        }
        "done" if args.is_empty() => Line::Done,
        "error" => Line::Error(if args.is_empty() {
            "failed".into()
        } else {
            args.to_string()
        }),
        _ => Line::Text(l.to_string()),
    }
}

/// A stage's name for people.
pub fn stage_text(stage: &str) -> String {
    match stage {
        "graph" => "Building the road network".into(),
        "tiles" => "Drawing the map tiles".into(),
        "labels" => "Placing the names".into(),
        "pack" => "Packing the tiles and names".into(),
        "manifest" => "Writing the checksums".into(),
        s => format!("Building: {s}"),
    }
}

/// How far the whole build is, 0..1, from the stage and its progress: the
/// stages' rough shares of a build's time.
pub fn overall(stage: &str, done: u64, total: u64) -> f32 {
    const SHARES: [(&str, f32); 5] = [
        ("graph", 0.15),
        ("tiles", 0.55),
        ("labels", 0.2),
        ("pack", 0.05),
        ("manifest", 0.05),
    ];
    let mut before = 0.0;
    for (name, share) in SHARES {
        if name == stage {
            let within = if total == 0 {
                0.0
            } else {
                (done.min(total) as f32) / total as f32
            };
            return before + share * within;
        }
        before += share;
    }
    -1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        assert_eq!(parse("@stage tiles\n"), Line::Stage("tiles".into()));
        assert_eq!(parse("@progress 200 3981"), Line::Progress(200, 3981));
        assert_eq!(parse("@done"), Line::Done);
        assert_eq!(
            parse("@error no such file: x.pbf"),
            Line::Error("no such file: x.pbf".into())
        );
        assert_eq!(parse("@error"), Line::Error("failed".into()));
        assert_eq!(
            parse("@estimate 34000000 200"),
            Line::Estimate(34_000_000, 200)
        );
        assert_eq!(parse("@progress 1"), Line::Text("@progress 1".into()));
        assert_eq!(parse("@progress a b"), Line::Text("@progress a b".into()));
        assert_eq!(
            parse("+ python map-graph.py"),
            Line::Text("+ python map-graph.py".into())
        );
        assert_eq!(parse("@stage"), Line::Text("@stage".into()));
    }

    #[test]
    fn overall_progress() {
        assert_eq!(overall("graph", 0, 1), 0.0);
        assert!((overall("tiles", 1, 2) - 0.425).abs() < 1e-6);
        assert!((overall("manifest", 5, 5) - 1.0).abs() < 1e-6);
        assert_eq!(overall("other", 1, 1), -1.0);
        assert_eq!(overall("tiles", 0, 0), 0.15);
    }
}
