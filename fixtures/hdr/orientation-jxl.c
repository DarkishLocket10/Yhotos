/* Offline fixture encoder: retain a deliberately conflicting codestream orientation. */
#include <jxl/encode.h>
#include <jxl/color_encoding.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc != 6) return 1;
    unsigned w = (unsigned)atoi(argv[3]), h = (unsigned)atoi(argv[4]);
    unsigned orientation = (unsigned)atoi(argv[5]);
    if (w > 96 || h > 96 || !w || !h || orientation < 1 || orientation > 8) return 1;
    size_t size = (size_t)w * h * 3;
    unsigned char pixels[96 * 96 * 3], encoded[65536];
    FILE *input = fopen(argv[1], "rb");
    if (!input || fread(pixels, 1, size, input) != size || fclose(input)) return 1;
    JxlEncoder *encoder = JxlEncoderCreate(NULL);
    if (!encoder) return 1;
    JxlBasicInfo info;
    JxlEncoderInitBasicInfo(&info);
    info.xsize = w; info.ysize = h; info.bits_per_sample = 8;
    info.num_color_channels = 3; info.uses_original_profile = JXL_TRUE;
    info.orientation = (JxlOrientation)orientation;
    JxlColorEncoding color;
    JxlColorEncodingSetToSRGB(&color, JXL_FALSE);
    JxlEncoderFrameSettings *settings = JxlEncoderFrameSettingsCreate(encoder, NULL);
    JxlPixelFormat format = {3, JXL_TYPE_UINT8, JXL_NATIVE_ENDIAN, 0};
    if (!settings || JxlEncoderSetBasicInfo(encoder, &info) != JXL_ENC_SUCCESS ||
        JxlEncoderSetColorEncoding(encoder, &color) != JXL_ENC_SUCCESS ||
        JxlEncoderSetFrameLossless(settings, JXL_TRUE) != JXL_ENC_SUCCESS ||
        JxlEncoderFrameSettingsSetOption(settings, JXL_ENC_FRAME_SETTING_EFFORT, 3) != JXL_ENC_SUCCESS ||
        JxlEncoderAddImageFrame(settings, &format, pixels, size) != JXL_ENC_SUCCESS) return 1;
    JxlEncoderCloseInput(encoder);
    unsigned char *next = encoded;
    size_t remaining = sizeof(encoded);
    if (JxlEncoderProcessOutput(encoder, &next, &remaining) != JXL_ENC_SUCCESS) return 1;
    JxlEncoderDestroy(encoder);
    size = sizeof(encoded) - remaining;
    FILE *output = fopen(argv[2], "wb");
    return !output || fwrite(encoded, 1, size, output) != size || fclose(output);
}
