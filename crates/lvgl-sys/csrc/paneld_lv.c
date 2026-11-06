#include "paneld_lv.h"
#include "src/draw/lv_draw_private.h"
#include "src/image/lv_image_decoder_private.h"
#include "src/misc/cache/instance/lv_image_cache.h"
#include "src/display/lv_display_private.h"
#include "src/misc/lv_area_private.h"
#include "src/misc/lv_event_private.h"
#include "src/misc/lv_timer_private.h"

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

/* The shadow buffer (DRM, always) and display rotation (schema 13),
 * for a DIRECT-mode display whose flush callback shows the buffer
 * LVGL drew into (`buf_act`): the DRM driver and paneld's memory
 * display. LVGL draws upright into one extra logical-size buffer in
 * ordinary (cached) memory and, on the last flush of a frame, the
 * changed areas are copied (rotated, if the display is) into the
 * panel's buffer (the next of the driver's one or two buffers), which
 * the driver's own flush then shows. With two buffers each one is two
 * frames behind, so the copied areas are this frame's and the last
 * frame's.
 *
 * Why always on DRM: the driver's dumb buffers are write-combined
 * (uncached) on the Pi's vc4. Drawn into directly, every blend and
 * LVGL's double-buffer sync read them back at uncached speed (a
 * full-screen fade frame took 100+ ms on a Pi 3B+); here they are only
 * written, row by row. The driver's wait for the last page flip runs
 * right before the copy into the buffer it frees, not before LVGL
 * starts drawing.
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
    lv_display_flush_wait_cb_t flush_wait;
    bool rotated;
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
    if(lv_display_get_rotation(disp) == LV_DISPLAY_ROTATION_0) {
        uint8_t * out = dst->data + (size_t)a->y1 * dst_stride + (size_t)a->x1 * px;
        size_t len = (size_t)lv_area_get_width(a) * px;
        for(int32_t y = a->y1; y <= a->y2; y++) {
            lv_memcpy(out, src, len);
            out += dst_stride;
            src += src_stride;
        }
        return;
    }
    lv_area_t ra = *a;
    lv_display_rotate_area(disp, &ra);
    uint8_t * out = dst->data + (size_t)ra.y1 * dst_stride + (size_t)ra.x1 * px;
    lv_draw_rotate(src, out, lv_area_get_width(a), lv_area_get_height(a), src_stride, dst_stride,
                   lv_display_get_rotation(disp), cf);
}

static void paneld_rot_flush(lv_display_t * disp, const lv_area_t * area, uint8_t * px_map)
{
    paneld_rot_t * r = paneld_rot_find(disp);
    if(r == NULL) {
        lv_display_flush_ready(disp);
        return;
    }
    paneld_areas_push(&r->cur, area);
    if(!lv_display_flush_is_last(disp)) {
        r->flush(disp, area, px_map);
        lv_display_flush_ready(disp);
        return;
    }
    /* The buffer about to be written was on screen until the last page
     * flip is done (the driver's wait returns at once if it is). */
    if(r->flush_wait != NULL) r->flush_wait(disp);
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
    /* LVGL may draw the next frame into the logical buffer at once. */
    lv_display_flush_ready(disp);
}

static void paneld_rot_delete(lv_event_t * e)
{
    paneld_rot_t * r = paneld_rot_find(lv_event_get_current_target(e));
    if(r == NULL) return;
    lv_draw_buf_destroy(r->logical);
    lv_memzero(r, sizeof(*r));
}

/* The logical buffer is new or turned: what the lists hold is in the
 * old layout. LVGL redraws all of it next (the screen is invalidated);
 * the last frame's list says all too, so both panel buffers get the
 * whole of it. */
static void paneld_rot_restart(paneld_rot_t * r)
{
    lv_area_t all = {0, 0, (int32_t)r->logical->header.w - 1, (int32_t)r->logical->header.h - 1};
    r->cur.n = 0;
    r->prev.n = 1;
    r->prev.a[0] = all;
}

/* Wrap `disp` (DIRECT mode, its buffers and flush callback set) so
 * LVGL draws into a new logical buffer of `lw` x `lh`. */
