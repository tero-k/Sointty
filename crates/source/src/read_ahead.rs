//! Bounded read-ahead `Source` with stall reporting for slow or mounted
//! (SMB/NFS) paths.
//!
//! A dedicated worker thread owns the underlying reader and fills a bounded
//! byte queue (default 1 MiB). `Read` blocks on a condvar while the queue is
//! empty; a wait that keeps timing out with an empty queue latches the stall
//! state exposed through [`StallState`], so the engine can surface an underrun
//! instead of claiming a continuous bit-perfect run. Data arrival clears the
//! stall.
//!
//! # Cancellation is best-effort
//!
//! [`StallState::cancel`] makes a *waiting* `Read` (and any later `Read`s
//! until [`StallState::reset_cancel`] or a seek) return
//! [`io::ErrorKind::Interrupted`] promptly. The worker thread itself, however,
//! performs blocking reads on the inner reader, and an OS-mounted share read
//! may stay stuck inside the kernel until the kernel returns (e.g. a stalled
//! SMB/NFS request). Cancellation cannot abort that in-flight kernel read;
//! when the kernel eventually returns, the worker observes the shutdown or
//! seek request and proceeds normally. `Drop` therefore does not join the
//! worker: a stuck kernel read would hang the drop, so the worker is detached
//! and finishes on its own once the kernel read returns.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sointty_core::{PlayerError, Source};

use crate::map_io;

const DEFAULT_CAPACITY: usize = 1 << 20;
const CHUNK: usize = 64 * 1024;
const READ_WAIT: Duration = Duration::from_millis(100);
const SEEK_ACK_TIMEOUT: Duration = Duration::from_secs(5);

struct State {
    queue: VecDeque<u8>,
    queued: usize,
    eof: bool,
    cancel: bool,
    stalled_since: Option<Instant>,
    generation: u64,
    /// Pending absolute-position seek request posted by `Seek::seek`.
    seek: Option<u64>,
    /// Result of the most recently posted seek, once the worker acked it.
    seek_ack: Option<io::Result<u64>>,
    shutdown: bool,
    reader_error: Option<io::ErrorKind>,
    /// Bytes the worker has read from the underlying reader (all generations).
    total_read: u64,
    /// Bytes handed out to consumers (all generations).
    total_taken: u64,
}

struct Shared {
    state: Mutex<State>,
    /// Signalled when the queue gains data, EOF/error/cancel is set, or a
    /// seek is acked: everything a blocked `Read` waits on.
    data_ready: Condvar,
    /// Signalled when the queue drains below the low-water mark, a seek is
    /// posted, or shutdown is requested: everything the worker waits on.
    space_available: Condvar,
}

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared stall/cancel handle for a [`ReadAheadSource`]. Cheap to clone.
#[derive(Clone)]
pub struct StallState {
    shared: Arc<Shared>,
}

impl StallState {
    /// True while a reader is starved: the read-ahead queue is empty and a
    /// `Read` has been waiting. Cleared when data arrives, on seek, or at
    /// EOF/read error — but not by [`StallState::cancel`], so a coordinator
    /// can tell "data arrived" apart from "reads now error with
    /// `Interrupted`".
    pub fn is_stalled(&self) -> bool {
        lock(&self.shared).stalled_since.is_some()
    }

    /// When the current stall began, if stalled.
    pub fn stalled_since(&self) -> Option<Instant> {
        lock(&self.shared).stalled_since
    }

    /// Makes blocked and subsequent `Read`s on the source return
    /// [`io::ErrorKind::Interrupted`] promptly. Cancellation of an in-flight
    /// *kernel* read itself is best-effort; see the module documentation.
    /// Does not clear the stall latch: a stalled source stays stalled until
    /// data arrives (or a seek/EOF resets it), so callers can distinguish
    /// starvation from cancellation.
    pub fn cancel(&self) {
        let mut state = lock(&self.shared);
        state.cancel = true;
        self.shared.data_ready.notify_all();
        self.shared.space_available.notify_all();
    }

