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

/* Draw `disp` (DRM: DIRECT mode, two write-combined dumb buffers)
 * into a cached buffer of its size and copy the changed areas into
 * the driver's buffer on each flush, so they are never read back; see
 * lv_paneld_set_rotation, which then only turns it. +1 frame of RAM
 * (1.5 MB at 800x480 XRGB8888). false: not DIRECT, or already done. */
bool lv_paneld_drm_shadow(lv_display_t * disp);

/* Add area `a` to `list` (`*n` used of `max`): dropped if inside one
 * already listed; when the list is full, joined into the entry it
 * grows least. For the rotation's changed areas; exposed for tests. */
void lv_paneld_areas_add(lv_area_t * list, int32_t * n, int32_t max, const lv_area_t * a);

/* How long LVGL's timers, all but the `n_skip` in `skip`, let the
 * loop sleep from now, at most `cap` ms: what lv_timer_handler would
 * hint without them (the SDL driver's own timers, to measure the
 * panel's idle wake rate on a desktop). */
uint32_t lv_paneld_timers_idle_ms(lv_timer_t * const * skip, uint32_t n_skip, uint32_t cap);

/* With PANELD_PERF set in the environment, print one line of timings
 * (us) per drawn frame to stderr; nothing is attached otherwise. Call
 * after lv_paneld_drm_shadow / lv_paneld_set_rotation. */
void lv_paneld_perf_attach(lv_display_t * disp);

/* CLOCK_MONOTONIC in us, the clock of the frame lines' `t`. */
uint64_t lv_paneld_mono_us(void);

/* In-place page updates (ADR 0024). */

/* A copy of `obj`'s local styles, every selector, to put back later
 * with lv_paneld_local_styles_restore (what the object looked like
 * when it was made, before a page set its properties). NULL when it
 * has none. Free with lv_paneld_local_styles_free. */
void * lv_paneld_local_styles_save(const lv_obj_t * obj);

/* Make `obj`'s local styles exactly `saved` (NULL: none) again. No
 * style refresh of its own: the caller refreshes once when done. */
void lv_paneld_local_styles_restore(lv_obj_t * obj, const void * saved);

void lv_paneld_local_styles_free(void * saved);

/* The `i`th local style property of `obj` (in style order, then
 * property order): its selector, property and value. false past the
 * last one. For comparing two trees. */
bool lv_paneld_local_style_at(const lv_obj_t * obj, uint32_t i, uint32_t * selector, lv_style_prop_t * prop,
                              lv_style_value_t * value);

/* `obj`'s hidden, clickable, checkable and scrollable flags, as the
 * bits of lv_obj_flag_t. */
uint32_t lv_paneld_obj_flags(const lv_obj_t * obj);

/* Set `obj`'s state bits as they are, with no restyle, transition or
 * event (an update setting a node up again; see
 * lv_paneld_state_settled). */
void lv_paneld_set_state_raw(lv_obj_t * obj, lv_state_t state);

/* `obj`'s state was set raw and differs from `prev`: restyle it for
 * the new state at once (no transition) and tell it. */
void lv_paneld_state_settled(lv_obj_t * obj, lv_state_t prev);

/* Delete the animations of `var` run by `cb` (custom exec): paneld's
 * own tweens, leaving a widget's own (a spinner's turn) alone. */
bool lv_paneld_anim_delete_custom(void * var, lv_anim_custom_exec_cb_t cb);

/* How many animations that run for good (a spinner's turn, a
 * scrolling label) `var` has, those run by `not_cb` left out. */
uint32_t lv_paneld_anim_count(void * var, lv_anim_custom_exec_cb_t not_cb);

/* An arc's value as before any was set (its indicator where a new
 * arc has it): an update of an arc whose `value` gives none. */
void lv_paneld_arc_unset(lv_obj_t * obj);

/* `obj`'s local styles are exactly `saved` (from
 * lv_paneld_local_styles_save), in any order. */
bool lv_paneld_local_styles_equal(const lv_obj_t * obj, const void * saved);

/* The name of `obj`'s class ("label", "bar", ...), "" if unnamed. */
const char * lv_paneld_class_name(const lv_obj_t * obj);

/* The event code an event descriptor is for, without the flag bits. */
lv_event_code_t lv_paneld_event_dsc_code(const lv_event_dsc_t * dsc);

#endif
