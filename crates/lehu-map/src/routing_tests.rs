// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The router and the search: on small networks built here, and on a real
//! one (scripts/map-graph.py's) when one is given.

use crate::graph::Graph;
use crate::graph::build::{M_PER_PX, Road, road, write};
use crate::route::Router;
use crate::search::{FoundKind, Places, fold};
use crate::track::{Leg, LegStep, Track, Turn};

/// A point `east`, `south` metres from a corner in Luxembourg.
fn at(east: f64, south: f64) -> (i64, i64) {
    (
        555_000_000 + (east / M_PER_PX) as i64,
        368_000_000 + (south / M_PER_PX) as i64,
    )
}

fn router(nodes: &[(i64, i64)], roads: &[Road]) -> Router {
    let graph = Graph::from_bytes(&write(nodes, roads)).unwrap();
    graph.check().unwrap();
    Router::new(graph, None)
}

fn turns(track: &Track) -> Vec<(Turn, String)> {
    track
        .maneuvers()
        .iter()
        .map(|m| (m.turn, m.road.clone()))
        .collect()
}

#[test]
fn a_network_is_read_and_checked() {
    let nodes = [at(0.0, 0.0), at(100.0, 0.0), at(100.0, 100.0)];
    let mut r = road(0, 1, "Rue A");
    r.between = vec![at(50.0, 10.0)];
    r.limit = 50;
    let bytes = write(&nodes, &[r, road(1, 2, "Rue B")]);
    let g = Graph::from_bytes(&bytes).unwrap();
    g.check().unwrap();
    assert_eq!((g.nodes(), g.edges(), g.top_kmh()), (3, 2, 130));
    assert_eq!(g.degree(1), 2);
    assert_eq!(g.line(0), vec![nodes[0], at(50.0, 10.0), nodes[1]]);
    let k = g.kind(g.edge(0).kind);
    assert_eq!((k.name, k.limit, k.roundabout), ("Rue A", Some(50), false));
    // Out of node 1: back along A and on along B.
    let mut out: Vec<_> = g.out(1).collect();
    out.sort_unstable();
    assert_eq!(out, vec![(0, 0, true), (2, 1, false)]);
    // Cut short, or another file: refused.
    assert!(Graph::from_bytes(&bytes[..bytes.len() - 4]).is_err());
    let mut other = bytes.clone();
    other[0] = b'X';
    assert!(Graph::from_bytes(&other).is_err());
    // An arc to a node that is not there: found by the check.
    let mut broken = bytes;
    let arcs_at = 18 * 4 + 4 * 4 * 4;
    broken[arcs_at..arcs_at + 4].copy_from_slice(&99u32.to_le_bytes());
    assert!(Graph::from_bytes(&broken).unwrap().check().is_err());
}

#[test]
fn a_point_snaps_to_the_nearest_road() {
    let nodes = [at(0.0, 0.0), at(200.0, 0.0), at(0.0, 300.0)];
    let g = Graph::from_bytes(&write(&nodes, &[road(0, 1, "East"), road(0, 2, "South")])).unwrap();
    let s = g.snap(at(50.0, 20.0).0, at(50.0, 20.0).1).unwrap();
    assert_eq!(s.edge, 0);
    assert!((s.y - nodes[0].1).abs() <= 1, "{s:?}");
    // A quarter of the way along: 50 of 200 m.
    assert!((s.from_a_dm as i64 - 500).abs() <= 2, "{s:?}");
    let s = g.snap(at(10.0, 150.0).0, at(10.0, 150.0).1).unwrap();
    assert_eq!(s.edge, 1);
    // Kilometres from any road: none.
    assert!(g.snap(at(9000.0, 9000.0).0, at(9000.0, 9000.0).1).is_none());
}

