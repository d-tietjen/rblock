#![doc = include_str!("../README.md")]

use core::sync::atomic::{AtomicUsize, Ordering};

use parking_lot_core::{ParkToken, SpinWait, UnparkToken};

/// Read-biased `RwLock`.
pub type RwLock<T> = lock_api::RwLock<RawReadBiasedRwLock, T>;

/// Read guard returned by [`RwLock::read`].
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawReadBiasedRwLock, T>;

/// Write guard returned by [`RwLock::write`].
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawReadBiasedRwLock, T>;

const READERS_PARKED: usize = 0b0001;
const WRITERS_PARKED: usize = 0b0010;
const ONE_READER: usize = 0b0100;
const PARKED_MASK: usize = READERS_PARKED | WRITERS_PARKED;
const ONE_WRITER: usize = !PARKED_MASK;
const SHARED_FAST_RETRIES: usize = 3;

/// Raw read-biased lock implementation.
///
/// The fast reader path only checks for an active writer. Parked writers do not
/// close a reader gate, which is the core read-biased behavior. Writers are
/// woken when the final reader exits, then race with new readers; that is
/// intentional for read-heavy cache shards.
#[derive(Debug)]
pub struct RawReadBiasedRwLock {
    state: AtomicUsize,
}

unsafe impl lock_api::RawRwLock for RawReadBiasedRwLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self {
        state: AtomicUsize::new(0),
    };

    type GuardMarker = lock_api::GuardNoSend;

    #[inline(always)]
    fn try_lock_exclusive(&self) -> bool {
        self.state
            .compare_exchange(0, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline(always)]
    fn lock_exclusive(&self) {
        if self
            .state
            .compare_exchange_weak(0, ONE_WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_exclusive_slow();
        }
    }

    #[inline(always)]
    unsafe fn unlock_exclusive(&self) {
        if self
            .state
            .compare_exchange(ONE_WRITER, 0, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            self.unlock_exclusive_slow();
        }
    }

    #[inline(always)]
    fn try_lock_shared(&self) -> bool {
        self.try_lock_shared_fast() || self.try_lock_shared_slow()
    }

    #[inline(always)]
    fn lock_shared(&self) {
        if !self.try_lock_shared_fast() {
            self.lock_shared_slow();
        }
    }

    #[inline(always)]
    unsafe fn unlock_shared(&self) {
        let state = self.state.fetch_sub(ONE_READER, Ordering::Release);
        if state == (ONE_READER | WRITERS_PARKED) {
            self.unlock_shared_slow();
        }
    }
}

unsafe impl lock_api::RawRwLockDowngrade for RawReadBiasedRwLock {
    #[inline(always)]
    unsafe fn downgrade(&self) {
        let state = self
            .state
            .fetch_and(ONE_READER | WRITERS_PARKED, Ordering::Release);
        if state & READERS_PARKED != 0 {
            // SAFETY: the reader queue key is derived from this lock address.
            unsafe {
                parking_lot_core::unpark_all(self.reader_queue_key(), UnparkToken(0));
            }
        }
    }
}

impl RawReadBiasedRwLock {
    #[inline(always)]
    fn writer_queue_key(&self) -> usize {
        self as *const _ as usize
    }

    #[inline(always)]
    fn reader_queue_key(&self) -> usize {
        self.writer_queue_key() + 1
    }