static paneld_rot_t * paneld_rot_wrap(lv_display_t * disp, int32_t lw, int32_t lh)
{
    if(disp->render_mode != LV_DISPLAY_RENDER_MODE_DIRECT || disp->buf_1 == NULL || disp->flush_cb == NULL) {
        return NULL;
    }
    paneld_rot_t * r = paneld_rot_find(NULL);
    if(r == NULL) return NULL;
    int starts = 0;
    for(uint32_t i = 0; i < lv_display_get_event_count(disp); i++) {
        if(lv_display_get_event_dsc(disp, i)->filter == LV_EVENT_REFR_START) starts++;
    }
    if(starts > PANELD_ROT_LISTENERS) return NULL;
    lv_draw_buf_t * logical = lv_draw_buf_create(lw, lh, lv_display_get_color_format(disp), LV_STRIDE_AUTO);
    if(logical == NULL) return NULL;
    lv_memzero(r, sizeof(*r));
    r->disp = disp;
    r->flush = disp->flush_cb;
    r->flush_wait = disp->flush_wait_cb;
    /* Copies: the display's own draw buffer structs are reused by
     * lv_display_set_buffers, so do not keep pointers to them. */
    r->scan[0] = *disp->buf_1;
    r->scan_cnt = 1;
    if(disp->buf_2 != NULL) {
        r->scan[1] = *disp->buf_2;
        r->scan_cnt = 2;
    }
    r->logical = logical;
    paneld_rot_restart(r);
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
    lv_display_set_draw_buffers(disp, logical, NULL);
    disp->flush_cb = paneld_rot_flush;
    /* Waited for in paneld_rot_flush, before the copy. */
    disp->flush_wait_cb = NULL;
    lv_display_add_event_cb(disp, paneld_rot_delete, LV_EVENT_DELETE, NULL);
    lv_obj_invalidate(lv_screen_active());
    return r;
}

bool lv_paneld_drm_shadow(lv_display_t * disp)
{
    if(disp == NULL || paneld_rot_find(disp) != NULL) return false;
    return paneld_rot_wrap(disp, disp->hor_res, disp->ver_res) != NULL;
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
    if(disp == NULL) return false;
    paneld_rot_t * r = paneld_rot_find(disp);
    if(r != NULL && r->rotated) return false;
    if(rot == LV_DISPLAY_ROTATION_0) return true;
    /* PARTIAL mode (fbdev): the driver rotates each area itself. */
    if(r == NULL && disp->render_mode == LV_DISPLAY_RENDER_MODE_PARTIAL) {
        lv_display_set_rotation(disp, rot);
        return true;
    }
    int32_t w = disp->hor_res;
    int32_t h = disp->ver_res;
    bool swap = rot == LV_DISPLAY_ROTATION_90 || rot == LV_DISPLAY_ROTATION_270;
    int32_t lw = swap ? h : w, lh = swap ? w : h;
    if(r == NULL) {
        r = paneld_rot_wrap(disp, lw, lh);
        if(r == NULL) return false;
    }
    else if(swap) {
        /* A shadowed display: its logical buffer turns. */
        lv_draw_buf_t * logical = lv_draw_buf_create(lw, lh, lv_display_get_color_format(disp), LV_STRIDE_AUTO);
        if(logical == NULL) return false;
        lv_display_set_draw_buffers(disp, logical, NULL);
        lv_draw_buf_destroy(r->logical);
        r->logical = logical;
    }
    r->rotated = true;
    lv_display_set_rotation(disp, rot);
    paneld_rot_restart(r);
    lv_obj_invalidate(lv_screen_active());
    return true;
}

uint32_t lv_paneld_timers_idle_ms(lv_timer_t * const * skip, uint32_t n_skip, uint32_t cap)
{
    uint32_t idle = cap;
    lv_timer_t * t = lv_timer_get_next(NULL);
    for(; t != NULL; t = lv_timer_get_next(t)) {
        if(t->paused) continue;
        bool skipped = false;
        for(uint32_t i = 0; i < n_skip; i++) {
            if(skip[i] == t) skipped = true;
        }
        if(skipped) continue;
        uint32_t elapsed = lv_tick_elaps(t->last_run);
        uint32_t left = t->period > elapsed ? t->period - elapsed : 0;
        if(left < idle) idle = left;
    }
    return idle;
}

/* PANELD_PERF (docs/deploy.md, Diagnostics): one stderr line per drawn
 * frame, times in us: gap (ms since the last frame start), pre
 * (layout and LVGL's buffer sync), render (drawing and the flush,
 * including the copy and the wait for the last page flip), post, wait
 * (LVGL's flush waits), total, px (area redrawn). */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static uint64_t perf_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000u + (uint64_t)ts.tv_nsec / 1000u;
}

static struct {
    uint64_t prev_start, start, render_start, render_ready, wait_start, wait_sum;
    uint64_t px;
} perf;

