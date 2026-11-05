//! The DRM shadow buffer (`lv_paneld_drm_shadow`): LVGL draws into a
//! cached buffer and each frame's changed areas are copied into the
//! next of two panel buffers, as the DRM driver has them. Its own
//! process: LVGL initializes once per process.

use lvgl_sys as sys;
use paneld_ui::Ui;
use std::sync::atomic::{AtomicUsize, Ordering};

static SHOWN: AtomicUsize = AtomicUsize::new(0);
/// Calls of the driver's flush wait, and their count at the last flush.
static WAITS: AtomicUsize = AtomicUsize::new(0);
static WAITS_AT_SHOW: AtomicUsize = AtomicUsize::new(0);

/// Like the DRM driver: shows the buffer on the last flush and leaves
/// the flush pending (it ends in the wait, at the page flip).
unsafe extern "C" fn show(disp: *mut sys::lv_display_t, _area: *const sys::lv_area_t, px: *mut u8) {
    unsafe {
        if sys::lv_display_flush_is_last(disp) {
            SHOWN.store(px as usize, Ordering::SeqCst);
            WAITS_AT_SHOW.store(WAITS.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }
}

unsafe extern "C" fn wait(_disp: *mut sys::lv_display_t) {
    WAITS.fetch_add(1, Ordering::SeqCst);
}

const W: usize = 40;
const H: usize = 20;

fn panel(ptrs: &[usize]) -> *mut sys::lv_display_t {
    unsafe {
        let d = sys::lv_display_create(W as i32, H as i32);
        sys::lv_display_set_flush_cb(d, Some(show));
        sys::lv_display_set_flush_wait_cb(d, Some(wait));
        sys::lv_display_set_buffers(
            d,
            ptrs[0] as *mut _,
            ptrs[1] as *mut _,
            (W * H * 4) as u32,
            sys::lv_display_render_mode_t_LV_DISPLAY_RENDER_MODE_DIRECT,
        );
        sys::lv_display_set_default(d);
        d
    }
}

fn new_box() -> *mut sys::lv_obj_t {
    unsafe {
        let screen = sys::lv_screen_active();
        sys::lv_obj_set_style_bg_color(screen, sys::lv_color_hex(0x000000), 0);
        sys::lv_obj_set_style_bg_opa(screen, 255, 0);
        let b = sys::lv_obj_create(screen);
        sys::lv_obj_remove_style_all(b);
        sys::lv_obj_set_size(b, 4, 4);
        sys::lv_obj_set_style_bg_color(b, sys::lv_color_hex(0xffffff), 0);
        sys::lv_obj_set_style_bg_opa(b, 255, 0);
        b
    }
}

/// Draw one frame and return the panel buffer shown, after checking
/// the flip wait ran once, before the copy.
fn frame(disp: *mut sys::lv_display_t, i: usize) -> usize {
    let before = WAITS.load(Ordering::SeqCst);
    unsafe { sys::lv_refr_now(disp) };
    assert_eq!(WAITS.load(Ordering::SeqCst), before + 1, "frame {i}: one flip wait a frame");
    assert_eq!(WAITS_AT_SHOW.load(Ordering::SeqCst), before + 1, "frame {i}: the wait comes before the flush");
    SHOWN.load(Ordering::SeqCst)
}

fn whites(shown: usize) -> Vec<(usize, usize)> {
    let px = unsafe { std::slice::from_raw_parts(shown as *const u32, W * H) };
    (0..H).flat_map(|y| (0..W).map(move |x| (x, y))).filter(|&(x, y)| px[y * W + x] & 0xff == 0xff).collect()
}

#[test]
fn a_shadowed_panel_shows_whole_frames_upright_and_turned() {
    let _ui = Ui::init().unwrap();
    for rotation in [0, 180, 90, 270] {
        let bufs: Vec<&'static mut [u32]> = (0..2).map(|_| Box::leak(vec![0u32; W * H].into_boxed_slice())).collect();
        let ptrs: Vec<usize> = bufs.iter().map(|b| b.as_ptr() as usize).collect();
        let disp = panel(&ptrs);
        unsafe {
            assert!(sys::lv_paneld_drm_shadow(disp));
            assert!(!sys::lv_paneld_drm_shadow(disp), "shadowed twice");
        }
        // A frame upright first, as on the device (drawn while the
        // config loads): its areas must not be copied turned.
        frame(disp, 0);
        unsafe {
            assert!(sys::lv_paneld_set_rotation(disp, rotation));
            if rotation != 0 {
                assert!(!sys::lv_paneld_set_rotation(disp, 90), "rotated twice");
            }
            // Not drawn into: LVGL draws into the shadow buffer.
            let drawn = (*sys::lv_display_get_buf_active(disp)).data as usize;
            assert!(!ptrs.contains(&drawn));
        }
        let b = new_box();
        let mut shown_bufs = Vec::new();
        let swap = rotation % 180 == 90;
        let moves = if swap { [(0, 0), (16, 36), (8, 18), (0, 0)] } else { [(0, 0), (36, 16), (18, 8), (0, 0)] };
        for (i, (lx, ly)) in moves.into_iter().enumerate() {
            unsafe { sys::lv_obj_set_pos(b, lx, ly) };
            let shown = frame(disp, i);
            shown_bufs.push(ptrs.iter().position(|&p| p == shown).expect("a panel buffer is shown"));
            let white = whites(shown);
            assert_eq!(white.len(), 16, "rotation {rotation} frame {i}: {white:?}");
            let (lx, ly) = (lx as usize, ly as usize);
            let (px0, py0) = match rotation {
                0 => (lx, ly),
                90 => (ly, H - 4 - lx),
                180 => (W - 4 - lx, H - 4 - ly),
                _ => (W - 4 - ly, lx),
            };
            assert!(
                white.iter().all(|&(x, y)| (px0..px0 + 4).contains(&x) && (py0..py0 + 4).contains(&y)),
                "rotation {rotation} frame {i}: {white:?}"
            );
        }
        assert_eq!(shown_bufs, [1, 0, 1, 0], "the buffers take turns");
        // Only changed areas are copied: a mark where nothing changes stays.
        const MARK: u32 = 0x0012_3456;
        let mid = 4 * W + 30; // never drawn at, whichever way up
        for &p in &ptrs {
            unsafe { *(p as *mut u32).add(mid) = MARK };
        }
        for (i, x) in [4, 0, 4].into_iter().enumerate() {
            unsafe { sys::lv_obj_set_pos(b, x, 0) };
            frame(disp, 10 + i);
        }
        for &p in &ptrs {
            assert_eq!(unsafe { *(p as *const u32).add(mid) }, MARK, "rotation {rotation}: the middle was copied");
        }
        unsafe { sys::lv_obj_delete(b) };
    }
}
