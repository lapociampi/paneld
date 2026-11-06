---
title: "ADR 0024: Pages update in place; recent pages are kept"
description: "How a value change updates the widgets that show it instead of rebuilding the page, when a subtree is still rebuilt, how recent pages are kept for navigation, and how tests prove an update matches a fresh render."
date: 2026-11-06
updated: 2026-11-06
tags: [paneld, adr, rendering]
series: paneld
order: 31
---

- Status: Accepted; Phases 0, 1 and 2 done (results below). Changes the rendering of [ADR 0003](0003-ui-model.md) ("every update rebuilds the page") and the "rebuilt whole" basis of [ADR 0023](0023-motion.md); keeps [ADR 0004](0004-cel.md) and [ADR 0010](0010-threads.md) (all LVGL work stays on the UI thread).
- Date: 2026-11-06

## Context

Every source update, var change, alert transition, clock tick and call state change runs `clear_children` + `render_page` (`crates/paneld/src/main.rs`). Measured on the Pi 3B+ (software rendering):

| Step | Value change | Navigation |
| --- | --- | --- |
| Build (objects, styles, exprs) | 12.7 ms | build + layout ~25 ms before the first frame |
| Layout | 6.3 ms | |
| Draw | 9 ms, full screen | full screen |
| Total | ~28 ms | |

A poll that changes one number pays all of it, and the DRM path (copy only changed areas) gains nothing because the whole page is invalidated. Goals:

- A value change touching a few labels: about 2-3 ms, redrawing only the changed areas.
- Navigating to a recently visited page skips the build.
- An in-place update can never look different from a fresh render.

What exists to build on: the parse cache (`PARSED`, `PARSED_INTERP` in `render.rs`), motion's node keys (scope hash of component calls and `repeat` item keys + the node's span, `motion.rs`), and the reference analysis of `visible.rs` (`sources.x`, remote members, `now:minute` / `now:second`).

What is in the way, found in the code:

| Where | Problem |
| --- | --- |
| `render.rs` 102, 271, 780 | Every component call and `repeat` item clones the whole context, sources' `history` lists (up to 2000 samples) included |
| `history.rs` `record` | Every run stamps `at` and grows `history`: a poll with the same output is not an equal value |
| `ui.rs` binding table, `INPUTS` | One table for the screen; `reset_action_bindings` clears it on each render and in `dialog_layer` (`main.rs`) |
| `ui.rs` `LAYOUT_MEM`, `LINE_MEM`, `CHART_ALPHA` | Grow until the whole screen is cleared |
| `render.rs` `DRAWN_CALLS`, `FAILED_BORDER`, `ROOT_TAP`; `expr.rs` `AGO_NEXT`; `motion.rs` `next` | Per pass globals: one page built at a time, a render has side effects |
| `ui.rs` `layout_grid` | Places only the children shown at build time |
| `ui.rs` `begin_page_change` / `end_page_transition` | Takes and deletes every screen child but the new page |
| `render.rs` `show_call_state` | A running button's content is deleted and a spinner added |

## Decision

### 1. Bindings

The renderer keeps, for each page it has built, a **node table** next to the LVGL tree. Fresh render and update call the same per-key resolution functions, so the two paths cannot disagree.

Per rendered node (`NodeRec`):

| Field | What |
| --- | --- |
| `key` | motion's node key (u64): scope hash (component calls, `repeat` item keys) + file + span start |
| `obj` | the LVGL object, and the parts a kind owns (a button's shortcut label / icon and its busy overlay, a keypad's keyboard) |
| `block` | index path to the `Block` in `ui_doc` (valid until reload, which drops every table) |
| `scope` | the `Scope` it evaluates in |
| `parent`, `children` | tree links, in render order |
| `bindings` | its `Binding` ids |
| `owned` | what was allocated for it: grid track arrays (today `LAYOUT_MEM`), line points (`LINE_MEM`), chart alpha (`CHART_ALPHA`), background image refs; freed with the node |
| `motion` | what motion showed (`Shown`), held here instead of the per-render `prev` / `next` maps |
| `flags` | hidden by `visible`, stale (skipped while hidden or touched), owns a content animation (spinner, `overflow = scroll`) |

Per `Scope` (page, component call, `repeat` item): parent scope, locals with their current values as `Rc<Value>` (component params, the repeat binding, `page`, `nav`, `value` / `event` where they apply), and for each local its deps. Globals are one `Rc` context per pass; evaluation looks up locals through the scope chain, then globals. No scope clones the context (Phase 0).

