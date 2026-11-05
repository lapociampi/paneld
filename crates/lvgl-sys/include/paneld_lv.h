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

#endif
