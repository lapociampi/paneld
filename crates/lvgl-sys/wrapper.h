/* bindgen entry point: pulls in the LVGL 9 public API with paneld's
 * `lv_conf.h`. The include path is set by build.rs so this is the
 * only header bindgen reads directly.
 */
#include "lvgl.h"
#include "paneld_lv.h"
#include "src/drivers/sdl/lv_sdl_window.h"
#include "src/drivers/sdl/lv_sdl_mouse.h"
#include "src/drivers/sdl/lv_sdl_keyboard.h"