static void perf_cb(lv_event_t * e)
{
    lv_event_code_t c = lv_event_get_code(e);
    uint64_t now = perf_us();
    switch(c) {
        case LV_EVENT_REFR_START:
            perf.prev_start = perf.start;
            perf.start = now;
            perf.render_start = perf.render_ready = 0;
            perf.wait_sum = 0;
            perf.px = 0;
            break;
        case LV_EVENT_FLUSH_WAIT_START:
            perf.wait_start = now;
            break;
        case LV_EVENT_FLUSH_WAIT_FINISH:
            perf.wait_sum += now - perf.wait_start;
            break;
        case LV_EVENT_RENDER_START: {
            perf.render_start = now;
            lv_display_t * d = lv_event_get_target(e);
            for(uint32_t i = 0; i < d->inv_p; i++)
                if(!d->inv_area_joined[i]) perf.px += lv_area_get_size(&d->inv_areas[i]);
            break;
        }
        case LV_EVENT_RENDER_READY:
            perf.render_ready = now;
            break;
        case LV_EVENT_REFR_READY:
            if(perf.render_start) {
                fprintf(stderr, "perf t=%llu gap=%llu pre=%llu render=%llu post=%llu wait=%llu total=%llu px=%llu\n",
                        (unsigned long long)perf.start,
                        (unsigned long long)(perf.prev_start ? (perf.start - perf.prev_start) / 1000 : 0),
                        (unsigned long long)(perf.render_start - perf.start),
                        (unsigned long long)(perf.render_ready - perf.render_start),
                        (unsigned long long)(now - perf.render_ready),
                        (unsigned long long)perf.wait_sum,
                        (unsigned long long)(now - perf.start),
                        (unsigned long long)perf.px);
            }
            break;
        default:
            break;
    }
}

uint64_t lv_paneld_mono_us(void)
{
    return perf_us();
}

void lv_paneld_perf_attach(lv_display_t * disp)
{
    if(getenv("PANELD_PERF") == NULL) return;
    lv_display_add_event_cb(disp, perf_cb, LV_EVENT_ALL, NULL);
}

/* ---- In-place page updates (ADR 0024) ---------------------------- */

#include "src/core/lv_obj_private.h"
#include "src/core/lv_obj_style_private.h"
#include "src/misc/lv_style_private.h"

typedef struct {
    uint32_t selector;
    uint32_t n;
    lv_style_prop_t * props;
    lv_style_value_t * values;
} paneld_saved_style_t;

typedef struct {
    uint32_t n;
    paneld_saved_style_t * styles;
} paneld_saved_t;

/* The props and values of a local (never const) style. */
static void paneld_style_parts(const lv_style_t * s, lv_style_prop_t ** props, lv_style_value_t ** values)
{
    *values = (lv_style_value_t *)s->values_and_props;
    *props = (lv_style_prop_t *)((uint8_t *)s->values_and_props + s->prop_cnt * sizeof(lv_style_value_t));
}

void * lv_paneld_local_styles_save(const lv_obj_t * obj)
{
    uint32_t n = 0;
    for(uint32_t i = 0; i < obj->style_cnt; i++) {
        const lv_obj_style_t * os = &obj->styles[i];
        if(os->is_local && os->style && os->style->prop_cnt > 0 && os->style->prop_cnt != 255) n++;
    }
    if(n == 0) return NULL;
    paneld_saved_t * saved = lv_malloc_zeroed(sizeof(paneld_saved_t));
    if(saved == NULL) return NULL;
    saved->styles = lv_malloc_zeroed(n * sizeof(paneld_saved_style_t));
    if(saved->styles == NULL) {
        lv_free(saved);
        return NULL;
    }
    for(uint32_t i = 0; i < obj->style_cnt; i++) {
        const lv_obj_style_t * os = &obj->styles[i];
        if(!(os->is_local && os->style && os->style->prop_cnt > 0 && os->style->prop_cnt != 255)) continue;
        paneld_saved_style_t * d = &saved->styles[saved->n];
        uint32_t cnt = os->style->prop_cnt;
        lv_style_prop_t * props;
        lv_style_value_t * values;
        paneld_style_parts(os->style, &props, &values);
        d->props = lv_malloc(cnt * sizeof(lv_style_prop_t));
        d->values = lv_malloc(cnt * sizeof(lv_style_value_t));
        if(d->props == NULL || d->values == NULL) {
            lv_free(d->props);
            lv_free(d->values);
            continue;
        }
        lv_memcpy(d->props, props, cnt * sizeof(lv_style_prop_t));
        lv_memcpy(d->values, values, cnt * sizeof(lv_style_value_t));
        d->selector = os->selector;
        d->n = cnt;
        saved->n++;
    }
    return saved;
}

