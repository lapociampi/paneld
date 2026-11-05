//! Display rotation (schema 13) with two panel buffers, as the DRM
//! driver has. Its own process: LVGL initializes once per process.

use lvgl_sys as sys;
use paneld_ui::Ui;
use std::sync::atomic::{AtomicUsize, Ordering};

static SHOWN: AtomicUsize = AtomicUsize::new(0);
/// REFR_START calls, and the active buffer at the last one.
static STARTS: AtomicUsize = AtomicUsize::new(0);
static START_BUF: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn show(disp: *mut sys::lv_display_t, _area: *const sys::lv_area_t, px: *mut u8) {
    unsafe {
        if sys::lv_display_flush_is_last(disp) {
            SHOWN.store(px as usize, Ordering::SeqCst);
        }
        sys::lv_display_flush_ready(disp);
    }
}

/// Stands in for the DRM driver's REFR_START listener, which picks
/// the buffer to show from the active one.
unsafe extern "C" fn start(e: *mut sys::lv_event_t) {
    unsafe {
        let disp = sys::lv_event_get_current_target(e) as *mut sys::lv_display_t;
        let buf = sys::lv_display_get_buf_active(disp);
        START_BUF.store((*buf).data as usize, Ordering::SeqCst);
    }
    STARTS.fetch_add(1, Ordering::SeqCst);
}

fn new_box(screen: *mut sys::lv_obj_t) -> *mut sys::lv_obj_t {
    unsafe {
        let b = sys::lv_obj_create(screen);
        sys::lv_obj_remove_style_all(b);
        sys::lv_obj_set_size(b, 4, 4);
        sys::lv_obj_set_style_bg_color(b, sys::lv_color_hex(0xffffff), 0);
        sys::lv_obj_set_style_bg_opa(b, 255, 0);
        b
    }
}

/// Like the DRM driver: two panel buffers shown in turn, so each one
/// is two frames behind when it is drawn into again.
#[test]
fn with_two_panel_buffers_each_shown_frame_is_whole() {
    let _ui = Ui::init().unwrap();
    let (w, h) = (40usize, 20usize);
    let bufs: Vec<&'static mut [u32]> = (0..2).map(|_| Box::leak(vec![0u32; w * h].into_boxed_slice())).collect();
    let ptrs: Vec<usize> = bufs.iter().map(|b| b.as_ptr() as usize).collect();
    let disp = unsafe {
        let d = sys::lv_display_create(w as i32, h as i32);
        sys::lv_display_set_flush_cb(d, Some(show));
        sys::lv_display_set_buffers(
            d,
            ptrs[0] as *mut _,
            ptrs[1] as *mut _,
            (w * h * 4) as u32,
            sys::lv_display_render_mode_t_LV_DISPLAY_RENDER_MODE_DIRECT,
        );
        sys::lv_display_add_event_cb(d, Some(start), sys::lv_event_code_t_LV_EVENT_REFR_START, std::ptr::null_mut());
        sys::lv_display_set_default(d);
        assert!(sys::lv_paneld_set_rotation(d, 90));
        d
    };
    let screen = unsafe {
        let screen = sys::lv_screen_active();
        sys::lv_obj_set_style_bg_color(screen, sys::lv_color_hex(0x000000), 0);
        sys::lv_obj_set_style_bg_opa(screen, 255, 0);
        screen
    };
    let b = new_box(screen);
    // Run one frame; the panel buffer shown, after checking the
    // listener ran once, for that buffer.
    let frame = |i: usize| -> usize {
        let before = STARTS.load(Ordering::SeqCst);
        unsafe { sys::lv_refr_now(disp) };
        let shown = SHOWN.load(Ordering::SeqCst);
        assert_eq!(STARTS.load(Ordering::SeqCst), before + 1, "frame {i}: REFR_START listeners run once");
        assert_eq!(START_BUF.load(Ordering::SeqCst), shown, "frame {i}: REFR_START saw another buffer");
        shown
    };
    let whites = |shown: usize| -> Vec<(usize, usize)> {
        let px = unsafe { std::slice::from_raw_parts(shown as *const u32, w * h) };
        (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).filter(|&(x, y)| px[y * w + x] & 0xff == 0xff).collect()
    };
    // Logical box positions, frame by frame; with rotation 90 the
    // logical (x, y) is the panel's (y, h - 1 - x).
    let mut shown_bufs = Vec::new();
    for (i, (lx, ly)) in [(0, 0), (16, 36), (8, 18), (0, 0)].into_iter().enumerate() {
        unsafe { sys::lv_obj_set_pos(b, lx, ly) };
        let shown = frame(i);
        shown_bufs.push(ptrs.iter().position(|&p| p == shown).expect("a panel buffer is shown"));
        let white = whites(shown);
        assert_eq!(white.len(), 16, "frame {i}: {white:?}");
        let (px0, py0) = (ly as usize, h - 4 - lx as usize);
        assert!(white.iter().all(|&(x, y)| (px0..px0 + 4).contains(&x) && (py0..py0 + 4).contains(&y)), "frame {i}: {white:?}");
    }
    assert_eq!(shown_bufs, [0, 1, 0, 1], "the buffers take turns");

    // Two small changes in far corners rotate those two areas, not the
    // box around both: a mark in the middle of both panel buffers
    // (logical (9, 20), nothing drawn there changes) stays.
    let b2 = new_box(screen);
    unsafe { sys::lv_obj_set_pos(b2, 16, 36) };
    frame(10);
    const MARK: u32 = 0x0012_3456;
    let mid = 10 * w + 20;
    for &p in &ptrs {
        unsafe { *(p as *mut u32).add(mid) = MARK };
    }
    for (i, (y1, y2)) in [(4, 32), (0, 36), (4, 32)].into_iter().enumerate() {
        unsafe {
            sys::lv_obj_set_pos(b, 0, y1);
            sys::lv_obj_set_pos(b2, 16, y2);
        }
        let shown = frame(20 + i);
        assert_eq!(whites(shown).len(), 32, "frame {}", 20 + i);
    }
    for &p in &ptrs {
        assert_eq!(unsafe { *(p as *const u32).add(mid) }, MARK, "the middle was redrawn");
    }
}

#[test]
fn the_area_list_keeps_far_areas_apart_and_joins_when_full() {
    let a = |x1, y1, x2, y2| sys::lv_area_t { x1, y1, x2, y2 };
    let mut list = [a(0, 0, 0, 0); 3];
    let mut n = 0;
    let add = |list: &mut [sys::lv_area_t; 3], n: &mut i32, x: sys::lv_area_t| unsafe {
        sys::lv_paneld_areas_add(list.as_mut_ptr(), n, 3, &x)
    };
    add(&mut list, &mut n, a(0, 0, 3, 3));
    add(&mut list, &mut n, a(100, 100, 103, 103));
    add(&mut list, &mut n, a(1, 1, 2, 2)); // inside the first: dropped
    assert_eq!(n, 2);
    add(&mut list, &mut n, a(0, 100, 3, 103));
    assert_eq!(n, 3);
    // Full: joined into the entry it grows least (the first).
    add(&mut list, &mut n, a(5, 0, 8, 3));
    assert_eq!(n, 3);
    assert_eq!((list[0].x1, list[0].y1, list[0].x2, list[0].y2), (0, 0, 8, 3));
    assert_eq!((list[1].x1, list[2].y1), (100, 100));
}
