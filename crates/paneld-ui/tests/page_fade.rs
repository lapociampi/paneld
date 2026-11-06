//! A page fade (ADR 0023) shows the new page from its first frame and
//! still takes its whole duration on the clock. Its own process: LVGL
//! initializes once per process.

use lvgl_sys as sys;
use paneld_ui::Ui;

#[test]
fn a_fade_starts_partly_in_and_lasts_its_duration() {
    let ui = Ui::init().unwrap();
    let _mem = ui.open_memory_display(40, 20);
    let screen = ui.screen();
    let old = screen.new_page_holder();
    ui.advance(40);
    paneld_ui::begin_page_change(&[old]);
    let page = screen.new_page_holder();
    let new = page.raw();
    paneld_ui::run_page_transition(&screen, Some(&page), false, false, 120);
    let opa = || unsafe { sys::lv_obj_get_style_opa(new, 0) };
    // Before any frame: already partly in, not at once.
    let first = opa();
    assert!(first > 0 && first < 200, "first frame opacity {first}");
    let mut t = 0;
    while paneld_ui::page_transition_running() && t < 1000 {
        ui.advance(10);
        t += 10;
    }
    assert!((120..=130).contains(&t), "the fade took {t} ms, not 120");
    assert_eq!(opa(), 255);
}
