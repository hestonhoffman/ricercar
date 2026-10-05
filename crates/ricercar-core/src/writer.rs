//! Files written off the calling thread. An fsync can wait seconds when
//! another process fills the disk (btrfs flushes everyone's pending
//! writes), so the UI thread and the engine-event thread hand their bytes
//! to a writer thread instead. Only the latest version of each file is
//! kept: a burst of saves ends in one write of the last one.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// How a file reaches the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Temp file + rename, synced before and after the rename
    /// ([`crate::config::write_atomic`]).
    Durable,
    /// Temp file + rename, no sync ([`crate::config::write_cache`]): never
    /// truncated, but a power cut can bring back an older version.
    Cache,
}

impl Durability {
    pub fn write(self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match self {
            Durability::Durable => crate::config::write_atomic(path, bytes),
            Durability::Cache => crate::config::write_cache(path, bytes),
        }
    }
}

type Sink = dyn Fn(&Path, &[u8], Durability) -> io::Result<()> + Send + Sync;

#[derive(Default)]
struct State {
    pending: BTreeMap<PathBuf, (Vec<u8>, Durability)>,
    /// The file being written now.
    busy: Option<PathBuf>,
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    sink: Box<Sink>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, g: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.cv.wait(g).unwrap_or_else(|e| e.into_inner())
    }
}

/// One thread writing the latest submitted version of each file.
pub struct LatestWriter {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl LatestWriter {
    pub fn new(name: &str) -> LatestWriter {
        LatestWriter::with_sink(name, |path, bytes, durability| {
            durability.write(path, bytes)
        })
    }

    /// Writes through `sink` instead of the filesystem (tests).
    pub fn with_sink(
        name: &str,
        sink: impl Fn(&Path, &[u8], Durability) -> io::Result<()> + Send + Sync + 'static,
    ) -> LatestWriter {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            sink: Box::new(sink),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(&s))
            .expect("spawn writer");
        LatestWriter {
            shared,
            thread: Mutex::new(Some(thread)),
        }
    }

    /// Queue `bytes` for `path`, replacing a version not written yet.
    /// Returns at once. Ignored after [`shutdown`](Self::shutdown).
    pub fn submit(&self, path: &Path, bytes: Vec<u8>, durability: Durability) {
        let mut st = self.shared.lock();
        if st.closed {
            return;
        }
        st.pending.insert(path.to_path_buf(), (bytes, durability));
        self.shared.cv.notify_all();
    }

    /// Write `bytes` now, on the calling thread: a version of `path` still
    /// queued is dropped and a write in progress finishes first, so nothing
    /// older lands afterwards. Works after shutdown too.
    pub fn write_now(&self, path: &Path, bytes: &[u8], durability: Durability) -> io::Result<()> {
        let mut st = self.shared.lock();
        st.pending.remove(path);
        while st.busy.is_some() {
            st = self.shared.wait(st);
        }
        st.busy = Some(path.to_path_buf());
        drop(st);
        let r = (self.shared.sink)(path, bytes, durability);
        self.shared.lock().busy = None;
        self.shared.cv.notify_all();
        r
    }

    /// Whether a version of `path` is queued or being written: the file is
    /// older than what was submitted.
    pub fn is_pending(&self, path: &Path) -> bool {
        let st = self.shared.lock();
        st.pending.contains_key(path) || st.busy.as_deref() == Some(path)
    }

    /// Wait until everything submitted so far is written.
    pub fn flush(&self) {
        let mut st = self.shared.lock();
        while !st.pending.is_empty() || st.busy.is_some() {
            st = self.shared.wait(st);
        }
    }

    /// Write what is queued, then stop the thread. Later submits are
    /// ignored.
    pub fn shutdown(&self) {
        self.shared.lock().closed = true;
        self.shared.cv.notify_all();
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = thread
            && t.thread().id() != std::thread::current().id()
        {
            let _ = t.join();
        }
    }
}

