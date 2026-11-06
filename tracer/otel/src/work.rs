//! Experimental explicit handoff for asynchronous element work.
//!
//! Capture at admission, move the work with the queued input, enter only while
//! executing that job, and finish after completion. Dropping unfinished work
//! records cancellation. This API creates no new trace roots.
use crate::{
    capture::{self, Operation, Record, ThreadInfo, Token},
    export::Runtime,
};
use gstreamer::{self as gst, prelude::*};
use opentelemetry_sdk::trace::IdGenerator;
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        Arc,
    },
};

/// Owned batch lifetime; contains no reference to the input buffer or element.
pub struct Work {
    runtime: &'static Runtime,
    token: Token,
    record: Option<Record>,
    profiled: AtomicBool,
    #[cfg(feature = "profiles")]
    owner: crate::profilecontext::Description,
}

impl Work {
    /// Capture the current call (or carried buffer context) without resampling.
    /// `element` must have a static sink pad; no instrumentation means `None`.
    pub fn capture(element: &gst::Element, buffer: &gst::Buffer) -> Option<Self> {
        let runtime = crate::tracer::runtime().ok()?;
        if !runtime.config.traces || !runtime.accepting() {
            return None;
        }
        let mut token = capture::work_parent(buffer)?;
        let start = runtime.hook_now().unwrap_or(0);
        if token.sampled() {
            if let Some(budget) = &token.budget {
                if let Err(reason) = budget.admit(start) {
                    let stats = runtime.stats.local();
                    match reason {
                        crate::tracebudget::Reason::Age => &stats.trace_age_rejections,
                        crate::tracebudget::Reason::Spans => &stats.trace_span_rejections,
                    }
                    .fetch_add(1, Relaxed);
                    token.flags = token.flags.with_sampled(false);
                    token.budget = None;
                }
            }
        }
        let record = if token.sampled() {
            let pad = element.static_pad("sink")?;
            let site = crate::metadata::site(&pad, runtime.config.value_length_limit);
            let parent = token.span_id;
            let parent_is_remote = token.is_remote;
            let links = crate::propagation::read_fan_in(buffer).and_then(|input| {
                let mut links = crate::propagation::Links::default();
                links.extend(&input, &token);
                links.finish()
            });
            token.span_id = capture::IDS.new_span_id();
            token.is_remote = false;
            runtime.stats.local().started_spans.fetch_add(1, Relaxed);
            let thread = std::thread::current();
            Some(Record {
                operation: Operation::Process,
                site,
                token: token.clone(),
                parent,
                parent_is_remote,
                start,
                end: start,
                buffer_id: buffer.as_ptr() as usize,
                buffer_size: buffer.size(),
                list_len: None,
                links,
                thread: Arc::new(ThreadInfo {
                    encoded: Default::default(),
                    id: crate::export::text_value(
                        &format!("{:?}", thread.id()),
                        runtime.config.value_length_limit,
                    ),
                    name: crate::export::text_value(
                        thread.name().unwrap_or("unnamed"),
                        runtime.config.value_length_limit,
                    ),
                }),
                flow_return: gst::ffi::GST_FLOW_FLUSHING,
                profiled: false,
            })
        } else {
            None
        };
        Some(Self {
            runtime,
            token,
            record,
            profiled: AtomicBool::new(false),
            #[cfg(feature = "profiles")]
            owner: crate::metadata::profile_id(element.upcast_ref()),
        })
    }

    /// Activate on the executing thread. Nesting is supported; drop in LIFO order.
    /// This guard must not cross an await point or move to another thread.
    pub fn enter(&self) -> WorkScope<'_> {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let identity = NEXT.fetch_add(1, Relaxed);
        crate::profiles::clear();
        #[cfg(feature = "profiles")]
        crate::profiles::register_thread();
        #[cfg(feature = "profiles")]
        crate::profilecontext::explicit_enter(identity, self.owner.clone());
        let (entered, profiled) = capture::work_enter(
            identity,
            self.token.clone(),
            self.record.is_some() && self.runtime.accepting(),
        );
        if profiled {
            self.profiled.store(true, Relaxed);
        }
        WorkScope {
            identity,
            entered,
            _work: PhantomData,
            _thread: PhantomData,
        }
    }

    /// Associate a completed output with this job. Does not retain the buffer.
    pub fn propagate(&self, buffer: &gst::Buffer) {
        crate::propagation::write(buffer, &self.token, true, &self.runtime.stats);
    }

    /// Finish once, after all scopes and asynchronous device work have completed.
    pub fn finish(mut self, flow_return: i32) {
        if let Some(record) = self.record.as_mut() {
            record.flow_return = flow_return;
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            record.end = self.runtime.hook_now().unwrap_or(record.start);
            record.profiled = self.profiled.load(Relaxed);
            self.runtime.submit(record);
        }
    }
}

