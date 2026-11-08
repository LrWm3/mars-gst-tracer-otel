//! Parent selection at packet assembly, copying, and rejected call boundaries.
use super::*;
use crate::export::{
    tests::{runtime, Collector},
    Config,
};
use std::time::Duration;

struct Fixture {
    runtime: Runtime,
    collector: Collector,
    source: gst::Element,
    middle: gst::Element,
}
impl Fixture {
    fn new(config: Config) -> Self {
        gst::init().unwrap();
        let collector = Collector::default();
        Self {
            runtime: runtime(config, collector.clone()),
            collector,
            source: gst::ElementFactory::make("fakesrc").build().unwrap(),
            middle: gst::ElementFactory::make("identity").build().unwrap(),
        }
    }
    fn source(&self) -> gst::Pad {
        self.source.static_pad("src").unwrap()
    }
    fn output(&self) -> gst::Pad {
        self.middle.static_pad("src").unwrap()
    }
    fn seed(&self) -> (gst::Buffer, SpanContext) {
        let buffer = gst::Buffer::new();
        begin(&self.runtime, 100, &self.source(), &buffer);
        let context = current_context().unwrap();
        end(&self.runtime, 101, &self.source(), 0);
        (buffer, context)
    }
    fn reject(&self, ts: u64) {
        begin(&self.runtime, ts, &self.source(), &gst::Buffer::new());
        assert!(!current_context().unwrap().is_sampled());
    }
    fn finish(&self) -> crate::export::ExportStats {
        self.runtime.shutdown(Duration::from_secs(2)).unwrap()
    }
}

#[test]
fn copied_contributor_continues_through_a_rejected_emitting_packet() {
    let f = Fixture::new(Config::default());
    let (input, parent) = f.seed();
    let output = input.copy();
    drop(input); // Metadata owns its context, independently of the input lifetime.
    f.reject(102);
    begin(&f.runtime, 103, &f.output(), &output);
    let frame = current_context().unwrap();
    assert!(frame.is_sampled());
    assert_eq!(frame.trace_id(), parent.trace_id());
    // Ordinary nested calls use the immediate parent, not the copied ancestor.
    begin(&f.runtime, 104, &f.output(), &output);
    let child = current_context().unwrap();
    end(&f.runtime, 105, &f.output(), 0);
    assert_eq!(current_context().unwrap(), frame);
    end(&f.runtime, 106, &f.output(), 0);
    assert!(!current_context().unwrap().is_sampled());
    end(&f.runtime, 107, &f.source(), 0);
    assert!(current_context().is_none());
    assert_eq!(f.finish().exported_spans, 3);
    let spans = f.collector.0.lock().unwrap();
    assert_eq!(
        spans
            .iter()
            .find(|s| s.span_context == frame)
            .unwrap()
            .parent_span_id,
        parent.span_id()
    );
    assert_eq!(
        spans
            .iter()
            .find(|s| s.span_context == child)
            .unwrap()
            .parent_span_id,
        frame.span_id()
    );
}

#[test]
fn external_rejection_cannot_be_overridden_by_copied_metadata() {
    for remote in [false, true] {
        let f = Fixture::new(Config::default());
        let (input, _) = f.seed();
        let output = input.copy();
        let rejected = SpanContext::new(
            TraceId::from(77),
            SpanId::from(88),
            TraceFlags::default(),
            remote,
            TraceState::default(),
        );
        let guard = opentelemetry::Context::new()
            .with_remote_span_context(rejected.clone())
            .attach();
        f.reject(102);
        begin(&f.runtime, 103, &f.output(), &output);
        assert_eq!(current_context().unwrap(), rejected);
        end(&f.runtime, 104, &f.output(), 0);
        end(&f.runtime, 105, &f.source(), 0);
        drop(guard);
        assert_eq!(f.finish().exported_spans, 1);
    }
}

#[test]
fn shared_ancestor_and_recycled_source_are_not_new_contributors() {
    let f = Fixture::new(Config::default());
    let (input, _) = f.seed();
    let shared = input.clone();
    f.reject(102);
    begin(&f.runtime, 103, &f.output(), &shared);
    assert!(!current_context().unwrap().is_sampled());
    end(&f.runtime, 104, &f.output(), 0);
    end(&f.runtime, 105, &f.source(), 0);
    let recycled = input.copy();
    let retained = recycled.clone();
    begin(&f.runtime, 106, &f.source(), &recycled);
    begin(&f.runtime, 107, &f.output(), &retained);
    assert!(!current_context().unwrap().is_sampled());
    end(&f.runtime, 108, &f.output(), 0);
    end(&f.runtime, 109, &f.source(), 0);
    assert_eq!(f.finish().exported_spans, 1);
}

#[test]
fn copied_contributors_keep_their_shared_span_and_age_limits() {
    for age in [false, true] {
        let f = Fixture::new(Config {
            max_trace_spans: if age { 0 } else { 2 },
            max_trace_age_ns: if age { 5 } else { 0 },
            ..Config::default()
        });
        let (input, parent) = f.seed();
        f.reject(102);
        let output = input.copy();
        begin(&f.runtime, 103, &f.output(), &output);
        assert_eq!(current_context().unwrap().trace_id(), parent.trace_id());
        assert!(current_context().unwrap().is_sampled());
        end(&f.runtime, 104, &f.output(), 0);
        // A fresh copy cannot reset the sampled contributor's shared allowance.
        let late = input.copy();
        begin(&f.runtime, 105, &f.output(), &late);
        assert!(!current_context().unwrap().is_sampled());
        end(&f.runtime, 106, &f.output(), 0);
        end(&f.runtime, 107, &f.source(), 0);
        let stats = f.finish();
        assert_eq!(stats.exported_spans, 2);
        assert_eq!(
            if age {
                stats.trace_age_rejections
            } else {
                stats.trace_span_rejections
            },
            1
        );
    }
}

#[test]
fn sampled_active_parent_still_wins_over_another_copied_input() {
    let f = Fixture::new(Config {
        root_min_period_ns: 0,
        ..Config::default()
    });
    let (input, first) = f.seed();
    begin(&f.runtime, 102, &f.source(), &gst::Buffer::new());
    let active = current_context().unwrap();
    assert_ne!(first.trace_id(), active.trace_id());
    begin(&f.runtime, 103, &f.output(), &input.copy());
    assert_eq!(current_context().unwrap().trace_id(), active.trace_id());
    end(&f.runtime, 104, &f.output(), 0);
    end(&f.runtime, 105, &f.source(), 0);
    assert_eq!(f.finish().exported_spans, 3);
}
