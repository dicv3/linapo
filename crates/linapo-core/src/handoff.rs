//! Lock-free "swap in a new value" mailbox between a control thread and a
//! real-time thread.
//!
//! * control thread: [`Handoff::publish`] (allocation, freeing and locks are fine here)
//! * real-time thread: [`Handoff::poll`] to pick up the newest value and
//!   [`Handoff::retire`] to give the replaced one back. Neither allocates,
//!   locks, or frees.
//!
//! Retired values are freed later on the control thread (next `publish`, or
//! `collect_retired`). If four retired values pile up uncollected, extra ones
//! are leaked rather than freed on the real-time thread.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};

const RETIRED_SLOTS: usize = 4;

pub struct Handoff<T> {
    incoming: AtomicPtr<T>,
    retired: [AtomicPtr<T>; RETIRED_SLOTS],
}

unsafe impl<T: Send> Send for Handoff<T> {}
unsafe impl<T: Send> Sync for Handoff<T> {}

impl<T> Default for Handoff<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Handoff<T> {
    pub fn new() -> Self {
        Handoff {
            incoming: AtomicPtr::new(null_mut()),
            retired: std::array::from_fn(|_| AtomicPtr::new(null_mut())),
        }
    }

    /// Control thread. Replaces any value the real-time thread has not picked up yet.
    pub fn publish(&self, value: Box<T>) {
        self.collect_retired();
        let old = self.incoming.swap(Box::into_raw(value), Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: `old` came from Box::into_raw in `publish` and was just removed from the slot.
            drop(unsafe { Box::from_raw(old) });
        }
    }

    /// Control thread. Frees values the real-time thread has retired.
    pub fn collect_retired(&self) {
        for slot in &self.retired {
            let p = slot.swap(null_mut(), Ordering::AcqRel);
            if !p.is_null() {
                // SAFETY: pointers in `retired` come from Box::into_raw in `retire`.
                drop(unsafe { Box::from_raw(p) });
            }
        }
    }

    /// Real-time thread. Takes the newest published value, if any.
    pub fn poll(&self) -> Option<Box<T>> {
        if self.incoming.load(Ordering::Acquire).is_null() {
            return None;
        }
        let p = self.incoming.swap(null_mut(), Ordering::AcqRel);
        // SAFETY: non-null pointers in `incoming` come from Box::into_raw in `publish`,
        // and the swap gave us exclusive ownership.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }

    /// Real-time thread. Hands a replaced value back for the control thread to free.
    pub fn retire(&self, value: Box<T>) {
        let p = Box::into_raw(value);
        for slot in &self.retired {
            if slot.compare_exchange(null_mut(), p, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
        // All slots busy: leak instead of freeing on the real-time thread.
    }
}

impl<T> Drop for Handoff<T> {
    fn drop(&mut self) {
        drop(self.poll());
        self.collect_retired();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    struct Counted {
        id: usize,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn mk(id: usize, drops: &Arc<AtomicUsize>) -> Box<Counted> {
        Box::new(Counted { id, drops: drops.clone() })
    }

    #[test]
    fn poll_returns_latest_and_empties() {
        let h = Handoff::new();
        assert!(h.poll().is_none());
        h.publish(Box::new(1));
        assert_eq!(*h.poll().unwrap(), 1);
        assert!(h.poll().is_none());
    }

    #[test]
    fn unconsumed_value_is_replaced_and_freed_on_control_side() {
        let drops = Arc::new(AtomicUsize::new(0));
        let h = Handoff::new();
        h.publish(mk(1, &drops));
        h.publish(mk(2, &drops));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(h.poll().unwrap().id, 2);
    }

    #[test]
    fn retired_values_are_freed_by_the_control_thread() {
        let drops = Arc::new(AtomicUsize::new(0));
        let h = Handoff::new();
        h.retire(mk(1, &drops));
        assert_eq!(drops.load(Ordering::SeqCst), 0, "retire must not free");
        h.collect_retired();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_frees_everything_left() {
        let drops = Arc::new(AtomicUsize::new(0));
        {
            let h = Handoff::new();
            h.publish(mk(1, &drops));
            h.retire(mk(2, &drops));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn threaded_stress_frees_every_value_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let h = Arc::new(Handoff::new());
        let total = 2000;
        let rt = {
            let (h, drops) = (h.clone(), drops.clone());
            std::thread::spawn(move || {
                let mut current = mk(0, &drops);
                while current.id != total {
                    if let Some(next) = h.poll() {
                        let old = std::mem::replace(&mut current, next);
                        h.retire(old);
                    }
                    std::hint::spin_loop();
                }
                h.retire(current);
            })
        };
        for i in 1..=total {
            h.publish(mk(i, &drops));
            if i % 50 == 0 {
                std::thread::yield_now();
            }
        }
        rt.join().unwrap();
        drop(Arc::try_unwrap(h).ok().expect("sole owner"));
        // 0..=total were created; every one is dropped exactly once unless the
        // (documented) overflow leak happened, which can only reduce the count.
        let d = drops.load(Ordering::SeqCst);
        assert!(d <= total + 1, "double free? {d}");
        assert!(d >= total + 1 - 8, "unexpected leaks: {d}");
    }
}
