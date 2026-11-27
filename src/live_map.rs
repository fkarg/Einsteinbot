//! Background map fitting; socket workers only enqueue observations and read beliefs.
use crate::estimator::{Estimator, Settings, UpdateReport};
use crate::game_stats::{MapRecord, RECENT_MAPS, Settled};
use crate::protocol::{Planet, ServerEvent};
use crate::strategy::MapBelief;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) enum WorkItem {
    Event {
        epoch: u64,
        watermark: u64,
        event: ServerEvent,
        arrived: Instant,
    },
    /// Settle the current and the retired estimators for up to `per_map` each, send each result
    /// to `reply` as it is ready, then stop the worker.
    Settle {
        per_map: Duration,
        reply: mpsc::Sender<Settled>,
    },
}

/// Whether an event is an observed path (the unit of map lag the inspector reports).
fn is_path(event: &ServerEvent) -> bool {
    matches!(event, ServerEvent::ShotFin { .. })
}

#[derive(Default)]
struct Published {
    epoch: u64,
    submitted: u64,
    processed: u64,
    belief: Option<MapBelief>,
    /// Work and timings of the estimator update that published `belief` (`None` when the fit
    /// function does not report, as in tests).
    report: Option<UpdateReport>,
    pending: usize,
    /// Queued or in-progress paths (`ShotFin`) that `belief` has not seen yet.
    pending_paths: usize,
    fit_cancel: Arc<AtomicBool>,
    /// The recent map epochs, the current one last (for the stop-time statistics).
    maps: VecDeque<MapRecord>,
}

impl Published {
    /// The record of the current epoch.
    fn current(&mut self) -> &mut MapRecord {
        if self.maps.back().is_none_or(|m| m.epoch != self.epoch) {
            self.maps.push_back(MapRecord {
                epoch: self.epoch,
                ..MapRecord::default()
            });
            if self.maps.len() > RECENT_MAPS {
                self.maps.pop_front();
            }
        }
        self.maps.back_mut().expect("just ensured")
    }

    /// Publish `belief` of the current epoch with its update report and estimator summary.
    fn publish(&mut self, belief: MapBelief, estimator: &Estimator) {
        let report = estimator.last_report().cloned();
        let record = self.current();
        record.belief = Some(belief.clone());
        record.report = report.clone();
        record.summary = Some(estimator.summary());
        self.belief = Some(belief);
        self.report = report;
    }
}

#[derive(Clone)]
pub(crate) struct LiveMap {
    published: Arc<Mutex<Published>>,
    sender: mpsc::Sender<WorkItem>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MapProgress {
    pub epoch: u64,
    pub submitted: u64,
    pub processed: u64,
    pub pending: usize,
    pub pending_paths: usize,
}

/// Run `estimator`'s pending work (and any path ingested by a cancelled update) for up to
/// `per_map`, each slice cut at the time left.
fn settle(estimator: &mut Estimator, epoch: u64, per_map: Duration) -> Settled {
    let started = Instant::now();
    let deadline = started + per_map;
    let never = AtomicBool::new(false);
    let ceiling = estimator.settings.slice_ceiling;
    estimator.observe_batch(&[], &never);
    while estimator.search_pending() && Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        estimator.settings.slice_ceiling = Some(ceiling.map_or(left, |c| c.min(left)));
        estimator.step(&never);
    }
    estimator.settings.slice_ceiling = ceiling;
    Settled {
        epoch,
        belief: estimator.belief().cloned(),
        summary: estimator.summary(),
        observations: estimator.observations(),
        settle_ms: started.elapsed().as_secs_f64() * 1e3,
    }
}

impl LiveMap {
    pub(crate) fn spawn(enabled: bool, settings: Settings) -> (Self, Option<JoinHandle<()>>) {
        if !enabled {
            let (sender, _) = mpsc::channel();
            return (
                Self {
                    published: Arc::new(Mutex::new(Published::default())),
                    sender,
                },
                None,
            );
        }
        let (feed, worker) = Self::spawn_with(settings, |estimator, event, cancelled| {
            estimator.observe(event, cancelled)
        });
        (feed, Some(worker))
    }

