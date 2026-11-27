//! Evaluation worlds: synthetic (`synth:SEED[:K]`), one unseen planet in an established map
//! (`w1:SEED[:DISTANCE]`, scenario-wishlist W1) and recorded maps (`capture:ID[:MAP]`). Truth is
//! for scoring only.
use einsteinbot::estimator::synthetic_world;
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, Termination, WorldParameters, simulate_shot};
use rusqlite::{Connection, OpenFlags};

#[derive(Clone, Debug)]
pub struct Ev {
    pub seq: i64,
    pub ts: i64,
    pub bot: Option<String>,
    pub event: ServerEvent,
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub seq: i64,
    pub ts: i64,
    pub bot: String,
    pub data: serde_json::Value,
}

pub struct World {
    /// Canonical id (`synth:7001:24`, `w1:3:300`, `capture:60:0`).
    pub id: String,
    pub truth: Vec<Planet>,
    /// Inbound events in order: for captures every event of the capture up to the end of the
    /// map (the ones before `start` only set up players and bot state), for synthetic worlds the
    /// paths alone.
    pub events: Vec<Ev>,
    /// Index of the map's first event (its `Planets` event for captures).
    pub start: usize,
    /// Recorded targeting decisions of the map (captures only).
    pub decisions: Vec<Decision>,
    /// Truth planets the world means to be hard (W1: the unseen one).
    pub intended: Vec<usize>,
}

impl World {
    pub fn is_capture(&self) -> bool {
        self.id.starts_with("capture:")
    }

    /// Distinct completed paths of the map, in order, with the index of their event.
    pub fn paths(&self) -> Vec<(usize, ShotObservation)> {
        let mut seen: Vec<(usize, ShotObservation)> = Vec::new();
        for (i, ev) in self.events.iter().enumerate().skip(self.start) {
            if let ev @ ServerEvent::ShotFin { .. } = &ev.event
                && let Ok(obs) = ShotObservation::try_from(ev)
                && !seen.iter().any(|(_, o)| *o == obs)
            {
                seen.push((i, obs));
            }
        }
        seen
    }
}

fn fin(obs: &ShotObservation) -> ServerEvent {
    ServerEvent::ShotFin {
        player_id: obs.player_id,
        angle: obs.angle_degrees,
        velocity: obs.velocity,
        points: obs.points.clone(),
    }
}

fn path_events(shots: &[ShotObservation]) -> Vec<Ev> {
    shots
        .iter()
        .enumerate()
        .map(|(i, obs)| Ev {
            seq: i as i64 + 1,
            ts: 1000 * (i as i64 + 1),
            bot: None,
            event: fin(obs),
        })
        .collect()
}

/// Load a world with (at least) `paths` paths where the world can make them.
pub fn load(spec: &str, paths: usize, db: &str) -> Result<World, String> {
    let parts: Vec<&str> = spec.split(':').collect();
    let num = |i: usize, default: Option<u64>| -> Result<u64, String> {
        match parts.get(i) {
            Some(v) => v.parse().map_err(|_| format!("bad number {v:?} in {spec}")),
            None => default.ok_or(format!("{spec}: missing field {i}")),
        }
    };
    match parts[0] {
        "synth" => {
            let (seed, k) = (num(1, None)?, num(2, Some(24))? as usize);
            let (truth, shots) = synthetic_world(seed, k, paths);
            Ok(World {
                id: format!("synth:{seed}:{k}"),
                truth,
                events: path_events(&shots),
                start: 0,
                decisions: vec![],
                intended: vec![],
            })
        }
        "w1" => {
            let (seed, distance) = (num(1, None)?, num(2, Some(300))? as f64);
            let (truth, shots) = w1_world(seed, distance, paths);
            Ok(World {
                id: format!("w1:{seed}:{distance}"),
                intended: vec![truth.len() - 1],
                truth,
                events: path_events(&shots),
                start: 0,
                decisions: vec![],
            })
        }
        "capture" => {
            let (id, map) = (num(1, None)? as i64, num(2, Some(0))? as usize);
            load_capture(db, id, map)
        }
        other => Err(format!(
            "unknown world kind {other:?} (synth:SEED[:K], w1:SEED[:DISTANCE], capture:ID[:MAP])"
        )),
    }
}