/// A square of streets, one way round it the long way: 0 1 along the
/// top, 1 2 down the right, 3 2 along the bottom and 0 3 down the left;
/// the top one way from 1 to 0.
#[test]
fn the_fastest_route_keeps_to_one_way_streets() {
    let nodes = [
        at(0.0, 0.0),
        at(400.0, 0.0),
        at(400.0, 400.0),
        at(0.0, 400.0),
    ];
    let mut top = road(1, 0, "Top");
    top.oneway = true;
    let r = router(
        &nodes,
        &[
            top,
            road(1, 2, "Right"),
            road(3, 2, "Bottom"),
            road(0, 3, "Left"),
        ],
    );
    // From the top left to the top right: not against the one-way top,
    // but round by the left, the bottom and the right.
    let track = r.route(at(5.0, 0.0), None, at(395.0, 0.0)).unwrap();
    let m = f64::from(track.metres());
    assert!((1190.0..1230.0).contains(&m), "{m}");
    let line = track.line();
    assert!(line.contains(&nodes[3]) && line.contains(&nodes[2]));
    // Its corners are bends, not junctions (two roads meet there): no
    // turns to announce, only the arrival.
    assert_eq!(turns(&track), vec![(Turn::Arrive, String::new())]);
    // The other way it goes along the top, straight.
    let back = r.route(at(395.0, 0.0), None, at(5.0, 0.0)).unwrap();
    let m = f64::from(back.metres());
    assert!((385.0..395.0).contains(&m), "{m}");
    assert_eq!(turns(&back), vec![(Turn::Arrive, String::new())]);
    // Straight along one road, ahead: no junction on the way.
    let ahead = r.route(at(0.0, 50.0), None, at(0.0, 300.0)).unwrap();
    assert!((245.0..255.0).contains(&f64::from(ahead.metres())));
}

#[test]
fn turns_are_named_by_their_angle() {
    // A crossroads at 1: in from the west, out north, east, south, and a
    // slight right.
    let nodes = [
        at(0.0, 500.0),
        at(500.0, 500.0),
        at(500.0, 0.0),
        at(1000.0, 500.0),
        at(500.0, 1000.0),
        at(900.0, 330.0),
    ];
    let r = router(
        &nodes,
        &[
            road(0, 1, "West"),
            road(1, 2, "North"),
            road(1, 3, "East"),
            road(1, 4, "South"),
            road(1, 5, "Bend"),
        ],
    );
    let go = |to: (i64, i64)| turns(&r.route(at(20.0, 500.0), None, to).unwrap())[0].clone();
    assert_eq!(go(at(500.0, 20.0)), (Turn::Left, "North".into()));
    assert_eq!(go(at(500.0, 980.0)), (Turn::Right, "South".into()));
    assert_eq!(go(at(980.0, 500.0)), (Turn::Straight, "East".into()));
    assert_eq!(go(at(880.0, 340.0)), (Turn::SlightLeft, "Bend".into()));
}

/// A roundabout of four nodes (ring 1 2 3 4, one way), roads in at 1 and
/// out at 2, 3 and 4.
#[test]
fn a_roundabout_says_which_exit() {
    let nodes = [
        at(0.0, 500.0),    // 0: in from the west
        at(480.0, 500.0),  // 1: the ring, west
        at(500.0, 520.0),  // 2: south
        at(520.0, 500.0),  // 3: east
        at(500.0, 480.0),  // 4: north
        at(500.0, 1000.0), // 5: out south
        at(1000.0, 500.0), // 6: out east
        at(500.0, 0.0),    // 7: out north
    ];
    let ring = |a, b| {
        let mut r = road(a, b, "Rond-point");
        r.roundabout = true;
        r.kmh = 30;
        r
    };
    // Driven anticlockwise on the map as drawn: west, south, east,
    // north (y grows southwards).
    let r = router(
        &nodes,
        &[
            road(0, 1, "Route d'Arlon"),
            ring(1, 2),
            ring(2, 3),
            ring(3, 4),
            ring(4, 1),
            road(2, 5, "Rue Sud"),
            road(3, 6, "Rue Est"),
            road(4, 7, "Rue Nord"),
        ],
    );
    let go = |to: (i64, i64)| turns(&r.route(at(10.0, 500.0), None, to).unwrap());
    assert_eq!(
        go(at(500.0, 990.0))[0],
        (Turn::Roundabout(1), "Rue Sud".into())
    );
    assert_eq!(
        go(at(990.0, 500.0))[0],
        (Turn::Roundabout(2), "Rue Est".into())
    );
    let north = go(at(500.0, 10.0));
    assert_eq!(north[0], (Turn::Roundabout(3), "Rue Nord".into()));
    assert_eq!(north.len(), 2, "{north:?}");
}

