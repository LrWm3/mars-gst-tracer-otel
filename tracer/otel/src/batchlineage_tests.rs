use super::*;
use opentelemetry::trace::{SpanId, TraceFlags, TraceId};
use std::sync::Arc;

fn input(id: u64, sampled: bool) -> Input {
    let mut token = Token::rejected();
    token.trace_id = TraceId::from(id as u128);
    token.span_id = SpanId::from(id);
    token.flags = if sampled {
        TraceFlags::SAMPLED
    } else {
        TraceFlags::default()
    };
    token.budget = Some(Arc::new(crate::tracebudget::Budget::new(0, 100, 4)));
    Input {
        token,
        links: None,
        derived: true,
    }
}

#[test]
fn batch_uses_one_sampled_contributor_but_demux_preserves_each_decision() {
    let rejected = input(1, false);
    let a = input(2, true);
    let b = input(3, true);
    let batch = select(
        vec![Some(rejected.clone()), Some(a.clone()), Some(b.clone())],
        false,
        None,
    );
    assert_eq!(batch.token, a.token);
    assert_eq!(batch.links.as_ref().unwrap().tokens.len(), 2);
    let mut active = batch.token.clone();
    active.span_id = SpanId::from(99);
    for original in [b, rejected, a] {
        // Out-of-order demux; no FIFO association.
        let frame = select(vec![Some(original.clone())], true, Some(&active));
        assert_eq!(frame.token, original.token);
        assert!(Arc::ptr_eq(
            frame.token.budget.as_ref().unwrap(),
            original.token.budget.as_ref().unwrap()
        ));
        if original.token.sampled() {
            assert!(frame.links.unwrap().tokens.contains(&active));
        } else {
            assert!(frame.links.is_none());
        }
    }
}

#[test]
fn missing_and_all_rejected_inputs_never_start_new_journeys() {
    let active = input(5, true);
    for frame in [false, true] {
        let result = select(vec![None], frame, Some(&active.token));
        assert!(!result.token.sampled());
    }
    assert!(!select(
        vec![None, Some(input(1, false)), Some(input(2, false))],
        false,
        None
    )
    .token
    .sampled());
}

