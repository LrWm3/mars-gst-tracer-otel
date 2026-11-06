/* Experimental optional async-work ABI. Discover mars_work_context_v1 from the
 * already loaded Mars plugin; retain its module while any handle is live.
 * All functions accept NULL handles as a no-op. GStreamer pointers are borrowed.
 * A Work is owned: move between threads only while idle; finish exactly once.
 * A scope borrows its Work: leave exactly once on its entering thread, in LIFO
 * order, before finishing the Work. Never hold a scope across an async yield.
 * Call capture at admission, enter around execution, propagate to completed
 * output, and finish after device completion. GST_FLOW_FLUSHING cancels a job.
 * Capture creates no roots and retains no buffers/elements. Its processing span
 * includes queue time and shares its parent's age/span admission budget.
 */
#ifndef MARS_WORK_CONTEXT_H
#define MARS_WORK_CONTEXT_H
#include <gst/gst.h>
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct {
    uint32_t version;
    size_t size;
    void *(*capture)(GstElement *, GstBuffer *);
    void *(*enter)(void *);
    void (*leave)(void *);
    void (*propagate)(void *, GstBuffer *);
    void (*finish)(void *, int32_t flow_return);
} MarsWorkContextV1;
const MarsWorkContextV1 *mars_work_context_v1(void);
#ifdef __cplusplus
}
#endif
#endif
