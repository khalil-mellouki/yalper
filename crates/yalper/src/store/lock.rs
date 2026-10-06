//! `.yalper/lock`: makes hook processes take snapshots one at a time.
//!
//! Claude Code runs the hooks of parallel tool calls at the same time. A hook that takes a snapshot holds this
//! lock from reading the latest snapshot and stat cache until it has saved the new ones with its step, so each
//! snapshot is built on the one before. Step numbers do not depend on it: they come from the write transaction
//! that inserts the step (see `Store::write_transaction`). Hooks without a snapshot (`Stop`, `SessionEnd`) and
//! hooks that could not get the lock before their deadline record their step without taking it.

use std::fs::{File, TryLockError};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::safe_fs::{Access, OwnedDir};

/// The lock file inside `.yalper/`.
pub const LOCK_FILE: &str = "lock";

/// How long commands such as `yalper init` wait for the lock, and the longest a writer waits for another one in
/// the database. Hooks wait at most until their snapshot deadline (see `record::DEADLINE`).
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(3);

const MAX_PAUSE: Duration = Duration::from_millis(10);

/// The exclusive writer lock. Released when dropped, and by the operating system if the process dies.
#[derive(Debug)]
pub struct WriterLock {
    file: File,
}

impl WriterLock {
    /// Takes the lock on `dir`'s lock file, waiting at most `timeout` for another process to release it.
    /// Fails with [`io::ErrorKind::TimedOut`] when the wait is over.
    pub fn acquire(dir: &OwnedDir, timeout: Duration) -> io::Result<Self> {
        let file = dir.open_file(LOCK_FILE, Access::ReadWrite)?;
        let deadline = Instant::now() + timeout;
        let mut pause = Duration::from_millis(1);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::Error(error)) => return Err(error),
                Err(TryLockError::WouldBlock) => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "another process held {} for more than {} ms",
                        dir.path().join(LOCK_FILE).display(),
                        timeout.as_millis()
                    ),
                ));
            }
            thread::sleep(pause.min(left));
            pause = (pause * 2).min(MAX_PAUSE);
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Closing the file releases the lock too, but Windows may do that with a delay.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_lock_is_taken_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let start = Instant::now();
        let _lock = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(dir.path().join(LOCK_FILE).is_file());
    }

    #[test]
    fn waiting_for_a_held_lock_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let _held = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();

        let timeout = Duration::from_millis(200);
        let start = Instant::now();
        let error = WriterLock::acquire(&owned, timeout).unwrap_err();
        let waited = start.elapsed();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(waited >= timeout, "gave up after {waited:?}");
        assert!(waited < Duration::from_secs(2), "hung for {waited:?}");
    }

    #[test]
    fn a_released_lock_can_be_taken_again() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        drop(WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap());
        WriterLock::acquire(&owned, Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn a_waiting_writer_gets_the_lock_when_it_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let owned = OwnedDir::open(dir.path()).unwrap();
        let held = WriterLock::acquire(&owned, LOCK_TIMEOUT).unwrap();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| WriterLock::acquire(&owned, LOCK_TIMEOUT).map(drop));
            thread::sleep(Duration::from_millis(50));
            drop(held);
            waiter.join().unwrap().unwrap();
        });
    }
}