#[test]
fn heading_makes_a_u_turn_cost() {
    // One straight road; the goal a little behind us, or ahead.
    let nodes = [at(0.0, 0.0), at(2000.0, 0.0)];
    let g = Graph::from_bytes(&write(&nodes, &[road(0, 1, "Long")])).unwrap();
    let mut work = crate::graph::Work::default();
    let snap = |p: (i64, i64)| g.snap(p.0, p.1).unwrap();
    let (here, behind, ahead) = (
        snap(at(1000.0, 0.0)),
        snap(at(900.0, 0.0)),
        snap(at(1100.0, 0.0)),
    );
    let east = Some((1024, 0));
    let mut cost = |to, heading| g.route(&here, heading, to, &mut work).unwrap().ms;
    // 100 m at 50 km/h, 7.2 s; behind us, a U-turn first.
    assert!((7_000..7_400).contains(&cost(&ahead, east)));
    assert!((7_000..7_400).contains(&cost(&behind, None)));
    assert!((37_000..37_400).contains(&cost(&behind, east)));
    let path = g.route(&here, east, &behind, &mut work).unwrap();
    assert!(path.direct && path.arcs.is_empty() && !path.start_forward);
}

#[test]
fn no_route_is_said_in_words() {
    // Two roads that never meet: one way out of the first, into nothing.
    let nodes = [
        at(0.0, 0.0),
        at(100.0, 0.0),
        at(0.0, 300.0),
        at(100.0, 300.0),
    ];
    let mut one = road(0, 1, "A");
    one.oneway = true;
    let r = router(&nodes, &[one, road(2, 3, "B")]);
    assert_eq!(
        r.route(at(10.0, 0.0), None, at(50.0, 300.0)).unwrap_err(),
        "No route there"
    );
    assert_eq!(
        r.route(at(10.0, 0.0), None, at(9000.0, 9000.0))
            .unwrap_err(),
        "No road near the destination"
    );
}

#[test]
fn a_route_is_driven_at_its_speeds_and_slowed_in_turns() {
    // 100 m at 36 km/h (10 m/s), a right angle, 100 m more: the steps at
    // both ends of the corner are taken at 20 km/h.
    let p = [at(0.0, 0.0), at(100.0, 0.0), at(100.0, 100.0)];
    let leg = Leg {
        points: p.to_vec(),
        steps: vec![
            LegStep {
                cm_s: 1000,
                limit: Some(50),
            },
            LegStep {
                cm_s: 1000,
                limit: None,
            },
        ],
        turns: vec![
            (1, Turn::Right, "Rue B".into()),
            (2, Turn::Arrive, String::new()),
        ],
    };
    let track = Track::from_route(&leg).unwrap();
    // 200 m at 20 km/h: 36 s.
    let s = track.duration() as f64 / 1000.0;
    assert!((35.0..37.0).contains(&s), "{s}");
    assert!(track.ends());
    // A point a second, and the corner.
    assert!(track.line().len() >= 36);
    assert!(track.line().contains(&p[1]));
    let fix = track.at(5_000);
    assert_eq!(fix.limit, Some(50));
    assert!((19..=21).contains(&fix.kmh), "{fix:?}");
    assert_eq!(track.at(30_000).limit, None);
    // The turn is at the corner: 100 m on from the start.
    let a = track.ahead(0).unwrap();
    assert_eq!(a.maneuver.turn, Turn::Right);
    assert!((99..=101).contains(&a.metres), "{a:?}");
    // Past its end it stands at the end, arrived: no going round.
    let end = track.at(track.duration() + 60_000);
    assert_eq!((end.x, end.y, end.kmh), (p[2].0, p[2].1, 0));
    assert_eq!(
        track.ahead(track.duration() + 5).unwrap().maneuver.turn,
        Turn::Arrive
    );
    // Straight on, it keeps its speed: 100 m at 10 m/s.
    let straight = Track::from_route(&Leg {
        points: vec![p[0], at(50.0, 0.0), p[1]],
        steps: vec![
            LegStep {
                cm_s: 1000,
                limit: None
            };
            2
        ],
        turns: vec![(2, Turn::Arrive, String::new())],
    })
    .unwrap();
    assert!(
        (9_950..10_050).contains(&straight.duration()),
        "{}",
        straight.duration()
    );
}