Per `Binding`:

| Field | What |
| --- | --- |
| `node`, `scope` | where it applies and evaluates |
| `prop` | the key and, for a style state, the selector (`:pressed`, ...) |
| `layers` | every source of this key for this node, in precedence order (below), each a cached parse + its scope |
| `deps` | union of the layers' dep keys and local names, computed once per parsed expression and cached with it |
| `last` | what was last applied: the value for scalars, a hash for lists (chart points, options); motion keeps the full points |
| `due` | for `ago()` readers: when the text next changes (replaces the per pass `AGO_NEXT` min) |

**Cascade.** A node's key can come from several layers. In `render.rs` order, later wins:

1. the kind's defaults set by `render_node` (gauge colors from `accent` / track, `FAILED_BORDER`, `rect` padding 0, ...),
2. theme styles `*`, the kind, the root's classes (`apply_styles`),
3. the node's own keys,
4. on a component root: the call's classes (`apply_styles` with `by_kind = false`),
5. on a component root: the call's own non-param keys.

A value that does not resolve (an eval error, a color that does not parse) falls through to the layer under it. So the winner is dynamic, and an update of key `k` re-runs the whole resolution of `k` for that node (all layers, top down, first valid wins, else the kind default from step 1); it never just re-applies the top layer, and never resets to LVGL's defaults. Coupled keys re-run together: a button's `background` also sets its text color unless the same body sets `color` (`render.rs` `"background"` arm), so on a button `background` and `color` are one binding. Classes are literal (`class` is not an expression), so the set of layers of a node is fixed until reload or theme switch.

**What can change at all.** Only keys the schema marks `expr_ok` (`schema.rs`) read expressions; all others are literal, and change only on reload or theme switch (a bare theme token in them is constant between switches).

| Key (block) | At run time | Phase |
| --- | --- | --- |
| `width`, `height` (all) | setter, layout marked dirty | 1 |
| `background` (all) | setter; image backgrounds refit after layout | 1 |
| `opacity` (all) | setter | 1 |
| `color` (all; arc / bar indicator, toggle on color, slider) | setter | 1 |
| `font` (all) | setter through the font cache | 1 |
| `enabled` (all) | disabled state | 1 |
| `visible` (all) | hidden flag (`set_hidden`, as today); a `grid` parent re-runs `layout_grid` | 1 |
| `enter` (all) | read when the node shows | 1 |
| `on_tap`, `on_hold` (`= {{ }}`), `action` (button), `on_change`, `on_submit`, handler block args | binding table entry rewritten, same index | 1 |
| `gap`, `row_gap`, `column_gap` (row, column, grid, scroll) | setter | 1 |
| `value` (text), `data` (qr), `points` (line) | setter | 1 |
| `value`, `min`, `max`, `track` (arc, bar) | setter; value through motion as today | 1 |
| `value`, `track` (toggle); `value`, `min`, `max`, `step`, `track` (slider) | setter, not while touched | 1 |
| `value` (select) | setter, not while touched | 1 |
| `options` (select dropdown, roller) | setter | 1 |
| `options` (select `style = buttons`) | rebuild the node | 2 |
| `values`, `color` (chart series), `min`, `max` (chart) | point setters, motion as today; a series count change cannot happen (series are blocks) | 1 |
| `src` (image) | setter through the image cache; empty <-> set: rebuild the node | 1 / 2 |
| `recolor` (image) | setter | 1 |
| `name`, `size` (icon) | glyph text or PNG src; glyph <-> PNG, unknown <-> known or a `size` that picks another glyph font: rebuild the node | 1 / 2 |
| `label`, `icon` (button) | setter on the shortcut part; a change of shape (none, label, icon, both): rebuild the button's content | 1 / 2 |
| `title` (page) | `page` / `nav` locals (below) | 1 |
| `repeat` source | items updated in place when the key list is unchanged, else keyed diff | 2 |
| component arg | a local: flows into whatever reads it | 1 |
| style body keys with expressions | a layer of the bindings above | 1 |

Phase 1 handles every row marked 1; a change hitting a row marked 2 (or a `repeat` source) falls back to a full rebuild of the page, as today.

Dep keys (one analysis, moved from `visible.rs` into `paneld-config` and used by both):

