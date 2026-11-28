//! Process-wide accounting for model observation and WebSocket frame buffers.
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
};

const LIMIT: usize = 128 * 1024 * 1024;
static SHARED: LazyLock<Arc<Budget>> = LazyLock::new(|| Arc::new(Budget::new(LIMIT)));

#[derive(Debug)]
pub(crate) struct Budget {
    limit: usize,
    used: AtomicUsize,
}
impl Budget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }
    fn claim(&self, bytes: usize) -> Result<(), Exhausted> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let total = used
                .checked_add(bytes)
                .filter(|total| *total <= self.limit)
                .ok_or(Exhausted)?;
            match self
                .used
                .compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(current) => used = current,
            }
        }
    }
    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(crate) struct Exhausted;
impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Model observation memory budget exhausted")
    }
}
impl std::error::Error for Exhausted {}
impl From<Exhausted> for std::io::Error {
    fn from(value: Exhausted) -> Self {
        Self::other(value)
    }
}

/// Charges allocated capacity, including empty buffers retained for reuse.
/// Moving a buffer also moves its charge; dropping or releasing it refunds it.
#[derive(Debug)]
pub(crate) struct Buffer {
    data: Vec<u8>,
    budget: Arc<Budget>,
    reserved: usize,
}
impl Default for Buffer {
    fn default() -> Self {
        Self::with_budget(SHARED.clone())
    }
}
impl Buffer {
    pub(crate) fn with_budget(budget: Arc<Budget>) -> Self {
        Self {
            data: Vec::new(),
            budget,
            reserved: 0,
        }
    }
    fn reserve(&mut self, additional: usize) -> Result<(), Exhausted> {
        let needed = self.len().checked_add(additional).ok_or(Exhausted)?;
        if needed <= self.data.capacity() {
            return Ok(());
        }
        if needed > self.budget.limit {
            return Err(Exhausted);
        }
        // Keep normal geometric growth, but avoid large spare allocations.
        let mut target = needed
            .max(self.data.capacity().saturating_mul(2))
            .max(512)
            .min(needed.saturating_add(1024 * 1024))
            .min(self.budget.limit);
        if self.budget.claim(target - self.reserved).is_err() {
            target = needed;
            self.budget.claim(target - self.reserved)?;
        }
        let charged = target - self.reserved;
        if self
            .data
            .try_reserve_exact(target - self.data.len())
            .is_err()
        {
            self.budget.release(charged);
            return Err(Exhausted);
        }
        self.reserved += charged;
        let extra = self.data.capacity() - self.reserved;
        if self.budget.claim(extra).is_err() {
            self.release();
            return Err(Exhausted);
        }
        self.reserved += extra;
        Ok(())
    }
    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), Exhausted> {
        self.reserve(bytes.len())?;
        self.data.extend_from_slice(bytes);
        Ok(())
    }
    pub(crate) fn extend(
        &mut self,
        bytes: impl ExactSizeIterator<Item = u8>,
    ) -> Result<(), Exhausted> {
        self.reserve(bytes.len())?;
        self.data.extend(bytes);
        Ok(())
    }
    pub(crate) fn push(&mut self, byte: u8) -> Result<(), Exhausted> {
        self.reserve(1)?;
        self.data.push(byte);
        Ok(())
    }
    pub(crate) fn resize(&mut self, len: usize, byte: u8) -> Result<(), Exhausted> {
        self.reserve(len.saturating_sub(self.len()))?;
        self.data.resize(len, byte);
        Ok(())
    }
    pub(crate) fn clear(&mut self) {
        self.data.clear();
    }
    pub(crate) fn release(&mut self) {
        self.data = Vec::new();
        self.budget.release(std::mem::take(&mut self.reserved));
    }
    pub(crate) fn take(&mut self) -> Self {
        Self {
            data: std::mem::take(&mut self.data),
            budget: self.budget.clone(),
            reserved: std::mem::take(&mut self.reserved),
        }
    }
    pub(crate) fn empty_sibling(&self) -> Self {
        Self::with_budget(self.budget.clone())
    }
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.data
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        self.release();
    }
}
impl Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}
impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}
impl AsRef<[u8]> for Buffer {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}
impl<T: AsRef<[u8]>> PartialEq<T> for Buffer {
    fn eq(&self, other: &T) -> bool {
        self.as_slice() == other.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_buffers_share_capacity_and_moves_keep_the_charge() {
        let budget = Arc::new(Budget::new(2048));
        let barrier = std::sync::Barrier::new(8);
        let buffers = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (budget, barrier) = (budget.clone(), &barrier);
                    scope.spawn(move || {
                        let mut buffer = Buffer::with_budget(budget);
                        barrier.wait();
                        buffer.resize(1024, 0).ok().map(|_| buffer)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(buffers.len(), 2);
        assert_eq!(budget.used.load(Ordering::Acquire), 2048);
        let mut buffers = buffers;
        let moved = buffers[0].take();
        buffers[1].clear();
        assert_eq!(budget.used.load(Ordering::Acquire), 2048);
        drop(moved);
        assert_eq!(budget.used.load(Ordering::Acquire), 1024);
        drop(buffers);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }
}