#[test]
fn names_fold_without_case_or_accents() {
    assert_eq!(fold("Rue d'Anvers"), "rue d anvers");
    assert_eq!(fold("  Esch-sur-Alzette "), "esch sur alzette");
    assert_eq!(fold("Tunnel René Konen"), "tunnel rene konen");
    assert_eq!(fold("Straße"), "strasse");
    assert_eq!(fold("ŒUVRE ÆØÅ"), "oeuvre aeoa");
    assert_eq!(fold("..."), "");
}

#[test]
fn search_ranks_whole_names_then_words_then_the_nearest() {
    let here = at(0.0, 0.0);
    let far = at(20_000.0, 0.0);
    let index = crate::search::build::write(
        &[
            // (name, town, kind, rank, x, y)
            ("Rue de la Gare", 1, 0, 12, far.0, far.1),
            ("Rue de la Gare", 0, 0, 12, here.0 + 4000, here.1),
            ("Avenue de la Gare", 0, 0, 10, here.0, here.1),
            ("Gare", 0, 1, 2, here.0, here.1),
            ("Garnich", 2, 1, 3, far.0, far.1),
            ("Boulevard Royal", 0, 0, 9, here.0, here.1),
        ],
        &["Luxembourg", "Ettelbruck", "Garnich"],
    );
    let p = Places::from_bytes(&index).unwrap();
    let names = |q: &str| -> Vec<(String, String)> {
        p.search(q, Some(here), 10)
            .into_iter()
            .map(|f| (f.name, f.town))
            .collect()
    };
    let gare = names("GARE");
    assert_eq!(gare[0], ("Gare".into(), "Luxembourg".into()));
    // Then the words: the nearest first.
    assert_eq!(gare[1].0, "Avenue de la Gare");
    assert_eq!(gare[2], ("Rue de la Gare".into(), "Luxembourg".into()));
    assert_eq!(gare[3], ("Rue de la Gare".into(), "Ettelbruck".into()));
    // "gar" begins Garnich too, a place: before the streets.
    let gar = names("gar");
    assert_eq!(gar[0].0, "Gare");
    assert_eq!(gar[1].0, "Garnich");
    // A town's name narrows it down.
    assert_eq!(
        names("rue gare ettel"),
        vec![("Rue de la Gare".into(), "Ettelbruck".into())]
    );
    // Accents and case typed or not.
    assert_eq!(names("bOUL roy")[0].0, "Boulevard Royal");
    assert!(names("xyz").is_empty());
    assert!(names("  ").is_empty());
    // The limit holds, the distance comes along.
    let one = p.search("gare", Some(here), 1);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].kind, FoundKind::Place);
    assert_eq!(one[0].metres, Some(0));
    let ett = p.search("rue de la gare ettelbruck", Some(here), 5);
    assert!((19_900..20_100).contains(&ett[0].metres.unwrap()));
}