| Expression reads | Dep key |
| --- | --- |
| `sources.x.f` (field `f`: `value`, `ok`, `error`, `at`, `history`, `exit`, ...) | `sources.x.f` |
| `sources.x` whole, `sources.x[expr]` | `sources.x.*` |
| `sources` whole, `sources[expr]` | `sources.*` |
| remote member `dev.temp`, `dev.status.temp` | as `visible.rs`, plus the field as above |
| `vars.y` / `vars` whole | `vars.y` / `vars.*` |
| `alerts` (any part) | `alerts` |
| `now.<minute field>` / `now.second`, `now.unix`, `now` whole | `now:minute` / `now:second` |
| `ago(...)` | `ago` (run when its `due` passes) |
| `theme.*` | none: constant until a theme switch, which rebuilds |
| a param, repeat binding | the local, resolved through the scope chain |
| `page` | the page's `title` deps |
| `nav` | the `title` deps of every page with `nav` |

A read the analysis does not understand gets the widest key of its root (`sources.*`, `vars.*`), never none.

**Change set**, built by the main loop per pass in place of the `dirty` bool; the deferrals (asleep, page transition, touch: `main.rs` 697, 703, 711) keep the set (union) instead of a bool:

| Site (`main.rs`) | Keys |
| --- | --- |
| Source update (605, 628) | `sources.x.f` for each field of the new map that differs from the old one. A poll with the same output changes only `at` (and `history`): only readers of those run. `ok` / `error` status updates: those two fields |
| `VarsChanged` (563), inline sets (531, 559), `ctl var set` (2196) | the vars whose value differs from the last pass's snapshot |
| Alert tick, reminders, `note_resolved` (643, 646, 652), builtin ack / snooze (1395), display problem (2111), `ctl alert_fire` / `alert_resolve` (2225, 2253) | `alerts` |
| Clock (676) | `now:minute`, or `now:second` + `now:minute` |
| `ago_due` (689) | `ago` |
| Call started (1557), `call_done` (378, 512) | `call:<action>` |
| Dialog stage ended (500) | none for the page (its bindings survived, see 2); the dialog layer goes |
| `action_done` (379, 514): toast, dialog, `goto` | none, a dialog, or a navigation |
| Goto (504, 2209) | navigation: cached page update or build |
| Wake (`screen_step`, 666) | the set held while asleep, values instant |
| Theme switch (1459), reload | full rebuild |

Mapping a change set to work:

1. Each page has a dep index `DepKey -> [Binding]`, filled while building.
2. Collect the bindings of the changed keys and run them in tree order (component args and repeat sources come before the nodes under them).
3. A binding whose result equals `last` stops there: no setter, no invalidation.
4. A local whose value changed adds the bindings of that scope (and the scopes under it) that read it.
5. Each changed binding runs its key's resolution and setter, or marks its node for a rebuild (section 2). Rebuilds run after the setters, outermost first; a rebuilt subtree's own bindings are not run again.

### 2. Subtree rebuilds, identity, buttons, touch

- Granularity: the smallest node whose structure changed (a rebuild row above, or the `repeat` range). The parent keeps its object; new objects go in at the old index (`lv_obj_move_to_index`). A `grid` parent runs `layout_grid` again (cells are given by position among shown children), also when a child's `visible` flips.
- Identity is motion's node key. Before a subtree is deleted its records are kept in a map by key; the new build reads them for motion `Shown` (values move from what was shown, entrances carry on), keypad text (by key, replacing the by-position `snapshot_inputs`) and "was shown" for `enter`.
- `repeat` items are keyed by `item_key` (`id`, `name`, else position). Duplicate ids: the second `id:x` is `id:x#1`, the third `id:x#2` (position among its duplicates), logged once. Same keys in the same order: each item scope gets its new value and its readers run in place. Otherwise a keyed diff: kept keys keep their objects (moved, updated in place), removed keys are deleted, new keys are built (with `enter` if set). Position keys make a list without `id` / `name` rebuild from the first changed position.
- `visible = false` stays the hidden flag. Bindings under a hidden node are not run; they are marked stale and run before it shows again, with its `enter` transition ("was not shown on the last render").
- **Running buttons, one model for both paths:** the content (shortcut parts and children) is hidden and a busy overlay (the `busy` text, else a spinner) is added as the button's last child; when the call ends the overlay is deleted and the content shown. A fresh render of a running button builds the content hidden plus the overlay, so verify compares like with like. A change of `call:<action>` runs only the buttons bound to it (in place of `shows_call` + full redraw); the content's own bindings keep running while hidden.
- **Handlers:** each object's binding table entry (action name, evaluated args, inline `set` items) is a binding. Its deps changing rewrites the entry in the same pass. A page never keeps an entry whose args are stale: if an entry cannot be rewritten, the node (Phase 2) or the page (Phase 1) is rebuilt in that pass.
- **Dialogs:** binding tables belong to a holder (page holder, dialog layer, toast). `dialog_layer` stops calling `reset_action_bindings`; the backdrop already takes the taps, and closing the dialog frees only its own table. The page under it keeps updating.
- **Touch** (commit eef9377: no redraw while a finger is down or a list is open): setters go ahead, except on the touched object (`lv_indev_get_active_obj`), its subtree and an open dropdown / roller; those bindings are marked stale and run on release (which wakes the loop). Rebuilds wait for the release as today, since one could delete the pressed object. A slider being dragged keeps the finger's value; the source's value lands on release.
- Page transitions: setters during a transition go to the incoming page; rebuilds wait for its end.

