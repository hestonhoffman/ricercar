//! Loading state and download progress of network sources. The engine
//! thread blocks while it opens, reads or seeks a slow stream, so a
//! separate thread reports on it.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::http::SpoolWatch;
use crate::player::{EngineEvent, EventHub};

/// Shorter waits are not reported, so that fast streams never flicker.
pub const WAIT_SHOWN_AFTER: Duration = Duration::from_millis(300);
/// Download progress is reported at most this often.
const FILL_EVERY: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    /// Source the engine waits for, and since when.
    wait: Option<(String, Instant)>,
    /// `Buffering { waiting: true }` went out for this wait.
    announced: bool,
    /// Spool of the current track and the last `seq` reported.
    spool: Option<(String, SpoolWatch, Option<u64>)>,
    last_fill: Option<Instant>,
    /// The reporting thread sleeps until notified.
    idle: bool,
    quit: bool,
}

pub struct Monitor {
    st: Mutex<State>,
    cv: Condvar,
    hub: Arc<EventHub>,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_network(uri: &str) -> bool {
    uri.starts_with("http://") || uri.starts_with("https://")
}

impl Monitor {
    pub fn spawn(hub: Arc<EventHub>) -> Arc<Monitor> {
        let m = Arc::new(Monitor {
            st: Mutex::new(State::default()),
            cv: Condvar::new(),
            hub,
        });
        let t = m.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("ricercar-buffering".into())
            .spawn(move || t.run())
        {
            tracing::warn!("no loading reports: {e}");
        }
        m
    }

    /// The engine is about to block on `uri` (opening, reading, seeking).
    /// The wait lasts until `resumed`; local sources are never reported.
    pub fn waiting(&self, uri: &str) {
        if !is_network(uri) {
            return;
        }
        let mut st = lock(&self.st);
        match &st.wait {
            Some((u, _)) if u == uri => return,
            Some(_) => self.end_wait(&mut st),
            None => {}
        }
        st.wait = Some((uri.to_string(), Instant::now()));
        // A sleeping thread with a deadline wakes before this one is due.
        if st.idle {
            self.cv.notify_all();
        }
    }

    /// Audio comes out again, or the engine no longer waits.
    pub fn resumed(&self) {
        let mut st = lock(&self.st);
        self.end_wait(&mut st);
    }

    fn end_wait(&self, st: &mut State) {
        if let Some((uri, _)) = st.wait.take()
            && std::mem::take(&mut st.announced)
        {
            self.hub.publish(EngineEvent::Buffering {
                uri,
                waiting: false,
            });
        }
    }

    /// Report the download progress of the track now playing, if spooled.
    pub fn follow(&self, uri: &str, spool: Option<SpoolWatch>) {
        let mut st = lock(&self.st);
        st.spool = spool.map(|s| (uri.to_string(), s, None));
        st.last_fill = None;
        self.cv.notify_all();
    }

    pub fn unfollow(&self) {
        lock(&self.st).spool = None;
    }

    pub fn stop(&self) {
        lock(&self.st).quit = true;
        self.cv.notify_all();
    }

    fn run(&self) {
        let mut st = lock(&self.st);
        loop {
            if st.quit {
                return;
            }
            let now = Instant::now();
            let mut wake: Option<Instant> = None;
            if !st.announced
                && let Some((uri, since)) = &st.wait
            {
                let due = *since + WAIT_SHOWN_AFTER;
                if now >= due {
                    self.hub.publish(EngineEvent::Buffering {
                        uri: uri.clone(),
                        waiting: true,
                    });
                    st.announced = true;
                } else {
                    wake = Some(due);
                }
            }
            let st_ = &mut *st;
            if let Some((uri, spool, sent)) = &mut st_.spool {
                let due = st_.last_fill.map_or(now, |t| t + FILL_EVERY);
                let mut done = false;
                let mut next = due;
                if now >= due {
                    next = now + FILL_EVERY;
                    let fill = spool.fill();
                    done = fill.done;
                    if *sent != Some(fill.seq) {
                        *sent = Some(fill.seq);
                        self.hub.publish(EngineEvent::Buffered {
                            uri: uri.clone(),
                            ranges: fill.ranges,
                            total: fill.len,
                        });
                    }
                    st_.last_fill = Some(now);
                }
                if !done {
                    wake = Some(wake.map_or(next, |w| w.min(next)));
                }
            }
            st.idle = wake.is_none();
            st = match wake {
                Some(t) => {
                    self.cv
                        .wait_timeout(st, t.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                None => self.cv.wait(st).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }
}