    /// Clears the cancel flag so `Read`s work again. The engine calls this on
    /// seek/resume/retry (a `Seek` also clears it implicitly).
    pub fn reset_cancel(&self) {
        let mut state = lock(&self.shared);
        state.cancel = false;
        self.shared.data_ready.notify_all();
    }
}

/// `Source` over a reader fed by a dedicated read-ahead worker thread with a
/// bounded byte queue (default 1 MiB via [`ReadAheadSource::open`]).
pub struct ReadAheadSource {
    shared: Arc<Shared>,
    /// Consumer-side generation; bumped locally on seek.
    generation: u64,
    /// Absolute stream position of the next byte the consumer will observe.
    pos: u64,
    size_hint: Option<u64>,
}

impl ReadAheadSource {
    /// Opens a file with the default 1 MiB read-ahead capacity.
    pub fn open(path: &Path) -> Result<(Self, Arc<StallState>), PlayerError> {
        let file = File::open(path).map_err(map_io)?;
        let size_hint = file.metadata().map_err(map_io)?.len();
        Ok(Self::with_reader(file, Some(size_hint), DEFAULT_CAPACITY))
    }

    /// Testable constructor over any reader. `capacity` bounds the total
    /// queued bytes; `size_hint` enables `SeekFrom::End`.
    pub fn with_reader<S: Read + Seek + Send + 'static>(
        reader: S,
        size_hint: Option<u64>,
        capacity: usize,
    ) -> (Self, Arc<StallState>) {
        let capacity = capacity.max(1);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                queued: 0,
                eof: false,
                cancel: false,
                stalled_since: None,
                generation: 0,
                seek: None,
                seek_ack: None,
                shutdown: false,
                reader_error: None,
                total_read: 0,
                total_taken: 0,
            }),
            data_ready: Condvar::new(),
            space_available: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("sointty-read-ahead".into())
            .spawn(move || worker_loop(reader, worker_shared, capacity))
            .expect("spawn read-ahead worker");
        let stall = Arc::new(StallState {
            shared: Arc::clone(&shared),
        });
        (
            Self {
                shared,
                generation: 0,
                pos: 0,
                size_hint,
            },
            stall,
        )
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        lock(&self.shared).queued
    }

    #[cfg(test)]
    fn total_buffered(&self) -> u64 {
        let state = lock(&self.shared);
        state.total_read - state.total_taken
    }
}

impl Read for ReadAheadSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = lock(&self.shared);
        loop {
            if state.cancel {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "read cancelled"));
            }
            if let Some(kind) = state.reader_error.take() {
                state.stalled_since = None;
                return Err(io::Error::new(kind, "read-ahead worker read error"));
            }
            if state.queued > 0 {
                let n = state.queued.min(buf.len());
                for (slot, byte) in buf[..n].iter_mut().zip(state.queue.drain(..n)) {
                    *slot = byte;
                }
                state.queued -= n;
                self.pos += n as u64;
                state.total_taken += n as u64;
                state.stalled_since = None;
                self.shared.space_available.notify_all();
                return Ok(n);
            }
            if state.eof {
                state.stalled_since = None;
                return Ok(0);
            }
            let (guard, wait) = self
                .shared
                .data_ready
                .wait_timeout(state, READ_WAIT)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
            if wait.timed_out() && state.queued == 0 && !state.eof {
                state.stalled_since.get_or_insert_with(Instant::now);
            }
        }
    }
}