### 3. Page cache

| Point | Rule |
| --- | --- |
| What | The LVGL tree, node table, scopes, dep index and binding tables of up to N pages, each under its own page holder |
| N | 3 by default (the page on screen plus 2 more), `app:page_cache = <0..8>`; 0 is today's behaviour |
| Parked | A page left is hidden (`LV_OBJ_FLAG_HIDDEN` on its holder) under the screen and not updated: each change set is checked against its own dep index (not `visible::on_page`, which only knows the page shown) and matching keys go to its pending set. Its animations are ended at their target for that holder only (per-object `lv_anim_delete` over its subtree, motion set to targets), not with the global `finish_motion`; spinners and scrolling text are rebuilt when it shows again |
| Transitions | `begin_page_change` / `end_page_transition` work on holders: the outgoing holder is parked (or deleted when not cached), not every screen child |
| Revisit | Run the pending set as one update, unhide, page transition as today. No build; layout only where updates dirtied it. Sources with `active = visible` resume polling then, as on a fresh visit |
| Eviction | Least recently shown first; also when the image cache (8 MiB, refcounted) is over budget and parked pages hold the entries, and when the cache's estimated bytes pass `app:page_cache_bytes` (default 1 MiB) |
| Invalidated (all dropped) | reload, theme switch (before `release_fonts`, which needs no object using the old fonts), a display size change |
| Asleep | the shown page and parked pages collect pending keys alike; the wake updates the shown one, values instant |

Memory per page, to be measured in Phase 0 (`ctl status` gains `nodes`, `bindings`, `cached_pages`, `cache_bytes`):

| Part | Estimate |
| --- | --- |
| LVGL objects | a rack page is ~45 objects: ~50-100 KB with styles, label text and widget data |
| Node records | ~150 B each |
| Bindings | ~100 B each + parse shared with the cache |
| Scopes and `last` | `Rc<Value>` shared with the pass's context, so a `history` list is not copied per scope; `last` of lists is a hash |
| Typical page | ~80-150 KB |

So N = 3 costs about 0.3-0.5 MB on a 5.5 MB RSS. The defaults are set from the Phase 0 measurement.

### 4. Correctness guard

- `PANELD_VERIFY_UPDATES=1` (on in every test, off by default in release): after each update, build the same page from scratch on a second, never-loaded screen (`lv_obj_create(NULL)`), with the same context, call states and keypad texts, motion forced to targets, lay both out and compare.
- The verify build must have no side effects. A render takes a `RenderPass` that owns what are globals today: motion writes (`next`), drawn calls, inputs, the binding table it registers into, `AGO_NEXT`, owned memory. The verify pass uses a scratch one that is dropped with its screen; background image refs it takes are released with it; log-once sets (`UNKNOWN_ICONS`, `BAD_FONTS`) are left as they are. Making render scoped like this is Phase 1 work, before any update path.
- Compared, as an object tree dump per node in render order: class, child count, flags (hidden, clickable, scrollable), states (checked, disabled, running, failed), coordinates after layout, local style properties per selector, label text, bar / arc / slider / toggle / select values (targets, not mid-animation), chart points, image src, binding table entry (action name, args) of each object.
- Left out: scroll offsets, pressed / focused states, the frame of a running animation.
- A mismatch logs the first differing node with its config file and line and fails the test. It never touches the screen shown.
- Pixels too: `paneld shot` gains `--verify`, rendering every step (taps, `--set`, source values) both ways with motion off and comparing the PNGs exactly.
- Coverage: every `tests/shot.rs` and `tests/daemon.rs` case runs with verify on; a new test drives random sequences of source, var, alert, call state and clock changes over `library.conf`, the examples and a copy of the rack config, comparing after each step.