    pub(crate) fn spawn_with(
        settings: Settings,
        mut fit: impl FnMut(&mut Estimator, &ServerEvent, &AtomicBool) -> Option<MapBelief>
        + Send
        + 'static,
    ) -> (Self, JoinHandle<()>) {
        let published = Arc::new(Mutex::new(Published::default()));
        let (sender, receiver) = mpsc::channel::<WorkItem>();
        let worker_state = Arc::clone(&published);
        let worker = thread::spawn(move || {
            // Without the background audit (`Settings::audit_enabled`, off since P13).
            let mut estimator = Estimator::new(settings.clone());
            let mut estimator_epoch = 0;
            // Estimators of the previous epochs, newest last, kept for the stop-time settle.
            let mut retired: VecDeque<(u64, Estimator)> = VecDeque::new();
            let new_epoch = |estimator: &mut Estimator,
                             estimator_epoch: &mut u64,
                             retired: &mut VecDeque<(u64, Estimator)>,
                             epoch| {
                let old = std::mem::replace(estimator, Estimator::new(settings.clone()));
                retired.push_back((*estimator_epoch, old));
                if retired.len() >= RECENT_MAPS {
                    retired.pop_front();
                }
                *estimator_epoch = epoch;
            };
            loop {
                // With search work left, events still go first; the work runs while none waits.
                let item = if estimator.search_pending() {
                    match receiver.try_recv() {
                        Ok(item) => item,
                        Err(mpsc::TryRecvError::Empty) => {
                            let (epoch, cancelled) = {
                                let state = worker_state.lock().unwrap();
                                (state.epoch, Arc::clone(&state.fit_cancel))
                            };
                            if estimator_epoch != epoch {
                                new_epoch(
                                    &mut estimator,
                                    &mut estimator_epoch,
                                    &mut retired,
                                    epoch,
                                );
                                continue;
                            }
                            let belief = estimator.step(&cancelled);
                            let mut state = worker_state.lock().unwrap();
                            if let (true, Some(belief)) = (state.epoch == epoch, belief) {
                                state.publish(belief, &estimator);
                            }
                            continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                } else if estimator.audit_running() {
                    // A finished audit is adopted even if no event comes.
                    match receiver.recv_timeout(Duration::from_millis(200)) {
                        Ok(item) => item,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else if let Ok(item) = receiver.recv() {
                    item
                } else {
                    break;
                };
                let (epoch, watermark, event, arrived) = match item {
                    WorkItem::Event {
                        epoch,
                        watermark,
                        event,
                        arrived,
                    } => (epoch, watermark, event, arrived),
                    WorkItem::Settle { per_map, reply } => {
                        // Newest map first, maps without paths left out; a dropped receiver
                        // only ends the reporting.
                        let maps = std::iter::once((estimator_epoch, &mut estimator))
                            .chain(retired.iter_mut().rev().map(|(e, est)| (*e, est)))
                            .filter(|(_, estimator)| estimator.summary().paths > 0);
                        for (epoch, estimator) in maps {
                            if reply.send(settle(estimator, epoch, per_map)).is_err() {
                                break;
                            }
                        }
                        break;
                    }
                };
                let cancelled = {
                    let state = worker_state.lock().unwrap();
                    if state.epoch != epoch {
                        continue;
                    }
                    Arc::clone(&state.fit_cancel)
                };
                if estimator_epoch != epoch {
                    // The canonical connection sends a complete player snapshot
                    // after reset; old queued leave events may have been discarded.
                    new_epoch(&mut estimator, &mut estimator_epoch, &mut retired, epoch);
                }
                let belief = fit(&mut estimator, &event, &cancelled);
                let mut state = worker_state.lock().unwrap();
                if state.epoch == epoch {
                    state.pending -= 1;
                    state.pending_paths -= usize::from(is_path(&event));
                    state.processed = watermark;
                    if let Some(belief) = belief {
                        state.publish(belief, &estimator);
                        if is_path(&event) {
                            state
                                .current()
                                .lag
                                .add(arrived.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                }
            }
        });
        (Self { published, sender }, worker)
    }

    /// A new map epoch.
    pub(crate) fn reset(&self) {
        let mut state = self.published.lock().unwrap();
        state.fit_cancel.store(true, Ordering::SeqCst);
        state.fit_cancel = Arc::new(AtomicBool::new(false));
        state.epoch += 1;
        state.submitted = 0;
        state.processed = 0;
        state.belief = None;
        state.report = None;
        state.pending = 0;
        state.pending_paths = 0;
        state.current();
    }

    /// The disclosed planets of the current map epoch (statistics only, never estimator input).
    pub(crate) fn set_truth(&self, planets: Vec<Planet>) {
        self.published.lock().unwrap().current().truth = Some(planets);
    }

    /// The capture recording the current map epoch.
    pub(crate) fn set_capture(&self, capture: i64) {
        self.published.lock().unwrap().current().capture = Some(capture);
    }

    pub(crate) fn observe(&self, event: &ServerEvent) -> Result<u64, mpsc::SendError<WorkItem>> {
        // Keep watermark assignment and channel order atomic. Otherwise concurrent
        // producers can enqueue watermark 2 before 1 and make `processed` regress.
        let mut state = self.published.lock().unwrap();
        state.pending += 1;
        state.pending_paths += usize::from(is_path(event));
        state.submitted += 1;
        let (epoch, watermark) = (state.epoch, state.submitted);
        match self.sender.send(WorkItem::Event {
            epoch,
            watermark,
            event: event.clone(),
            arrived: Instant::now(),
        }) {
            Ok(()) => Ok(watermark),
            Err(error) => {
                state.pending = state.pending.saturating_sub(1);
                state.pending_paths = state
                    .pending_paths
                    .saturating_sub(usize::from(is_path(event)));
                state.submitted = state.submitted.saturating_sub(1);
                Err(error)
            }
        }
    }

    /// The records of the recent map epochs as they are now, the current one with its queue.
    pub(crate) fn maps(&self) -> Vec<MapRecord> {
        let mut state = self.published.lock().unwrap();
        let pending_paths = state.pending_paths;
        state.current().pending_paths = pending_paths;
        state.maps.iter().cloned().collect()
    }

    /// Ask the worker to settle each recent map for up to `per_map` after the work queued now,
    /// then stop. The results arrive on the returned channel, newest map first; it closes when
    /// the worker is done (at once without a worker).
    pub(crate) fn settle(&self, per_map: Duration) -> mpsc::Receiver<Settled> {
        let (reply, results) = mpsc::channel();
        let _ = self.sender.send(WorkItem::Settle { per_map, reply });
        results
    }

    /// Read readiness and its corresponding belief under one lock.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (bool, Option<MapBelief>) {
        let state = self.published.lock().unwrap();
        (state.pending > 0, state.belief.clone())
    }

    /// Progress, the latest belief and the report of the update that published it, under one lock.
    pub(crate) fn snapshot_with_progress(
        &self,
    ) -> (MapProgress, Option<MapBelief>, Option<UpdateReport>) {
        let state = self.published.lock().unwrap();
        (state.progress(), state.belief.clone(), state.report.clone())
    }

    pub(crate) fn progress(&self) -> MapProgress {
        self.published.lock().unwrap().progress()
    }
}

impl Published {
    fn progress(&self) -> MapProgress {
        MapProgress {
            epoch: self.epoch,
            submitted: self.submitted,
            processed: self.processed,
            pending: self.pending,
            pending_paths: self.pending_paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::PlanetMap;
    use std::time::Duration;

    #[test]
    fn latest_belief_remains_available_while_next_fit_runs_and_reset_clears_it() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut fits = 0;
        let (feed, worker) = LiveMap::spawn_with(Settings::default(), move |_, _, _| {
            fits += 1;
            if fits == 2 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Some(MapBelief::new(PlanetMap { planets: vec![] }, None))
        });
        feed.observe(&ServerEvent::PlayerLeave { player_id: 1 })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while feed.snapshot().1.is_none() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        feed.observe(&ServerEvent::PlayerLeave { player_id: 2 })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (updating, belief) = feed.snapshot();
        assert!(updating);
        assert!(belief.is_some());
        feed.reset();
        assert!(feed.snapshot().1.is_none());
        release_tx.send(()).unwrap();
        drop(feed);
        worker.join().unwrap();
    }

    #[test]
    fn observation_watermark_advances_only_after_ordered_processing_finishes() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut fits = 0;
        let (feed, worker) = LiveMap::spawn_with(Settings::default(), move |_, _, _| {
            fits += 1;
            if fits == 1 {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            Some(MapBelief::new(PlanetMap { planets: vec![] }, None))
        });
        let event = |player_id| ServerEvent::PlayerLeave { player_id };
        assert_eq!(feed.observe(&event(1)).unwrap(), 1);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(feed.observe(&event(2)).unwrap(), 2);
        assert_eq!(
            feed.progress(),
            MapProgress {
                epoch: 0,
                submitted: 2,
                processed: 0,
                pending: 2,
                pending_paths: 0,
            }
        );
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while feed.progress().processed < 2 {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(feed.progress().pending, 0);
        drop(feed);
        worker.join().unwrap();
    }

    #[test]
    fn reset_cancels_active_fit_and_does_not_leave_pending_evidence() {
        let (started_tx, started_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let (feed, worker) = LiveMap::spawn_with(Settings::default(), move |_, _, cancelled| {
            started_tx.send(()).unwrap();
            while !cancelled.load(Ordering::SeqCst) {
                thread::yield_now();
            }
            cancelled_tx.send(()).unwrap();
            None
        });
        feed.observe(&ServerEvent::PlayerLeave { player_id: 1 })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(feed.snapshot().0);
        feed.reset();
        cancelled_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!feed.snapshot().0);
        assert!(feed.snapshot().1.is_none());
        drop(feed);
        worker.join().unwrap();
    }

    #[test]
    fn reset_snapshot_drops_a_departed_player_even_when_leave_was_queued() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (feed, worker) =
            LiveMap::spawn_with(Settings::default(), move |estimator, event, cancelled| {
                if matches!(event, ServerEvent::PlayerLeave { player_id: 99 }) {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    return None;
                }
                let result = estimator.observe(event, cancelled);
                if matches!(event, ServerEvent::ShotFin { .. }) {
                    done_tx.send(()).unwrap();
                }
                result
            });
        feed.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 10.0,
            y: 10.0,
        })
        .unwrap();
        feed.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 100.0,
            y: 100.0,
        })
        .unwrap();
        feed.observe(&ServerEvent::PlayerLeave { player_id: 99 })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        feed.observe(&ServerEvent::PlayerLeave { player_id: 2 })
            .unwrap();
        feed.reset();
        feed.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 10.0,
            y: 10.0,
        })
        .unwrap();
        feed.observe(&ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: (95..=100)
                .map(|x| crate::protocol::Point {
                    x: x as f32,
                    y: 100.0,
                })
                .collect(),
        })
        .unwrap();
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let published = Arc::clone(&feed.published);
        drop(feed);
        worker.join().unwrap();
        assert_eq!(
            published
                .lock()
                .unwrap()
                .belief
                .as_ref()
                .unwrap()
                .contact_points
                .len(),
            1
        );
    }