impl Seek for ReadAheadSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut state = lock(&self.shared);
        let new_pos: i128 = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => self
                .size_hint
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "seek from end requires a size hint",
                    )
                })?
                .cast_signed() as i128
                + d as i128,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative seek position",
            ));
        }
        let new_pos = new_pos as u64;
        // Invalidate any bytes the worker read before this seek. Its next
        // enqueue is generation-tagged and discarded if stale.
        state.generation = state.generation.wrapping_add(1);
        self.generation = state.generation;
        self.pos = new_pos;
        state.queue.clear();
        state.queued = 0;
        state.eof = false;
        state.reader_error = None;
        state.stalled_since = None;
        state.cancel = false;
        state.seek = Some(new_pos);
        state.seek_ack = None;
        self.shared.space_available.notify_all();
        self.shared.data_ready.notify_all();
        let deadline = Instant::now() + SEEK_ACK_TIMEOUT;
        while state.seek_ack.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "read-ahead seek ack timed out (worker read may be stuck in the kernel)",
                ));
            }
            let (guard, _) = self
                .shared
                .data_ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
        match state.seek_ack.take() {
            Some(Ok(pos)) => {
                self.pos = pos;
                Ok(pos)
            }
            Some(Err(error)) => {
                let kind = error.kind();
                Err(io::Error::new(kind, "read-ahead seek failed"))
            }
            None => unreachable!("loop exits only once seek_ack is set"),
        }
    }
}

impl Source for ReadAheadSource {
    fn size_hint(&self) -> Option<u64> {
        self.size_hint
    }
}

impl Drop for ReadAheadSource {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        state.shutdown = true;
        self.shared.space_available.notify_all();
        self.shared.data_ready.notify_all();
        // Best-effort: intentionally no worker join here. A worker stuck in a
        // blocked kernel read would hang the drop; the detached worker keeps
        // the shared state alive via Arc and exits once that read returns.
    }
}