void lv_paneld_local_styles_restore(lv_obj_t * obj, const void * saved_p)
{
    const paneld_saved_t * saved = saved_p;
    for(uint32_t i = 0; i < obj->style_cnt; i++) {
        lv_obj_style_t * os = &obj->styles[i];
        if(os->is_local && os->style && os->style->prop_cnt != 255) lv_style_reset((lv_style_t *)os->style);
    }
    if(saved == NULL) return;
    for(uint32_t i = 0; i < saved->n; i++) {
        const paneld_saved_style_t * d = &saved->styles[i];
        for(uint32_t k = 0; k < d->n; k++) {
            lv_obj_set_local_style_prop(obj, d->props[k], d->values[k], d->selector);
        }
    }
}

void lv_paneld_local_styles_free(void * saved_p)
{
    paneld_saved_t * saved = saved_p;
    if(saved == NULL) return;
    for(uint32_t i = 0; i < saved->n; i++) {
        lv_free(saved->styles[i].props);
        lv_free(saved->styles[i].values);
    }
    lv_free(saved->styles);
    lv_free(saved);
}

bool lv_paneld_local_style_at(const lv_obj_t * obj, uint32_t i, uint32_t * selector, lv_style_prop_t * prop,
                              lv_style_value_t * value)
{
    for(uint32_t s = 0; s < obj->style_cnt; s++) {
        const lv_obj_style_t * os = &obj->styles[s];
        if(!(os->is_local && os->style && os->style->prop_cnt != 255)) continue;
        uint32_t cnt = os->style->prop_cnt;
        if(i < cnt) {
            lv_style_prop_t * props;
            lv_style_value_t * values;
            paneld_style_parts(os->style, &props, &values);
            *selector = os->selector;
            *prop = props[i];
            *value = values[i];
            return true;
        }
        i -= cnt;
    }
    return false;
}

uint32_t lv_paneld_obj_flags(const lv_obj_t * obj)
{
    uint32_t f = 0;
    if(lv_obj_is_hidden(obj)) f |= LV_OBJ_FLAG_HIDDEN;
    if(lv_obj_is_clickable(obj)) f |= LV_OBJ_FLAG_CLICKABLE;
    if(lv_obj_is_checkable(obj)) f |= LV_OBJ_FLAG_CHECKABLE;
    if(lv_obj_is_scrollable(obj)) f |= LV_OBJ_FLAG_SCROLLABLE;
    return f;
}

void lv_paneld_set_state_raw(lv_obj_t * obj, lv_state_t state)
{
    obj->state = state;
}

void lv_paneld_state_settled(lv_obj_t * obj, lv_state_t prev)
{
    lv_obj_invalidate(obj);
    lv_obj_refresh_style(obj, LV_PART_ANY, LV_STYLE_PROP_ANY);
    lv_obj_send_event(obj, LV_EVENT_STATE_CHANGED, &prev);
}

/* Two values of `prop` alike: a color by its three bytes, a pointer
 * whole, a number by its value (the rest of the union is padding). */
static bool paneld_value_eq(lv_style_prop_t prop, lv_style_value_t a, lv_style_value_t b)
{
    switch(prop) {
        case LV_STYLE_BG_COLOR:
        case LV_STYLE_BG_GRAD_COLOR:
        case LV_STYLE_BG_IMAGE_RECOLOR:
        case LV_STYLE_BORDER_COLOR:
        case LV_STYLE_OUTLINE_COLOR:
        case LV_STYLE_SHADOW_COLOR:
        case LV_STYLE_IMAGE_RECOLOR:
        case LV_STYLE_LINE_COLOR:
        case LV_STYLE_ARC_COLOR:
        case LV_STYLE_TEXT_COLOR:
        case LV_STYLE_TEXT_OUTLINE_STROKE_COLOR:
        case LV_STYLE_RECOLOR:
        case LV_STYLE_DROP_SHADOW_COLOR:
            return a.color.red == b.color.red && a.color.green == b.color.green && a.color.blue == b.color.blue;
        case LV_STYLE_TEXT_FONT:
        case LV_STYLE_BG_GRAD:
        case LV_STYLE_BG_IMAGE_SRC:
        case LV_STYLE_ARC_IMAGE_SRC:
        case LV_STYLE_TRANSITION:
        case LV_STYLE_BITMAP_MASK_SRC:
        case LV_STYLE_COLOR_FILTER_DSC:
        case LV_STYLE_ANIM:
        case LV_STYLE_GRID_COLUMN_DSC_ARRAY:
        case LV_STYLE_GRID_ROW_DSC_ARRAY:
            return a.ptr == b.ptr;
        default:
            return a.num == b.num;
    }
}

