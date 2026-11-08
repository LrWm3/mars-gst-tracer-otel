//! Optional video batch provenance, owned by SDK metadata rather than a map.
use crate::{
    capture::Token,
    export::Counters,
    propagation::{self, Input, Links},
};
use gstreamer::{self as gst};
use std::{ffi::c_void, sync::atomic::Ordering::Relaxed};

pub(crate) const FRAME_INPUT: u8 = 1;
pub(crate) const BATCH_OUTPUT: u8 = 2;
pub(crate) const FRAME_OUTPUT: u8 = 4;
const MAX_FRAMES: usize = 256;

type Visit = unsafe extern "C" fn(*mut c_void, *mut gst::ffi::GstBuffer);
#[repr(C)]
#[derive(Clone, Copy)]
struct Api {
    version: u32,
    size: usize,
    attach: Option<unsafe extern "C" fn(*mut gst::ffi::GstBuffer, *mut gst::ffi::GstBuffer) -> i32>,
    read: Option<unsafe extern "C" fn(*mut gst::ffi::GstBuffer, Visit, *mut c_void, u32) -> i32>,
}
pub(crate) struct Adapter {
    api: Api,
    #[cfg(target_os = "linux")]
    module: *mut c_void,
}
// ABI implementations must be thread-safe, and callbacks never outlive read().
unsafe impl Send for Adapter {}
unsafe impl Sync for Adapter {}

impl Adapter {
    #[cfg(not(target_os = "linux"))]
    pub fn load(_: &str) -> std::io::Result<Self> {
        Err(std::io::Error::other("trace-batch-adapter requires Linux"))
    }
    #[cfg(target_os = "linux")]
    pub fn load(path: &str) -> std::io::Result<Self> {
        use std::ffi::{CStr, CString};
        let path = CString::new(path).map_err(std::io::Error::other)?;
        unsafe {
            // Buffer-owned SDK callbacks can outlive the tracer runtime. Keep
            // code resident while balancing the loader reference in Drop.
            let module = libc::dlopen(
                path.as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
            );
            if module.is_null() {
                let error = CStr::from_ptr(libc::dlerror()).to_string_lossy();
                return Err(std::io::Error::other(format!(
                    "trace-batch-adapter: {error}"
                )));
            }
            let symbol = libc::dlsym(module, c"mars_batch_adapter_v1".as_ptr());
            if symbol.is_null() {
                libc::dlclose(module);
                return Err(std::io::Error::other("missing mars_batch_adapter_v1"));
            }
            let get: unsafe extern "C" fn() -> *const Api = std::mem::transmute(symbol);
            let ptr = get();
            // Read only the ABI header before accepting the complete layout.
            if ptr.is_null() || (*ptr).version != 1 || (*ptr).size < std::mem::size_of::<Api>() {
                libc::dlclose(module);
                return Err(std::io::Error::other("unsupported batch adapter ABI"));
            }
            let api = *ptr;
            if api.attach.is_none() || api.read.is_none() {
                libc::dlclose(module);
                return Err(std::io::Error::other("incomplete batch adapter ABI"));
            }
            Ok(Self { api, module })
        }
    }

    pub fn parent(
        &self,
        buffer: &gst::Buffer,
        frame: bool,
        active: Option<&Token>,
        stats: &Counters,
    ) -> Input {
        unsafe extern "C" fn visit(user: *mut c_void, buffer: *mut gst::ffi::GstBuffer) {
            let inputs = &mut *user.cast::<Vec<Option<Input>>>();
            if inputs.len() == MAX_FRAMES {
                return;
            }
            inputs.push(if buffer.is_null() {
                None
            } else {
                propagation::read_input(gst::BufferRef::from_ptr(buffer))
            });
        }
        let mut inputs: Vec<Option<Input>> = Vec::with_capacity(if frame { 1 } else { 16 });
        let count = unsafe {
            self.api.read.unwrap()(
                buffer.as_ptr() as *mut _,
                visit,
                (&mut inputs as *mut Vec<Option<Input>>).cast(),
                if frame { 1 } else { MAX_FRAMES as u32 },
            )
        };
        if count <= 0 || count as usize != inputs.len() || (frame && count != 1) {
            stats.local().batch_adapter_failures.fetch_add(1, Relaxed);
            return rejected();
        }
        stats.local().batch_frames.fetch_add(count as u64, Relaxed);
        stats.local().batch_missing_context.fetch_add(
            inputs.iter().filter(|i| i.is_none()).count() as u64,
            Relaxed,
        );
        select(inputs, frame, active)
    }

    pub fn attach(&self, buffer: &gst::Buffer, token: &Token, stats: &Counters) {
        // The carrier is immutable, contains no payload, and retains no source
        // buffer, pad or element. Rejected inputs must travel too: after demux
        // they must not inherit a different frame's sampled batch context.
        let context = gst::Buffer::new();
        // Shared branches carry the last common ancestor, not whichever
        // branch last updated the SDK slot before a mux copied it.
        let shared = (!buffer.is_writable())
            .then(|| propagation::read(buffer))
            .flatten();
        propagation::write(&context, shared.as_ref().unwrap_or(token), true, stats);
        let ok = unsafe {
            self.api.attach.unwrap()(buffer.as_ptr() as *mut _, context.as_ptr() as *mut _)
        };
        if ok != 1 {
            stats.local().batch_adapter_failures.fetch_add(1, Relaxed);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Adapter {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.module);
        }
    }
}

fn rejected() -> Input {
    Input {
        token: Token::rejected(),
        links: None,
        derived: true,
    }
}

fn select(inputs: Vec<Option<Input>>, frame: bool, active: Option<&Token>) -> Input {
    if frame {
        // The SDK's single output frame supplies the parent and decision, even
        // when a different sampled batch is active on this thread.
        let mut selected = inputs.into_iter().next().flatten().unwrap_or_else(rejected);
        if selected.token.sampled() {
            if let Some(active) = active {
                let mut links = selected.links.as_deref().cloned().unwrap_or_default();
                links.insert(active, &selected.token);
                selected.links = links.finish();
            }
        }
        return selected;
    }
    // Account shared work once, under the first already sampled contributor.
    // A rejected input is not promoted; it remains rejected after demux.
    let primary = inputs
        .iter()
        .flatten()
        .find(|i| i.token.sampled() && i.token.context().is_valid());
    let Some(primary) = primary else {
        return rejected();
    };
    let mut selected = primary.clone();
    let mut links = Links::default();
    for input in inputs.iter().flatten() {
        links.extend(input, &selected.token);
    }
    selected.links = links.finish();
    selected.derived = true;
    selected
}

#[cfg(test)]
#[path = "batchlineage_tests.rs"]
mod tests;