fn worker_loop<R: Read + Seek>(mut reader: R, shared: Arc<Shared>, capacity: usize) {
    let mut chunk = vec![0u8; CHUNK];
    'outer: loop {
        // Honor control requests and low-water gating before filling.
        {
            let mut state = lock(&shared);
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(pos) = state.seek.take() {
                    drop(state);
                    let ack = reader.seek(SeekFrom::Start(pos));
                    let mut state = lock(&shared);
                    state.seek_ack = Some(ack);
                    shared.data_ready.notify_all();
                    shared.space_available.notify_all();
                    continue 'outer;
                }
                if state.queued <= capacity / 2 {
                    break;
                }
                state = shared
                    .space_available
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        let (to_read, generation) = {
            let state = lock(&shared);
            (chunk.len().min(capacity - state.queued), state.generation)
        };
        if to_read == 0 {
            continue;
        }
        match reader.read(&mut chunk[..to_read]) {
            Ok(0) => {
                let mut state = lock(&shared);
                // A seek may have invalidated this in-flight read. Stale
                // terminal results must not poison the new generation.
                if generation != state.generation {
                    continue;
                }
                state.eof = true;
                state.stalled_since = None;
                shared.data_ready.notify_all();
                loop {
                    if state.shutdown {
                        return;
                    }
                    if state.seek.is_some() {
                        continue 'outer;
                    }
                    state = shared
                        .space_available
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
            Ok(n) => {
                let mut state = lock(&shared);
                state.total_read += n as u64;
                if generation == state.generation {
                    state.queue.extend(chunk[..n].iter().copied());
                    state.queued += n;
                    // Data arrived: clear the stall even when no Read is in
                    // flight (e.g. suspended decoder waiting on is_stalled).
                    state.stalled_since = None;
                    shared.data_ready.notify_all();
                }
                // Stale generation: bytes were read before a seek; discard.
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let mut state = lock(&shared);
                if generation != state.generation {
                    continue;
                }
                state.reader_error = Some(error.kind());
                state.eof = true;
                state.stalled_since = None;
                shared.data_ready.notify_all();
                loop {
                    if state.shutdown {
                        return;
                    }
                    if state.seek.is_some() {
                        continue 'outer;
                    }
                    state = shared
                        .space_available
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar as TestCondvar, Mutex as TestMutex};
    use std::thread;

    /// Deterministic byte pattern.
    fn fake_data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Cursor-like fake that injects a per-read delay, can block from a given
    /// 1-based read index until released through a gate, and logs seeks.
    struct FakeReader {
        data: Vec<u8>,
        pos: u64,
        delay: Duration,
        block_from: Option<usize>,
        gate: Arc<(TestMutex<bool>, TestCondvar)>,
        reads: Arc<AtomicUsize>,
        seek_log: Arc<TestMutex<Vec<u64>>>,
    }

    impl FakeReader {
        fn new(
            data: Vec<u8>,
            delay: Duration,
            block_from: Option<usize>,
            gate: Arc<(TestMutex<bool>, TestCondvar)>,
        ) -> Self {
            Self {
                data,
                pos: 0,
                delay,
                block_from,
                gate,
                reads: Arc::new(AtomicUsize::new(0)),
                seek_log: Arc::new(TestMutex::new(Vec::new())),
            }
        }

        fn open_gate(gate: &Arc<(TestMutex<bool>, TestCondvar)>) {
            let mut open = gate.0.lock().unwrap();
            *open = true;
            gate.1.notify_all();
        }
    }

    impl Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            if self.block_from.is_some_and(|from| n >= from) {
                let mut open = self.gate.0.lock().unwrap();
                while !*open {
                    open = self.gate.1.wait(open).unwrap();
                }
            }
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            let start = self.pos as usize;
            let available = self.data.len() - start;
            let n = buf.len().min(available);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for FakeReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let new_pos: i64 = match pos {
                SeekFrom::Start(n) => n as i64,
                SeekFrom::Current(d) => self.pos as i64 + d,
                SeekFrom::End(d) => self.data.len() as i64 + d,
            };
            if new_pos < 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
            }
            self.pos = new_pos as u64;
            self.seek_log.lock().unwrap().push(self.pos);
            Ok(self.pos)
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !condition() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[test]
    fn absorbs_bounded_delay() {
        let data = fake_data(512 * 1024);
        let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
        let reader = FakeReader::new(data.clone(), Duration::from_millis(50), None, gate);
        let (mut src, stall) =
            ReadAheadSource::with_reader(reader, Some(data.len() as u64), 1 << 20);
        let mut got = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        while got.len() < 256 * 1024 {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got.len(), 256 * 1024, "pipeline starved under 50 ms reads");
        assert_eq!(got, data[..256 * 1024], "bytes corrupted by read-ahead");
        assert!(
            !stall.is_stalled(),
            "stall latched despite flowing pipeline"
        );
    }

    #[test]
    fn prolonged_stall_marks_state() {
        let data = fake_data(1024 * 1024);
        let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
        // The worker's 2nd read blocks until the gate opens.
        let reader = FakeReader::new(data.clone(), Duration::ZERO, Some(2), Arc::clone(&gate));
        let (src, stall) = ReadAheadSource::with_reader(reader, Some(data.len() as u64), 1 << 20);
        // Consumer drains the single primed 64 KiB chunk slowly, then keeps
        // reading (blocked) so a reader is genuinely waiting on the starved
        // queue while we observe the stall state.
        let shared_src = Arc::new(TestMutex::new(src));
        let thread_src = Arc::clone(&shared_src);
        let consumer = thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while got.len() < 128 * 1024 {
                let n = thread_src.lock().unwrap().read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                thread::sleep(Duration::from_millis(2));
            }
            got
        });
        assert!(
            wait_for(|| stall.is_stalled(), Duration::from_secs(2)),
            "stall never latched while the reader was blocked"
        );
        let since = stall.stalled_since().expect("stalled_since not set");
        assert!(since <= Instant::now());
        FakeReader::open_gate(&gate);
        let got = consumer.join().unwrap();
        assert_eq!(got, data[..got.len()], "bytes corrupted across stall");
        assert!(
            wait_for(|| !stall.is_stalled(), Duration::from_secs(2)),
            "stall never cleared after data flowed"
        );
    }

    #[test]
    fn cancel_interrupts_blocked_read() {
        let data = fake_data(1024 * 1024);
        let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
        let reader = FakeReader::new(data.clone(), Duration::ZERO, Some(2), Arc::clone(&gate));
        let (mut src, stall) =
            ReadAheadSource::with_reader(reader, Some(data.len() as u64), 1 << 20);
        let mut buf = [0u8; 1024];
        let mut primed = 0;
        while primed < 64 * 1024 {
            primed += src.read(&mut buf).unwrap();
        }
        assert_eq!(primed, 64 * 1024);

        let shared_src = Arc::new(TestMutex::new(src));
        let thread_src = Arc::clone(&shared_src);
        let handle = thread::spawn(move || {
            let mut b = [0u8; 16];
            thread_src.lock().unwrap().read(&mut b)
        });
        // Ensure the reader thread is parked in the queue wait.
        thread::sleep(Duration::from_millis(300));
        stall.cancel();
        let started = Instant::now();
        let result = handle.join().unwrap();
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::Interrupted,
            "cancelled read did not return Interrupted"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancelled read took too long to wake"
        );

        // Reads keep failing until reset.
        let again = shared_src.lock().unwrap().read(&mut [0u8; 16]);
        assert_eq!(again.unwrap_err().kind(), io::ErrorKind::Interrupted);

        // The blocked reader starved for over 100 ms before cancelling, so
        // the stall latched — and cancel alone must not clear it: only data
        // arrival (or seek/EOF) does.
        assert!(
            stall.is_stalled(),
            "cancel must not clear the stall latch"
        );
        assert!(stall.stalled_since().is_some());

        // Let the stuck worker read finish so data becomes available, then resume.
        FakeReader::open_gate(&gate);
        thread::sleep(Duration::from_millis(300));
        stall.reset_cancel();
        let mut got = Vec::new();
        while got.len() < 64 * 1024 {
            let n = shared_src.lock().unwrap().read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got.len(), 64 * 1024);
        assert_eq!(got, data[64 * 1024..128 * 1024], "bytes lost across cancel");
        assert!(
            !stall.is_stalled(),
            "stall must clear when data arrives after reset"
        );
    }

    #[test]
    fn seek_discards_and_repositions() {
        let data = fake_data(1024 * 1024);
        let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
        let seek_log = Arc::new(TestMutex::new(Vec::new()));
        let mut reader = FakeReader::new(data.clone(), Duration::from_millis(20), None, gate);
        reader.seek_log = Arc::clone(&seek_log);
        let (mut src, _stall) =
            ReadAheadSource::with_reader(reader, Some(data.len() as u64), 1 << 20);
        let mut buf = [0u8; 8192];

        let mut first = Vec::new();
        while first.len() < 100_000 {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            first.extend_from_slice(&buf[..n]);
        }
        first.truncate(100_000);
        assert_eq!(first, data[..100_000]);

        // Rapid seeks back-to-back while the worker is likely mid-read: the
        // generation guard must discard pre-seek bytes.
        assert_eq!(src.seek(SeekFrom::Start(50_000)).unwrap(), 50_000);
        assert_eq!(src.seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(
            *seek_log.lock().unwrap(),
            vec![50_000, 0],
            "worker did not honor both seeks in order"
        );
        let mut second = Vec::new();
        while second.len() < 100_000 {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            second.extend_from_slice(&buf[..n]);
        }
        second.truncate(100_000);
        assert_eq!(second, data[..100_000], "stale pre-seek bytes delivered");

        // Relative seek against the consumer position.
        let pos = src.seek(SeekFrom::Current(0)).unwrap();
        let target = pos - 1000;
        assert_eq!(src.seek(SeekFrom::Current(-1000)).unwrap(), target);
        let mut third = Vec::new();
        while third.len() < 1000 {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            third.extend_from_slice(&buf[..n]);
        }
        third.truncate(1000);
        assert_eq!(third, data[target as usize..pos as usize]);

        // SeekFrom::End resolves through the size hint.
        assert_eq!(src.seek(SeekFrom::End(-64 * 1024)).unwrap(), 960 * 1024);
        let mut tail = Vec::new();
        while tail.len() < 64 * 1024 {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
        }
        assert_eq!(tail, data[960 * 1024..]);
    }

    #[test]
    fn capacity_respected() {
        let len = 4 * 1024 * 1024;
        let data = fake_data(len);
        let capacity = 256 * 1024;
        let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
        let reader = FakeReader::new(data.clone(), Duration::ZERO, None, gate);
        let (mut src, _stall) =
            ReadAheadSource::with_reader(reader, Some(len as u64), capacity);
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < len {
            assert!(
                src.queued_bytes() <= capacity,
                "queue exceeded capacity: {}",
                src.queued_bytes()
            );
            assert!(
                src.total_buffered() <= capacity as u64,
                "worker ran more than capacity ahead: {}",
                src.total_buffered()
            );
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got.len(), len, "did not reach EOF");
        assert_eq!(got, data, "bytes corrupted under capacity pressure");
        assert!(
            src.total_buffered() <= capacity as u64,
            "worker kept more than capacity buffered at EOF"
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum StaleTerminal {
        Eof,
        Error,
    }

    /// Blocks the first terminal read so a seek can invalidate its generation
    /// before it returns EOF (or an error).
    struct StaleTerminalReader {
        pos: usize,
        terminal: StaleTerminal,
        terminal_pending: bool,
        entered: Arc<AtomicBool>,
        gate: Arc<(TestMutex<bool>, TestCondvar)>,
    }

    impl Read for StaleTerminalReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let data = b"ABCD";
            if self.pos < data.len() {
                let n = (data.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&data[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.terminal_pending {
                self.terminal_pending = false;
                self.entered.store(true, Ordering::SeqCst);
                let mut open = self.gate.0.lock().unwrap();
                while !*open {
                    open = self.gate.1.wait(open).unwrap();
                }
                return match self.terminal {
                    StaleTerminal::Eof => Ok(0),
                    StaleTerminal::Error => Err(io::ErrorKind::Other.into()),
                };
            }
            Ok(0)
        }
    }

    impl Seek for StaleTerminalReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            if let SeekFrom::Start(n) = pos {
                self.pos = n as usize;
                Ok(n)
            } else {
                Err(io::ErrorKind::Unsupported.into())
            }
        }
    }

    #[test]
    fn seek_ignores_stale_eof_and_error() {
        for terminal in [StaleTerminal::Eof, StaleTerminal::Error] {
            let entered = Arc::new(AtomicBool::new(false));
            let gate = Arc::new((TestMutex::new(false), TestCondvar::new()));
            let reader = StaleTerminalReader {
                pos: 0,
                terminal,
                terminal_pending: true,
                entered: Arc::clone(&entered),
                gate: Arc::clone(&gate),
            };
            let (mut src, _stall) = ReadAheadSource::with_reader(reader, Some(4), 64);
            let mut buf = [0u8; 4];
            assert_eq!(src.read(&mut buf).unwrap(), 4);
            assert_eq!(&buf, b"ABCD");
            assert!(wait_for(|| entered.load(Ordering::SeqCst), Duration::from_secs(1)));

            let shared = Arc::clone(&src.shared);
            let seeking = thread::spawn(move || {
                src.seek(SeekFrom::Start(0)).unwrap();
                let mut buf = [0u8; 4];
                (src.read(&mut buf).unwrap(), buf)
            });
            assert!(
                wait_for(|| lock(&shared).seek.is_some(), Duration::from_secs(1)),
                "seek was not posted before stale {terminal:?} returned"
            );
            FakeReader::open_gate(&gate);
            let (n, buf) = seeking.join().unwrap();
            assert_eq!(n, 4, "stale {terminal:?} poisoned the new generation");
            assert_eq!(&buf, b"ABCD");
        }
    }
}
