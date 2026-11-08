/* Optional DeepStream video lineage adapter. Build against the deployed SDK.
 * Legacy mux: explicit GstNvDsMeta -> per-frame NvDsUserMeta callbacks.
 * New mux: also recognizes its documented per-frame copied GstMeta carrier.
 * State is owned by buffers, with no timestamp/address lookup or pending map.
 */
#include "batch_adapter.h"
#include <gstnvdsmeta.h>

typedef struct { GMutex lock; GstBuffer *buffer; } Context;
static Context *new_context(GstBuffer *buffer) {
    Context *context = g_new0(Context, 1);
    g_mutex_init(&context->lock);
    context->buffer = gst_buffer_ref(buffer);
    return context;
}

static gint context_type(void) {
    static gsize value;
    if (g_once_init_enter(&value)) {
        gint type = nvds_get_user_meta_type("MARS.TRACE.FRAME_CONTEXT.V1");
        g_once_init_leave(&value, (gsize)type);
    }
    return (gint)value;
}
static gpointer copy_context(gpointer data, gpointer unused) {
    (void)unused;
    if (!data) return NULL;
    Context *context = data;
    g_mutex_lock(&context->lock);
    Context *copy = new_context(context->buffer);
    g_mutex_unlock(&context->lock);
    return copy;
}
static void free_context(gpointer data, gpointer unused) {
    (void)unused;
    if (!data) return;
    Context *context = data;
    gst_buffer_unref(context->buffer);
    g_mutex_clear(&context->lock);
    g_free(context);
}
static gpointer copy_frame_context(gpointer data, gpointer unused) {
    NvDsUserMeta *meta = data;
    return copy_context(meta->user_meta_data, unused);
}
static void free_frame_context(gpointer data, gpointer unused) {
    NvDsUserMeta *meta = data;
    free_context(meta->user_meta_data, unused);
    meta->user_meta_data = NULL;
}
static int32_t attach(GstBuffer *buffer, GstBuffer *context) {
    if (!context) return 0;
    /* Cascaded batches require a separate contract. Do not flatten them. */
    if (gst_buffer_get_nvds_batch_meta(buffer)) return 0;
    gpointer state = NULL;
    GstMeta *raw;
    unsigned visited = 0;
    while ((raw = gst_buffer_iterate_meta(buffer, &state))) {
        if (++visited > 256) return 0;
        if (raw->info->api == NVDS_META_API_TYPE) {
            NvDsMeta *meta = (NvDsMeta *)raw;
            if (meta->meta_type == context_type()) {
                /* A recycled buffer must not keep an earlier frame's token. */
                Context *slot = meta->meta_data;
                g_mutex_lock(&slot->lock);
                GstBuffer *previous = slot->buffer;
                slot->buffer = gst_buffer_ref(context);
                g_mutex_unlock(&slot->lock);
                gst_buffer_unref(previous);
                return 1;
            }
        }
    }
    if (!gst_buffer_is_writable(buffer)) return 0;
    Context *owned = new_context(context);
    NvDsMeta *meta = gst_buffer_add_nvds_meta(buffer, owned, NULL,
                                            copy_context, free_context);
    if (!meta) { free_context(owned, NULL); return 0; }
    meta->meta_type = context_type();
    meta->gst_to_nvds_meta_transform_func = copy_frame_context;
    meta->gst_to_nvds_meta_release_func = free_frame_context;
    return 1;
}
static int32_t read_contexts(GstBuffer *buffer, MarsBatchVisit visit,
                             void *user, uint32_t limit) {
    NvDsBatchMeta *batch = gst_buffer_get_nvds_batch_meta(buffer);
    if (!batch) return -1;
    nvds_acquire_meta_lock(batch);
    int32_t count = 0;
    for (GList *node = batch->frame_meta_list; node; node = node->next) {
        if ((uint32_t)count >= limit) { count = -2; break; }
        NvDsFrameMeta *frame = node->data;
        Context *explicit_context = NULL;
        GstBuffer *copied_context = NULL;
        unsigned visited = 0;
        for (GList *item = frame->frame_user_meta_list; item; item = item->next) {
            if (++visited > 256) { count = -2; break; }
            NvDsUserMeta *meta = item->data;
            if ((gint)meta->base_meta.meta_type == context_type()) {
                if (explicit_context) { count = -1; break; }
                explicit_context = meta->user_meta_data;
            } else if ((gint)meta->base_meta.meta_type == NVDS_BUFFER_GST_AS_FRAME_USER_META) {
                if (copied_context) { count = -1; break; }
                copied_context = meta->user_meta_data;
            }
        }
        if (count < 0) break;
        if (explicit_context) {
            g_mutex_lock(&explicit_context->lock);
            visit(user, explicit_context->buffer);
            g_mutex_unlock(&explicit_context->lock);
        } else {
            visit(user, copied_context);
        }
        count++;
    }
    nvds_release_meta_lock(batch);
    return count;
}
static const MarsBatchAdapterV1 api = {1, sizeof(MarsBatchAdapterV1), attach, read_contexts};
const MarsBatchAdapterV1 *mars_batch_adapter_v1(void) {
    /* Initializes SDK tag quarks before querying a buffer with ordinary meta. */
    (void)nvds_meta_get_info();
    return &api;
}
