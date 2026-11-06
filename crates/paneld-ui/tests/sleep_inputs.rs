//! Digits typed into a keypad survive a sleep in which the page's
//! styles change (a night theme switching while asleep: the page is
//! built again at the wake and gets them back). Its own process: LVGL
//! initializes once per process.

use paneld_ui::Ui;

#[test]
fn typed_digits_survive_a_restyle_while_asleep() {
    let ui = Ui::init().unwrap();
    let _mem = ui.open_memory_display(320, 240);
    let screen = ui.screen();
    let src = "page home { keypad { length = 4 } }\n";
    let mut map = paneld_config::source::SourceMap::new();
    let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
    let (toks, _) = paneld_config::lex::tokenize(id, src);
    let (doc, diags) = paneld_config::parse(&toks, &map);
    assert!(diags.is_empty(), "{diags:?}");
    let (tx, _rx) = std::sync::mpsc::channel();
    paneld_ui::render_page(&doc, "home", &paneld_ui::Context::new(), &tx, &screen);
    let typed: Vec<(u64, String)> = paneld_ui::snapshot_inputs().into_iter().map(|(k, _)| (k, "12".to_string())).collect();
    assert_eq!(typed.len(), 1);
    paneld_ui::restore_inputs(&typed);
    paneld_ui::show_sleep_cover(false, tx.clone());
    // Asleep: the theme switches (other styles: no page table kept).
    paneld_ui::set_styles(Default::default());
    paneld_ui::hide_sleep_cover();
    // What the main loop takes before building the page again.
    assert_eq!(paneld_ui::snapshot_inputs(), typed, "the typed digits were lost");
}
