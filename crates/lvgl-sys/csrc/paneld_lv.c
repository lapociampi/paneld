#include "paneld_lv.h"
#include "src/draw/lv_draw_private.h"

const lv_area_t * lv_paneld_layer_clip_area(const lv_layer_t * layer)
{
    return &layer->_clip_area;
}
