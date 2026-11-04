/* Small accessors paneld needs that LVGL keeps in private headers. */
#ifndef PANELD_LV_H
#define PANELD_LV_H

#include "lvgl.h"

/* The area a layer is drawing now (what a draw callback may skip
 * outside of). */
const lv_area_t * lv_paneld_layer_clip_area(const lv_layer_t * layer);

#endif
