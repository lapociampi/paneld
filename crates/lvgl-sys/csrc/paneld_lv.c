#include "paneld_lv.h"
#include "src/draw/lv_draw_private.h"
#include "src/image/lv_image_decoder_private.h"
#include "src/misc/cache/instance/lv_image_cache.h"
#include "src/display/lv_display_private.h"
#include "src/misc/lv_area_private.h"
#include "src/misc/lv_event_private.h"

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

/* The source box [sx0, sx0 + sw) x [sy0, sy0 + sh) of `in` sampled
 * into a new dw x dh ARGB8888 buffer: each target pixel the average of
 * its source box (alpha weighted), the nearest pixel when scaling up. */
static lv_draw_buf_t * paneld_sample(const lv_draw_buf_t * in, int32_t sx0, int32_t sy0, int32_t sw, int32_t sh,
                                     int32_t dw, int32_t dh)
{
    lv_draw_buf_t * out = lv_draw_buf_create(dw, dh, LV_COLOR_FORMAT_ARGB8888, LV_STRIDE_AUTO);
    if(out == NULL) return NULL;
    for(int32_t y = 0; y < dh; y++) {
        int32_t y0 = sy0 + (int32_t)((int64_t)y * sh / dh);
        int32_t y1 = LV_MAX(y0 + 1, sy0 + (int32_t)((int64_t)(y + 1) * sh / dh));
        uint8_t * row = out->data + (size_t)y * out->header.stride;
        for(int32_t x = 0; x < dw; x++) {
            int32_t x0 = sx0 + (int32_t)((int64_t)x * sw / dw);
            int32_t x1 = LV_MAX(x0 + 1, sx0 + (int32_t)((int64_t)(x + 1) * sw / dw));
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
    return out;
}

void lv_paneld_image_cache_drop(const void * src)
{
    lv_image_cache_drop(src);
}

lv_draw_buf_t * lv_paneld_load_fitted(const char * src, int32_t w, int32_t h, int32_t fit)
{
    if(w < 1 || h < 1) return NULL;
    lv_image_decoder_args_t args;
    lv_memzero(&args, sizeof(args));
    args.no_cache = true;
    lv_image_decoder_dsc_t dsc;
    if(lv_image_decoder_open(&dsc, src, &args) != LV_RESULT_OK) return NULL;
    const lv_draw_buf_t * in = dsc.decoded;
    lv_draw_buf_t * out = NULL;
    if(in != NULL && in->header.cf == LV_COLOR_FORMAT_ARGB8888 && in->header.w > 0 && in->header.h > 0) {
        int32_t sw = in->header.w, sh = in->header.h;
        switch(fit) {
            case 0: { /* cover: scale to cover w x h, crop the middle */
                    int32_t cw = sw, ch = sh;
                    if((int64_t)sw * h > (int64_t)sh * w) cw = LV_MAX(1, (int32_t)((int64_t)sh * w / h));
                    else ch = LV_MAX(1, (int32_t)((int64_t)sw * h / w));
                    out = paneld_sample(in, (sw - cw) / 2, (sh - ch) / 2, cw, ch, w, h);
                    break;
                }
            case 1: { /* contain: scale to fit inside w x h */
                    int32_t dw = w, dh = h;
                    if((int64_t)sw * h > (int64_t)sh * w) dh = LV_MAX(1, (int32_t)((int64_t)sh * w / sw));
                    else dw = LV_MAX(1, (int32_t)((int64_t)sw * h / sh));
                    out = paneld_sample(in, 0, 0, sw, sh, dw, dh);
                    break;
                }
            case 2: /* fill */
                out = paneld_sample(in, 0, 0, sw, sh, w, h);
                break;
            default: /* tile: its own size */
                out = paneld_sample(in, 0, 0, sw, sh, sw, sh);
                break;
        }
    }
    lv_image_decoder_close(&dsc);
    return out;
}

/* Display rotation (schema 13) for a DIRECT-mode display whose flush
 * callback shows the buffer LVGL drew into (`buf_act`): the DRM
 * driver and paneld's memory display. LVGL's software renderer cannot
 * draw rotated into such a buffer, so LVGL draws upright into one
 * extra logical-size buffer and, on the last flush of a frame, the
 * changed areas are rotated into the panel's buffer (the next of the
 * driver's one or two buffers), which the driver's own flush then
 * shows. With two buffers each one is two frames behind, so the
 * rotated areas are this frame's and the last frame's.
 *
 * The display's REFR_START listeners (the DRM driver's: it picks the
 * buffer to show from `buf_act`, and starts the CPU access on a GBM
 * buffer) are taken off the display and run once per frame, for the
 * panel buffer about to be written, right before the rotation. */
#define PANELD_ROT_AREAS 8
#define PANELD_ROT_LISTENERS 4

typedef struct {
    lv_area_t a[PANELD_ROT_AREAS];
    int n;
} paneld_areas_t;

typedef struct {
    lv_display_t * disp;
    lv_display_flush_cb_t flush;
    lv_draw_buf_t scan[2];
    int scan_cnt;
    int next;
    lv_draw_buf_t * logical;
    paneld_areas_t cur, prev;
    lv_event_cb_t start_cb[PANELD_ROT_LISTENERS];
    void * start_ud[PANELD_ROT_LISTENERS];
    int start_cnt;
} paneld_rot_t;

#define PANELD_ROT_MAX 4
static paneld_rot_t paneld_rot[PANELD_ROT_MAX];

static paneld_rot_t * paneld_rot_find(lv_display_t * disp)
{
    for(int i = 0; i < PANELD_ROT_MAX; i++) {
        if(paneld_rot[i].disp == disp) return &paneld_rot[i];
    }
    return NULL;
}

/* Add `a` to the list; a full list joins it into the entry it grows
 * least. An area inside one already listed is dropped. */
void lv_paneld_areas_add(lv_area_t * list, int32_t * n, int32_t max, const lv_area_t * a)
{
    int best = -1;
    int64_t best_growth = INT64_MAX;
    for(int i = 0; i < *n; i++) {
        lv_area_t j;
        lv_area_join(&j, &list[i], a);
        int64_t growth = (int64_t)lv_area_get_size(&j) - lv_area_get_size(&list[i]);
        if(lv_area_is_in(a, &list[i], 0)) return;
        if(growth < best_growth) {
            best_growth = growth;
            best = i;
        }
    }
    if(*n < max) list[(*n)++] = *a;
    else lv_area_join(&list[best], &list[best], a);
}

static void paneld_areas_push(paneld_areas_t * l, const lv_area_t * a)
{
    int32_t n = l->n;
    lv_paneld_areas_add(l->a, &n, PANELD_ROT_AREAS, a);
    l->n = n;
}

static void paneld_rot_area(paneld_rot_t * r, lv_draw_buf_t * dst, const lv_area_t * a)
{
    lv_display_t * disp = r->disp;
    lv_color_format_t cf = lv_display_get_color_format(disp);
    uint32_t px = lv_color_format_get_size(cf);
    uint32_t src_stride = r->logical->header.stride;
    uint32_t dst_stride = dst->header.stride;
    const uint8_t * src = r->logical->data + (size_t)a->y1 * src_stride + (size_t)a->x1 * px;
    lv_area_t ra = *a;
    lv_display_rotate_area(disp, &ra);
    uint8_t * out = dst->data + (size_t)ra.y1 * dst_stride + (size_t)ra.x1 * px;
    lv_draw_rotate(src, out, lv_area_get_width(a), lv_area_get_height(a), src_stride, dst_stride,
                   lv_display_get_rotation(disp), cf);
}

static void paneld_rot_flush(lv_display_t * disp, const lv_area_t * area, uint8_t * px_map)
{
    paneld_rot_t * r = paneld_rot_find(disp);
    if(r == NULL) return;
    paneld_areas_push(&r->cur, area);
    if(!lv_display_flush_is_last(disp)) {
        r->flush(disp, area, px_map);
        return;
    }
    lv_draw_buf_t * dst = &r->scan[r->next];
    r->next = (r->next + 1) % r->scan_cnt;

    /* The driver's frame start, for the buffer about to be written. */
    lv_draw_buf_t * own = disp->buf_act;
    disp->buf_act = dst;
    for(int i = 0; i < r->start_cnt; i++) {
        lv_event_t e;
        lv_memzero(&e, sizeof(e));
        e.current_target = disp;
        e.original_target = disp;
        e.code = LV_EVENT_REFR_START;
        e.user_data = r->start_ud[i];
        r->start_cb[i](&e);
    }

    for(int i = 0; i < r->cur.n; i++) paneld_rot_area(r, dst, &r->cur.a[i]);
    if(r->scan_cnt > 1) {
        for(int i = 0; i < r->prev.n; i++) paneld_rot_area(r, dst, &r->prev.a[i]);
    }
    r->prev = r->cur;
    r->cur.n = 0;

    r->flush(disp, area, dst->data);
    disp->buf_act = own;
}

static void paneld_rot_delete(lv_event_t * e)
{
    paneld_rot_t * r = paneld_rot_find(lv_event_get_current_target(e));
    if(r == NULL) return;
    lv_draw_buf_destroy(r->logical);
    lv_memzero(r, sizeof(*r));
}

bool lv_paneld_set_rotation(lv_display_t * disp, int32_t degrees)
{
    lv_display_rotation_t rot;
    switch(degrees) {
        case 0: rot = LV_DISPLAY_ROTATION_0; break;
        case 90: rot = LV_DISPLAY_ROTATION_90; break;
        case 180: rot = LV_DISPLAY_ROTATION_180; break;
        case 270: rot = LV_DISPLAY_ROTATION_270; break;
        default: return false;
    }
    if(disp == NULL || paneld_rot_find(disp) != NULL) return false;
    if(rot == LV_DISPLAY_ROTATION_0) return true;
    /* PARTIAL mode (fbdev): the driver rotates each area itself. */
    if(disp->render_mode == LV_DISPLAY_RENDER_MODE_PARTIAL) {
        lv_display_set_rotation(disp, rot);
        return true;
    }
    if(disp->render_mode != LV_DISPLAY_RENDER_MODE_DIRECT || disp->buf_1 == NULL || disp->flush_cb == NULL) {
        return false;
    }
    paneld_rot_t * r = paneld_rot_find(NULL);
    if(r == NULL) return false;
    int starts = 0;
    for(uint32_t i = 0; i < lv_display_get_event_count(disp); i++) {
        if(lv_display_get_event_dsc(disp, i)->filter == LV_EVENT_REFR_START) starts++;
    }
    if(starts > PANELD_ROT_LISTENERS) return false;
    int32_t w = lv_display_get_horizontal_resolution(disp);
    int32_t h = lv_display_get_vertical_resolution(disp);
    bool swap = rot == LV_DISPLAY_ROTATION_90 || rot == LV_DISPLAY_ROTATION_270;
    lv_draw_buf_t * logical = lv_draw_buf_create(swap ? h : w, swap ? w : h, lv_display_get_color_format(disp),
                                                 LV_STRIDE_AUTO);
    if(logical == NULL) return false;
    lv_memzero(r, sizeof(*r));
    r->disp = disp;
    r->flush = disp->flush_cb;
    /* Copies: the display's own draw buffer structs are reused by
     * lv_display_set_buffers, so do not keep pointers to them. */
    r->scan[0] = *disp->buf_1;
    r->scan_cnt = 1;
    if(disp->buf_2 != NULL) {
        r->scan[1] = *disp->buf_2;
        r->scan_cnt = 2;
    }
    r->logical = logical;
    for(int32_t i = (int32_t)lv_display_get_event_count(disp) - 1; i >= 0; i--) {
        lv_event_dsc_t * d = lv_display_get_event_dsc(disp, i);
        if(d->filter != LV_EVENT_REFR_START) continue;
        /* Kept in the order they were added. */
        for(int k = r->start_cnt; k > 0; k--) {
            r->start_cb[k] = r->start_cb[k - 1];
            r->start_ud[k] = r->start_ud[k - 1];
        }
        r->start_cb[0] = d->cb;
        r->start_ud[0] = d->user_data;
        r->start_cnt++;
        lv_display_remove_event(disp, i);
    }
    lv_display_set_rotation(disp, rot);
    lv_display_set_draw_buffers(disp, logical, NULL);
    disp->flush_cb = paneld_rot_flush;
    lv_display_add_event_cb(disp, paneld_rot_delete, LV_EVENT_DELETE, NULL);
    lv_obj_invalidate(lv_screen_active());
    return true;
}