### 5. Phases

Each phase ships alone, keeps a full rebuild as the fallback for what it does not handle, and runs verify in all tests from Phase 1 on.

| Phase | Scope | Tests must show | Expected gain (Pi) |
| --- | --- | --- | --- |
| 0. Measure, stop copying | Perf slots for clone / eval / objects / styles / layout; scopes with `Rc` locals and one `Rc` context per pass in place of `ctx.clone()` per component and repeat item (`eval` takes a lookup); `ctl status` `nodes`, memory | Same pixels as before on every shot test; `Slot::Clone` near 0 on the rack config | Part of the 12.7 ms build (history lists copied per scope), measured first; also every rebuild that stays |
| 1. Scoped render, bindings, leaf updates | `RenderPass` owning today's globals (binding table and inputs per holder, owned grid / line / chart memory per node, per-binding `ago` due, motion state per node), `dialog_layer` keeps the page's bindings; node table, field-level dep keys, typed change set at every site above, output cutoff, the phase 1 rows (incl. `visible` with grid relayout, handler entries rewritten, call states in the overlay model); verify mode and `shot --verify`. Rebuild rows and `repeat` sources: full rebuild | A source update changing one label keeps every `lv_obj_t` pointer, invalidates only that label's area, bumps `ctl status` `updates` not `renders`; a same-output poll runs only `at` / `history` readers; a bar moves from the value shown; a page stays tappable after a dialog closes; a running button's spinner and its end match a fresh render; toggling a grid child's `visible` matches a fresh render; verify passes everywhere | Value change on a rack page: ~28 ms -> ~3-5 ms; same-output poll < 0.5 ms |
| 2. Structure in place | Subtree rebuilds by key, `repeat` keyed diff (duplicate ids as above), the rebuild rows, stale hidden subtrees, keypad text by key, the touch rule relaxed for setters | A `repeat` gaining an item keeps the other items' objects and motion; duplicate ids render like a fresh render; a slider dragged keeps its value while a label beside it changes | Alerts list, repeat lists: ~28 ms -> ~3-8 ms; nothing short of reload / theme rebuilds the page |
| 3. Page cache | Holder-based transitions, parked pages with their own dep sets and pending keys, per-holder animation finishing, eviction, invalidation, `ctl status` `cached_pages` / `cache_bytes` | Back to a cached page creates no objects, shows what changed while away, verify passes there; reload and theme switch empty the cache; RSS with N = 3 within budget; idle CPU unchanged with parked spinners | Navigation to a cached page: ~25 ms build -> pending update (~1-3 ms) before the first frame |
| 4. Narrowing | `alerts.<name>` keys, labels with fixed width not dirtying their parents, docs (schema 8.1.1 and 16 cost notes, ADR 0023 context) with Pi figures | Rack home page clock tick under 2 ms; a history chart poll redraws only the chart's area | The 2-3 ms target on the rack pages |

### Phase 0 results

