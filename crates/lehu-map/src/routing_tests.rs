// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The router and the search: on small networks built here, and on a real
//! one (scripts/map-graph.py's) when one is given.

use crate::cameras::Cameras;
use crate::graph::build::{CameraWords, M_PER_PX, Road, road, write, write_with};
use crate::graph::{Graph, RouteBy, RouteOptions};
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
    let arcs_at = 26 * 4 + 4 * 4 * 4;
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
    let mut cost = |to, heading| g.route(&here, heading, to, &mut work).unwrap().cost;
    // 100 m at 50 km/h, 7.2 s; behind us, a U-turn first.
    assert!((7_000..7_400).contains(&cost(&ahead, east)));
    assert!((7_000..7_400).contains(&cost(&behind, None)));
    assert!((37_000..37_400).contains(&cost(&behind, east)));
    let path = g.route(&here, east, &behind, &mut work).unwrap();
    assert!(path.direct && path.arcs.is_empty() && !path.start_forward);
}

/// Three ways from 0 to 1, 3 km apart: straight along a slow street
/// (3.0 km at 30 km/h, 6 min), round by a country road (3.4 km at 45
/// km/h, 4.5 min), or out on a motorway and back (5.2 km, 4 min); and a
/// service area, 5, only the motorway reaches.
fn three_ways() -> (Router, [(i64, i64); 6]) {
    let nodes = [
        at(0.0, 0.0),
        at(3000.0, 0.0),
        at(500.0, 1500.0),
        at(2500.0, 1500.0),
        at(1500.0, -800.0),
        at(1500.0, 1700.0),
    ];
    let mut direct = road(0, 1, "Rue Longue");
    direct.kmh = 30;
    let way = |a, b, name, kmh, motorway| {
        let mut r = road(a, b, name);
        r.kmh = kmh;
        r.motorway = motorway;
        r
    };
    let roads = [
        direct,
        way(0, 2, "A1", 60, true),
        way(2, 3, "A1", 130, true),
        way(3, 1, "A1", 60, true),
        way(0, 4, "N7", 45, false),
        way(4, 1, "N7", 45, false),
        way(2, 5, "Aire", 30, true),
    ];
    let graph = Graph::from_bytes(&write(&nodes, &roads)).unwrap();
    graph.check().unwrap();
    (Router::new(graph, None), nodes)
}

#[test]
fn route_options_pick_different_roads() {
    let (r, n) = three_ways();
    let go = |by, avoid_motorways| {
        let o = RouteOptions {
            by,
            avoid_motorways,
        };
        let track = r
            .route_with(at(10.0, 0.0), None, at(2990.0, 0.0), &o)
            .unwrap();
        let line = track.line();
        if line.contains(&n[2]) {
            "A1"
        } else if line.contains(&n[4]) {
            "N7"
        } else {
            "Rue Longue"
        }
    };
    assert_eq!(go(RouteBy::Fastest, false), "A1");
    assert_eq!(go(RouteBy::Fastest, true), "N7");
    assert_eq!(go(RouteBy::Shortest, false), "Rue Longue");
    assert_eq!(go(RouteBy::Shortest, true), "Rue Longue");
    // The default is the fastest.
    let plain = r.route(at(10.0, 0.0), None, at(2990.0, 0.0)).unwrap();
    assert!(plain.line().contains(&n[2]));
    // Only a motorway goes to the service area: avoided, not forbidden.
    let o = RouteOptions {
        by: RouteBy::Fastest,
        avoid_motorways: true,
    };
    let track = r
        .route_with(at(10.0, 0.0), None, at(1500.0, 1690.0), &o)
        .unwrap();
    assert!(track.line().contains(&n[2]));
}

#[test]
fn each_mode_costs_what_it_says() {
    let (r, _) = three_ways();
    let g = r.graph();
    let mut work = crate::graph::Work::default();
    let snap = |p: (i64, i64)| g.snap(p.0, p.1).unwrap();
    let (from, to) = (snap(at(0.0, 0.0)), snap(at(3000.0, 0.0)));
    let cost = |by, avoid_motorways, work: &mut crate::graph::Work| {
        let o = RouteOptions {
            by,
            avoid_motorways,
        };
        g.route_with(&from, None, &to, &o, work).unwrap().cost
    };
    // The motorway: about 4 min; by distance the street, 3 km (30,000
    // dm); off motorways the country road, 4.5 min.
    let fastest = cost(RouteBy::Fastest, false, &mut work);
    assert!((240_000..250_000).contains(&fastest), "{fastest}");
    let shortest = cost(RouteBy::Shortest, false, &mut work);
    assert!((29_900..30_100).contains(&shortest), "{shortest}");
    let avoid = cost(RouteBy::Fastest, true, &mut work);
    assert!((268_000..276_000).contains(&avoid), "{avoid}");
}