static bool paneld_has_prop(const paneld_saved_t * s, uint32_t selector, lv_style_prop_t prop, lv_style_value_t v)
{
    for(uint32_t i = 0; s && i < s->n; i++) {
        const paneld_saved_style_t * d = &s->styles[i];
        if(d->selector != selector) continue;
        for(uint32_t k = 0; k < d->n; k++) {
            if(d->props[k] == prop) return paneld_value_eq(prop, d->values[k], v);
        }
    }
    return false;
}

static uint32_t paneld_count(const paneld_saved_t * s)
{
    uint32_t n = 0;
    for(uint32_t i = 0; s && i < s->n; i++) n += s->styles[i].n;
    return n;
}

bool lv_paneld_local_styles_equal(const lv_obj_t * obj, const void * saved_p)
{
    const paneld_saved_t * saved = saved_p;
    uint32_t n = 0;
    for(uint32_t i = 0; i < obj->style_cnt; i++) {
        const lv_obj_style_t * os = &obj->styles[i];
        if(!(os->is_local && os->style && os->style->prop_cnt != 255)) continue;
        lv_style_prop_t * props;
        lv_style_value_t * values;
        paneld_style_parts(os->style, &props, &values);
        for(uint32_t k = 0; k < os->style->prop_cnt; k++) {
            if(!paneld_has_prop(saved, os->selector, props[k], values[k])) return false;
            n++;
        }
    }
    return n == paneld_count(saved);
}

lv_event_code_t lv_paneld_event_dsc_code(const lv_event_dsc_t * dsc)
{
    return (lv_event_code_t)(dsc->filter & ~((uint32_t)LV_EVENT_PREPROCESS | (uint32_t)LV_EVENT_MARKED_DELETING));
}

#include "src/core/lv_obj_class_private.h"

const char * lv_paneld_class_name(const lv_obj_t * obj)
{
    const lv_obj_class_t * c = lv_obj_get_class(obj);
    return (c && c->name) ? c->name : "";
}

#include "src/widgets/arc/lv_arc_private.h"

void lv_paneld_arc_unset(lv_obj_t * obj)
{
    lv_arc_t * arc = (lv_arc_t *)obj;
    if(arc->value == INT16_MIN) return;
    lv_obj_invalidate(obj);
    arc->value = INT16_MIN;
    arc->indic_angle_start = 135;
    arc->indic_angle_end = 270;
    lv_obj_invalidate(obj);
}

#include "src/misc/lv_anim_private.h"
#include "src/core/lv_global.h"

/* Never run: marks the animations lv_paneld_anim_delete_custom takes. */
static void paneld_doomed(void * var, int32_t v)
{
    LV_UNUSED(var);
    LV_UNUSED(v);
}

bool lv_paneld_anim_delete_custom(void * var, lv_anim_custom_exec_cb_t cb)
{
    bool any = false;
    /* lv_anim_delete matches on `exec_cb` only: mark ours, then take
     * them by that mark. */
    for(lv_anim_t * x = lv_ll_get_head(&LV_GLOBAL_DEFAULT()->anim_state.anim_ll); x != NULL;
        x = lv_ll_get_next(&LV_GLOBAL_DEFAULT()->anim_state.anim_ll, x)) {
        if(x->var == var && x->custom_exec_cb == cb) {
            x->exec_cb = paneld_doomed;
            any = true;
        }
    }
    if(any) lv_anim_delete(var, paneld_doomed);
    return any;
}

uint32_t lv_paneld_anim_count(void * var, lv_anim_custom_exec_cb_t not_cb)
{
    uint32_t n = 0;
    for(lv_anim_t * x = lv_ll_get_head(&LV_GLOBAL_DEFAULT()->anim_state.anim_ll); x != NULL;
        x = lv_ll_get_next(&LV_GLOBAL_DEFAULT()->anim_state.anim_ll, x)) {
        if(x->var == var && x->custom_exec_cb != not_cb && x->repeat_cnt == LV_ANIM_REPEAT_INFINITE) n++;
    }
    return n;
}

void lv_paneld_size_again(lv_obj_t * obj)
{
    obj->w_layout = 0;
    obj->h_layout = 0;
    lv_obj_mark_layout_as_dirty(obj);
}

#include "src/indev/lv_indev_private.h"

lv_obj_t * lv_paneld_indev_pressed_obj(const lv_indev_t * indev)
{
    if(indev == NULL || indev->state != LV_INDEV_STATE_PRESSED) return NULL;
    return indev->pointer.act_obj;
}
