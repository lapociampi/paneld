/* Small accessors paneld needs that LVGL keeps in private headers. */
#ifndef PANELD_LV_H
#define PANELD_LV_H

#include "lvgl.h"

/* The area a layer is drawing now (what a draw callback may skip
 * outside of). */
const lv_area_t * lv_paneld_layer_clip_area(const lv_layer_t * layer);

/* Decode the image `src` (e.g. "A:/path/x.png") once, without
 * LVGL's image cache, and return an ARGB8888 copy that fits in
 * `max_side` x `max_side` (box-filtered down; a smaller image is kept
 * at its size), or NULL. Free with lv_draw_buf_destroy. */
lv_draw_buf_t * lv_paneld_load_scaled(const char * src, int32_t max_side);

/* Decode the image `src` once, without LVGL's image cache, and return
 * an ARGB8888 copy fitted to a w x h box: `fit` 0 cover (scaled to
 * cover, the middle cropped to exactly w x h), 1 contain (scaled to
 * fit inside), 2 fill (stretched to w x h), 3 tile (its own size). Box
 * filtered down, nearest pixel up. NULL on failure. Free with
 * lv_draw_buf_destroy. */
lv_draw_buf_t * lv_paneld_load_fitted(const char * src, int32_t w, int32_t h, int32_t fit);

/* Drop the image source `src` (a buffer about to be freed) from
 * LVGL's image and header caches. */
void lv_paneld_image_cache_drop(const void * src);

/* Rotate `disp` by `degrees` (0, 90, 180, 270), once, right after it
 * is opened. A PARTIAL-mode display (fbdev) rotates in its driver; a
 * DIRECT-mode one (DRM, the memory display) gets an upright logical
 * buffer that is rotated into the driver's buffer on each flush.
 * Pointer input on the display is rotated by LVGL. false: bad angle,
 * already rotated, or a display this cannot rotate. */
bool lv_paneld_set_rotation(lv_display_t * disp, int32_t degrees);

/* Add area `a` to `list` (`*n` used of `max`): dropped if inside one
 * already listed; when the list is full, joined into the entry it
 * grows least. For the rotation's changed areas; exposed for tests. */
void lv_paneld_areas_add(lv_area_t * list, int32_t * n, int32_t max, const lv_area_t * a);

#endif
