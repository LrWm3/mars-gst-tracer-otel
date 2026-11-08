/* Optional batch lineage ABI. No vendor layouts cross this boundary.
 * Callbacks are synchronous, on the calling streaming thread. Buffer pointers
 * are borrowed. attach() retains an immutable, metadata-only context buffer.
 * read() visits one context (possibly NULL) per frame in vendor order, at most
 * limit times, and returns the frame count; negative means missing/unsupported
 * metadata (-1) or a traversal limit (-2). Never infer a frame from timestamps.
 * The module must remain resident while attached buffers can call its copy/free
 * functions. Only attach() may mutate metadata. Adding/removing GstMeta requires
 * a writable data buffer; updating an existing owned payload requires internal
 * synchronization and copy callbacks that snapshot it.
 */
#ifndef MARS_BATCH_ADAPTER_H
#define MARS_BATCH_ADAPTER_H
#include <gst/gst.h>
#include <stdint.h>
#include <stddef.h>
typedef void (*MarsBatchVisit)(void *user, GstBuffer *context);
typedef struct {
    uint32_t version;
    size_t size;
    int32_t (*attach)(GstBuffer *buffer, GstBuffer *context);
    int32_t (*read)(GstBuffer *buffer, MarsBatchVisit visit, void *user, uint32_t limit);
} MarsBatchAdapterV1;
const MarsBatchAdapterV1 *mars_batch_adapter_v1(void);
#endif
