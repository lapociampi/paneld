/* paneld's LVGL 9.6 build configuration (ADR 0011).
 *
 * This file only overrides the options paneld needs; everything else
 * falls through to the defaults in `vendor/lvgl/include/lvgl/config/lv_conf_internal.h`.
 * Enabling a feature here (a widget, a decoder, a driver) is a
 * deliberate change: it costs binary size.
 */

#ifndef LV_CONF_H
#define LV_CONF_H

/* clang-format off */

/* ---------------------------------------------------------------------
 * Memory and standard library
 * --------------------------------------------------------------------- */

/* Use the C library's malloc; the daemon lives in a Linux process. */
#define LV_USE_STDLIB_MALLOC    LV_STDLIB_CLIB
#define LV_USE_STDLIB_STRING    LV_STDLIB_CLIB
#define LV_USE_STDLIB_SPRINTF   LV_STDLIB_CLIB

/* OS: pthread, on a PC (SDL window) as on a device. With LV_OS_SDL2
 * the draw threads are joined through SDL after lv_deinit has already
 * quit SDL (deleting the window), and LVGL logs an error at exit. */
#define LV_USE_OS               LV_OS_PTHREAD

/* ---------------------------------------------------------------------
 * Rendering
 * --------------------------------------------------------------------- */

/* 32-bit XRGB (XRGB8888) everywhere; the Pi can also run RGB565,
 * which we can switch to in a later build target. */
#define LV_COLOR_FORMAT_DEFAULT LV_COLOR_FORMAT_XRGB8888

/* Software renderer; no GPU acceleration on the first-target Pi. */
#define LV_USE_DRAW_SW          1

/* `background` gradients (schema 8.1): up to 4 color stops, and the
 * angled `linear` and the `radial` ones (computed per pixel; the
 * vertical and horizontal ones are the cheap path). */
#define LV_GRADIENT_MAX_STOPS   4
#define LV_USE_DRAW_SW_COMPLEX_GRADIENTS 1

/* `shadow`: the blurred corner of the last shadow drawn is kept, so
 * cards with the same shadow and radius (a page is rebuilt on every
 * value change) blur once. Size: shadow + radius up to 64 px, 4 KiB. */
#define LV_DRAW_SW_SHADOW_CACHE_SIZE 64

/* ---------------------------------------------------------------------
 * Timing and DPI
 * --------------------------------------------------------------------- */

/* 30 FPS refresh target (~33 ms); the Pi draws in software. The
 * default only: `screen:fps` (schema 12) sets the display refresh and
 * animation timers at run time (paneld_ui::set_fps). */
#define LV_DEF_REFR_PERIOD      33
#define LV_DPI_DEF              130

/* The default theme's own press transitions are off: the theme's
 * `motion` decides what animates (ADR 0023), `tap_transition` for a
 * press. */
#define LV_THEME_DEFAULT_TRANSITION_TIME 0

/* ---------------------------------------------------------------------
 * Logging and asserts (dev build; tighten for release)
 * --------------------------------------------------------------------- */

#define LV_USE_LOG              1
#define LV_LOG_LEVEL            LV_LOG_LEVEL_WARN
#define LV_LOG_PRINTF           1
#define LV_USE_ASSERT_NULL      1
#define LV_USE_ASSERT_MALLOC    1

/* ---------------------------------------------------------------------
 * Fonts
 * --------------------------------------------------------------------- */

/* Bitmap Montserrat sizes for the symbol glyphs: icons, and the fallback
 * of every TTF font (ADR 0015). Text is drawn in TTF through TinyTTF. */
#define LV_FONT_MONTSERRAT_12   1
#define LV_FONT_MONTSERRAT_14   1
#define LV_FONT_MONTSERRAT_16   1
#define LV_FONT_MONTSERRAT_20   1
#define LV_FONT_MONTSERRAT_24   1
#define LV_FONT_MONTSERRAT_32   1
#define LV_FONT_DEFAULT         LV_FONT_DEFAULT_MONTSERRAT_16

