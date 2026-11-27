//! `devtool games`: the independent games of a capture database. A game is one map of one
//! capture; maps of different captures are the same game when they disclose the same planets
//! and share a recorded path (concurrent managers on one server, or a capture that continues
//! into the next map). Each game is listed once, as the instance with the most decisions.
use crate::world::{maps, open};
use einsteinbot::protocol::{Planet, RecordedEvent};
use einsteinbot::simulation::ShotObservation;
use serde_json::json;
use std::collections::HashSet;

struct Instance {
    id: String,
    planets: Vec<Planet>,
    paths: HashSet<String>,
    decisions: usize,
    started_ms: i64,
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut db, mut from, mut to) = ("einsteinbot.sqlite3".to_owned(), 0i64, i64::MAX);
    let mut min_paths = 8usize;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--db" => db = value()?,
            "--from" => from = value()?.parse().map_err(|e| format!("{e}"))?,
            "--to" => to = value()?.parse().map_err(|e| format!("{e}"))?,
            "--min-paths" => min_paths = value()?.parse().map_err(|e| format!("{e}"))?,
            _ => {
                println!(
                    "Usage: devtool games [--db PATH] [--from ID] [--to ID] [--min-paths N (8)]\nOne JSON line per independent game with disclosed planets and at least N distinct paths: its world spec, duplicates, paths, decisions, start time."
                );
                return Ok(());
            }
        }
    }
    let connection = open(&db)?;
    let ids: Vec<i64> = connection
        .prepare("SELECT id FROM captures WHERE id BETWEEN ?1 AND ?2 ORDER BY id")
        .and_then(|mut q| {
            q.query_map([from, to], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()
        })
        .map_err(|e| e.to_string())?;
    let mut instances: Vec<Instance> = Vec::new();
    for capture in ids {
        let mut q = connection
            .prepare("SELECT sequence, timestamp_unix_ms, event_json FROM events WHERE capture_id = ?1 AND direction = 'inbound' AND event_type IN ('planets', 'shot_fin') ORDER BY sequence")
            .map_err(|e| e.to_string())?;
        let events: Vec<crate::world::Ev> = q
            .query_map([capture], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .filter_map(|(seq, ts, json)| match serde_json::from_str(&json).ok()? {
                RecordedEvent::Inbound(event) => Some(crate::world::Ev {
                    seq,
                    ts,
                    bot: None,
                    event,
                }),
                _ => None,
            })
            .collect();
        let decision_seqs: Vec<i64> = connection
            .prepare("SELECT sequence FROM diagnostics WHERE capture_id = ?1 AND kind = 'target_decision' ORDER BY sequence")
            .and_then(|mut q| q.query_map([capture], |r| r.get(0))?.collect())
            .map_err(|e| e.to_string())?;
        for (index, (start, end, planets)) in maps(&events).into_iter().enumerate() {
            let paths: HashSet<String> = events[start..end]
                .iter()
                .filter_map(|e| ShotObservation::try_from(&e.event).ok())
                .map(|o| {
                    format!(
                        "{} {} {} {:?}",
                        o.player_id,
                        o.angle_degrees,
                        o.velocity,
                        o.points.first()
                    )
                })
                .collect();
            if planets.is_empty() || paths.len() < min_paths {
                continue;
            }
            let (s, e) = (
                events[start].seq,
                events.get(end).map_or(i64::MAX, |e| e.seq),
            );
            instances.push(Instance {
                id: format!("capture:{capture}:{index}"),
                planets,
                paths,
                decisions: decision_seqs.iter().filter(|d| **d > s && **d < e).count(),
                started_ms: events[start].ts,
            });
        }
    }
    // Union instances of the same game.
    let n = instances.len();
    let mut group: Vec<usize> = (0..n).collect();
    for i in 0..n {
        for j in 0..i {
            if instances[i].planets == instances[j].planets
                && !instances[i].paths.is_disjoint(&instances[j].paths)
            {
                let g = group[j];
                for k in 0..n {
                    if group[k] == group[i] {
                        group[k] = g;
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    for i in 0..n {
        if !seen.insert(group[i]) {
            continue;
        }
        let members: Vec<&Instance> = (0..n)
            .filter(|&k| group[k] == group[i])
            .map(|k| &instances[k])
            .collect();
        let best = members
            .iter()
            .max_by_key(|m| (m.decisions, m.paths.len()))
            .unwrap();
        let duplicates: Vec<&str> = members
            .iter()
            .filter(|m| m.id != best.id)
            .map(|m| m.id.as_str())
            .collect();
        println!(
            "{}",
            json!({"world": best.id, "duplicates": duplicates, "paths": best.paths.len(),
                   "decisions": best.decisions, "planets": best.planets.len(), "started_ms": best.started_ms})
        );
    }
    Ok(())
}