impl Drop for LatestWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(shared: &Shared) {
    let mut st = shared.lock();
    loop {
        if st.busy.is_none()
            && let Some((path, (bytes, durability))) = st.pending.pop_first()
        {
            st.busy = Some(path.clone());
            drop(st);
            if let Err(e) = (shared.sink)(&path, &bytes, durability) {
                tracing::warn!("write {}: {e}", path.display());
            }
            st = shared.lock();
            st.busy = None;
            shared.cv.notify_all();
            continue;
        }
        if st.closed && st.pending.is_empty() {
            return;
        }
        st = shared.wait(st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn many_saves_end_on_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let w = LatestWriter::new("test-writer");
        for i in 0..500 {
            w.submit(&p, format!("v{i}").into_bytes(), Durability::Cache);
        }
        w.flush();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v499");
        assert!(!w.is_pending(&p));
    }

    #[test]
    fn only_the_latest_waiting_version_is_written() {
        let gate = Arc::new(Barrier::new(2));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (g, log) = (gate.clone(), writes.clone());
        let w = LatestWriter::with_sink("test-writer", move |_, bytes, _| {
            let first = log.lock().unwrap().is_empty();
            log.lock().unwrap().push(bytes.to_vec());
            if first {
                g.wait();
            }
            Ok(())
        });
        let p = Path::new("x");
        w.submit(p, b"first".to_vec(), Durability::Cache);
        // The first write is held; these pile up behind it.
        while !w.shared.lock().pending.is_empty() {
            std::thread::yield_now();
        }
        for i in 0..10 {
            w.submit(p, format!("v{i}").into_bytes(), Durability::Cache);
        }
        assert!(w.is_pending(p));
        gate.wait();
        w.flush();
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"first".to_vec(), b"v9".to_vec()]
        );
    }

    #[test]
    fn submit_returns_while_a_write_is_stuck() {
        let gate = Arc::new(Barrier::new(2));
        let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g, h) = (gate.clone(), held.clone());
        // The first write hangs until released, like a stuck fsync.
        let w = LatestWriter::with_sink("test-writer", move |_, _, _| {
            if !h.swap(true, Ordering::SeqCst) {
                g.wait();
            }
            Ok(())
        });
        let t = Instant::now();
        for _ in 0..100 {
            w.submit(Path::new("x"), vec![0; 1024], Durability::Durable);
        }
        assert!(t.elapsed() < Duration::from_secs(1));
        gate.wait();
        w.flush();
    }

    #[test]
    fn shutdown_writes_what_is_queued_and_nothing_after() {
        let count = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::new()));
        let (c, l) = (count.clone(), last.clone());
        let w = LatestWriter::with_sink("test-writer", move |_, bytes, _| {
            std::thread::sleep(Duration::from_millis(20));
            c.fetch_add(1, Ordering::SeqCst);
            *l.lock().unwrap() = bytes.to_vec();
            Ok(())
        });
        for i in 0..5 {
            w.submit(Path::new("x"), vec![i], Durability::Cache);
        }
        w.shutdown();
        assert_eq!(*last.lock().unwrap(), vec![4]);
        let n = count.load(Ordering::SeqCst);
        w.submit(Path::new("x"), vec![9], Durability::Cache);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(count.load(Ordering::SeqCst), n);
        assert_eq!(*last.lock().unwrap(), vec![4]);
        // A synchronous write still goes through (the final save at exit).
        w.write_now(Path::new("x"), &[7], Durability::Durable)
            .unwrap();
        assert_eq!(*last.lock().unwrap(), vec![7]);
    }

    #[test]
    fn drop_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        {
            let w = LatestWriter::new("test-writer");
            w.submit(&p, b"kept".to_vec(), Durability::Cache);
        }
        assert_eq!(std::fs::read(&p).unwrap(), b"kept");
    }

    #[test]
    fn write_now_drops_an_older_queued_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let other = dir.path().join("o.json");
        let w = LatestWriter::new("test-writer");
        w.submit(&p, b"old".to_vec(), Durability::Cache);
        w.submit(&other, b"other".to_vec(), Durability::Cache);
        w.write_now(&p, b"new", Durability::Durable).unwrap();
        w.shutdown();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(std::fs::read(&other).unwrap(), b"other");
    }
}
