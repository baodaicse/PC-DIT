use crate::{
    Result,
    driver::{Driver, Handle, check},
};
use std::{
    cell::Cell,
    ptr,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Resources this crate currently owns and has not released. Exact and process-local, so a
/// rollback test can prove nothing leaked without reading the driver's global free memory.
pub(crate) static LIVE_BUFFERS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LIVE_PINNED: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static LIVE_EVENTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveResources {
    pub device_buffers: usize,
    pub pinned_buffers: usize,
    pub streams: usize,
    pub events: usize,
}

#[must_use]
pub fn live_resources() -> LiveResources {
    LiveResources {
        device_buffers: LIVE_BUFFERS.load(Ordering::SeqCst),
        pinned_buffers: LIVE_PINNED.load(Ordering::SeqCst),
        streams: LIVE_STREAMS.load(Ordering::SeqCst),
        events: LIVE_EVENTS.load(Ordering::SeqCst),
    }
}

pub struct Context {
    pub driver: Driver,
    pub handle: Handle,
    pub stopped: Cell<bool>,
}
impl Context {
    pub fn current(&self) -> Result<()> {
        check("cuCtxSetCurrent", unsafe {
            (self.driver.context_current)(self.handle)
        })
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        // On timeout, retain all resources until process exit. Destroying a CUDA
        // context can otherwise block indefinitely on a hung device.
        if !self.stopped.get() {
            unsafe {
                (self.driver.context_destroy)(self.handle);
            }
        }
    }
}
pub struct Buffer {
    pub context: Rc<Context>,
    pub address: u64,
    pub len: usize,
}
impl Buffer {
    pub fn new(context: &Rc<Context>, len: usize) -> Result<Self> {
        context.current()?;
        let mut address = 0;
        check("cuMemAlloc", unsafe {
            (context.driver.allocate)(&raw mut address, len)
        })?;
        LIVE_BUFFERS.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            context: Rc::clone(context),
            address,
            len,
        })
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.context.stopped.get() && self.context.current().is_ok() {
            unsafe {
                (self.context.driver.free)(self.address);
            }
            LIVE_BUFFERS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
pub struct Pinned {
    pub context: Rc<Context>,
    pub address: Handle,
    pub len: usize,
}
impl Pinned {
    pub fn new(context: &Rc<Context>, len: usize) -> Result<Self> {
        context.current()?;
        let mut address = ptr::null_mut();
        check("cuMemHostAlloc", unsafe {
            (context.driver.host_allocate)(&raw mut address, len, 0)
        })?;
        LIVE_PINNED.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            context: Rc::clone(context),
            address,
            len,
        })
    }
    /// Copies `u32` words as little-endian bytes without an intermediate `Vec<u8>`.
    pub fn write_u32(&self, data: &[u32]) {
        assert!(data.len() * 4 <= self.len);
        #[cfg(target_endian = "little")]
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr().cast::<u8>(),
                self.address.cast::<u8>(),
                data.len() * 4,
            );
        }
        #[cfg(not(target_endian = "little"))]
        for (index, word) in data.iter().enumerate() {
            let bytes = word.to_le_bytes();
            unsafe {
                ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.address.cast::<u8>().add(index * 4),
                    4,
                );
            }
        }
    }
    /// Flips one bit of the first byte. Negative-gate injection only.
    pub fn corrupt_first_byte(&self) {
        if self.len > 0 {
            unsafe {
                *self.address.cast::<u8>() ^= 1;
            }
        }
    }
    /// # Safety
    /// No DMA may read or write the region while the slice is alive.
    pub unsafe fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.address.cast::<u8>(), len) }
    }
    /// # Safety
    /// Caller must prove DMA completion and prevent reuse/free until readers finish.
    pub unsafe fn completed_bytes(&self, len: usize) -> &[u8] {
        assert!(len <= self.len);
        unsafe { std::slice::from_raw_parts(self.address.cast::<u8>(), len) }
    }
}
impl Drop for Pinned {
    fn drop(&mut self) {
        if !self.context.stopped.get() && self.context.current().is_ok() {
            unsafe {
                (self.context.driver.host_free)(self.address);
            }
            LIVE_PINNED.fetch_sub(1, Ordering::SeqCst);
        }
    }
}
