//! Capture replay through the same background worker used by live targeting.
use crate::event_store::StoredEvent;
use crate::live_map::{LiveMap, WorkProfile};
use crate::online_map::OnlineStageProfile;
use crate::protocol::{RecordedEvent, ServerEvent};
use crate::strategy::BotState;
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ProfileSample {
    pub sequence: i64,
    pub recorded_at_unix_ms: i64,
    pub epoch: u64,
    pub watermark: u64,
    pub event: &'static str,
    pub queue_wait: Duration,
    pub service_time: Duration,
    pub submitted: u64,
    pub processed: u64,
    pub pending: usize,
    pub stale: bool,
    pub stages: OnlineStageProfile,
}

struct ReplayContext {
    sequence: i64,
    recorded_at_unix_ms: i64,
    event_label: &'static str,
}

/// Replay only events submitted to the live map worker, preserving capture arrival timing.
/// A speed of zero disables pacing; otherwise 1.0 replays at recorded time intervals.
pub fn replay_capture(events: &[StoredEvent], speed: f64) -> Result<Vec<ProfileSample>, String> {
    if !speed.is_finite() || speed < 0.0 {
        return Err("replay speed must be finite and nonnegative".to_owned());
    }

    let replay_started = std::time::Instant::now();
    let inbound_times = events
        .iter()
        .filter_map(|stored| {
            matches!(&stored.event, RecordedEvent::Inbound(_)).then_some(stored.timestamp_unix_ms)
        })
        .collect::<Vec<_>>();
    let capture_origin_ms = inbound_times.first().copied();
    if speed > 0.0
        && let Some(origin_ms) = capture_origin_ms
    {
        let elapsed_ms = inbound_times
            .iter()
            .map(|timestamp| timestamp.saturating_sub(origin_ms).max(0))
            .max()
            .unwrap_or_default() as f64;
        let elapsed_seconds = (elapsed_ms / 1000.0) / speed;
        if !elapsed_seconds.is_finite() || elapsed_seconds >= u64::MAX as f64 {
            return Err("replay speed produces an unrepresentable delay".to_owned());
        }
        let _ = replay_started
            .checked_add(Duration::from_secs_f64(elapsed_seconds))
            .ok_or_else(|| "replay deadline is out of range".to_owned())?;
    }

    let (feed, worker, profiles) = LiveMap::spawn_profiled();
    let mut state = BotState::default();
    let mut contexts = VecDeque::new();

    for stored in events {
        let RecordedEvent::Inbound(event) = &stored.event else {
            continue;
        };
        if speed > 0.0 {
            let origin_ms = capture_origin_ms.expect("inbound event established replay origin");
            let elapsed_ms = stored.timestamp_unix_ms.saturating_sub(origin_ms).max(0) as f64;
            let elapsed_seconds = (elapsed_ms / 1000.0) / speed;
            let delay = Duration::from_secs_f64(elapsed_seconds);
            let deadline = replay_started + delay;
            if let Some(delay) = deadline.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(delay);
            }
        }
        state.observe(event);

        match event {
            ServerEvent::Planets { .. } => {
                feed.reset();
                for (player_id, position) in state.player_positions() {
                    submit(
                        &feed,
                        &mut contexts,
                        stored,
                        "reset_seed_player_position",
                        &ServerEvent::PlayerPosition {
                            player_id: *player_id,
                            x: position.x,
                            y: position.y,
                        },
                    )?;
                }
            }
            ServerEvent::ShotFin { .. } => {
                submit(&feed, &mut contexts, stored, "shot_fin", event)?;
            }
            ServerEvent::PlayerPosition { .. } => {
                submit(&feed, &mut contexts, stored, "player_position", event)?;
            }
            ServerEvent::PlayerLeave { .. } => {
                submit(&feed, &mut contexts, stored, "player_leave", event)?;
            }
            _ => {}
        }
    }

    drop(feed);
    worker
        .join()
        .map_err(|_| "online estimator worker panicked during replay".to_owned())?;
    let mut samples = Vec::new();
    for profile in profiles {
        let context = contexts.pop_front().ok_or_else(|| {
            format!(
                "missing replay context for epoch {} watermark {}",
                profile.epoch, profile.watermark
            )
        })?;
        samples.push(ProfileSample::from_profile(
            profile,
            context.sequence,
            context.recorded_at_unix_ms,
            context.event_label,
        ));
    }
    if !contexts.is_empty() {
        return Err(format!(
            "{} submitted map events had no worker profile",
            contexts.len()
        ));
    }
    Ok(samples)
}

fn submit(
    feed: &LiveMap,
    contexts: &mut VecDeque<ReplayContext>,
    stored: &StoredEvent,
    event_label: &'static str,
    event: &ServerEvent,
) -> Result<(), String> {
    feed.observe(event)
        .map_err(|error| format!("cannot submit map event: {error}"))?;
    contexts.push_back(ReplayContext {
        sequence: stored.sequence,
        recorded_at_unix_ms: stored.timestamp_unix_ms,
        event_label,
    });
    Ok(())
}

impl ProfileSample {
    fn from_profile(
        profile: WorkProfile,
        sequence: i64,
        recorded_at_unix_ms: i64,
        event_label: &'static str,
    ) -> Self {
        Self {
            sequence,
            recorded_at_unix_ms,
            epoch: profile.epoch,
            watermark: profile.watermark,
            event: event_label,
            queue_wait: profile.queue_wait,
            service_time: profile.service_time,
            submitted: profile.submitted,
            processed: profile.processed,
            pending: profile.pending,
            stale: profile.stale,
            stages: profile.stages,
        }
    }
}
