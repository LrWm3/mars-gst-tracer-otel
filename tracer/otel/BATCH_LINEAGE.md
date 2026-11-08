# Experimental video batch lineage

The optional DeepStream adapter preserves each input frame's trace context
through `nvstreammux` and restores it at `nvstreamdemux` output. It uses SDK
metadata ownership and copy callbacks; it does not match frames by PTS, buffer
address, source order, or a timeout. It is separate from GPU profiling and does
not require the `gpu-profiles` Cargo feature.

**Status:** local parent-selection tests and native DeepStream 6.2 metadata
round trips pass. Actual mux/demux execution on a GPU device is still pending.
Keep this opt-in until validated against the deployed SDK and pipeline.

## Enable

Build Mars normally, and build the separate adapter against the target's SDK:

```sh
sh tracer/otel/native/build-deepstream-batch.sh \
  /opt/nvidia/deepstream/deepstream /opt/mars/libmars-deepstream-batch.so

export GST_TRACERS='mars-instrument(signals="metrics,traces",monitoring=core,trace-batch-adapter="/opt/mars/libmars-deepstream-batch.so")'
```

`trace-batch-adapter` is a construct-only tracer property, disabled when unset.
It works with `mars-instrument` and `mars-otel` on Linux. A missing or incompatible
adapter fails trace initialization. When traces are disabled, it is not loaded.
SDK headers/libraries are required only for the adapter, not Mars. The build
embeds the supplied SDK library directory as a runtime search path; build on
the target or use the same SDK path in its runtime image.

## Trace relationships

1. A push into `nvstreammux` attaches a metadata-only context snapshot through
   `GstNvDsMeta`. Rejected inputs carry their decision too. SDK transforms retain
   the snapshot on that frame's `NvDsUserMeta`.
2. At mux output, shared batch work uses the **first already sampled frame**
   in SDK frame-list order as its parent, with at most eight contributor links.
   Other frames are not resampled. A batch with no sampled inputs is rejected.
   The chosen parent's existing age/span budget still applies; an exhausted
   parent is not replaced with another contributor to evade its budget.
3. At demux output, the single output frame's SDK metadata restores its original
   parent and sampling decision, even if a different batch trace is active.
   Sampled outputs link to the active batch span when one exists. These links
   do not duplicate the batch span into every trace.
4. Each restored frame retains its original shared budget. Branches of that
   frame continue to share it. Unrelated input journeys do not acquire the
   chosen batch parent's remaining allowance or rejection.

A frame's waterfall may jump from its mux-input span to its demux output, with
a **span link** to shared batch work. A single parent/child tree cannot put the
same batch span under several different trace IDs.

The adapter also reads the new mux's documented
`NVDS_BUFFER_GST_AS_FRAME_USER_META` carrier when an explicit Mars snapshot is
absent. See NVIDIA's [metadata propagation contract](https://docs.nvidia.com/metropolis/deepstream/7.1/text/DS_plugin_gst-nvstreammux2.html#metadata-propagation-through-nvstreammux-and-nvstreamdemux).

## Limits and ownership

- Video only; no audio batches or cascaded batch-of-batch muxing.
- Up to 256 frame records and 256 user-meta records per frame are inspected.
  Oversized associations are rejected rather than partially guessed. A demux
  output must describe exactly one frame. Output buffer lists are unsupported.
- Missing metadata suppresses the affected frame. It never starts a replacement
  root or inherits whichever other frame happens to be active.
- Initially shared input buffers cannot acquire new vendor metadata. Existing
  Mars vendor slots can be updated under a short mutex; copies snapshot them.
  Shared inputs use their common-ancestor context, matching normal Mars buffer
  propagation. Recycled inputs cannot change contexts already copied to a batch.
- Storage follows live input/batch/output buffers. There is no pending-job map
  or process-lifetime frame registry. Carriers retain contexts and budgets,
  not payload buffers or elements. Adapter code stays resident because SDK
  callbacks can outlive the tracer runtime; this is a fixed mapping.
- Elements that remove vendor frame metadata can still break lineage. Copies
  that lose Mars's initially-shared qdata fallback retain the usual propagation
  limitation; the adapter does not infer missing context.

`mars-otel-stats` adds `batch_frames`, `batch_missing_context`, and
`batch_adapter_failures`. With core/all monitoring, it also exposes
`mars_instrument_batch_contexts_total{outcome="observed|missing|adapter_failure"}`.
Observed counts include mux and demux inspections. Failures include failed
attachment, missing batch metadata, ambiguous outputs, and traversal limits.
Contributor links retain the usual `dropped_links_count` bound.

## Validation

Normal `cargo test` covers selection, rejection, links and budget identity.
To exercise real SDK metadata callbacks without a GPU:

```sh
sh tracer/otel/native/build-deepstream-batch.sh \
  /opt/nvidia/deepstream/deepstream /tmp/libmars-deepstream-batch-test.so --test
MARS_BATCH_TEST_ADAPTER=/tmp/libmars-deepstream-batch-test.so \
  G_DEBUG=fatal-criticals cargo test -p gst-otel-tracer --lib sdk_metadata \
  -- --ignored --test-threads=1
```

The fixture uses real SDK metadata allocation, copying, and destruction. It
simulates the mux's documented transform handoff; it does **not** execute the
actual GPU mux/demux plugins. Tests verify exported parent IDs, out-of-order
restoration, a rejected first input, shared output overrides, the new-mux copy
helper, missing/ambiguous/oversized metadata, recycled inputs, and 1,000 ownership
cycles that release every tracked journey budget.

Before deployment, run the real pipeline and check these counters, contributor
links, per-frame trace IDs, and enabled/disabled overhead with normal trace
limits. Private processor evidence belongs outside this repository.

This does not activate context inside an element's private inference worker.
That integration remains unchanged; see [worker context](WORK_CONTEXT.md).
