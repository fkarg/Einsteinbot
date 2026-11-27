//! Background map fitting; socket workers only enqueue observations and read beliefs.
use crate::estimator::Estimator;
use crate::protocol::ServerEvent;
use crate::strategy::MapBelief;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

fn live_estimator() -> Estimator {
    let mut estimator = Estimator::default();
    estimator.settings.audit_enabled = true;
    estimator
}

pub(crate) struct WorkItem {
    epoch: u64,
    watermark: u64,
    event: ServerEvent,
}

#[derive(Default)]
struct Published {
    epoch: u64,
    submitted: u64,
    processed: u64,
    belief: Option<MapBelief>,
    pending: usize,
    fit_cancel: Arc<AtomicBool>,
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
}

impl LiveMap {
    pub(crate) fn spawn(enabled: bool) -> (Self, Option<JoinHandle<()>>) {
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
        let (feed, worker) =
            Self::spawn_with(|estimator, event, cancelled| estimator.observe(event, cancelled));
        (feed, Some(worker))
    }

    pub(crate) fn spawn_with(
        mut fit: impl FnMut(&mut Estimator, &ServerEvent, &AtomicBool) -> Option<MapBelief>
        + Send
        + 'static,
    ) -> (Self, JoinHandle<()>) {
        let published = Arc::new(Mutex::new(Published::default()));
        let (sender, receiver) = mpsc::channel::<WorkItem>();
        let worker_state = Arc::clone(&published);
        let worker = thread::spawn(move || {
            let mut estimator = live_estimator();
            let mut estimator_epoch = 0;
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
                                estimator = live_estimator();
                                estimator_epoch = epoch;
                                continue;
                            }
                            let belief = estimator.step(&cancelled);
                            let mut state = worker_state.lock().unwrap();
                            if let (true, Some(belief)) = (state.epoch == epoch, belief) {
                                state.belief = Some(belief);
                            }
                            continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                } else if let Ok(item) = receiver.recv() {
                    item
                } else {
                    break;
                };
                let WorkItem {
                    epoch,
                    watermark,
                    event,
                } = item;
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
                    estimator = live_estimator();
                    estimator_epoch = epoch;
                }
                let belief = fit(&mut estimator, &event, &cancelled);
                let mut state = worker_state.lock().unwrap();
                if state.epoch == epoch {
                    state.pending -= 1;
                    state.processed = watermark;
                    if let Some(belief) = belief {
                        state.belief = Some(belief);
                    }
                }
            }
        });
        (Self { published, sender }, worker)
    }

    pub(crate) fn reset(&self) {
        let mut state = self.published.lock().unwrap();
        state.fit_cancel.store(true, Ordering::SeqCst);
        state.fit_cancel = Arc::new(AtomicBool::new(false));
        state.epoch += 1;
        state.submitted = 0;
        state.processed = 0;
        state.belief = None;
        state.pending = 0;
    }

    pub(crate) fn observe(&self, event: &ServerEvent) -> Result<u64, mpsc::SendError<WorkItem>> {
        // Keep watermark assignment and channel order atomic. Otherwise concurrent
        // producers can enqueue watermark 2 before 1 and make `processed` regress.
        let mut state = self.published.lock().unwrap();
        state.pending += 1;
        state.submitted += 1;
        let (epoch, watermark) = (state.epoch, state.submitted);
        match self.sender.send(WorkItem {
            epoch,
            watermark,
            event: event.clone(),
        }) {
            Ok(()) => Ok(watermark),
            Err(error) => {
                state.pending = state.pending.saturating_sub(1);
                state.submitted = state.submitted.saturating_sub(1);
                Err(error)
            }
        }
    }

    /// Read readiness and its corresponding belief under one lock.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (bool, Option<MapBelief>) {
        let state = self.published.lock().unwrap();
        (state.pending > 0, state.belief.clone())
    }

    pub(crate) fn snapshot_with_progress(&self) -> (MapProgress, Option<MapBelief>) {
        let state = self.published.lock().unwrap();
        (
            MapProgress {
                epoch: state.epoch,
                submitted: state.submitted,
                processed: state.processed,
                pending: state.pending,
            },
            state.belief.clone(),
        )
    }

    pub(crate) fn progress(&self) -> MapProgress {
        let state = self.published.lock().unwrap();
        MapProgress {
            epoch: state.epoch,
            submitted: state.submitted,
            processed: state.processed,
            pending: state.pending,
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
        let (feed, worker) = LiveMap::spawn_with(move |_, _, _| {
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
        let (feed, worker) = LiveMap::spawn_with(move |_, _, _| {
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
        let (feed, worker) = LiveMap::spawn_with(move |_, _, cancelled| {
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
        let (feed, worker) = LiveMap::spawn_with(move |estimator, event, cancelled| {
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
    fn shutdown_discards_queued_fits_and_joins_active_fit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, worker) = LiveMap::spawn_with(move |_, _, _| {
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
        let (feed, worker) = LiveMap::spawn_with(move |_, _, _| {
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
