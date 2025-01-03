/// Epoch-based garbage collector.
use core::fmt;
use core::sync::atomic::Ordering;
use std::sync::Arc;

use super::guard::Guard;
use super::internal::{Global, Local};
use super::Epoch;

/// A garbage collector based on *epoch-based reclamation* (EBR).
pub struct Collector {
    pub(crate) global: Arc<Global>,
}

unsafe impl Send for Collector {}
unsafe impl Sync for Collector {}

impl Default for Collector {
    // https://github.com/rust-lang/rust-clippy/issues/11382
    #[allow(clippy::arc_with_non_send_sync)]
    fn default() -> Self {
        Self {
            global: Arc::new(Global::new()),
        }
    }
}

impl Collector {
    /// Creates a new collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new handle for the collector.
    pub fn register(&self) -> LocalHandle {
        Local::register(self)
    }

    /// Reads the global epoch, without issueing a fence.
    #[inline]
    pub fn global_epoch(&self) -> Epoch {
        self.global.epoch.load(Ordering::Relaxed)
    }
}

impl Clone for Collector {
    /// Creates another reference to the same garbage collector.
    fn clone(&self) -> Self {
        Collector {
            global: self.global.clone(),
        }
    }
}

impl fmt::Debug for Collector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Collector { .. }")
    }
}

impl PartialEq for Collector {
    /// Checks if both handles point to the same collector.
    fn eq(&self, rhs: &Collector) -> bool {
        Arc::ptr_eq(&self.global, &rhs.global)
    }
}
impl Eq for Collector {}

/// A handle to a garbage collector.
pub struct LocalHandle {
    pub(crate) local: *const Local,
}

impl LocalHandle {
    /// Pins the handle.
    #[inline]
    pub fn pin(&self) -> Guard {
        unsafe { (*self.local).pin() }
    }

    /// Returns `true` if the handle is pinned.
    #[cfg(test)]
    #[inline]
    pub(crate) fn is_pinned(&self) -> bool {
        unsafe { (*self.local).is_pinned() }
    }
}

impl Drop for LocalHandle {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            Local::release_handle(&*self.local);
        }
    }
}

impl fmt::Debug for LocalHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("LocalHandle { .. }")
    }
}

#[cfg(test)]
mod tests {
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crossbeam_utils::thread;

    use crate::ebr_impl::{collector::Collector, RawShared};

    const NUM_THREADS: usize = 8;

    #[test]
    fn pin_reentrant() {
        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }
        let collector = Collector::new();
        let handle = collector.register();
        drop(collector);

