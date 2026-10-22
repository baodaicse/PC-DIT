//! CUDA Driver API ABI, dynamically loaded. No CUDA toolkit/ICD dependency at runtime.
use crate::{Error, Result};
use libloading::Library;
use std::ffi::{c_char, c_void};
pub type Handle = *mut c_void;
macro_rules! api {
    ($($field:ident: $symbol:literal ($($arg:ty),*);)*) => {
        pub struct Driver { _library: Library, $(pub $field: unsafe extern "C" fn($($arg),*) -> i32,)* }
        impl Driver {
            pub fn load() -> Result<Self> {
                let library = unsafe { Library::new("libcuda.so.1") }.map_err(|e| Error::Unavailable(e.to_string()))?;
                $(let $field = unsafe { *library.get::<unsafe extern "C" fn($($arg),*) -> i32>(concat!($symbol,"\0").as_bytes())
                    .map_err(|e| Error::Unavailable(e.to_string()))? };)*
                let driver = Self { _library: library, $($field,)* };
                check("cuInit", unsafe { (driver.init)(0) })?;
                Ok(driver)
            }
        }
    }
}
api! {
    init: "cuInit" (u32);
    version: "cuDriverGetVersion" (*mut i32);
    count: "cuDeviceGetCount" (*mut i32);
    device: "cuDeviceGet" (*mut i32, i32);
    name: "cuDeviceGetName" (*mut c_char, i32, i32);
    uuid: "cuDeviceGetUuid" (*mut [u8;16], i32);
    pci: "cuDeviceGetPCIBusId" (*mut c_char, i32, i32);
    total: "cuDeviceTotalMem_v2" (*mut usize, i32);
    attribute: "cuDeviceGetAttribute" (*mut i32, i32, i32);
    context_create: "cuCtxCreate_v2" (*mut Handle, u32, i32);
    context_destroy: "cuCtxDestroy_v2" (Handle);
    context_current: "cuCtxSetCurrent" (Handle);
    allocate: "cuMemAlloc_v2" (*mut u64, usize);
    free: "cuMemFree_v2" (u64);
    host_allocate: "cuMemHostAlloc" (*mut Handle, usize, u32);
    host_free: "cuMemFreeHost" (Handle);
    memory_info: "cuMemGetInfo_v2" (*mut usize, *mut usize);
    upload: "cuMemcpyHtoDAsync_v2" (u64, *const c_void, usize, Handle);
    download: "cuMemcpyDtoHAsync_v2" (*mut c_void, u64, usize, Handle);
    fill: "cuMemsetD32Async" (u64, u32, usize, Handle);
    module_load: "cuModuleLoadData" (*mut Handle, *const c_void);
    module_unload: "cuModuleUnload" (Handle);
    function: "cuModuleGetFunction" (*mut Handle, Handle, *const c_char);
    launch: "cuLaunchKernel" (Handle,u32,u32,u32,u32,u32,u32,u32,Handle,*mut *mut c_void,*mut *mut c_void);
    stream_create: "cuStreamCreate" (*mut Handle, u32);
    stream_create_priority: "cuStreamCreateWithPriority" (*mut Handle, u32, i32);
    stream_priority_range: "cuCtxGetStreamPriorityRange" (*mut i32, *mut i32);
    stream_query: "cuStreamQuery" (Handle);
    stream_destroy: "cuStreamDestroy_v2" (Handle);
    event_create: "cuEventCreate" (*mut Handle, u32);
    event_record: "cuEventRecord" (Handle, Handle);
    stream_wait_event: "cuStreamWaitEvent" (Handle, Handle, u32);
    event_query: "cuEventQuery" (Handle);
    event_elapsed: "cuEventElapsedTime" (*mut f32, Handle, Handle);
    event_destroy: "cuEventDestroy_v2" (Handle);
}
pub fn check(call: &'static str, status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Api { call, status })
    }
}