/// Thread-bound activation. The owned Work may move between threads when idle.
pub struct WorkScope<'a> {
    identity: usize,
    entered: bool,
    _work: PhantomData<&'a Work>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for WorkScope<'_> {
    fn drop(&mut self) {
        crate::profiles::clear();
        #[cfg(feature = "profiles")]
        crate::profilecontext::explicit_leave(self.identity);
        capture::work_leave(self.identity, self.entered);
    }
}

// Versioned optional plugin boundary. No Rust layouts cross the ABI; the caller
// retains this loaded module and pairs each opaque handle exactly once.
#[repr(C)]
pub struct ApiV1 {
    pub version: u32,
    pub size: usize,
    pub capture: unsafe extern "C" fn(
        *mut gst::ffi::GstElement,
        *mut gst::ffi::GstBuffer,
    ) -> *mut std::ffi::c_void,
    pub enter: unsafe extern "C" fn(*mut std::ffi::c_void) -> *mut std::ffi::c_void,
    pub leave: unsafe extern "C" fn(*mut std::ffi::c_void),
    pub propagate: unsafe extern "C" fn(*mut std::ffi::c_void, *mut gst::ffi::GstBuffer),
    pub finish: unsafe extern "C" fn(*mut std::ffi::c_void, i32),
}

unsafe extern "C" fn capture_ffi(
    element: *mut gst::ffi::GstElement,
    buffer: *mut gst::ffi::GstBuffer,
) -> *mut std::ffi::c_void {
    if element.is_null() || buffer.is_null() {
        return std::ptr::null_mut();
    }
    let element: glib::translate::Borrowed<gst::Element> =
        unsafe { glib::translate::from_glib_borrow(element) };
    let buffer: glib::translate::Borrowed<gst::Buffer> =
        unsafe { glib::translate::from_glib_borrow(buffer) };
    Work::capture(&element, &buffer).map_or(std::ptr::null_mut(), |work| {
        Box::into_raw(Box::new(work)).cast()
    })
}
unsafe extern "C" fn enter_ffi(work: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    if work.is_null() {
        return std::ptr::null_mut();
    }
    let work = unsafe { &*work.cast::<Work>() };
    Box::into_raw(Box::new(work.enter())).cast()
}
unsafe extern "C" fn leave_ffi(scope: *mut std::ffi::c_void) {
    if !scope.is_null() {
        drop(unsafe { Box::from_raw(scope.cast::<WorkScope<'_>>()) });
    }
}
unsafe extern "C" fn propagate_ffi(work: *mut std::ffi::c_void, buffer: *mut gst::ffi::GstBuffer) {
    if work.is_null() || buffer.is_null() {
        return;
    }
    let work = unsafe { &*work.cast::<Work>() };
    let buffer: glib::translate::Borrowed<gst::Buffer> =
        unsafe { glib::translate::from_glib_borrow(buffer) };
    work.propagate(&buffer);
}
unsafe extern "C" fn finish_ffi(work: *mut std::ffi::c_void, flow_return: i32) {
    if !work.is_null() {
        unsafe { Box::from_raw(work.cast::<Work>()) }.finish(flow_return);
    }
}
static API: ApiV1 = ApiV1 {
    version: 1,
    size: std::mem::size_of::<ApiV1>(),
    capture: capture_ffi,
    enter: enter_ffi,
    leave: leave_ffi,
    propagate: propagate_ffi,
    finish: finish_ffi,
};

/// Experimental ABI; see `native/work_context.h` for ownership requirements.
#[no_mangle]
pub extern "C" fn mars_work_context_v1() -> *const ApiV1 {
    &API
}
