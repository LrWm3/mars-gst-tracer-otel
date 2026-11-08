//! Real DeepStream mux/demux check. Run on a compatible CUDA device:
//! cargo run -p gst-otel-tracer --release --example batch_lineage -- ADAPTER
//! Add `mixed` for a 16 ms root gate at 100 fps; use `-` instead of ADAPTER for
//! the control. PTS is only a test oracle: the adapter never matches by PTS.
use gstreamer::{self as gst, prelude::*};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug)]
struct Frame {
    stage: &'static str,
    pts: Option<gst::ClockTime>,
    trace: Option<opentelemetry::trace::TraceId>,
}

fn main() {
    let adapter = std::env::args()
        .nth(1)
        .expect("usage: batch_lineage ADAPTER_OR_- [mixed]");
    let mixed = std::env::args().nth(2).as_deref() == Some("mixed");
    gst::init().unwrap();
    let mut builder = glib::Object::builder::<gstoteltracer::InstrumentTracer>()
        .property("signals", "traces")
        .property(
            "trace-root-min-period-us",
            if mixed { 16_000u32 } else { 0 },
        );
    if adapter != "-" {
        builder = builder.property("trace-batch-adapter", &adapter);
    }
    let _tracer = builder.build();
    gstoteltracer::stats().expect("tracer initialization failed");
    let rate = if mixed { 100 } else { 30 };
    let mut launch = String::from(
        "nvstreammux name=mux batch-size=2 width=320 height=240 live-source=0 \
         batched-push-timeout=100000 ! identity ! nvstreamdemux name=demux ",
    );
    for stream in 0..2 {
        launch.push_str(&format!(
            "demux.src_{stream} ! queue ! fakesink name=out{stream} sync=false \
             async=false signal-handoffs=true videotestsrc num-buffers=20 is-live={mixed} \
             ! video/x-raw,width=320,height=240,framerate={rate}/1 ! nvvideoconvert \
             ! video/x-raw(memory:NVMM),format=NV12,width=320,height=240 \
             ! identity name=in{stream} ! mux.sink_{stream} "
        ));
    }
    let pipeline = gst::parse::launch(&launch)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
    let frames = Arc::new(Mutex::new(Vec::new()));
    for stage in ["in0", "in1", "out0", "out1"] {
        let frames = frames.clone();
        pipeline
            .by_name(stage)
            .unwrap()
            .connect("handoff", false, move |values| {
                let buffer = values[1].get::<gst::Buffer>().unwrap();
                frames.lock().unwrap().push(Frame {
                    stage,
                    pts: buffer.pts(),
                    trace: gstoteltracer::current_span_context()
                        .filter(|c| c.is_sampled())
                        .map(|c| c.trace_id()),
                });
                None
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let message = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    // Stop even on an error or timeout before reporting a failed check.
    pipeline.set_state(gst::State::Null).unwrap();
    let stats = gstoteltracer::shutdown(Duration::from_secs(10)).unwrap();
    let message = message.expect("pipeline timed out");
    if let gst::MessageView::Error(error) = message.view() {
        panic!("{} {:?}", error.error(), error.debug());
    }
    let frames = frames.lock().unwrap();
    let outputs: Vec<_> = frames
        .iter()
        .filter(|f| f.stage.starts_with("out"))
        .collect();
    let mut matched = 0;
    for frame in &outputs {
        let input = if frame.stage == "out0" { "in0" } else { "in1" };
        let candidates: Vec<_> = frames
            .iter()
            .filter(|f| f.stage == input && f.pts == frame.pts)
            .collect();
        assert_eq!(
            candidates.len(),
            1,
            "test requires unique input PTS per stream"
        );
        matched += usize::from(candidates[0].trace == frame.trace);
    }
    let sampled = outputs.iter().filter(|f| f.trace.is_some()).count();
    println!(
        "outputs={} sampled={sampled} matched={matched}\n{stats:?}",
        outputs.len()
    );
    assert_eq!(outputs.len(), 40);
    if adapter != "-" {
        assert_eq!(
            matched, 40,
            "each frame must retain its own decision and trace"
        );
        assert_eq!(stats.batch_adapter_failures, 0);
        assert_eq!(stats.batch_missing_context, 0);
        if mixed {
            assert!(sampled > 0 && sampled < 40);
        }
    }
}
