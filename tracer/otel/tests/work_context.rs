//! Inspect exported spans from deferred work, including cancellation and limits.
use gstreamer::{self as gst, prelude::*, subclass::prelude::*};
use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest, trace::v1::Span,
};
use prost::Message;
use std::{
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

mod delayed {
    use super::*;
    #[derive(Default)]
    pub struct Dispatch(std::sync::atomic::AtomicBool);
    #[glib::object_subclass]
    impl ObjectSubclass for Dispatch {
        const NAME: &'static str = "MarsWorkDelayedDispatch";
        type Type = super::DelayedDispatch;
        type ParentType = gst::Tracer;
    }
    impl ObjectImpl for Dispatch {
        fn constructed(&self) {
            self.parent_constructed();
            self.register_hook(TracerHook::PadPushPre);
        }
    }
    impl GstObjectImpl for Dispatch {}
    impl TracerImpl for Dispatch {
        fn pad_push_pre(&self, _: u64, _: &gst::Pad, _: &gst::Buffer) {
            if !self.0.swap(true, std::sync::atomic::Ordering::Relaxed) {
                // GStreamer timestamps before dispatching to all tracers. Delay
                // Mars's first calibration while later hooks run normally.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
glib::wrapper! {
    pub struct DelayedDispatch(ObjectSubclass<delayed::Dispatch>) @extends gst::Tracer, gst::Object;
}

#[test]
fn queued_work_preserves_parentage_cancellation_and_shared_limits() {
    for mode in ["normal", "delayed-dispatch", "budget", "age", "off"] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let (stop, stopping) = mpsc::channel();
        let receiver = std::thread::spawn(move || {
            let mut spans = Vec::<Span>::new();
            while stopping.try_recv().is_err() {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                else {
                    continue;
                };
                let mut bytes = Vec::new();
                request.as_reader().read_to_end(&mut bytes).unwrap();
                let request_data = ExportTraceServiceRequest::decode(bytes.as_slice()).unwrap();
                for resource in request_data.resource_spans {
                    for scope in resource.scope_spans {
                        spans.extend(scope.spans);
                    }
                }
                request.respond(tiny_http::Response::empty(200)).unwrap();
            }
            spans
        });
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "work_child", "--ignored", "--nocapture"])
            .env_clear()
            .env("WORK_MODE", mode)
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", &url)
            .env(
                "OTEL_TRACES_SAMPLER",
                if mode == "off" {
                    "parentbased_always_off"
                } else {
                    "parentbased_always_on"
                },
            )
            .output()
            .unwrap();
        stop.send(()).unwrap();
        let spans = receiver.join().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "off" {
            assert!(spans.is_empty());
            continue;
        }
        let work: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "gst.element.process identity")
            .collect();
        assert_eq!(
            work.len(),
            if matches!(mode, "normal" | "delayed-dispatch") {
                3
            } else {
                2
            },
            "{mode}"
        );
        assert_eq!(
            work.iter()
                .filter(|s| s.status.as_ref().is_some_and(|s| s.code == 2))
                .count(),
            1,
            "one cancelled job"
        );
        for job in &work {
            let parent = spans
                .iter()
                .find(|s| s.span_id == job.parent_span_id)
                .unwrap();
            assert_eq!(parent.trace_id, job.trace_id);
            assert!(job.start_time_unix_nano >= parent.start_time_unix_nano);
            assert!(job.end_time_unix_nano >= job.start_time_unix_nano);
            if parent.name != "gst.element.process identity" {
                assert!(
                    job.end_time_unix_nano > parent.end_time_unix_nano,
                    "processing outlives enqueue call"
                );
            }
        }
        if mode == "budget" {
            assert_eq!(
                spans.len(),
                4,
                "two roots each admit only their processing span"
            );
        }
    }
}

#[test]
#[ignore = "isolated tracer globals"]
fn work_child() {
    let mode = std::env::var("WORK_MODE").unwrap();
    gst::init().unwrap();
    let _tracer: gstoteltracer::InstrumentTracer = glib::Object::builder()
        .property("params", "signals=traces")
        .property("trace-root-min-period-us", 0u32)
        .property(
            "trace-max-spans",
            if mode == "budget" { 2u32 } else { 64u32 },
        )
        .property(
            "trace-max-age-ms",
            if mode == "age" { 100u32 } else { 5000u32 },
        )
        .build();
    let _delay = (mode == "delayed-dispatch").then(glib::Object::new::<DelayedDispatch>);
    let pipeline =
        gst::parse::launch("fakesrc num-buffers=2 ! identity name=worker ! fakesink sync=false")
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
    let owner = pipeline.by_name("worker").unwrap();
    let jobs = Arc::new(Mutex::new(Vec::new()));
    let sink = jobs.clone();
    owner.connect("handoff", false, move |values| {
        let owner = values[0].get::<gst::Element>().unwrap();
        let buffer = values[1].get::<gst::Buffer>().unwrap();
        sink.lock().unwrap().push((
            gstoteltracer::work::Work::capture(&owner, &buffer).unwrap(),
            buffer,
        ));
        None
    });
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .unwrap();
    assert_eq!(message.type_(), gst::MessageType::Eos);
    if mode == "age" {
        std::thread::sleep(Duration::from_millis(150));
    }
    let mut jobs = std::mem::take(&mut *jobs.lock().unwrap());
    let cancelled = jobs.pop().unwrap();
    drop(cancelled);
    let (job, buffer) = jobs.pop().unwrap();
    let sampled = matches!(mode.as_str(), "normal" | "delayed-dispatch");
    std::thread::spawn(move || {
        assert!(gstoteltracer::current_span_context().is_none());
        {
            let scope = job.enter();
            let parent = gstoteltracer::current_span_context();
            let nested = gstoteltracer::work::Work::capture(&owner, &buffer).unwrap();
            {
                let _scope = nested.enter();
                assert_eq!(
                    gstoteltracer::current_span_context().is_some_and(|s| s.is_sampled()),
                    sampled
                );
            }
            assert_eq!(gstoteltracer::current_span_context(), parent);
            nested.finish(gst::ffi::GST_FLOW_OK);
            drop(scope);
        }
        assert!(gstoteltracer::current_span_context().is_none());
        job.propagate(&buffer);
        job.finish(gst::ffi::GST_FLOW_OK);
    })
    .join()
    .unwrap();
    pipeline.set_state(gst::State::Null).unwrap();
    gstoteltracer::shutdown(Duration::from_secs(5)).unwrap();
    assert!(
        gstoteltracer::work::Work::capture(pipeline.upcast_ref(), &gst::Buffer::new()).is_none()
    );
    let stats = gstoteltracer::stats().unwrap();
    assert_eq!(stats.mismatched_hooks, 0);
    if mode == "budget" {
        assert!(stats.trace_span_rejections > 0);
    }
    if mode == "age" {
        assert!(stats.trace_age_rejections > 0);
    }
    #[cfg(feature = "profiles")]
    assert_eq!(
        gstoteltracer::profile_stats().context_descriptions,
        0,
        "trace-only work must not allocate profile descriptions"
    );
}
