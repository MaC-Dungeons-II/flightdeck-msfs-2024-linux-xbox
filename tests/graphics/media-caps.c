/* SPDX-License-Identifier: MIT */
/* The CPU postprocessing boundary must negotiate system memory. This uses
 * real GStreamer caps and elements; CUDA hardware is not needed. */
#include <gst/gst.h>

int main(int argc, char **argv)
{
    GstCaps *system, *cuda, *gl, *plain;
    GstElement *pipeline;
    GstBus *bus;
    GstMessage *message;
    GError *error = NULL;
    int passed;
    gst_init(&argc, &argv);
    system = gst_caps_from_string("video/x-raw");
    plain = gst_caps_from_string("video/x-raw,format=NV12,width=64,height=64");
    cuda = gst_caps_from_string("video/x-raw(memory:CUDAMemory),format=NV12,width=64,height=64");
    gl = gst_caps_from_string("video/x-raw(memory:GLMemory),format=NV12,width=64,height=64");
    passed = gst_caps_can_intersect(system, plain)
            && !gst_caps_can_intersect(system, cuda)
            && !gst_caps_can_intersect(system, gl);
    gst_caps_unref(system); gst_caps_unref(plain);
    gst_caps_unref(cuda); gst_caps_unref(gl);
    if (!passed) return 1;
    pipeline = gst_parse_launch("videotestsrc num-buffers=8 ! video/x-raw,format=NV12,width=64,height=64 ! capsfilter caps=video/x-raw ! capssetter ! videoflip ! fakesink", &error);
    if (!pipeline || error) {
        g_printerr("CPU pipeline: %s\n", error ? error->message : "missing pipeline");
        g_clear_error(&error);
        if (pipeline) gst_object_unref(pipeline);
        return 1;
    }
    bus = gst_element_get_bus(pipeline);
    passed = gst_element_set_state(pipeline, GST_STATE_PLAYING) != GST_STATE_CHANGE_FAILURE;
    message = gst_bus_timed_pop_filtered(bus, 10 * GST_SECOND, GST_MESSAGE_EOS | GST_MESSAGE_ERROR);
    passed = passed && message && GST_MESSAGE_TYPE(message) == GST_MESSAGE_EOS;
    if (message) gst_message_unref(message);
    gst_element_set_state(pipeline, GST_STATE_NULL);
    gst_object_unref(bus); gst_object_unref(pipeline);
    g_print("SUMMARY media-caps system=%s GPU-memory=rejected CPU-chain=%s\n", "accepted", passed ? "PASS" : "FAIL");
    return passed ? 0 : 1;
}
