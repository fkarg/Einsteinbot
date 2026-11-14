//! Background map fitting; socket workers only enqueue observations and read beliefs.
use crate::estimator::{LiveEstimator, ObservationOutcome};
use crate::protocol::ServerEvent;
use crate::strategy::MapBelief;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

#[derive(Default)]
struct Published {
    epoch: u64,
    belief: Option<MapBelief>,
}

#[derive(Clone)]
pub(crate) struct LiveMap {
    published: Arc<Mutex<Published>>,
    sender: mpsc::Sender<(u64, ServerEvent)>,
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
        let (feed, worker) = Self::spawn_with(|estimator, event| {
            if estimator.observe(event) != ObservationOutcome::Refitted {
                return None;
            }
            if let Some(error) = estimator.last_fit_error() {
                eprintln!("LiveMapFitError {error}");
                return None;
            }
            estimator.best_map().map(|(map, covariance)| {
                eprintln!(
                    "LiveMapFit sources={} observations={}",
                    map.planets.len(),
                    estimator.observations().len()
                );
                MapBelief::new(map, covariance)
            })
        });
        (feed, Some(worker))
    }

    fn spawn_with(
        mut fit: impl FnMut(&mut LiveEstimator, &ServerEvent) -> Option<MapBelief> + Send + 'static,
    ) -> (Self, JoinHandle<()>) {
        let published = Arc::new(Mutex::new(Published::default()));
        let (sender, receiver) = mpsc::channel::<(u64, ServerEvent)>();
        let worker_state = Arc::clone(&published);
        let worker = thread::spawn(move || {
            let mut estimator = LiveEstimator::default();
            let mut estimator_epoch = 0;
            while let Ok((epoch, event)) = receiver.recv() {
                if worker_state.lock().unwrap().epoch != epoch {
                    continue;
                }
                if estimator_epoch != epoch {
                    estimator.reset();
                    estimator_epoch = epoch;
                }
                if let Some(belief) = fit(&mut estimator, &event) {
                    let mut state = worker_state.lock().unwrap();
                    if state.epoch == epoch {
                        state.belief = Some(belief);
                    }
                }
            }
        });
        (Self { published, sender }, worker)
    }

    pub(crate) fn reset(&self) {
        let mut state = self.published.lock().unwrap();
        state.epoch += 1;
        state.belief = None;
    }

    pub(crate) fn observe(
        &self,
        event: &ServerEvent,
    ) -> Result<(), mpsc::SendError<(u64, ServerEvent)>> {
        let epoch = self.published.lock().unwrap().epoch;
        self.sender.send((epoch, event.clone()))
    }

    pub(crate) fn belief(&self) -> Option<MapBelief> {
        self.published.lock().unwrap().belief.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::PlanetMap;
    use std::time::Duration;

    #[test]
    fn shutdown_discards_queued_fits_and_joins_active_fit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, worker) = LiveMap::spawn_with(move |_, _| {
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
        let (feed, worker) = LiveMap::spawn_with(move |_, _| {
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
        assert!(feed.belief().is_none());
        release_tx.send(()).unwrap();
        let published = Arc::clone(&feed.published);
        drop(feed);
        worker.join().unwrap();
        assert!(published.lock().unwrap().belief.is_some());
    }
}