    #[test]
    fn pending_paths_count_queued_shots_and_the_report_travels_with_its_belief() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, worker) =
            LiveMap::spawn_with(Settings::default(), move |estimator, event, cancelled| {
                if matches!(event, ServerEvent::PlayerLeave { .. }) {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                estimator.observe(event, cancelled)
            });
        let shot = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: (95..=100)
                .map(|x| crate::protocol::Point {
                    x: x as f32,
                    y: 100.0,
                })
                .collect(),
        };
        feed.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 95.0,
            y: 100.0,
        })
        .unwrap();
        feed.observe(&ServerEvent::PlayerLeave { player_id: 9 })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        feed.observe(&shot).unwrap();
        feed.observe(&shot).unwrap();
        let progress = feed.progress();
        assert_eq!((progress.pending, progress.pending_paths), (3, 2));
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while feed.progress().pending > 0 {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        let (progress, belief, report) = feed.snapshot_with_progress();
        assert_eq!(progress.pending_paths, 0);
        assert!(belief.is_some());
        // The recorded shape of `map_update_report` (contract open request 7).
        let report =
            serde_json::to_value(report.expect("a fitted path reports its update")).unwrap();
        assert!(report["evaluations"].is_u64());
        assert!(report["violations"].as_array().unwrap().is_empty());
        let part = &report["parts"].as_array().unwrap()[0];
        assert!(part["part"].is_string());
        assert!(part["calls"].is_u64());
        assert!(part["total_ms"].is_f64());
        assert!(part["longest_ms"].is_f64());
        feed.reset();
        assert!(feed.snapshot_with_progress().2.is_none());
        drop(feed);
        worker.join().unwrap();
    }

    #[test]
    fn settle_reports_the_recent_maps_newest_first_and_stops_the_worker() {
        let (feed, worker) = LiveMap::spawn(true, Settings::default());
        let worker = worker.unwrap();
        let shot = |y: f32| ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: (95..=100)
                .map(|x| crate::protocol::Point { x: x as f32, y })
                .collect(),
        };
        feed.observe(&shot(100.0)).unwrap();
        // A reset discards the old epoch's queue: let the first path be fitted.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while feed.progress().processed < 1 {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        let truth = vec![Planet {
            x: 500.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        }];
        feed.reset();
        feed.set_truth(truth.clone());
        feed.set_capture(7);
        feed.observe(&shot(200.0)).unwrap();
        feed.observe(&shot(300.0)).unwrap();
        let settled: Vec<Settled> = feed.settle(Duration::from_millis(200)).iter().collect();
        let epochs: Vec<(u64, usize)> =
            settled.iter().map(|s| (s.epoch, s.summary.paths)).collect();
        assert_eq!(epochs, vec![(1, 2), (0, 1)]);
        let maps = feed.maps();
        assert_eq!(maps.iter().map(|m| m.epoch).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(maps[1].truth.as_ref(), Some(&truth));
        assert_eq!((maps[0].capture, maps[1].capture), (None, Some(7)));
        assert_eq!(maps[1].lag.paths, 2);
        assert!(maps[1].summary.is_some());
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_discards_queued_fits_and_joins_active_fit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, worker) = LiveMap::spawn_with(Settings::default(), move |_, _, _| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Some(MapBelief::new(PlanetMap { planets: vec![] }, None))
        });
        let event = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: vec![],
        };
        feed.observe(&event).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        feed.observe(&event).unwrap();
        let published = Arc::clone(&feed.published);
        feed.reset();
        drop(feed);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(published.lock().unwrap().belief.is_none());
    }

    #[test]
    fn reset_and_ingestion_do_not_wait_for_fit_and_old_fit_cannot_publish() {
        // Hold a fit open across a reset: neither the reset nor the next enqueue may block,
        // and releasing the old fit must not restore its stale belief.
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (processed_tx, processed_rx) = mpsc::channel();
        let (feed, worker) = LiveMap::spawn_with(Settings::default(), move |_, _, _| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Some(MapBelief::new(PlanetMap { planets: vec![] }, None))
        });
        let event = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: vec![],
        };
        feed.observe(&event).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // A queued old-epoch path must be discarded without fitting after the active fit.
        feed.observe(&event).unwrap();
        let reset_feed = feed.clone();
        let resetter = thread::spawn(move || {
            reset_feed.reset();
            reset_feed.observe(&event).unwrap();
            processed_tx.send(()).unwrap();
        });
        processed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        resetter.join().unwrap();
        release_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(feed.snapshot().1.is_none());
        release_tx.send(()).unwrap();
        let published = Arc::clone(&feed.published);
        drop(feed);
        worker.join().unwrap();
        assert!(published.lock().unwrap().belief.is_some());
    }
}