#[test]
fn a_broken_index_is_refused() {
    let index = crate::search::build::write(&[("A", 0, 0, 12, 1, 1)], &["T"]);
    assert!(Places::from_bytes(&index).is_ok());
    assert!(Places::from_bytes(&index[..index.len() - 4]).is_err());
    let mut bad = index.clone();
    // The entry's town: one that is not there.
    let at = 6 * 4 + 2 * 4 + 4 * 4;
    bad[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
    assert!(Places::from_bytes(&bad).is_err());
}

/// The router on scripts/map-graph.py's network agrees with
/// scripts/route-gpx.py's drive: `LEHU_ROUTE_DIR=<dir with roads.bin>
/// LEHU_SAMPLE_GPX=<route-gpx.py's drive> cargo test -p lehu-map
/// sample_route -- --ignored --nocapture`.
#[test]
#[ignore]
fn sample_route_matches_route_gpx() {
    let dir = std::env::var("LEHU_ROUTE_DIR").expect("LEHU_ROUTE_DIR");
    let gpx = std::env::var("LEHU_SAMPLE_GPX").expect("LEHU_SAMPLE_GPX");
    let r = Router::open(std::path::Path::new(&dir)).unwrap();
    let theirs = Track::parse_gpx(&std::fs::read_to_string(gpx).unwrap()).unwrap();
    let line = theirs.line();
    let ours = r.route(line[0], None, line[line.len() - 1]).unwrap();
    let (tm, om) = (
        theirs.ahead(0).unwrap().left_metres,
        ours.ahead(0).unwrap().left_metres,
    );
    println!(
        "route-gpx.py: {tm} m, {} s, {} turns\nLEHU:         {om} m, {} s, {} turns",
        theirs.duration() / 1000,
        theirs.maneuvers().len(),
        ours.duration() / 1000,
        ours.maneuvers().len()
    );
    for (a, b) in turns(&theirs).iter().zip(
        turns(&ours)
            .iter()
            .chain(std::iter::repeat(&(Turn::Arrive, "-".to_string()))),
    ) {
        println!("  {:<40} {:?}", format!("{a:?}"), b);
    }
    assert!(om.abs_diff(tm) * 50 <= tm, "lengths differ by more than 2%");
    let (ts, os) = (theirs.duration(), ours.duration());
    assert!(os.abs_diff(ts) * 20 <= ts, "times differ by more than 5%");
    assert_eq!(turns(&theirs), turns(&ours));
}

/// What a route and a search cost on the machine they run on, from
/// scripts/map-graph.py's files: `LEHU_ROUTE_DIR=<dir> <test binary>
/// routing_cost --ignored --nocapture`.
#[test]
#[ignore]
fn routing_cost() {
    use std::time::Instant;
    let dir = std::env::var("LEHU_ROUTE_DIR").expect("LEHU_ROUTE_DIR");
    let began = Instant::now();
    let r = Router::open(std::path::Path::new(&dir)).unwrap();
    println!("open: {:.1} ms", began.elapsed().as_secs_f64() * 1000.0);
    let began = Instant::now();
    r.graph().check().unwrap();
    println!("check: {:.1} ms", began.elapsed().as_secs_f64() * 1000.0);
    let deg = |lat: f64, lon: f64| crate::view::world_from_degrees(lat, lon).unwrap();
    let drives = [
        (
            "Gare to Kirchberg",
            deg(49.604_493_7, 6.130_200_3),
            deg(49.619_188_5, 6.143_713_1),
        ),
        (
            "Esch-sur-Alzette to Clervaux",
            deg(49.4958, 5.9806),
            deg(50.0547, 6.0287),
        ),
        (
            "Wasserbillig to Steinfort",
            deg(49.7150, 6.4997),
            deg(49.6611, 5.9183),
        ),
        (
            "Schengen to Troisvierges",
            deg(49.4706, 6.3657),
            deg(50.1210, 6.0019),
        ),
    ];
    for round in 0..2 {
        for (name, from, to) in drives {
            let began = Instant::now();
            let track = r.route(from, None, to).unwrap();
            println!(
                "{}route {name}: {:.1} km, {} min, {} turns in {:.0} ms",
                if round == 0 { "first " } else { "" },
                f64::from(track.metres()) / 1000.0,
                track.duration() / 60_000,
                track.maneuvers().len(),
                began.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    let here = Some(deg(49.6116, 6.1300));
    for q in [
        "k",
        "kirch",
        "gare",
        "rue de la gare",
        "boulevard roy",
        "esch alz",
        "zzz",
    ] {
        let began = Instant::now();
        let found = r.search(q, here, 20);
        let ms = began.elapsed().as_secs_f64() * 1000.0;
        let first = found
            .first()
            .map_or(String::new(), |f| format!("{}, {}", f.name, f.town));
        println!("search {q:?}: {} in {ms:.1} ms; first {first}", found.len());
    }
}
