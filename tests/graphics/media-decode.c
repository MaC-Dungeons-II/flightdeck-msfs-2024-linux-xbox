// SPDX-License-Identifier: MIT
// Decode and rewind a local video through the same Media Foundation reader.
// No window, account, GPU device manager or proprietary game code is started.
#define COBJMACROS
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mfreadwrite.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int wmain(int argc, wchar_t **argv)
{
    IMFSourceReader *reader = NULL;
    IMFMediaType *type = NULL;
    UINT64 size = 0;
    unsigned width, height, wanted;
    HRESULT hr;
    int result = 1;
    if (argc != 5) return 2;
    width = wcstoul(argv[2], NULL, 10);
    height = wcstoul(argv[3], NULL, 10);
    wanted = wcstoul(argv[4], NULL, 10);
    if (!width || !height || !wanted) return 2;
    hr = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(hr)) return 1;
    hr = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    if (FAILED(hr)) { CoUninitialize(); return 1; }
    hr = MFCreateSourceReaderFromURL(argv[1], NULL, &reader);
    if (FAILED(hr)) goto finish;
    hr = IMFSourceReader_SetStreamSelection(reader, MF_SOURCE_READER_ALL_STREAMS, FALSE);
    if (FAILED(hr)) goto finish;
    hr = IMFSourceReader_SetStreamSelection(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, TRUE);
    if (FAILED(hr)) goto finish;
    hr = MFCreateMediaType(&type);
    if (FAILED(hr)) goto finish;
    hr = IMFMediaType_SetGUID(type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video);
    if (FAILED(hr)) goto finish;
    hr = IMFMediaType_SetGUID(type, &MF_MT_SUBTYPE, &MFVideoFormat_NV12);
    if (FAILED(hr)) goto finish;
    hr = IMFSourceReader_SetCurrentMediaType(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, NULL, type);
    if (FAILED(hr)) goto finish;
    IMFMediaType_Release(type);
    type = NULL;
    hr = IMFSourceReader_GetCurrentMediaType(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM, &type);
    if (FAILED(hr)) goto finish;
    hr = IMFMediaType_GetUINT64(type, &MF_MT_FRAME_SIZE, &size);
    if (FAILED(hr)) goto finish;
    for (unsigned pass = 1; pass <= 2; ++pass) {
        unsigned frames = 0;
        unsigned long long bytes = 0;
        if (pass == 2) {
            PROPVARIANT position;
            memset(&position, 0, sizeof(position));
            position.vt = VT_I8;
            hr = IMFSourceReader_SetCurrentPosition(reader, &GUID_NULL, &position);
            if (FAILED(hr)) goto finish;
        }
        for (unsigned i = 0; i < wanted + 512 && frames < wanted; ++i) {
            IMFSample *sample = NULL;
            DWORD flags = 0, stream = 0, length = 0;
            LONGLONG timestamp = 0;
            hr = IMFSourceReader_ReadSample(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
                0, &stream, &flags, &timestamp, &sample);
            if (sample) {
                HRESULT length_hr = IMFSample_GetTotalLength(sample, &length);
                IMFSample_Release(sample);
                if (SUCCEEDED(hr)) hr = length_hr;
                if (length) { ++frames; bytes += length; }
            }
            if (FAILED(hr)) break;
            if (flags & MF_SOURCE_READERF_ERROR) { hr = E_FAIL; break; }
            if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
        }
        printf("flightdeck-media-probe: pass=%u hr=%08lx width=%u height=%u frames=%u bytes=%llu\n",
            pass, (unsigned long)hr, (unsigned)(size >> 32), (unsigned)size, frames, bytes);
        fflush(stdout);
        if (FAILED(hr) || size != (((UINT64)width << 32) | height) || frames != wanted
                || bytes != (UINT64)width * height * 3 / 2 * wanted) goto finish;
    }
    // Reach EOS before shutting down the process-wide MF work queues. This
    // also exercises the rest of the file after the rewind, not just its intro.
    for (unsigned i = 0; i < 30000; ++i) {
        IMFSample *sample = NULL;
        DWORD flags = 0, stream = 0;
        LONGLONG timestamp = 0;
        hr = IMFSourceReader_ReadSample(reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
            0, &stream, &flags, &timestamp, &sample);
        if (sample) IMFSample_Release(sample);
        if (FAILED(hr) || (flags & MF_SOURCE_READERF_ERROR)) goto finish;
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM) break;
        if (i == 29999) goto finish;
    }
    fprintf(stderr, "flightdeck-media-probe: reached end of stream\n");
    result = 0;
finish:
    if (FAILED(hr)) fprintf(stderr, "flightdeck-media-probe: failed hr=%08lx\n", (unsigned long)hr);
    if (type) IMFMediaType_Release(type);
    if (reader) {
        fprintf(stderr, "flightdeck-media-probe: flushing reader\n");
        HRESULT flushed = IMFSourceReader_Flush(reader, MF_SOURCE_READER_ALL_STREAMS);
        if (FAILED(flushed)) result = 1;
        fprintf(stderr, "flightdeck-media-probe: releasing reader\n");
        IMFSourceReader_Release(reader);
    }
    fprintf(stderr, "flightdeck-media-probe: shutting down Media Foundation\n");
    if (FAILED(MFShutdown())) result = 1;
    CoUninitialize();
    fprintf(stderr, "flightdeck-media-probe: closed\n");
    return result;
}