/// W1: three planets of the W7 generator plus one more at least `distance` from their surfaces;
/// every path passes the extra planet without touching it (its field bends them).
fn w1_world(seed: u64, distance: f64, paths: usize) -> (Vec<Planet>, Vec<ShotObservation>) {
    let world = WorldParameters::default();
    let (mut truth, _) = synthetic_world(seed, 3, 0);
    let mut state = seed ^ 0x5755_4e53_4545_4e31;
    let mut uniform = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let extra = loop {
        let radius = 20.0 + 20.0 * uniform();
        let p = Planet {
            x: radius + (world.battlefield_width - 2.0 * radius) * uniform(),
            y: radius + (world.battlefield_height - 2.0 * radius) * uniform(),
            radius,
            mass: radius.powi(3) / 10.0,
        };
        if truth
            .iter()
            .all(|t| (t.x - p.x).hypot(t.y - p.y) - t.radius - p.radius >= distance)
        {
            break p;
        }
    };
    truth.push(extra);
    let map = PlanetMap {
        planets: truth.clone(),
    };
    let extra_index = truth.len() - 1;
    let mut shots = Vec::new();
    while shots.len() < paths {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: (360.0 * uniform()).floor(),
            velocity: 8.0,
            points: vec![Point {
                x: (100.0 + (world.battlefield_width - 200.0) * uniform()) as f32,
                y: (100.0 + (world.battlefield_height - 200.0) * uniform()) as f32,
            }],
        };
        let path = simulate_shot(&map, &shot, world, 2000).unwrap();
        if path.points.len() < 40 || path.termination == Termination::PlanetCollision(extra_index) {
            continue;
        }
        shots.push(ShotObservation {
            points: path.points,
            ..shot
        });
    }
    (truth, shots)
}

pub fn open(db: &str) -> Result<Connection, String> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{db}: {e}"))
}

/// Every inbound event of a capture, in order.
pub fn capture_events(connection: &Connection, capture: i64) -> Result<Vec<Ev>, String> {
    let mut q = connection
        .prepare("SELECT sequence, timestamp_unix_ms, bot_name, event_json FROM events WHERE capture_id = ?1 AND direction = 'inbound' ORDER BY sequence")
        .map_err(|e| e.to_string())?;
    let rows = q
        .query_map([capture], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, ts, bot, json) = row.map_err(|e| e.to_string())?;
        if let Ok(RecordedEvent::Inbound(event)) = serde_json::from_str(&json) {
            out.push(Ev {
                seq,
                ts,
                bot,
                event,
            });
        }
    }
    Ok(out)
}

/// Index ranges of the maps of a capture: a map starts at a `Planets` event whose planets differ
/// from the current ones (a repeated identical `Planets` event stays in the map).
pub fn maps(events: &[Ev]) -> Vec<(usize, usize, Vec<Planet>)> {
    let mut out: Vec<(usize, usize, Vec<Planet>)> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        if let ServerEvent::Planets { planets } = &ev.event
            && out.last().is_none_or(|m| m.2 != *planets)
        {
            if let Some(last) = out.last_mut() {
                last.1 = i;
            }
            out.push((i, events.len(), planets.clone()));
        }
    }
    out
}

fn load_capture(db: &str, capture: i64, map: usize) -> Result<World, String> {
    let connection = open(db)?;
    let mut events = capture_events(&connection, capture)?;
    let all_maps = maps(&events);
    let (start, end, truth) = all_maps.get(map).cloned().ok_or(format!(
        "capture {capture} has {} map(s), not map {map}",
        all_maps.len()
    ))?;
    if truth.is_empty() {
        return Err(format!("capture {capture} map {map} discloses no planets"));
    }
    let (first_seq, end_seq) = (
        events[start].seq,
        events.get(end).map_or(i64::MAX, |e| e.seq),
    );
    events.truncate(end);
    let mut q = connection
        .prepare("SELECT sequence, timestamp_unix_ms, bot_name, data_json FROM diagnostics WHERE capture_id = ?1 AND kind = 'target_decision' ORDER BY sequence")
        .map_err(|e| e.to_string())?;
    let decisions = q
        .query_map([capture], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .filter(|(seq, ..)| *seq > first_seq && *seq < end_seq)
        .filter_map(|(seq, ts, bot, json)| {
            Some(Decision {
                seq,
                ts,
                bot: bot?,
                data: serde_json::from_str(&json).ok()?,
            })
        })
        .collect();
    Ok(World {
        id: format!("capture:{capture}:{map}"),
        truth,
        events,
        start,
        decisions,
        intended: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_paths_are_prefix_stable_and_w1_never_touches_its_extra_planet() {
        let short = load("synth:7001:6", 3, "").unwrap();
        let long = load("synth:7001:6", 5, "").unwrap();
        assert_eq!(short.truth, long.truth);
        assert_eq!(short.paths()[..], long.paths()[..3]);
        let w1 = load("w1:3:300", 6, "").unwrap();
        assert_eq!(w1.intended, vec![3]);
        let extra = &w1.truth[3];
        for (_, obs) in w1.paths() {
            assert!(obs.points.iter().all(|p| {
                (f64::from(p.x) - extra.x).hypot(f64::from(p.y) - extra.y) > extra.radius
            }));
        }
        assert!(
            w1.truth[..3].iter().all(|t| {
                (t.x - extra.x).hypot(t.y - extra.y) - t.radius - extra.radius >= 300.0
            })
        );
    }
}