/* TinyTTF for the built-in and theme TTF fonts (ADR 0015). */
#define LV_USE_TINY_TTF         1
#define LV_TINY_TTF_FILE_SUPPORT 1

/* ---------------------------------------------------------------------
 * Widgets paneld uses (schema.md section 8)
 * --------------------------------------------------------------------- */

#define LV_USE_LABEL            1
#define LV_LABEL_TEXT_SELECTION 0
#define LV_LABEL_LONG_TXT_HINT  1

#define LV_USE_BUTTON           1
#define LV_USE_IMAGE            1
#define LV_USE_LINE             1
#define LV_USE_ARC              1
#define LV_USE_BAR              1
#define LV_USE_SLIDER           1
#define LV_USE_SWITCH           1
#define LV_USE_CHECKBOX         1
#define LV_USE_DROPDOWN         1
#define LV_USE_ROLLER           1
#define LV_USE_TEXTAREA         1
#define LV_USE_KEYBOARD         1
#define LV_USE_CHART            1
#define LV_USE_CANVAS           1
#define LV_USE_QRCODE           1
#define LV_USE_SPINNER          1

/* Layouts the UI tree builder relies on. */
#define LV_USE_FLEX             1
#define LV_USE_GRID             1

/* ---------------------------------------------------------------------
 * Platform drivers
 * --------------------------------------------------------------------- */

/* SDL2 window on a PC (WSLg). Controlled by the `sdl` Cargo
 * feature: build.rs defines PANELD_USE_SDL to 0 or 1. */
#define LV_USE_SDL              PANELD_USE_SDL
#define LV_SDL_INCLUDE_PATH     "SDL2/SDL.h"
#define LV_SDL_BACKEND          LV_SDL_BACKEND_SW
#define LV_SDL_RENDER_MODE      LV_DISPLAY_RENDER_MODE_DIRECT
#define LV_SDL_BUF_COUNT        1
/* LVGL draws in software; SDL only shows the frame. A software
 * renderer also works under SDL2's headless `dummy` driver (tests, CI),
 * which has no accelerated one. */
#define LV_SDL_ACCELERATED      0
#define LV_SDL_FULLSCREEN       0
#define LV_SDL_DIRECT_EXIT      1

/* Device backends (ADR 0012): DRM/KMS primary, fbdev as fallback,
 * evdev for touch. DRM is controlled by the `drm` Cargo feature;
 * fbdev + evdev have no external dependency so stay on. */
#define LV_USE_LINUX_DRM        PANELD_USE_DRM
#define LV_LINUX_DRM_GPU_PLANE  0
#define LV_LINUX_DRM_HOR_RES    800
#define LV_LINUX_DRM_VER_RES    480

#define LV_USE_LINUX_FBDEV      1
#define LV_LINUX_FBDEV_BSD      0
#define LV_LINUX_FBDEV_RENDER_MODE  LV_DISPLAY_RENDER_MODE_PARTIAL
#define LV_LINUX_FBDEV_BUFFER_COUNT 0
#define LV_LINUX_FBDEV_BUFFER_SIZE  60

#define LV_USE_EVDEV            1

/* `image` (schema 7): files through POSIX calls under the drive
 * letter `A:` (the path after it is a normal absolute path), PNG
 * through the bundled lodepng. Decoded images are cached so a redraw
 * does not decode them again: 4 MiB holds two full 800x480 ones (an
 * image larger than the cache would not be drawn at all). */
#define LV_USE_FS_POSIX         1
#define LV_FS_POSIX_LETTER      'A'
#define LV_FS_POSIX_PATH        ""
#define LV_FS_POSIX_CACHE_SIZE  0
#define LV_USE_LODEPNG          1
#define LV_CACHE_DEF_SIZE       (4 * 1024 * 1024)

#endif /*LV_CONF_H*/