#[test]
fn version_1_road_data_still_routes() {
    let nodes = [at(0.0, 0.0), at(400.0, 0.0), at(400.0, 400.0)];
    let mut m = road(0, 1, "A1");
    m.motorway = true;
    let bytes = write_with(&nodes, &[m, road(1, 2, "B")], &[], 1);
    let g = Graph::from_bytes(&bytes).unwrap();
    g.check().unwrap();
    assert!(g.v2_cameras().is_none() && g.build_id().is_none());
    assert!(!g.edge(0).motorway, "version 1 has no motorway marks");
    let r = Router::new(g, None);
    let o = RouteOptions {
        by: RouteBy::Shortest,
        avoid_motorways: true,
    };
    let t = r
        .route_with(at(5.0, 0.0), None, at(400.0, 395.0), &o)
        .unwrap();
    assert!((785.0..805.0).contains(&f64::from(t.metres())));
    // And a version LEHU does not know is refused.
    let mut later = bytes;
    later[8..12].copy_from_slice(&4u32.to_le_bytes());
    assert!(Graph::from_bytes(&later).is_err());
}

/// One street of 2 km, 0 to 1, then on to 2; a camera 500 m along it
/// watching eastwards, one at 1500 m watching both ways, one on the
/// second street watching westwards only.
fn camera_street() -> ([(i64, i64); 3], Vec<Road>, [CameraWords; 3]) {
    let nodes = [at(0.0, 0.0), at(2000.0, 0.0), at(3000.0, 0.0)];
    let dm = |east: f64| (east * 10.0) as u32;
    let cam = |east: f64, edge: u32, from_a: f64, ways: u32, limit: u32| {
        let (x, y) = at(east, 0.0);
        [x as u32, y as u32, edge, dm(from_a), ways, limit]
    };
    let cameras = [
        cam(500.0, 0, 500.0, 1, 50),
        cam(1500.0, 0, 1500.0, 3, 0),
        cam(2500.0, 1, 500.0, 2, 70),
    ];
    (nodes, vec![road(0, 1, "Est"), road(1, 2, "Ouest")], cameras)
}

#[test]
fn a_route_carries_the_cameras_that_watch_its_way() {
    let (nodes, roads, cameras) = camera_street();
    let g = Graph::from_bytes(&write(&nodes, &roads)).unwrap();
    g.check().unwrap();
    let id = g.build_id().unwrap();
    let cams = Cameras::from_bytes(&crate::cameras::write(id, &cameras), &g).unwrap();
    assert_eq!(cams.len(), 3);
    assert_eq!(cams.on(0).count(), 2);
    assert_eq!(cams.on(1).next().unwrap().limit, Some(70));
    let r = Router::new(g, None).with_cameras(Some(cams));
    assert_eq!(r.cameras(), 3);
    let east = r.route(at(10.0, 0.0), None, at(2990.0, 0.0)).unwrap();
    let c = east.cameras();
    assert_eq!(c.len(), 2, "{c:?}");
    // Along the route from the start, 10 m in.
    let from = |m: f64| f64::from(crate::metres_between(at(10.0, 0.0), at(m, 0.0)));
    assert!((c[0].metres - from(500.0)).abs() < 3.0, "{c:?}");
    assert!((c[1].metres - from(1500.0)).abs() < 3.0, "{c:?}");
    assert_eq!((c[0].limit, c[1].limit), (Some(50), None));
    let west = r.route(at(2990.0, 0.0), None, at(10.0, 0.0)).unwrap();
    let c: Vec<_> = west.cameras().iter().map(|c| c.limit).collect();
    assert_eq!(c, [Some(70), None]);
    // Starting past a camera: not on the route.
    let late = r.route(at(1000.0, 0.0), None, at(1900.0, 0.0)).unwrap();
    assert_eq!(late.cameras().len(), 1);
    // No cameras: routes as before, none on them.
    let (nodes, roads, _) = camera_street();
    let bare = Router::new(Graph::from_bytes(&write(&nodes, &roads)).unwrap(), None);
    let t = bare.route(at(10.0, 0.0), None, at(2990.0, 0.0)).unwrap();
    assert!(t.cameras().is_empty());
}

