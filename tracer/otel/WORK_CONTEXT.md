# Experimental asynchronous worker integration

An element's private worker can announce ownership with GStreamer
`STREAM_STATUS ENTER/LEAVE` messages, posted synchronously by the owning element
on that worker thread. Mars already uses these to register CPU sampling and
attribute CPU samples and GPU launches to a task owner. Pair the messages on
all exit paths and avoid an element/worker reference cycle.

Ownership alone does not identify a queued job's trace. The experimental
[work API](src/work.rs) carries an admitted input's context across a worker queue
and adds a `gst.element.process <factory>` span. Its duration covers admission,
queueing and processing through completion. Its thread attributes identify the
admission thread. Individual execution scopes supply CPU/GPU correlation on
whichever thread actually executes that job.

```rust,ignore
let work = gstoteltracer::work::Work::capture(element, input);
// Move work with the queued input, retaining it through device completion.
if let Some(work) = work.as_ref() {
    let _scope = work.enter();
    // Preprocess and enqueue this job's device work.
}
// Scope is inactive while waiting or executing another job.
if let Some(work) = work.as_ref() {
    let _scope = work.enter();
    // Finish this job's CPU postprocessing after device completion.
}
if let Some(work) = work {
    work.propagate(output);
    work.finish(gstreamer::ffi::GST_FLOW_OK);
}
```

Capture uses the active Mars call when present, otherwise the input buffer's
carried context. It creates no new trace roots, does not resample rejected work,
and shares the parent's age/span budget. Dropping an unfinished job records a
cancelled processing span. An admitted span can finish after the journey's age
limit; new descendants still undergo admission. The caller must keep its own
queue and in-flight job count bounded. Work retains no input buffer or element.

Scopes are thread-bound, nested, and must be dropped in reverse entry order.
Do not hold one across an async yield, idle queue wait, or another job's work.
GPU launch context is retained by the existing profiler until activity delivery;
the host scope need not remain active throughout GPU execution. Calls made by
additional hidden library threads need their own explicit handoff.

For a separately loaded processing plugin, use the optional versioned
[C function table](native/work_context.h) exported as `mars_work_context_v1`.
Discover it from the already loaded Mars module and keep that module loaded.
Check both version and structure size. Null handles are no-ops. Do not link a
second static copy of Mars into a plugin: that would create separate globals.
The header specifies handle ownership and the lifetime of borrowed scopes.
This ABI is experimental and is not yet a compatibility commitment.

Use `mars-instrument(signals="traces,profiles")` with the desired profile
sources. GPU collection still requires a `gpu-profiles` build and its native
adapter; CPU correlation works without that feature. The API does not enable
signals or change tracer configuration. Profiles-only collection can use
thread-ownership messages without creating processing spans.

One batch gets one processing span and one accounting of its GPU work. Bounded
links can identify additional contributing inputs; this does not restore
individual frame lineage after demultiplexing. It also does not recover context
already lost upstream.

Validation includes exported parent/span relationships, cancellation, shared
age/span limits, context restoration, CPU worker attribution, and GPU activities
delivered after their host scopes finish. Adapter regression tests use simulated
activities. A separate native C API probe also passed with a real CUDA/CUPTI
runtime: two jobs executed in reverse order after their source calls returned,
and each span selected only its own CPU samples and GPU kernel profile. Their
combined GPU profiles accounted for all recorded kernel time; a cancelled job
and an unrelated span ID selected no samples. This validates explicit handoff,
not automatic propagation through arbitrary third-party worker queues.
