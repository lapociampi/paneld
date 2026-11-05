//! Display rotation (schema 13) on the memory display, which takes
//! the same path as the DRM one. Its own process: LVGL initializes
//! once per process.

use lvgl_sys as sys;
use paneld_ui::Ui;

#[test]
fn a_rotated_display_draws_upright_into_the_panel_buffer() {
    let ui = Ui::init().unwrap();
    let mut mem = ui.open_memory_display(40, 20);
    assert!(!mem.set_rotation(45));
    assert!(mem.set_rotation(90));
    assert!(!mem.set_rotation(90), "rotated twice");
    let disp = unsafe { sys::lv_display_get_default() };
    let logical = unsafe { (sys::lv_display_get_horizontal_resolution(disp), sys::lv_display_get_vertical_resolution(disp)) };
    assert_eq!(logical, (20, 40), "pages lay out in the rotated size");
    let b = unsafe {
        let screen = sys::lv_screen_active();
        sys::lv_obj_set_style_bg_color(screen, sys::lv_color_hex(0x000000), 0);
        sys::lv_obj_set_style_bg_opa(screen, 255, 0);
        // A white box in the logical top-left corner.
        let b = sys::lv_obj_create(screen);
        sys::lv_obj_remove_style_all(b);
        sys::lv_obj_set_pos(b, 0, 0);
        sys::lv_obj_set_size(b, 4, 4);
        sys::lv_obj_set_style_bg_color(b, sys::lv_color_hex(0xffffff), 0);
        sys::lv_obj_set_style_bg_opa(b, 255, 0);
        b
    };
    ui.advance(10);
    let rgb = mem.rgb();
    assert_eq!(mem.size(), (40, 20));
    let at = |x: usize, y: usize| rgb[(y * 40 + x) * 3];
    // Turned 90 degrees clockwise, the panel shows the logical
    // top-left at its bottom-left.
    let corners = [at(0, 0), at(39, 0), at(0, 19), at(39, 19)];
    assert_eq!(corners, [0, 0, 255, 0]);
    // A touch there (panel coordinates) is on the box.
    let mut p = sys::lv_point_t { x: 0, y: 19 };
    unsafe { sys::lv_display_rotate_point(disp, &mut p) };
    assert!(p.x < 4 && p.y < 4, "panel (0,19) maps to logical ({},{})", p.x, p.y);
    // A partial redraw (no full invalidate): moving the box to the
    // logical bottom-right clears its old place on the panel and
    // draws the new one at the panel's top-right.
    unsafe { sys::lv_obj_set_pos(b, 16, 36) };
    ui.advance(50);
    let raw = |x: usize, y: usize| mem.pixels()[y * 40 + x] & 0xff;
    assert_eq!(raw(0, 19), 0, "old place not redrawn");
    assert_eq!(raw(39, 0), 0xff, "new place not drawn");
    assert_eq!(raw(20, 10), 0, "the middle was drawn on");
}
