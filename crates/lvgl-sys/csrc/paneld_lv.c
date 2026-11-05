#include "paneld_lv.h"
#include "src/draw/lv_draw_private.h"
#include "src/image/lv_image_decoder_private.h"

const lv_area_t * lv_paneld_layer_clip_area(const lv_layer_t * layer)
{
    return &layer->_clip_area;
}

lv_draw_buf_t * lv_paneld_load_scaled(const char * src, int32_t max_side)
{
    if(max_side < 1) return NULL;
    lv_image_decoder_args_t args;
    lv_memzero(&args, sizeof(args));
    args.no_cache = true;
    lv_image_decoder_dsc_t dsc;
    if(lv_image_decoder_open(&dsc, src, &args) != LV_RESULT_OK) return NULL;
    const lv_draw_buf_t * in = dsc.decoded;
    lv_draw_buf_t * out = NULL;
    if(in != NULL && in->header.cf == LV_COLOR_FORMAT_ARGB8888 && in->header.w > 0 && in->header.h > 0) {
        int32_t sw = in->header.w, sh = in->header.h;
        int32_t dw = sw, dh = sh;
        if(sw > max_side || sh > max_side) {
            if(sw >= sh) {
                dw = max_side;
                dh = LV_MAX(1, (int32_t)((int64_t)sh * max_side / sw));
            }
            else {
                dh = max_side;
                dw = LV_MAX(1, (int32_t)((int64_t)sw * max_side / sh));
            }
        }
        out = lv_draw_buf_create(dw, dh, LV_COLOR_FORMAT_ARGB8888, LV_STRIDE_AUTO);
        if(out != NULL) {
            /* Each target pixel: the average of its source box, the
             * color weighted by alpha so edges do not darken. */
            for(int32_t y = 0; y < dh; y++) {
                int32_t y0 = (int32_t)((int64_t)y * sh / dh);
                int32_t y1 = LV_MAX(y0 + 1, (int32_t)((int64_t)(y + 1) * sh / dh));
                uint8_t * row = out->data + (size_t)y * out->header.stride;
                for(int32_t x = 0; x < dw; x++) {
                    int32_t x0 = (int32_t)((int64_t)x * sw / dw);
                    int32_t x1 = LV_MAX(x0 + 1, (int32_t)((int64_t)(x + 1) * sw / dw));
                    uint64_t b = 0, g = 0, r = 0, a = 0, n = 0;
                    for(int32_t sy = y0; sy < y1; sy++) {
                        const uint8_t * p = in->data + (size_t)sy * in->header.stride + (size_t)x0 * 4;
                        for(int32_t sx = x0; sx < x1; sx++, p += 4) {
                            b += (uint64_t)p[0] * p[3];
                            g += (uint64_t)p[1] * p[3];
                            r += (uint64_t)p[2] * p[3];
                            a += p[3];
                            n++;
                        }
                    }
                    uint8_t * q = row + (size_t)x * 4;
                    q[0] = a ? (uint8_t)(b / a) : 0;
                    q[1] = a ? (uint8_t)(g / a) : 0;
                    q[2] = a ? (uint8_t)(r / a) : 0;
                    q[3] = (uint8_t)(a / n);
                }
            }
        }
    }
    lv_image_decoder_close(&dsc);
    return out;
}