        assert!(!handle.is_pinned());
        {
            let _guard = &handle.pin();
            assert!(handle.is_pinned());
            {
                let _guard = &handle.pin();
                assert!(handle.is_pinned());
            }
            assert!(handle.is_pinned());
        }
        assert!(!handle.is_pinned());
    }

    #[test]
    fn flush_local_bag() {
        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }
        let collector = Collector::new();
        let handle = collector.register();
        drop(collector);

        for _ in 0..100 {
            let guard = &handle.pin();
            unsafe {
                let a = RawShared::from_owned(7);
                guard.defer_destroy(a);

                let is_empty = || (*(*guard.local).bag.get()).is_empty();
                assert!(!is_empty());

                while !is_empty() {
                    guard.flush();
                }
            }
        }
    }

    #[test]
    fn garbage_buffering() {
        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }
        let collector = Collector::new();
        let handle = collector.register();
        drop(collector);

        let guard = &handle.pin();
        unsafe {
            for _ in 0..10 {
                let a = RawShared::from_owned(7);
                guard.defer_destroy(a);
            }
            assert!(!(*(*guard.local).bag.get()).is_empty());
        }
    }

    #[test]
    fn pin_holds_advance() {
        #[cfg(miri)]
        const N: usize = 500;
        #[cfg(not(miri))]
        const N: usize = 500_000;

        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }
        let collector = Collector::new();

        thread::scope(|scope| {
            for _ in 0..NUM_THREADS {
                scope.spawn(|_| {
                    unsafe {
                        context.initialize();
                    }
                    let handle = collector.register();
                    for _ in 0..N {
                        let guard = &handle.pin();

                        let before = collector.global.epoch.load(Ordering::Relaxed);
                        collector.global.collect(guard);
                        let after = collector.global.epoch.load(Ordering::Relaxed);

                        assert!(after.wrapping_sub(before) <= 2);
                    }
                });
            }
        })
        .unwrap();
    }

    #[test]
    fn buffering() {
        const COUNT: usize = 10;
        #[cfg(miri)]
        const N: usize = 500;
        #[cfg(not(miri))]
        const N: usize = 100_000;
        dyntls::lazy_static! {
            static ref DESTROYS_BUFFERING: AtomicUsize = AtomicUsize::new(0);
        }
        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        let collector = Collector::new();
        let handle = collector.register();

        unsafe {
            let guard = &handle.pin();
            for _ in 0..COUNT {
                let a = RawShared::from_owned(7);
                guard.defer_unchecked(move || {
                    a.drop();
                    DESTROYS_BUFFERING.fetch_add(1, Ordering::Relaxed);
                });
            }
        }

        for _ in 0..N {
            collector.global.collect(&handle.pin());
        }
        assert!(DESTROYS_BUFFERING.load(Ordering::Relaxed) < COUNT);

        handle.pin().flush();

        while DESTROYS_BUFFERING.load(Ordering::Relaxed) < COUNT {
            let guard = &handle.pin();
            collector.global.collect(guard);
        }
        assert_eq!(DESTROYS_BUFFERING.load(Ordering::Relaxed), COUNT);
    }

    #[test]
    fn count_drops() {
        #[cfg(miri)]
        const COUNT: usize = 500;
        #[cfg(not(miri))]
        const COUNT: usize = 100_000;
        dyntls::lazy_static! {
            static ref DROPS_COUNT_DROPS: AtomicUsize = AtomicUsize::new(0);
        }

        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        #[allow(dead_code)]
        struct Elem(i32);

        impl Drop for Elem {
            fn drop(&mut self) {
                DROPS_COUNT_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let collector = Collector::new();
        let handle = collector.register();

        unsafe {
            let guard = &handle.pin();

            for _ in 0..COUNT {
                let a = RawShared::from_owned(Elem(7));
                guard.defer_destroy(a);
            }
            guard.flush();
        }

        while DROPS_COUNT_DROPS.load(Ordering::Relaxed) < COUNT {
            let guard = &handle.pin();
            collector.global.collect(guard);
        }
        assert_eq!(DROPS_COUNT_DROPS.load(Ordering::Relaxed), COUNT);
    }

    #[test]
    fn count_destroy() {
        #[cfg(miri)]
        const COUNT: usize = 500;
        #[cfg(not(miri))]
        const COUNT: usize = 100_000;
        dyntls::lazy_static! {
            static ref DESTROYS_COUNT_DESTROY: AtomicUsize = AtomicUsize::new(0);
        }

        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        let collector = Collector::new();
        let handle = collector.register();

        unsafe {
            let guard = &handle.pin();

            for _ in 0..COUNT {
                let a = RawShared::from_owned(7);
                guard.defer_unchecked(move || {
                    a.drop();
                    DESTROYS_COUNT_DESTROY.fetch_add(1, Ordering::Relaxed);
                });
            }
            guard.flush();
        }

        while DESTROYS_COUNT_DESTROY.load(Ordering::Relaxed) < COUNT {
            let guard = &handle.pin();
            collector.global.collect(guard);
        }
        assert_eq!(DESTROYS_COUNT_DESTROY.load(Ordering::Relaxed), COUNT);
    }

    #[test]
    fn drop_array() {
        const COUNT: usize = 700;
        dyntls::lazy_static! {
            static ref DROPS_DROP_ARRAY: AtomicUsize = AtomicUsize::new(0);
        }
        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        #[allow(dead_code)]
        struct Elem(i32);

        impl Drop for Elem {
            fn drop(&mut self) {
                DROPS_DROP_ARRAY.fetch_add(1, Ordering::Relaxed);
            }
        }

        let collector = Collector::new();
        let handle = collector.register();

        let mut guard = handle.pin();

        let mut v = Vec::with_capacity(COUNT);
        for i in 0..COUNT {
            v.push(Elem(i as i32));
        }

        {
            let a = RawShared::from_owned(v);
            unsafe {
                guard.defer_destroy(a);
            }
            guard.flush();
        }

        while DROPS_DROP_ARRAY.load(Ordering::Relaxed) < COUNT {
            guard.reactivate();
            collector.global.collect(&guard);
        }
        assert_eq!(DROPS_DROP_ARRAY.load(Ordering::Relaxed), COUNT);
    }

    #[test]
    fn destroy_array() {
        #[cfg(miri)]
        const COUNT: usize = 500;
        #[cfg(not(miri))]
        const COUNT: usize = 100_000;
        dyntls::lazy_static! {
            static ref DESTROYS_DESTROY_ARRAY: AtomicUsize = AtomicUsize::new(0);
        }

        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        let collector = Collector::new();
        let handle = collector.register();

        unsafe {
            let guard = &handle.pin();

            let mut v = Vec::with_capacity(COUNT);
            for i in 0..COUNT {
                v.push(i as i32);
            }

            let len = v.len();
            let ptr = ManuallyDrop::new(v).as_mut_ptr() as usize;
            guard.defer_unchecked(move || {
                drop(Vec::from_raw_parts(ptr as *const i32 as *mut i32, len, len));
                DESTROYS_DESTROY_ARRAY.fetch_add(len, Ordering::Relaxed);
            });
            guard.flush();
        }

        while DESTROYS_DESTROY_ARRAY.load(Ordering::Relaxed) < COUNT {
            let guard = &handle.pin();
            collector.global.collect(guard);
        }
        assert_eq!(DESTROYS_DESTROY_ARRAY.load(Ordering::Relaxed), COUNT);
    }

    #[test]
    fn stress() {
        const THREADS: usize = 8;
        #[cfg(miri)]
        const COUNT: usize = 500;
        #[cfg(not(miri))]
        const COUNT: usize = 100_000;
        dyntls::lazy_static! {
            static ref DROPS_STRESS: AtomicUsize = AtomicUsize::new(0);
        }

        let context = dyntls_host::get();
        unsafe {
            context.initialize();
        }

        #[allow(dead_code)]
        struct Elem(i32);

        impl Drop for Elem {
            fn drop(&mut self) {
                DROPS_STRESS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let collector = Collector::new();

        thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|_| {
                    unsafe {
                        context.initialize();
                    }
                    let handle = collector.register();
                    for _ in 0..COUNT {
                        let guard = &handle.pin();
                        unsafe {
                            let a = RawShared::from_owned(Elem(7i32));
                            guard.defer_destroy(a);
                        }
                    }
                });
            }
        })
        .unwrap();

        let handle = collector.register();
        while DROPS_STRESS.load(Ordering::Relaxed) < COUNT * THREADS {
            let guard = &handle.pin();
            collector.global.collect(guard);
        }
        assert_eq!(DROPS_STRESS.load(Ordering::Relaxed), COUNT * THREADS);
    }
}
