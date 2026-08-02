use super::{MemoryLimitExceededError, SharedMemoryLimiter};

/// Preallocated region of memory that can grow and never deallocates during the lifetime of
/// the limiter.
#[derive(Debug)]
pub(crate) struct Arena {
    limiter: SharedMemoryLimiter,
    data: Vec<u8>,
    /// Offset of the live region within `data`. `shift` only advances this;
    /// the dead prefix is reclaimed lazily by the next `append`.
    start: usize,
}

impl Arena {
    pub fn new(limiter: SharedMemoryLimiter, preallocated_size: usize) -> Self {
        let mut data = Vec::new();

        let preallocated = limiter
            .increase_usage(preallocated_size)
            .ok()
            .and_then(|()| data.try_reserve_exact(preallocated_size).ok())
            .is_some();
        // HtmlRewriter::new() has no way to report this
        debug_assert!(
            preallocated,
            "Total preallocated memory size should be less than `MemorySettings::max_allowed_memory_usage`."
        );

        Self {
            limiter,
            data,
            start: 0,
        }
    }

    /// Reclaim the dead prefix left behind by `shift`, in one memmove.
    fn compact(&mut self) {
        if self.start > 0 {
            self.data.copy_within(self.start.., 0);
            let live = self.data.len() - self.start;
            self.data.truncate(live);
            self.start = 0;
        }
    }

    pub fn append(&mut self, slice: &[u8]) -> Result<(), MemoryLimitExceededError> {
        // Only the multi-write streaming path appends after a `shift`, and it
        // is the only place the dead prefix has to go away.
        self.compact();

        // this specific form of capacity check optimizes out redundant resizing in extend_from_slice
        if self.data.capacity() - self.data.len() < slice.len() {
            let additional = slice.len() + self.data.len() - self.data.capacity();

            // NOTE: approximate usage, as `Vec::(try_)reserve_exact` doesn't
            // give guarantees about exact capacity value :).
            self.limiter.increase_usage(additional)?;

            // NOTE: with wisely chosen preallocated size this branch should be
            // executed quite rarely. We can't afford to use double capacity
            // strategy used by default (see: https://github.com/rust-lang/rust/blob/bdfd698f37184da42254a03ed466ab1f90e6fb6c/src/liballoc/raw_vec.rs#L424)
            // as we'll run out of the space allowance quite quickly.
            self.data
                .try_reserve_exact(slice.len())
                .map_err(|_| MemoryLimitExceededError)?;
        }

        self.data.extend_from_slice(slice);

        Ok(())
    }

    pub fn init_with(&mut self, slice: &[u8]) -> Result<(), MemoryLimitExceededError> {
        self.data.clear();
        self.start = 0;
        self.append(slice)
    }

    /// O(1): advances the live-region start instead of memmoving the tail.
    /// A suspended rewrite resumes by re-feeding the unconsumed tail, so this
    /// runs once per suspension — memmoving there is O(suspensions × tail).
    pub fn shift(&mut self, byte_count: usize) {
        debug_assert!(byte_count <= self.data.len() - self.start);
        self.start += byte_count;
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data[self.start..]
    }
}

#[cfg(test)]
mod tests {
    use super::super::limiter::SharedMemoryLimiter;
    use super::*;

    #[test]
    fn append() {
        let limiter = SharedMemoryLimiter::new(10);
        let mut arena = Arena::new(limiter.clone(), 2);

        arena.append(&[1, 2]).unwrap();
        assert_eq!(arena.bytes(), &[1, 2]);
        assert_eq!(limiter.current_usage(), 2);

        arena.append(&[3, 4]).unwrap();
        assert_eq!(arena.bytes(), &[1, 2, 3, 4]);
        assert_eq!(limiter.current_usage(), 4);

        arena.append(&[5, 6, 7, 8, 9, 10]).unwrap();
        assert_eq!(arena.bytes(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(limiter.current_usage(), 10);

        let err = arena.append(&[11]).unwrap_err();

        assert_eq!(err, MemoryLimitExceededError);
    }

    #[test]
    fn init_with() {
        let limiter = SharedMemoryLimiter::new(5);
        let mut arena = Arena::new(limiter.clone(), 0);

        arena.init_with(&[1]).unwrap();
        assert_eq!(arena.bytes(), &[1]);
        assert_eq!(limiter.current_usage(), 1);

        arena.append(&[1, 2]).unwrap();
        assert_eq!(arena.bytes(), &[1, 1, 2]);
        assert_eq!(limiter.current_usage(), 3);

        arena.init_with(&[1, 2, 3]).unwrap();
        assert_eq!(arena.bytes(), &[1, 2, 3]);
        assert_eq!(limiter.current_usage(), 3);

        arena.init_with(&[]).unwrap();
        assert!(arena.bytes().is_empty());
        assert_eq!(limiter.current_usage(), 3);

        let err = arena.init_with(&[1, 2, 3, 4, 5, 6, 7]).unwrap_err();

        assert_eq!(err, MemoryLimitExceededError);
    }

    #[test]
    fn shift() {
        let limiter = SharedMemoryLimiter::new(10);
        let mut arena = Arena::new(limiter.clone(), 0);

        arena.append(&[0, 1, 2, 3]).unwrap();
        arena.shift(2);
        assert_eq!(arena.bytes(), &[2, 3]);
        assert_eq!(limiter.current_usage(), 4);

        arena.append(&[0, 1]).unwrap();
        assert_eq!(arena.bytes(), &[2, 3, 0, 1]);
        assert_eq!(limiter.current_usage(), 4);

        arena.shift(3);
        assert_eq!(arena.bytes(), &[1]);
        assert_eq!(limiter.current_usage(), 4);

        arena.append(&[2, 3, 4, 5]).unwrap();
        arena.shift(1);
        assert_eq!(arena.bytes(), &[2, 3, 4, 5]);
        assert_eq!(limiter.current_usage(), 5);
    }

    // A suspended rewrite shifts once per resume with no append in between;
    // the dead prefix must accumulate and then be reclaimed by the next append.
    #[test]
    fn consecutive_shifts_without_append() {
        let limiter = SharedMemoryLimiter::new(10);
        let mut arena = Arena::new(limiter.clone(), 0);

        arena.append(&[0, 1, 2, 3, 4]).unwrap();
        arena.shift(1);
        arena.shift(2);
        assert_eq!(arena.bytes(), &[3, 4]);

        arena.shift(2);
        assert!(arena.bytes().is_empty());

        arena.append(&[9]).unwrap();
        assert_eq!(arena.bytes(), &[9]);
    }
}