Done: the read analysis (which names and paths an expression reads) is `paneld_config::reads`, used by `paneld check` (every name in scope, schema 16) and `visible.rs`, and next by the dep keys of section 1. `Scope` in `expr.rs` (a component call's params, a `repeat` binding, a macro's variable, the page's `page` / `nav`, each over the scope around it, looked up innermost first); `render.rs` opens scopes instead of `ctx.clone()`; indexing, `len()` and macros read a path in place (`sources.x.history`) instead of copying it. `ctl status` has `nodes` and `rss_kb`. The pass's context (`build_ctx`, `ctx=` in the perf line, ~0.3 ms on the Pi) is still one copy per pass.

`perf build` medians, `us` (`clone` inside it) before -> after:

| Page | Before | After |
| --- | --- | --- |
| Pi, rack `home` (navigation) | 12.3 ms (2.3) | 8.1 ms (0) |
| Pi, rack `host` | 13.4 ms (2.4) | 9.1 ms (0) |
| Pi, rack `network` | 18.1 ms (3.1) | 12.6 ms (0) |
| Pi, rack `alerts` | 7.7 ms (1.1) | 5.8 ms (0) |
| x86, homelab `overview` + 3 sources with 2000-sample histories (value change) | 17.4 ms (8.6) | 1.2 ms (0) |
| x86, 24 component cells in a `repeat` + a history chart (value change) | 48.8 ms (17.4) | 1.3 ms (0) |

The gain is larger than `clone` alone: dropping the copies was not timed. RSS on the Pi after the same run: 6.6 -> 6.5 MB; a rack page is ~50 LVGL objects (`nodes` 52 on `home` with the shell).

### Phase 1 results

Done: `crates/paneld-ui/src/nodes.rs` (node table, dep keys, change set, the update walk) and the render code it walks with; typed change names at every redraw site of `main.rs` (`paneld_ui::Changes`); `PANELD_VERIFY_UPDATES` on in every test; `paneld shot --verify`, `--set`, `--source`, `--fuzz`, `--no-sources`; `ctl status` `updates`, `node_records`, `node_objects`; `PANELD_UPDATES=0` builds on every change, as before (to compare).

What updates in place, and what still builds the page:

| In place | Built afresh |
| --- | --- |
| Every key of the phase 1 rows, `visible` with a grid placing its children again, handler entries, button call states (overlay model), component args, `page` / `nav` titles | Navigation (phase 3), wake, theme switch, reload |
| A `repeat` whose item keys are the same (its items walked in place) | A `repeat` whose items changed (phase 2) |
| Under a dialog: the page updates, the dialog stays | A node that made other objects (button shortcut shape, icon glyph <-> PNG or size, select `style = buttons` options, keypad `masked` / `length`) |

Deviations from section 1, kept simpler with the same guarantee:

| Point | As built |
| --- | --- |
| Unit of an update | The node, not each key: a node whose deps changed is set up again from what it was made as (its local styles, clickable, managed states, as saved when made) by the same render code as a fresh build, so every layer of the cascade, the button `background` / `color` coupling and component call keys come out as in a fresh render. A node whose inputs hash as before (output cutoff) is not touched; one set up again is restyled (redrawn) only if its styles or states now differ (states are set raw during the walk, compared at its end) |
| `RenderPass` | A thread-local mode instead (build, update, scratch): a scratch render (verify) records nothing, moves nothing, leaves `ago()` timing, drawn calls and the top layer alone |
| Dep keys | Mapped in `nodes.rs` from `paneld_config::reads` through frames for component params, `repeat` bindings and `page` / `nav`; a node with a handler reads every local (its binding carries them) |
| `ago()` due | Per record: a node reading `ago`, a component call whose args do, and the page for its titles; the page's due is their soonest |
| Touch | As before: the whole update waits for the release |
| Fingerprints | Taken lazily: a node's first update after a build sets it up again (no cost added to a build); taken again whenever a node is set up again, forced by its component call's keys too |
| Setting up again | Ends only motion's own tweens on the node; a widget's own animations (a spinner's turn, a cursor's blink) go on. A value that gives nothing (a bar's, an arc's, a slider's, a select's choice, a dropdown's options) is put back as a new widget has it, unless the user changed it |
| Change names and the context | Vars and `running` are told apart from the same snapshot the update's context is built from (the action worker can `set` a var, a poller start a run, between reading the queues and building it) |
| Another shape found mid-walk | Its objects are made off the page (deleted after the walk), so a page under a dialog, built again only when it closes, shows no extras meanwhile |
| A theme PNG icon | Reads what its ancestors read, and its fingerprint holds the text color it is tinted with |

Verify (after every update, and after a change nothing on the page reads) compares the object trees per node (class, children, flags, states, place after layout, local styles, text, values at their target, chart points, image source, binding entries, animations that run for good, event callbacks other than those freeing memory on delete), and when the page's `ago()` texts are next drawn. What the user can change (a select's choice, a slider's value, a toggle's position) is compared while the user has not changed it since the widget was made; an update keeps a user's change until what the widget shows changes. Left out besides the ADR's list: a roller's label, where a hidden object would be. `shot --verify` compares pixels once motion has ended, without spinners and scrolling text. Two render fixes came out of it: a content-sized button no longer fits its label to the size it had while its call ran, and an entrance ends without leaving an opacity of its own.

Coverage: every `shot.rs` and `daemon.rs` test with verify on; new tests for a value change (no object made, a label's area redrawn, a same-output poll touches no value reader), a page taking taps after a dialog closes, a grid child shown again, running buttons and their end, a spinner shown again, a root set up again by its call, `ago()` in a component arg and a page title; `--fuzz --verify` (1-3 changes per pass: source values, `running`, vars, alerts, call states with args and `busy`, the clock) with three seeds over the shipped shell and alerts page, every page of the three examples and of the rack config (when next to the repo).

Measured, `perf update` against `perf build same=true` (`PANELD_UPDATES=0`), medians:

| Where | Built (before) | Updated (after) | Frame after |
| --- | --- | --- | --- |
| x86, homelab `overview`, source values | 1.1 ms | 0.06-0.1 ms | |
| x86, smart-home `home`, var sets | 0.96 ms | 0.07 ms | |
| x86, system `overview`, polls | 0.62 ms | 0.12-0.19 ms | |
| Pi, rack `home`, polls | 8-14 ms, then a 13-21 ms frame (384000 px) | 1.3 ms (1.0-2.3; the first after start 17 ms) | 1.6-3.3 ms (56000 px) |

RSS on the Pi after 90 s on `home`: 5.7 MB (`node_records` 38, `node_objects` 45, 48 LVGL objects).

### Phase 2 results

Done: a `repeat` item is a record of its own (its key, what it draws under it); a node that makes other objects is built again where it was; a finger on the page holds only what it touches.

| What | As built |
| --- | --- |
| Same keys | Each item walked in place with its new value (as in phase 1) |
| Other keys | Keyed diff: the repeat's records are cut out of the table by item; kept keys go back where the new list has them (their objects moved with `lv_obj_move_to_index`) and are walked in place; new keys are built in place (with `enter` if set); the rest are deleted, and motion forgets what they showed. A list without `id` / `name` keeps its items by position (each updated in place, the tail added or removed) |
| Duplicate keys | `id:x`, `id:x#1`, `id:x#2` by order among them, logged once per `repeat` |
| A `repeat` that draws no items | Its text (`[repeat eval: ...]`) is its own object: to and from it the repeat is drawn again in place (was: the page could only be built again) |
| Another shape | A node whose setup made other objects (button shortcut shape, icon glyph <-> PNG or another glyph size, select `style = buttons` options, image `src` empty <-> set, keypad `masked` / `length`, a QR code with <-> without data) is built again with its subtree, at its place in its parent; typed keypad text is carried over, motion goes on from what was shown, a component root gets its call's keys again. An unknown node's text is recorded too (was: the page) |
| Building within an update | The same build code in build mode, recorded at the end of the table and spliced in at the walk's place; what the walk is in (the records open) grows or shrinks around it. Objects are styled as they are made; new ones are noted, so the update restyles them if it sets them after |
| Dep keys within an update | The frames of a fresh build are bound in an update too (a call's params as recorded, the `repeat` binding, `page` / `nav`), so a subtree built in an update reads what a fresh build would |
| Touch | An update goes ahead while a finger is down or a list is open, with what the finger is on kept as the user sees it. Held, with their fingerprints, until the release: the node whose objects are, or are under, what a pointer holds pressed (or an open dropdown), with its subtree; every node around it (its `visible`, its size: walked into, not set up again); the whole `repeat` item it is in (so a row and the args of its button agree, also for a list keyed by position); and every change of structure (a diff, a node of another shape). A finger on the page's background or on a `scroll`'s own background (a drag that started there) holds no node, only structure; a drag that started on a child of a `scroll` holds that child. Anything else may change, also around it; if that moves what the finger holds (a sibling shown above it) or hides it, the press is cancelled, so lifting the finger taps nothing. The update says so (`held`), and its changes run again on release (a release wakes the loop); verify compares the page then, not while changes wait, and checks after each update that what a finger holds is still there and shows the same |

Deviations from section 2, kept simpler with the same guarantee:

| Point | As built |
| --- | --- |
| Rebuild rows | Not listed in the code: the node is set up as in phase 1, and a maker of another kind or with other fixed args, or another count of objects, marks it; what that setup made is dropped (made off the page) and the node is built again. One path for every row marked 2 |
| Unit | A node with its subtree, not a key: what is under it is built again too |
| Position keys | Kept by position and updated in place, not built again from the first changed position: fewer objects made, the same result |
| Touch, the held part | Not marked stale per binding: the whole change set runs again on release, and fingerprints skip what already shows it. An `ago()` text held is not timed until then; the page's other `ago()` texts go on |
| Keypad text | Kept by the key of its node (motion's), not by position: a full build of the same page (a wake, a dialog closed) puts typed digits back into the same `repeat` item's keypad wherever it is now |
| Stale hidden subtrees | Not done: what is under a hidden node is still updated (verify compares hidden objects too) |

Fixes that came out of the wider fuzz, all older than phase 2: a grid's content row kept the height of a child hidden since (LVGL keeps the size it stretched a child to; children are measured again when a grid is laid out again); a QR code without data showed the palette its new buffer happened to hold (set when made); a text whose failed `{{ }}` part changed its error kept the old message (its fingerprint took any failed part as the same); a QR code whose data stopped resolving kept the old code (with and without data are other shapes now); an open dropdown is in LVGL's checked state (compared as the user's, as a toggle's); a button two running calls cover (a call with args and one without) showed the busy text of whichever came first in a hash map, so an unrelated call could turn it (call states are kept in a fixed order); a minute that turned between the loop's look at the clock and the update's context was drawn one pass late (now looked at again with the context's time); `shot --verify` raced the clock the same way; `shot` ran the config's screen commands (the rack's backlight); the fuzz test went over only the rack config's alerts page (its pages are in included files); an `ago()` in a `repeat`'s list was never timed; a keypad in a parent as tall as its content grew it on every layout pass (its keys were half the parent's height; they get 200 px of their own there).

Coverage: new `shot.rs` tests for a `repeat` gaining, losing and reordering items (objects made: the new item's only), items with the same `id`, a `repeat` to and from its error text, nodes of another shape, a slider dragged while a rect beside it changes (and the var's value landing on release), items changing under a finger only after the release, the QR, error text and grid fixes; each fails without its change. The fuzz (`--fuzz`) changes lists: items gone, copied (same key), added with a new key, swapped, reversed, turned, a key or another field changed, lists in maps too; it seeds the examples' sources with their demo data (`--source name=@file.json`), taps and keeps a finger down (dragging now and then) through one to four passes' changes (`--fuzz-taps`; in the rack config with `--no-actions`), and goes back to the page under test when a tap left it; every page of the rack config, three seeds each.

Measured with `paneld shot --no-sources --fuzz 200 --seed 7` per page and `PANELD_PERF=1`: medians of `perf update` with and without `restructured`, against `perf build same=true` with `PANELD_UPDATES=0` (the same changes). Over all these runs no update fell back to a build.

| Where | Built (before) | Items changed, updated | Values only, updated |
| --- | --- | --- | --- |
| x86, rack `alerts` | 0.43 ms | 0.17 ms | 0.08 ms |
| x86, rack `host` (pools) | 0.61 ms | 0.27 ms | 0.15 ms |
| x86, homelab `overview` (services grid) | 1.05 ms | 0.31 ms | 0.07 ms |
| x86, homelab `vms` | 1.08 ms | 0.28 ms | 0.07 ms |
| Pi, rack `alerts` | 8.5 ms (p90 10.4) | 2.5 ms (p90 4.0) | 1.4 ms |
| Pi, rack `host` | 11.0 ms | 3.7 ms | 2.6 ms |
| Pi, rack `network` (steps) | 12.4 ms | 0.7 ms (3 changes) | 1.4 ms |
| Pi, rack `home` | 12.6 ms | - | 2.2 ms |

On the Pi: the static aarch64 build run by hand as the `paneld` user beside the running service, a real homelab config read only. A build is not slower for the item records: x86 rack `alerts` 0.41 -> 0.39 ms, `home` 0.56 -> 0.55 ms (phase 1 -> phase 2, `PANELD_UPDATES=0`). The frame after a change of items is not smaller than a build's when the list moves what is under it: on the rack `alerts` page a median of 270000 of 384000 px.

## What stays a full rebuild

- Reload (every key may mean another node; motion `forget`), theme switch (tokens, styles, fonts, components), a display size change.
- A page drawn the first time or evicted from the cache; navigation and the wake until phase 3.
- A page that is not there (`page not found`).
- Errors: a config error keeps the last good config (and its trees); a node that fails to build shows its error text as today and rebuilds whenever its bindings change.

## Risks

- Drift between update and fresh render: shared resolution per key (all layers, kind defaults), one model for running buttons, and verify in every test are the guard.
- LVGL state an update keeps and a rebuild reset: scroll position, focus, an open list. Intended (the touch rule relies on it), but a list scrolled down now stays scrolled after a poll.
- Moving today's globals into `RenderPass` / holders touches most of `render.rs` and `ui.rs`; Phase 1 is the large one.
- A dangling `obj` after something deletes an object behind the renderer (`clear_children` elsewhere, a modal): records are dropped only through the renderer, and a delete event on owned objects asserts in debug builds.
- Layout cost does not shrink by itself: a content-sized label in a flex row dirties its parents (Phase 4).
- Parked pages on a 1 GB shared Pi: bounded by N, the byte budget and the image cache rule.
- The dep analysis misses a read: the widest key is the default, and verify catches the rest in tests.
- More work in the first build (records, bindings): Phase 0's savings should cover it; Phase 1 must not make a first build slower than today.
