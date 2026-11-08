/* SDK metadata fixture, not a replacement for a real mux/demux device test.
 * Reproduces the documented GstNvDsMeta transform handoff; exercises real SDK
 * frame copies, user-meta callbacks and destruction without requiring a GPU.
 */
#include "batch_adapter.h"
#include <gstnvdsmeta.h>

static GstBuffer *new_batch(unsigned count, NvDsBatchMeta **out) {
    GstBuffer *buffer = gst_buffer_new();
    NvDsBatchMeta *batch = nvds_create_batch_meta(count);
    g_assert(batch);
    batch->max_frames_in_batch = count;
    NvDsMeta *meta = gst_buffer_add_nvds_meta(buffer, batch, NULL,
        nvds_batch_meta_copy_func, nvds_batch_meta_release_func);
    g_assert(meta);
    meta->meta_type = NVDS_BATCH_GST_META;
    *out = batch;
    return buffer;
}

GstBuffer *mars_test_make_batch(GstBuffer **inputs, uint32_t count) {
    NvDsBatchMeta *batch;
    GstBuffer *output = new_batch(count, &batch);
    for (uint32_t i = 0; i < count; i++) {
        NvDsFrameMeta *frame = nvds_acquire_frame_meta_from_pool(batch);
        g_assert(frame);
        frame->source_id = i;
        frame->batch_id = i;
        nvds_add_frame_meta_to_batch(batch, frame);
        gpointer state = NULL;
        GstMeta *raw;
        while ((raw = gst_buffer_iterate_meta(inputs[i], &state))) {
            if (raw->info->api != NVDS_META_API_TYPE) continue;
            NvDsMeta *meta = (NvDsMeta *)raw;
            if (!meta->gst_to_nvds_meta_transform_func) continue;
            NvDsUserMeta *user = nvds_acquire_user_meta_from_pool(batch);
            g_assert(user);
            user->base_meta.meta_type = meta->meta_type;
            user->base_meta.copy_func = meta->gst_to_nvds_meta_transform_func;
            user->base_meta.release_func = meta->gst_to_nvds_meta_release_func;
            user->user_meta_data = meta->meta_data;
            user->user_meta_data = meta->gst_to_nvds_meta_transform_func(user, NULL);
            nvds_add_user_meta_to_frame(frame, user);
        }
    }
    return output;
}

GstBuffer *mars_test_demux(GstBuffer *input, uint32_t index) {
    NvDsBatchMeta *src = gst_buffer_get_nvds_batch_meta(input), *dest;
    g_assert(src);
    NvDsFrameMeta *frame = g_list_nth_data(src->frame_meta_list, index);
    g_assert(frame);
    GstBuffer *output = new_batch(1, &dest);
    NvDsFrameMeta *copy = nvds_acquire_frame_meta_from_pool(dest);
    g_assert(copy);
    nvds_copy_frame_meta(frame, copy);
    nvds_add_frame_meta_to_batch(dest, copy);
    return output;
}

GstBuffer *mars_test_new_mux(GstBuffer *input) {
    NvDsBatchMeta *batch;
    GstBuffer *output = new_batch(1, &batch);
    NvDsFrameMeta *frame = nvds_acquire_frame_meta_from_pool(batch);
    g_assert(frame);
    nvds_add_frame_meta_to_batch(batch, frame);
    nvds_copy_gst_meta_to_frame_meta(input, batch, frame);
    return output;
}