#[test]
fn batch_links_are_bounded_and_budgets_are_not_replaced() {
    let inputs: Vec<_> = (1..=32).map(|id| Some(input(id, true))).collect();
    let budget = inputs[0].as_ref().unwrap().token.budget.clone().unwrap();
    let batch = select(inputs, false, None);
    assert!(Arc::ptr_eq(batch.token.budget.as_ref().unwrap(), &budget));
    let links = batch.links.unwrap();
    assert_eq!(links.tokens.len(), propagation::MAX_LINKS);
    assert_eq!(links.dropped, 23);
    for _ in 0..4 {
        budget.admit(1).unwrap();
    }
    assert_eq!(
        batch.token.budget.unwrap().admit(1),
        Err(crate::tracebudget::Reason::Spans)
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires SDK-built fixture; see BATCH_LINEAGE.md"]
fn sdk_metadata_roundtrip_exports_each_frames_trace_after_demux() {
    use crate::{
        capture,
        export::{
            tests::{runtime, Collector},
            Config,
        },
    };
    use gstreamer::prelude::*;
    use opentelemetry::trace::TraceContextExt;
    use std::time::Duration;
    gst::init().unwrap();
    let path = std::env::var("MARS_BATCH_TEST_ADAPTER").unwrap();
    let adapter = Adapter::load(&path).unwrap();
    let make_batch: unsafe extern "C" fn(
        *const *mut gst::ffi::GstBuffer,
        u32,
    ) -> *mut gst::ffi::GstBuffer = unsafe {
        let p = libc::dlsym(adapter.module, c"mars_test_make_batch".as_ptr());
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let demux: unsafe extern "C" fn(*mut gst::ffi::GstBuffer, u32) -> *mut gst::ffi::GstBuffer = unsafe {
        let p = libc::dlsym(adapter.module, c"mars_test_demux".as_ptr());
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let collector = Collector::default();
    let mut rt = runtime(Config::default(), collector.clone());
    rt.batch_lineage = Some(adapter);
    let source = gst::ElementFactory::make("fakesrc").build().unwrap();
    let source_pad = source.static_pad("src").unwrap();
    let mux = gst::ElementFactory::make("identity").build().unwrap();
    let mux_pad = mux.static_pad("src").unwrap();
    let demux_element = gst::ElementFactory::make("identity").build().unwrap();
    let demux_pad = demux_element.static_pad("src").unwrap();
    crate::metadata::set_test_batch_role(&source_pad, FRAME_INPUT);
    crate::metadata::set_test_batch_role(&mux_pad, BATCH_OUTPUT);
    crate::metadata::set_test_batch_role(&demux_pad, FRAME_OUTPUT);
    let originals: Vec<_> = (1..=3).map(|id| input(id, id != 1)).collect();
    let mut buffers = Vec::new();
    for original in &originals {
        let guard = opentelemetry::Context::new()
            .with_remote_span_context(original.token.context())
            .attach();
        let buffer = gst::Buffer::new();
        capture::begin(&rt, 10, &source_pad, &buffer);
        capture::end(&rt, 11, &source_pad, 0);
        drop(guard);
        buffers.push(buffer);
    }
    let pointers: Vec<_> = buffers.iter().map(|b| b.as_ptr() as *mut _).collect();
    let batch: gst::Buffer =
        unsafe { glib::translate::from_glib_full(make_batch(pointers.as_ptr(), 3)) };
    drop(buffers); // Native user-meta callbacks now own the input contexts.
    let batch_copy = batch.copy(); // Exercises SDK frame/user-meta copy callbacks.
    drop(batch);
    capture::begin(&rt, 12, &mux_pad, &batch_copy);
    let shared = capture::current_context().unwrap();
    assert_eq!(shared.trace_id(), TraceId::from(2));
    for index in [2, 0, 1] {
        let frame: gst::Buffer =
            unsafe { glib::translate::from_glib_full(demux(batch_copy.as_ptr() as *mut _, index)) };
        // Shared output has stale batch context: selection must replace it so
        // an asynchronous downstream push reads this frame's context instead.
        propagation::write(&frame, &Token::from_context(&shared), true, &rt.stats);
        let shared_output = frame.clone();
        capture::begin(&rt, 13, &demux_pad, &frame);
        let context = capture::current_context().unwrap();
        assert_eq!(context.trace_id(), originals[index as usize].token.trace_id);
        assert_eq!(context.is_sampled(), index != 0);
        assert_eq!(
            propagation::read(&shared_output).unwrap().context(),
            context
        );
        capture::end(&rt, 14, &demux_pad, 0);
        assert_eq!(capture::current_context().unwrap(), shared);
    }
    capture::end(&rt, 15, &mux_pad, 0);
    drop(batch_copy);
    let stats = rt.shutdown(Duration::from_secs(2)).unwrap();
    assert_eq!(stats.exported_spans, 5); // Two input + one shared + two output.
    assert_eq!(stats.batch_adapter_failures, 0);
    assert_eq!(stats.batch_missing_context, 0);
    assert_eq!(stats.batch_frames, 6);
    let spans = collector.0.lock().unwrap();
    for trace in [2, 3] {
        let output = spans
            .iter()
            .find(|s| {
                s.span_context.trace_id() == TraceId::from(trace)
                    && s.links
                        .iter()
                        .any(|l| l.span_context.span_id() == shared.span_id())
            })
            .unwrap();
        let parent = spans
            .iter()
            .find(|s| s.span_context.span_id() == output.parent_span_id)
            .unwrap();
        assert_eq!(
            parent.span_context.trace_id(),
            output.span_context.trace_id()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires SDK-built fixture; see BATCH_LINEAGE.md"]
fn sdk_metadata_limits_recycling_and_ownership() {
    gst::init().unwrap();
    let path = std::env::var("MARS_BATCH_TEST_ADAPTER").unwrap();
    let adapter = Adapter::load(&path).unwrap();
    let make_batch: unsafe extern "C" fn(
        *const *mut gst::ffi::GstBuffer,
        u32,
    ) -> *mut gst::ffi::GstBuffer = unsafe {
        let p = libc::dlsym(adapter.module, c"mars_test_make_batch".as_ptr());
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let new_mux: unsafe extern "C" fn(*mut gst::ffi::GstBuffer) -> *mut gst::ffi::GstBuffer = unsafe {
        let p = libc::dlsym(adapter.module, c"mars_test_new_mux".as_ptr());
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let stats = Counters::default();
    for _ in 0..1000 {
        let a = input(1, true);
        let weak = Arc::downgrade(a.token.budget.as_ref().unwrap());
        let buffer = gst::Buffer::new();
        propagation::write(&buffer, &a.token, true, &stats);
        adapter.attach(&buffer, &a.token, &stats);
        let batch: gst::Buffer = unsafe {
            glib::translate::from_glib_full(make_batch([buffer.as_ptr() as *mut _].as_ptr(), 1))
        };
        let b = input(2, true);
        // Reuse the input after a mux copied its metadata. Already queued
        // frames retain the earlier snapshot, including across SDK copies.
        propagation::write(&buffer, &b.token, true, &stats);
        let shared = buffer.clone();
        adapter.attach(&shared, &b.token, &stats);
        drop(shared);
        let batch_copy = batch.copy();
        assert_eq!(
            adapter.parent(&batch_copy, true, None, &stats).token,
            a.token
        );
        let newer: gst::Buffer = unsafe {
            glib::translate::from_glib_full(make_batch([buffer.as_ptr() as *mut _].as_ptr(), 1))
        };
        assert_eq!(adapter.parent(&newer, true, None, &stats).token, b.token);
        drop(batch);
        drop(batch_copy);
        drop(newer);
        drop(buffer);
        drop(a);
        drop(b);
        assert!(
            weak.upgrade().is_none(),
            "SDK metadata must release journey budgets"
        );
    }
    assert_eq!(stats.snapshot().batch_adapter_failures, 0);
    // New-mux SDK helper copies ordinary GstMeta into per-frame user metadata.
    let buffer = gst::Buffer::new();
    let a = input(7, true);
    propagation::write(&buffer, &a.token, true, &stats);
    let batch: gst::Buffer =
        unsafe { glib::translate::from_glib_full(new_mux(buffer.as_ptr() as *mut _)) };
    assert_eq!(adapter.parent(&batch, true, None, &stats).token, a.token);
    let plain = gst::Buffer::new();
    let missing: gst::Buffer = unsafe {
        glib::translate::from_glib_full(make_batch([plain.as_ptr() as *mut _].as_ptr(), 1))
    };
    assert!(!adapter
        .parent(&missing, true, Some(&a.token), &stats)
        .token
        .sampled());
    assert_eq!(stats.snapshot().batch_missing_context, 1);
    let pointers = vec![plain.as_ptr() as *mut _; MAX_FRAMES + 1];
    let oversized: gst::Buffer = unsafe {
        glib::translate::from_glib_full(make_batch(pointers.as_ptr(), pointers.len() as u32))
    };
    assert!(!adapter
        .parent(&oversized, false, Some(&a.token), &stats)
        .token
        .sampled());
    assert_eq!(stats.snapshot().batch_adapter_failures, 1);
    // A demux output with several frame records is ambiguous: never pick one.
    let ambiguous: gst::Buffer =
        unsafe { glib::translate::from_glib_full(make_batch(pointers.as_ptr(), 2)) };
    assert!(!adapter
        .parent(&ambiguous, true, Some(&a.token), &stats)
        .token
        .sampled());
    assert_eq!(stats.snapshot().batch_adapter_failures, 2);
    // Shared, initially unannotated inputs cannot acquire vendor metadata.
    let shared = plain.clone();
    adapter.attach(&shared, &a.token, &stats);
    assert_eq!(stats.snapshot().batch_adapter_failures, 3);
}