#[test]
fn cameras_from_another_build_or_broken_are_refused() {
    let (nodes, roads, cameras) = camera_street();
    let g = Graph::from_bytes(&write(&nodes, &roads)).unwrap();
    let id = g.build_id().unwrap();
    // The same network gives the same ID; another, another.
    let again = Graph::from_bytes(&write(&nodes, &roads)).unwrap();
    assert_eq!(again.build_id(), Some(id));
    let mut longer = camera_street().1;
    longer[1].kmh = 70;
    let other = Graph::from_bytes(&write(&nodes, &longer)).unwrap();
    assert_ne!(other.build_id(), Some(id));
    let bytes = crate::cameras::write(id, &cameras);
    assert!(Cameras::from_bytes(&bytes, &g).is_ok());
    let e = Cameras::from_bytes(&bytes, &other).err().unwrap();
    assert!(e.contains("not on this one"), "{e}");
    // A camera on an edge that is not there, out of order, cut short, a
    // version LEHU does not know.
    let mut bad = cameras;
    bad[2][2] = 9;
    assert!(Cameras::from_bytes(&crate::cameras::write(id, &bad), &g).is_err());
    let mut bad = cameras;
    bad.swap(0, 2);
    assert!(Cameras::from_bytes(&crate::cameras::write(id, &bad), &g).is_err());
    assert!(Cameras::from_bytes(&bytes[..bytes.len() - 4], &g).is_err());
    let mut later = bytes.clone();
    later[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert!(Cameras::from_bytes(&later, &g).is_err());

    // From a map directory: read when it matches, else left out (with a
    // line in the log) and the routes go on without.
    let dir = std::env::temp_dir().join(format!("lehu-cameras-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(crate::ROADS_FILE), write(&nodes, &roads)).unwrap();
    std::fs::write(dir.join(crate::CAMERAS_FILE), &bytes).unwrap();
    assert_eq!(Router::open(&dir).unwrap().cameras(), 3);
    std::fs::write(dir.join(crate::ROADS_FILE), write(&nodes, &longer)).unwrap();
    let r = Router::open(&dir).unwrap();
    assert_eq!(r.cameras(), 0);
    assert!(r.route(at(10.0, 0.0), None, at(2990.0, 0.0)).is_ok());
    std::fs::remove_file(dir.join(crate::CAMERAS_FILE)).unwrap();
    assert_eq!(Router::open(&dir).unwrap().cameras(), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_2_road_data_brings_its_own_cameras() {
    let (nodes, roads, cameras) = camera_street();
    let g = Graph::from_bytes(&write_with(&nodes, &roads, &cameras, 2)).unwrap();
    g.check().unwrap();
    assert_eq!(g.layer_junctions(), 0);
    let cams = Cameras::open(std::path::Path::new("/nonexistent"), &g).unwrap();
    assert_eq!(cams.len(), 3);
    let r = Router::new(g, None).with_cameras(Some(cams));
    let east = r.route(at(10.0, 0.0), None, at(2990.0, 0.0)).unwrap();
    assert_eq!(east.cameras().len(), 2);
    // A camera on an edge that is not there: refused.
    let mut bad = cameras;
    bad[2][2] = 9;
    let g = Graph::from_bytes(&write_with(&nodes, &roads, &bad, 2)).unwrap();
    assert!(Cameras::open(std::path::Path::new("/nonexistent"), &g).is_none());
}

/// A country in a line, 60 km wide: a main road (N1, 70 km/h) from Ville
/// A at 0 to Ville B at 60 km, a village street joining it each kilometre
/// (so its nodes are many, its layer junctions few), and, halfway, a
/// back road that cuts a bend of N1: the main road bends 2 km south
/// between 25 and 35 km, the back road goes straight at 90 km/h. A
/// motorway (A1, 130 km/h) runs parallel 3 km north, with links at 4 and
/// 56 km.
fn a_country() -> (Vec<(i64, i64)>, Vec<Road>) {
    let mut nodes = Vec::new();
    let mut roads = Vec::new();
    let mut node = |p: (i64, i64)| {
        nodes.push(p);
        nodes.len() as u32 - 1
    };
    // The ends: side streets 800 m south of N1 at 0 and at 60 km.
    let start = node(at(0.0, 800.0));
    let goal = node(at(60_000.0, 800.0));
    let main_road = |a, b, name| {
        let mut r = road(a, b, name);
        r.kmh = 70;
        r.main = true;
        r
    };
    // N1, a node a kilometre, bent south between 25 and 35 km.
    let south = |km: f64| {
        if (25.0..=35.0).contains(&km) {
            2000.0 * (1.0 - ((km - 30.0) / 5.0).abs())
        } else {
            0.0
        }
    };
    let n1: Vec<u32> = (0..=60)
        .map(|km| node(at(f64::from(km) * 1000.0, south(f64::from(km)))))
        .collect();
    for w in n1.windows(2) {
        roads.push(main_road(w[0], w[1], "N1"));
    }
    // A village street at each kilometre, 500 m south (none in the bend).
    for (km, &n) in n1.iter().enumerate() {
        if (25..=35).contains(&km) {
            continue;
        }
        let v = node(at(km as f64 * 1000.0 + 10.0, 500.0));
        roads.push(road(n, v, "Rue du Village"));
    }
    roads.push(road(start, n1[0], "Rue de Départ"));
    roads.push(road(n1[60], goal, "Rue d'Arrivée"));
    // The back road across the bend: shorter and faster.
    let mut back = road(n1[25], n1[35], "Chemin");
    back.kmh = 90;
    roads.push(back);
    // A1 3 km north, one way each way (two carriageways), links at 4 and
    // 56 km.
    let a1: Vec<u32> = (0..=13)
        .map(|i| node(at(4000.0 + f64::from(i) * 4000.0, -3000.0)))
        .collect();
    for w in a1.windows(2) {
        for (a, b) in [(w[0], w[1]), (w[1], w[0])] {
            let mut r = main_road(a, b, "A1");
            r.kmh = 130;
            r.motorway = true;
            r.oneway = true;
            roads.push(r);
        }
    }
    for (i, km) in [(0, 4), (13, 56)] {
        let mut link = main_road(a1[i], n1[km], "A1");
        link.kmh = 60;
        link.motorway = true;
        roads.push(link);
    }
    (nodes, roads)
}

fn country(near_m: u32) -> Router {
    let (nodes, roads) = a_country();
    let g = Graph::from_bytes(&write(&nodes, &roads))
        .unwrap()
        .with_near(near_m);
    g.check().unwrap();
    Router::new(g, None)
}

#[test]
fn the_main_roads_layer_is_built_and_checked() {
    let (nodes, roads) = a_country();
    let bytes = write(&nodes, &roads);
    let g = Graph::from_bytes(&bytes).unwrap();
    g.check().unwrap();
    // N1's kilometres with a village street are not junctions of the
    // layer, only where main roads meet (the links at 4 and 56 km, the
    // ends at 0 and 60) and the motorway's nodes, whose carriageways go
    // one way each.
    assert_eq!(g.layer_junctions(), 4 + 14);
    // Its steps no longer join: found by the check.
    let w: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let steps = w[24] as usize;
    let mut broken = bytes.clone();
    let last = broken.len() - 4 * steps;
    let code = u32::from_le_bytes(broken[last..last + 4].try_into().unwrap());
    broken[last..last + 4].copy_from_slice(&(code ^ 1).to_le_bytes());
    assert!(Graph::from_bytes(&broken).unwrap().check().is_err());
}

/// The same trip found both ways: the best of all, and over the layer.
fn both(by: RouteBy, avoid_motorways: bool) -> (Track, Track, u32, u32) {
    let o = RouteOptions {
        by,
        avoid_motorways,
    };
    let (from, to) = (at(0.0, 700.0), at(60_000.0, 700.0));
    let plain = country(0);
    let layered = country(crate::NEAR_M);
    let p = plain.route_with(from, None, to, &o).unwrap();
    let l = layered.route_with(from, None, to, &o).unwrap();
    (p, l, plain.last_settled(), layered.last_settled())
}

#[test]
fn a_long_trip_runs_on_the_main_roads_far_from_both_ends() {
    // The fastest: the motorway, both ways the same route, turn by turn,
    // the layer's search settling fewer junctions (not many fewer here:
    // N1's run is 52 km long, followed node by node from where the
    // start's surroundings end).
    let (p, l, ps, ls) = both(RouteBy::Fastest, false);
    assert_eq!(p.line(), l.line());
    assert_eq!(turns(&p), turns(&l));
    assert_eq!(p.duration(), l.duration());
    assert!(turns(&l).iter().any(|t| t.1 == "A1"), "{:?}", turns(&l));
    assert!(ls < ps, "{ls} settled on the layer, {ps} without");
    // Off motorways: N1, but the back road across the bend is 15 km from
    // either end, so the layer's route keeps to N1, a little longer.
    let (p, l, _, _) = both(RouteBy::Fastest, true);
    assert!(turns(&p).iter().any(|t| t.1 == "Chemin"), "{:?}", turns(&p));
    assert!(!turns(&l).iter().any(|t| t.1 == "Chemin" || t.1 == "A1"));
    assert!(l.duration() > p.duration());
    assert!(l.duration() < p.duration() * 11 / 10);
    // The shortest likewise: N1 with the bend, the motorway is longer.
    let (p, l, _, _) = both(RouteBy::Shortest, false);
    assert!(turns(&p).iter().any(|t| t.1 == "Chemin"));
    assert!(!turns(&l).iter().any(|t| t.1 == "A1"));
    assert!(l.metres() > p.metres() && l.metres() < p.metres() * 11 / 10);
}

#[test]
fn the_layer_comes_down_to_a_goal_in_the_middle_of_a_main_road() {
    // A village street at 49 km: in the middle of N1's run from the link
    // at 4 km to the one at 56, 7 km from either junction of the layer.
    // The fastest way is the motorway, then back along N1 from 56 km:
    // the search comes down off the layer at 56 km to follow N1.
    let plain = country(0);
    let layered = country(crate::NEAR_M);
    let (from, to) = (at(0.0, 700.0), at(49_000.0, 450.0));
    let p = plain.route(from, None, to).unwrap();
    let l = layered.route(from, None, to).unwrap();
    assert!(turns(&l).iter().any(|t| t.1 == "A1"), "{:?}", turns(&l));
    assert_eq!(p.line(), l.line());
    assert_eq!(turns(&p), turns(&l));
    // Off motorways: N1 all the way.
    let o = RouteOptions {
        by: RouteBy::Fastest,
        avoid_motorways: true,
    };
    let l = layered.route_with(from, None, to, &o).unwrap();
    assert!(!turns(&l).iter().any(|t| t.1 == "A1"), "{:?}", turns(&l));
    // And back, starting there.
    let p = plain.route(to, None, from).unwrap();
    let l = layered.route(to, None, from).unwrap();
    assert_eq!(p.line(), l.line());
}

#[test]
fn a_start_far_from_any_main_road_still_finds_its_route() {
    // A lane 8 km long to N1 at 0 km: more than NEAR_M of minor road
    // before the first main road. The search finds nothing within NEAR_M
    // of the ends, then the way within four times that.
    let (mut nodes, mut roads) = a_country();
    nodes.push(at(-8000.0, 800.0));
    let far = nodes.len() as u32 - 1;
    roads.push(road(far, 0, "Chemin Long"));
    let g = Graph::from_bytes(&write(&nodes, &roads)).unwrap();
    g.check().unwrap();
    let r = Router::new(g, None);
    let t = r
        .route(at(-7990.0, 800.0), None, at(60_000.0, 700.0))
        .unwrap();
    assert!(f64::from(t.metres()) > 68_000.0);
    assert_eq!(turns(&t).last().unwrap().0, Turn::Arrive);
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
        cameras: Vec::new(),
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
        cameras: Vec::new(),
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
fn the_nearest_fuel_parking_and_charging_are_found() {
    let here = at(0.0, 0.0);
    let p = |east: f64| at(east, 0.0);
    let pois = [
        // (name, detail, town, category, x, y): fuel, parking, charging.
        ("Aral", "", 0, 1, p(3000.0).0, p(3000.0).1),
        ("Shell", "", 1, 1, p(800.0).0, p(800.0).1),
        ("Q8", "", 0, 1, p(-1500.0).0, p(-1500.0).1),
        (
            "Parking Glacis",
            "Multi-storey, 400 spaces",
            0,
            2,
            p(200.0).0,
            p(200.0).1,
        ),
        ("Chargy", "4 points, 22 kW", 0, 3, p(5000.0).0, p(5000.0).1),
    ];
    let bytes = crate::search::build::write_with(
        &[("Rue A", 0, 0, 12, here.0, here.1)],
        &pois,
        &["Luxembourg", "Strassen"],
        2,
    );
    let places = Places::from_bytes(&bytes).unwrap();
    assert_eq!(places.count(FoundKind::Fuel), 3);
    assert_eq!(places.count(FoundKind::Parking), 1);
    assert_eq!(places.count(FoundKind::Charging), 1);
    assert_eq!(places.count(FoundKind::Street), 0);
    let fuel = places.nearest(FoundKind::Fuel, here, 2);
    let names: Vec<_> = fuel
        .iter()
        .map(|f| (f.name.as_str(), f.town.as_str()))
        .collect();
    assert_eq!(names, [("Shell", "Strassen"), ("Q8", "Luxembourg")]);
    assert_eq!(fuel[0].kind, FoundKind::Fuel);
    assert!((795..805).contains(&fuel[0].metres.unwrap()), "{fuel:?}");
    let park = places.nearest(FoundKind::Parking, here, 5);
    assert_eq!(park.len(), 1);
    assert_eq!(
        (park[0].kind, park[0].detail.as_str()),
        (FoundKind::Parking, "Multi-storey, 400 spaces")
    );
    assert_eq!(
        places.nearest(FoundKind::Charging, here, 5)[0].name,
        "Chargy"
    );
    assert!(places.nearest(FoundKind::Street, here, 5).is_empty());
    // Not found by name: the search is the streets' and places'.
    assert!(places.search("shell", Some(here), 5).is_empty());
    // Out of the order of their categories: refused.
    let mut mixed = pois;
    mixed.swap(0, 4);
    let bytes = crate::search::build::write_with(&[], &mixed, &["Luxembourg", "Strassen"], 2);
    assert!(Places::from_bytes(&bytes).is_err());
}

#[test]
fn a_version_1_index_still_searches() {
    let bytes = crate::search::build::write_with(
        &[("Rue A", 0, 0, 12, 1, 1)],
        &[("Aral", "", 0, 1, 1, 1)],
        &["T"],
        1,
    );
    let places = Places::from_bytes(&bytes).unwrap();
    assert_eq!(places.search("rue", None, 5).len(), 1);
    assert_eq!(places.count(FoundKind::Fuel), 0);
    assert!(places.nearest(FoundKind::Fuel, (1, 1), 5).is_empty());
}

#[test]
fn a_broken_index_is_refused() {
    let index = crate::search::build::write(&[("A", 0, 0, 12, 1, 1)], &["T"]);
    assert!(Places::from_bytes(&index).is_ok());
    assert!(Places::from_bytes(&index[..index.len() - 4]).is_err());
    let mut bad = index.clone();
    // The entry's town: one that is not there.
    let at = 9 * 4 + 2 * 4 + 4 * 4;
    bad[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
    assert!(Places::from_bytes(&bad).is_err());
}

#[test]
fn house_numbers_and_postcodes_are_found_on_their_streets() {
    let here = at(0.0, 0.0);
    let p = |east: f64| at(east, 0.0);
    let index = crate::search::build::write_houses(
        &[
            // (name, town, kind, rank, x, y)
            ("Main Street", 0, 0, 12, p(500.0).0, p(500.0).1),
            ("Main Street", 1, 0, 12, p(30_000.0).0, p(30_000.0).1),
            ("Mill Lane", 0, 0, 12, p(900.0).0, p(900.0).1),
            ("Ring Road", 0, 0, 9, p(1500.0).0, p(1500.0).1),
            ("Oldtown", 0, 1, 1, here.0, here.1),
        ],
        &[],
        &["Oldtown", "Newbury"],
        &[
            // (entry, number, postcode, x, y), by entry
            (0, "5", "1100", p(450.0).0, p(450.0).1),
            (0, "55", "1200", p(600.0).0, p(600.0).1),
            (0, "57/A", "1200", p(610.0).0, p(610.0).1),
            (0, "59", "1200", p(620.0).0, p(620.0).1),
            (0, "30-32", "1100", p(400.0).0, p(400.0).1),
            (1, "57", "2300", p(30_010.0).0, p(30_010.0).1),
            (2, "2", "", p(910.0).0, p(910.0).1),
        ],
        3,
    );
    let places = Places::from_bytes(&index).unwrap();
    let names = |q: &str| -> Vec<(String, String)> {
        places
            .search(q, Some(here), 10)
            .into_iter()
            .map(|f| (f.name, f.town))
            .collect()
    };
    // The number whole first, the nearest first; then the ones it begins;
    // the streets after their houses.
    let n57 = names("main st 57");
    assert_eq!(n57[0], ("Main Street 57".into(), "2300 Newbury".into()));
    assert_eq!(n57[1], ("Main Street 57/A".into(), "1200 Oldtown".into()));
    assert_eq!(n57[2].0, "Main Street");
    // A letter typed apart is the number's; a postcode keeps its houses.
    assert_eq!(names("main 57 a")[0].0, "Main Street 57/A");
    assert_eq!(
        names("main street 57 1200")[0],
        ("Main Street 57/A".into(), "1200 Oldtown".into())
    );
    assert_eq!(names("1200 main 5")[0].0, "Main Street 55");
    // A town typed: only its streets.
    assert_eq!(
        names("main 57 newbury")[0],
        ("Main Street 57".into(), "2300 Newbury".into())
    );
    assert!(
        names("main 57 newbury")
            .iter()
            .all(|n| n.1.contains("Newbury"))
    );
    // Either number of a range; the house found is where it is.
    assert_eq!(names("main 32")[0].0, "Main Street 30-32");
    let house = &places.search("mill 2", Some(here), 1)[0];
    assert_eq!(
        (house.name.as_str(), house.town.as_str()),
        ("Mill Lane 2", "Oldtown")
    );
    assert!((905..915).contains(&house.metres.unwrap()), "{house:?}");
    // No such number: the nearest on the same side, then the street.
    let n53 = names("main street 53 1200");
    assert_eq!(n53[0].0, "Main Street 55");
    assert_eq!(n53[1].0, "Main Street");
    assert_eq!(names("1200 main street 53")[0].0, "Main Street 55");
    // A street without houses is still found with a number.
    assert_eq!(names("ring 12")[0].0, "Ring Road");
    // A number alone finds nothing; the short forms are written out.
    assert!(names("57").is_empty());
    // Places still come first without a number, and the old search agrees.
    assert_eq!(names("old")[0].0, "Oldtown");
    assert_eq!(
        names("main oldtown")[0],
        ("Main Street".into(), "Oldtown".into())
    );
}

#[test]
fn the_index_reads_only_what_a_word_can_match() {
    // Two thousand streets in two towns: what the words find is what a
    // search of every entry finds.
    let names: Vec<String> = (0..2000)
        .map(|i| format!("Street {i} {}", ["Alpha", "Beta", "Gamma"][i % 3]))
        .collect();
    let entries: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), (i % 2) as u32, 0, 12, i as i64 * 1000, 0))
        .collect();
    let indexed = Places::from_bytes(&crate::search::build::write_with(
        &entries,
        &[],
        &["Northam", "Southam"],
        3,
    ))
    .unwrap();
    let scanned = Places::from_bytes(&crate::search::build::write_with(
        &entries,
        &[],
        &["Northam", "Southam"],
        2,
    ))
    .unwrap();
    for q in [
        "street",
        "gam",
        "beta south",
        "1999",
        "street 12 alpha",
        "north",
        "southam gamma",
        "zeta",
    ] {
        let a: Vec<_> = indexed
            .search(q, Some((0, 0)), 20)
            .into_iter()
            .map(|f| (f.name, f.town))
            .collect();
        let b: Vec<_> = scanned
            .search(q, Some((0, 0)), 20)
            .into_iter()
            .map(|f| (f.name, f.town))
            .collect();
        assert_eq!(a, b, "{q}");
    }
    assert!(!indexed.search("southam gamma", None, 5).is_empty());
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
    let modes = [
        ("fastest", RouteOptions::default()),
        (
            "shortest",
            RouteOptions {
                by: RouteBy::Shortest,
                avoid_motorways: false,
            },
        ),
        (
            "no motorways",
            RouteOptions {
                by: RouteBy::Fastest,
                avoid_motorways: true,
            },
        ),
    ];
    println!("{} speed cameras", r.cameras());
    for round in 0..2 {
        for (mode, options) in modes {
            for (name, from, to) in drives {
                let began = Instant::now();
                let track = r.route_with(from, None, to, &options).unwrap();
                println!(
                    "{}{mode} {name}: {:.1} km, {} min, {} turns, {} cameras in {:.0} ms",
                    if round == 0 { "first " } else { "" },
                    f64::from(track.metres()) / 1000.0,
                    track.duration() / 60_000,
                    track.maneuvers().len(),
                    track.cameras().len(),
                    began.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
    }
    for kind in [FoundKind::Fuel, FoundKind::Parking, FoundKind::Charging] {
        let began = Instant::now();
        let found = r.nearest(kind, deg(49.6116, 6.1300), 20);
        let ms = began.elapsed().as_secs_f64() * 1000.0;
        let first = found.first().map_or(String::new(), |f| {
            format!(
                "{}, {} ({}) {} m",
                f.name,
                f.town,
                f.detail,
                f.metres.unwrap_or(0)
            )
        });
        println!(
            "nearest {kind:?}: {} of {} in {ms:.2} ms; first {first}",
            found.len(),
            r.count(kind)
        );
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

/// Routes across a country, as the board finds them: what each costs in
/// time, junctions settled and bytes of roads.bin read from the card (the
/// file's pages dropped from memory first, read-ahead off).
/// `LEHU_ROUTE_DIR=<dir> <test binary> country_routes --ignored
/// --nocapture`; `LEHU_DRIVES="name:lat,lon:lat,lon;..."` names other
/// drives than Hungary's.
#[test]
#[ignore]
fn country_routes() {
    use std::time::Instant;
    let dir = std::env::var("LEHU_ROUTE_DIR").expect("LEHU_ROUTE_DIR");
    let dir = std::path::Path::new(&dir);
    let roads = dir.join(crate::graph::ROADS_FILE);
    let drives = std::env::var("LEHU_DRIVES").unwrap_or_else(|_| {
        "Budapest-Debrecen:47.4979,19.0540:47.5316,21.6273;\
         Sopron-Szeged:47.6817,16.5845:46.2530,20.1414;\
         Gyor-Pecs:47.6875,17.6504:46.0727,18.2323;\
         Ujpest-Kelenfold:47.5600,19.0900:47.4640,19.0200"
            .into()
    });
    let deg = |p: &str| {
        let (lat, lon) = p.split_once(',').unwrap();
        crate::view::world_from_degrees(lat.trim().parse().unwrap(), lon.trim().parse().unwrap())
            .unwrap()
    };
    type Drive = (String, (i64, i64), (i64, i64));
    let drives: Vec<Drive> = drives
        .split(';')
        .filter(|d| !d.trim().is_empty())
        .map(|d| {
            let mut it = d.trim().split(':');
            let name = it.next().unwrap().to_string();
            (name, deg(it.next().unwrap()), deg(it.next().unwrap()))
        })
        .collect();
    let modes = [
        ("fastest", RouteOptions::default()),
        (
            "shortest",
            RouteOptions {
                by: RouteBy::Shortest,
                avoid_motorways: false,
            },
        ),
        (
            "no motorways",
            RouteOptions {
                by: RouteBy::Fastest,
                avoid_motorways: true,
            },
        ),
    ];
    // LEHU_NEAR=<metres>: the search's near either end (0 the plain one).
    let near = std::env::var("LEHU_NEAR").map_or(crate::NEAR_M, |m| m.parse().unwrap());
    let open = |near: u32| {
        let g = Graph::open(dir).unwrap().with_near(near);
        let c = Cameras::open(dir, &g);
        Router::new(g, None).with_cameras(c)
    };
    let r = open(near);
    let plain = open(0);
    let began = Instant::now();
    r.prepare();
    println!(
        "{} junctions, {} of main roads, {} cameras; check {:.0} ms; near {near} m",
        r.graph().nodes(),
        r.graph().layer_junctions(),
        r.cameras(),
        began.elapsed().as_secs_f64() * 1000.0
    );
    for (mode, options) in modes {
        for (name, from, to) in &drives {
            // The best of all, to compare with.
            if let Ok(best) = plain.route_with(*from, None, *to, &options) {
                println!(
                    "{mode:>12} {name:<18} {:6.1} km {:5.1} min: every road everywhere",
                    f64::from(best.metres()) / 1000.0,
                    best.duration() as f64 / 60_000.0,
                );
            }
            // Cold: what the board reads from the card, for the search
            // alone and then for the whole route (its line, turns, limits
            // and cameras too).
            let g = r.graph();
            plain.graph().blob().evict(&roads);
            g.blob().evict(&roads);
            let mut work = crate::graph::Work::default();
            let (Some(s), Some(e)) = (g.snap(from.0, from.1), g.snap(to.0, to.1)) else {
                println!("{mode:>12} {name:<18} not on the network");
                continue;
            };
            let _ = g.route_with(&s, None, &e, &options, &mut work);
            let searched = g.blob().resident();
            plain.graph().blob().evict(&roads);
            g.blob().evict(&roads);
            let began = Instant::now();
            let track = r.route_with(*from, None, *to, &options);
            let cold = began.elapsed().as_secs_f64() * 1000.0;
            let read = g.blob().resident();
            // Warm: the search itself, the best of three.
            let mut warm = f64::MAX;
            for _ in 0..3 {
                let began = Instant::now();
                let _ = r.route_with(*from, None, *to, &options);
                warm = warm.min(began.elapsed().as_secs_f64() * 1000.0);
            }
            let settled = r.last_settled();
            match track {
                Ok(t) => println!(
                    "{mode:>12} {name:<18} {:6.1} km {:5.1} min {:4} turns {:2} cameras: \
                     {settled:7} settled {warm:6.1} ms warm {cold:6.1} ms cold, \
                     {:5} KB read searching, {:5} KB in all",
                    f64::from(t.metres()) / 1000.0,
                    t.duration() as f64 / 60_000.0,
                    t.maneuvers().len(),
                    t.cameras().len(),
                    searched / 1024,
                    read / 1024
                ),
                Err(e) => println!("{mode:>12} {name:<18} {e}"),
            }
        }
    }
}

/// The layer's routes against the best of all, between junctions drawn
/// at random across the network: none may cost less (the search would be
/// wrong), and how much more each costs. `LEHU_ROUTE_DIR=<dir>
/// LEHU_PAIRS=<n> <test binary> layer_against_plain --ignored --nocapture`.
#[test]
#[ignore]
fn layer_against_plain() {
    let dir = std::env::var("LEHU_ROUTE_DIR").expect("LEHU_ROUTE_DIR");
    let pairs: usize = std::env::var("LEHU_PAIRS").map_or(200, |n| n.parse().unwrap());
    let dir = std::path::Path::new(&dir);
    let layered = Graph::open(dir).unwrap();
    let plain = Graph::open(dir).unwrap().with_near(0);
    layered.check().unwrap();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut work = crate::graph::Work::default();
    for (mode, options) in [
        ("fastest", RouteOptions::default()),
        (
            "shortest",
            RouteOptions {
                by: RouteBy::Shortest,
                avoid_motorways: false,
            },
        ),
        (
            "no motorways",
            RouteOptions {
                by: RouteBy::Fastest,
                avoid_motorways: true,
            },
        ),
    ] {
        let (mut same, mut worse, mut sum, mut most) = (0, 0, 0.0, 1.0f64);
        let (mut settled_l, mut settled_p) = (0u64, 0u64);
        for _ in 0..pairs {
            let pick = |r: u64| {
                let p = layered.node((r % layered.nodes() as u64) as u32);
                layered.snap(p.0, p.1).unwrap()
            };
            // Every other pair near each other: nodes near in the file
            // are near on the map.
            let a = next();
            let b = if a % 2 == 0 {
                next()
            } else {
                (a % layered.nodes() as u64 + next() % 6000).saturating_sub(3000)
            };
            let (s, e) = (pick(a), pick(b));
            let (Some(l), Some(p)) = (
                layered.route_with(&s, None, &e, &options, &mut work),
                plain.route_with(&s, None, &e, &options, &mut work),
            ) else {
                panic!("{mode}: no route from {s:?} to {e:?}");
            };
            assert!(
                l.cost >= p.cost,
                "{mode}: {} beats the best, {}",
                l.cost,
                p.cost
            );
            let ratio = l.cost as f64 / p.cost.max(1) as f64;
            if l.cost == p.cost {
                same += 1;
            } else {
                worse += 1;
            }
            sum += ratio;
            most = most.max(ratio);
            if ratio > 1.03 {
                let km = f64::from(crate::metres_between((s.x, s.y), (e.x, e.y))) / 1000.0;
                println!(
                    "  {mode}: {:.1} % dearer, {km:.0} km apart",
                    (ratio - 1.0) * 100.0
                );
            }
            settled_l += u64::from(l.settled);
            settled_p += u64::from(p.settled);
        }
        println!(
            "{mode:>12}: {same} the best, {worse} dearer; on average {:.2} % dearer, at most \
             {:.1} %; {} settled on average, {} without the layer",
            (sum / pairs as f64 - 1.0) * 100.0,
            (most - 1.0) * 100.0,
            settled_l / pairs as u64,
            settled_p / pairs as u64
        );
    }
}