    #[inline(always)]
    fn try_lock_shared_fast(&self) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);

        for _ in 0..SHARED_FAST_RETRIES {
            let Some(new_state) = state.checked_add(ONE_READER) else {
                return false;
            };

            if new_state & ONE_WRITER == ONE_WRITER {
                return false;
            }

            match self.state.compare_exchange_weak(
                state,
                new_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(error) => {
                    state = error;
                }
            }
        }

        false
    }

    #[cold]
    fn try_lock_shared_slow(&self) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        while let Some(new_state) = state.checked_add(ONE_READER) {
            if new_state & ONE_WRITER == ONE_WRITER {
                break;
            }
            match self.state.compare_exchange_weak(
                state,
                new_state,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(error) => state = error,
            }
        }
        false
    }

    #[cold]
    fn lock_shared_slow(&self) {
        loop {
            let mut spin = SpinWait::new();
            let mut state = self.state.load(Ordering::Relaxed);

            loop {
                let mut backoff = SpinWait::new();
                while let Some(new_state) = state.checked_add(ONE_READER) {
                    assert_ne!(
                        new_state & ONE_WRITER,
                        ONE_WRITER,
                        "reader count overflowed",
                    );

                    match self.state.compare_exchange_weak(
                        state,
                        new_state,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(error) => state = error,
                    }

                    backoff.spin_no_yield();
                }

                if state & READERS_PARKED == 0 {
                    if spin.spin() {
                        state = self.state.load(Ordering::Relaxed);
                        continue;
                    }

                    if let Err(error) = self.state.compare_exchange_weak(
                        state,
                        state | READERS_PARKED,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        state = error;
                        continue;
                    }
                }

                // SAFETY: the reader queue key is derived from this lock
                // address, and the validation closure only reads this lock.
                let _ = unsafe {
                    parking_lot_core::park(
                        self.reader_queue_key(),
                        || {
                            let state = self.state.load(Ordering::Relaxed);
                            state & ONE_WRITER == ONE_WRITER && state & READERS_PARKED != 0
                        },
                        || {},
                        |_, _| {},
                        ParkToken(0),
                        None,
                    )
                };
                break;
            }
        }
    }

    #[cold]
    fn lock_exclusive_slow(&self) {
        let mut acquire_with = 0;

        loop {
            let mut spin = SpinWait::new();
            let mut state = self.state.load(Ordering::Relaxed);

            loop {
                while state & ONE_WRITER == 0 {
                    match self.state.compare_exchange_weak(
                        state,
                        state | ONE_WRITER | acquire_with,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return,
                        Err(error) => state = error,
                    }
                }

                if state & WRITERS_PARKED == 0 {
                    if spin.spin() {
                        state = self.state.load(Ordering::Relaxed);
                        continue;
                    }

                    if let Err(error) = self.state.compare_exchange_weak(
                        state,
                        state | WRITERS_PARKED,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        state = error;
                        continue;
                    }
                }

                // SAFETY: the writer queue key is derived from this lock
                // address, and the validation closure only reads this lock.
                let _ = unsafe {
                    parking_lot_core::park(
                        self.writer_queue_key(),
                        || {
                            let state = self.state.load(Ordering::Relaxed);
                            state & ONE_WRITER != 0 && state & WRITERS_PARKED != 0
                        },
                        || {},
                        |_, _| {},
                        ParkToken(0),
                        None,
                    )
                };

                acquire_with = WRITERS_PARKED;
                break;
            }
        }
    }

    #[cold]
    fn unlock_shared_slow(&self) {
        if self
            .state
            .compare_exchange(WRITERS_PARKED, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: the writer queue key is derived from this lock address.
            unsafe {
                parking_lot_core::unpark_one(self.writer_queue_key(), |_| UnparkToken(0));
            }
        }
    }

    #[cold]
    fn unlock_exclusive_slow(&self) {
        let state = self.state.load(Ordering::Relaxed);
        assert_eq!(state & ONE_WRITER, ONE_WRITER);

        let mut parked = state & PARKED_MASK;
        assert_ne!(parked, 0);

        if parked != PARKED_MASK
            && let Err(new_state) =
                self.state
                    .compare_exchange(state, 0, Ordering::Release, Ordering::Relaxed)
        {
            assert_eq!(new_state, ONE_WRITER | PARKED_MASK);
            parked = PARKED_MASK;
        }

        if parked == PARKED_MASK {
            self.state.store(WRITERS_PARKED, Ordering::Release);
            parked = READERS_PARKED;
        }

        if parked == READERS_PARKED {
            // SAFETY: the reader queue key is derived from this lock address.
            unsafe {
                parking_lot_core::unpark_all(self.reader_queue_key(), UnparkToken(0));
            }
        } else {
            debug_assert_eq!(parked, WRITERS_PARKED);
            // SAFETY: the writer queue key is derived from this lock address.
            unsafe {
                parking_lot_core::unpark_one(self.writer_queue_key(), |_| UnparkToken(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::RwLock;

    #[test]
    fn allows_multiple_readers() {
        let lock = RwLock::new(7usize);
        let first = lock.read();
        let second = lock.read();
        assert_eq!(*first, 7);
        assert_eq!(*second, 7);
    }

    #[test]
    fn writer_updates_value() {
        let lock = RwLock::new(1usize);
        *lock.write() = 2;
        assert_eq!(*lock.read(), 2);
    }

    #[test]
    fn waiting_writer_does_not_gate_readers() {
        let lock = Arc::new(RwLock::new(0usize));
        let read_guard = lock.read();
        let writer_started = Arc::new(AtomicBool::new(false));

        let writer_lock = Arc::clone(&lock);
        let writer_started_for_thread = Arc::clone(&writer_started);
        let writer = thread::spawn(move || {
            writer_started_for_thread.store(true, Ordering::Release);
            *writer_lock.write() = 1;
        });

        while !writer_started.load(Ordering::Acquire) {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(10));

        let second_reader = lock
            .try_read()
            .expect("read-biased lock should allow readers while a writer waits");
        assert_eq!(*second_reader, 0);
        drop(second_reader);
        drop(read_guard);
        writer.join().expect("writer should finish");
        assert_eq!(*lock.read(), 1);
    }

    #[test]
    fn concurrent_readers_and_writers_make_progress() {
        let lock = Arc::new(RwLock::new(0usize));
        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();

        for _ in 0..4 {
            let lock = Arc::clone(&lock);
            let stop = Arc::clone(&stop);
            workers.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let value = *lock.read();
                    std::hint::black_box(value);
                }
            }));
        }

        let writer_lock = Arc::clone(&lock);
        let writer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut writes = 0usize;
            while Instant::now() < deadline {
                *writer_lock.write() += 1;
                writes += 1;
            }
            writes
        });

        let writes = writer.join().expect("writer should finish");
        stop.store(true, Ordering::Relaxed);
        for worker in workers {
            worker.join().expect("reader should finish");
        }
        assert!(writes > 0);
        assert!(*lock.read() > 0);
    }
}
