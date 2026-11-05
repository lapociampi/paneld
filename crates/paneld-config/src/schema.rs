//! Schema check.
//!
//! Four passes, run in order:
//!
//! - **Structural** (Phase 4): top-level items are one of the known
//!   kinds; named vs. category correctness; duplicate named items;
//!   `include` form; stray `param` / `repeat` outside their contexts.
//! - **Per-key** (Phase 5-6): inside blocks whose kind has a rule
//!   table below, every assignment's key is known and its raw value
//!   parses as the declared type (via the parsers in [`crate::value`]).
//!   Phase 6 extended this to UI building blocks (section 8), alert
//!   and reminder (sections 9-10), and page / component bodies. A
//!   `{{ ... }}` expression is accepted wherever an `expr_ok` rule
//!   allows it; CEL type-checking of the expression body itself is
//!   deferred to a later phase.
//! - **Cross-reference** (Phase 7): a second AST walk resolves name
//!   references — `on_tap`/`on_hold`/`on_change`/`on_submit`/`button.action`/
//!   `alert.action`/`action.then`/`action.else` against the collected
//!   `action` names; `action.refresh`/`alert.from` against `source`
//!   names; `screen.theme`/`night.theme` against `theme` names;
//!   `app.start_page`/`goto` against `page` names (with `back` as a
//!   special value); `security.pin` against `secret` names. Values
//!   wrapped in `{{ }}` are skipped; CEL will handle them.
//! - **Arg type-check** (Phase 8): a [`Params`] table is built from
//!   every top-level `component` and `action` body (`param <name>:
//!   <type> [= <default>]`). User-component call blocks
//!   (`stat_tile { label = CPU; value = ... }`) and named
//!   action-call shortcuts (`on_tap vm_stop { vm = ... }`) are then
//!   validated: unknown arg keys are flagged, each value's raw type
//!   is checked against the param's declared type, missing required
//!   args are reported, and `action` / `source`-typed args that are
//!   bare identifiers are cross-referenced against the collected
//!   `Names`.
//!
//! What is still deferred (future phases):
//!
//! - Theme body validation (section 11: tokens, styles, overrides).
//! - CEL parsing + type checking of `{{ }}` expression bodies.
//!
//! Diagnostics include file:line:col thanks to the [`SourceMap`].

use std::collections::HashMap;

use crate::parse::{Assign, AssignTarget, Block, Diagnostic, Document, Item, OptionsBlock, Param};
use crate::value::{
    classify, parse_bool, parse_bytes, parse_color, parse_duration, parse_int, parse_number,
    parse_size, ValueShape,
};
use crate::{SourceMap, Span};

pub(crate) mod remote;

// ---------------------------------------------------------------------------
// Structural tables
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq)]
enum KindKind {
    Named,
    Category,
}

fn classify_kind(kind: &str) -> Option<KindKind> {
    match kind {
        "secret" | "source" | "action" | "component" | "page" | "alert" | "reminder" | "theme"
        | "remote_kind" | "remote" => {
            Some(KindKind::Named)
        }
        "screen" | "display" | "security" | "app" | "alerts" => Some(KindKind::Category),
        _ => None,
    }
}

/// `true` for a settings category (`app`, `screen`, ...), whose keys
/// are options: settable many times, last one in load order wins.
pub(crate) fn is_category(kind: &str) -> bool {
    matches!(classify_kind(kind), Some(KindKind::Category))
}

fn is_legal_top_assign(key: &str) -> bool {
    matches!(key, "include")
}

// ---------------------------------------------------------------------------
// Per-key rules
// ---------------------------------------------------------------------------

/// Expected shape of a value's raw text, before `{{ }}` consideration.
#[derive(Clone, Copy, Debug)]
enum ExpectedType {
    Bool,
    Int,
    Number,
    Str,
    Dur,
    /// A calendar date, `YYYY-MM-DD`.
    Date,
    Size,
    Bytes,
    /// A hex color or the name of a theme token (`accent`).
    Color,
    /// A hex color only: an action param's value goes to a command,
    /// which knows no theme.
    HexColor,
    /// A path (currently a free string; no fs checks).
    Path,
    /// A command: raw text run as argv or `sh -c '...'`. Validated as
    /// non-empty text.
    Command,
    /// A bare identifier reference (page name, source name, etc.).
    Ident,
    /// A list literal.
    List,
    /// Identifier or list of identifiers (`refresh = vms` or
    /// `refresh = [vms, cpu_temp]`).
    IdentOrList,
    /// Exactly one of the given keyword strings.
    OneOf(&'static [&'static str]),
    /// `padding`: 1, 2 or 4 px sizes.
    Padding,
    /// A grid `cell`: `col row [colspan rowspan]`, from 0.
    Cell,
    /// A grid's `columns` / `rows`: a count, or a list of tracks (px,
    /// `Nfr`, `content`).
    Tracks,
    /// A `stack` child's `x` / `y`: px or a `%`, either may be negative.
    Offset,
    /// An integer of at least 1 (a line's or an arc's `thickness`).
    PositiveInt,
    /// An icon name (schema 8.3): a built-in, or any name when the
    /// active theme has an `icons` folder (its file may supply it).
    Icon,
    /// A font value (ADR 0015): `<family> <size> [bold]`, or a bare
    /// theme font token (`font_big`).
    Font,
    /// A `background` (schema 8.1): a color, `none`, a gradient, or a
    /// `.png` path (inside the config folder, a PNG) with a fit.
    Background,
    /// A `shadow`: `none`, `<size> [<color>] [<x> <y>]`, or a theme
    /// token holding one.
    Shadow,
    /// A theme's `background_image`: a `.png` inside its folder.
    Image,
    /// Any text accepted; no shape check.
    Any,
}

#[derive(Clone, Copy)]
struct KeyRule {
    name: &'static str,
    ty: ExpectedType,
    required: bool,
    /// Allow `{{ expr }}` or interpolated text.
    expr_ok: bool,
}

/// What's permitted inside a block body.
#[derive(Clone, Copy)]
enum BodyMode {
    /// Only keys in `keys` are allowed; any sub-block kind must appear
    /// in `nested`.
    Keyed,
    /// Any key is allowed; every value is validated as `ty`. Nested
    /// blocks are not permitted.
    AnyKey(ExpectedType),
}

#[derive(Clone, Copy)]
struct NestedBlock {
    kind: &'static str,
    name_mode: NameMode,
    schema: &'static BlockSchema,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameMode {
    /// A `kind name { ... }` named sub-block; the name is required.
    /// Reserved for schemas that will need it (chart series with ids,
    /// future named sub-blocks); kept here so adding one does not
    /// churn the enum.
    #[allow(dead_code)]
    Required,
    /// A `kind { ... }` sub-block; a name is forbidden.
    Forbidden,
    /// Either form is accepted (`then` / `else`: name is an optional
    /// called-action reference).
    Optional,
}

#[derive(Clone, Copy)]
struct BlockSchema {
    keys: &'static [KeyRule],
    nested: &'static [NestedBlock],
    body_mode: BodyMode,
}

// --- generic small blocks ---

const ENV_BLOCK: &BlockSchema = &BlockSchema {
    keys: &[],
    nested: &[],
    body_mode: BodyMode::AnyKey(ExpectedType::Str),
};

const SET_BLOCK: &BlockSchema = &BlockSchema {
    keys: &[],
    nested: &[],
    body_mode: BodyMode::AnyKey(ExpectedType::Any),
};

/// Body of a `then <action> { arg = val }` / `else <action> { ... }`
/// block inside an `action`. Keys are the called action's parameter
/// names, which the caller cannot cross-check without the action's
/// param table (deferred). Keep lenient for now.
const THEN_ELSE_BLOCK: &BlockSchema = &BlockSchema {
    keys: &[],
    nested: &[],
    body_mode: BodyMode::AnyKey(ExpectedType::Any),
};

const NIGHT_BLOCK: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "from",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "to",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "theme",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "idle",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "brightness",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

// --- top-level item schemas ---

const SECRET_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "env",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "file",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const SOURCE_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "run",
            ty: ExpectedType::Command,
            required: true,
            expr_ok: false,
        },
        KeyRule {
            name: "poll",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "stream",
            ty: ExpectedType::Bool,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "parse",
            ty: ExpectedType::OneOf(&["text", "number", "json", "lines"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "value",
            ty: ExpectedType::Any,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "timeout",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "history",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "active",
            ty: ExpectedType::OneOf(&["always", "visible"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "cwd",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[NestedBlock {
        kind: "env",
        name_mode: NameMode::Forbidden,
        schema: ENV_BLOCK,
    }],
    body_mode: BodyMode::Keyed,
};

const ACTION_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "pin",
            ty: ExpectedType::Bool,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "confirm",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        // How the action is named in its dialogs and messages.
        KeyRule {
            name: "title",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "run",
            ty: ExpectedType::Command,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "timeout",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "cwd",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "goto",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "refresh",
            ty: ExpectedType::IdentOrList,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "then",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "else",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "busy",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "done",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "failed",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
    ],
    nested: &[
        NestedBlock {
            kind: "env",
            name_mode: NameMode::Forbidden,
            schema: ENV_BLOCK,
        },
        NestedBlock {
            kind: "set",
            name_mode: NameMode::Forbidden,
            schema: SET_BLOCK,
        },
        NestedBlock {
            kind: "then",
            name_mode: NameMode::Optional,
            schema: THEN_ELSE_BLOCK,
        },
        NestedBlock {
            kind: "else",
            name_mode: NameMode::Optional,
            schema: THEN_ELSE_BLOCK,
        },
    ],
    body_mode: BodyMode::Keyed,
};

const SCREEN_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "theme",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "idle",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "brightness",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "on",
            ty: ExpectedType::Command,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "off",
            ty: ExpectedType::Command,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "dim",
            ty: ExpectedType::Command,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "wake_on",
            ty: ExpectedType::List,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "first_tap",
            ty: ExpectedType::OneOf(&["wake", "act"]),
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[NestedBlock {
        kind: "night",
        name_mode: NameMode::Forbidden,
        schema: NIGHT_BLOCK,
    }],
    body_mode: BodyMode::Keyed,
};

const DISPLAY_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "backend",
            ty: ExpectedType::OneOf(&["drm", "fbdev", "sdl"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "device",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "touch",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "width",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "height",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "rotation",
            ty: ExpectedType::OneOf(&["0", "90", "180", "270"]),
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const SECURITY_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "pin",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "unlock_for",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "attempts",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "lockout",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const APP_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "start_page",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "timeout",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "max_output",
            ty: ExpectedType::Bytes,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "max_running",
            ty: ExpectedType::Int,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "state",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "log",
            ty: ExpectedType::OneOf(&["error", "warn", "info", "debug"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "action_log",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "socket",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "shell",
            ty: ExpectedType::Path,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const ALERT_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "from",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "when",
            ty: ExpectedType::Bool,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "for",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "id",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "state",
            ty: ExpectedType::OneOf(&["firing", "resolved"]),
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "severity",
            ty: ExpectedType::OneOf(&["critical", "warning", "info"]),
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "title",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "text",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "link",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "action",
            ty: ExpectedType::Ident,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const REMINDER_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "every",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "start",
            ty: ExpectedType::Date,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "once",
            ty: ExpectedType::Date,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "title",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "text",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "link",
            ty: ExpectedType::Str,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "severity",
            ty: ExpectedType::OneOf(&["critical", "warning", "info"]),
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

const ALERTS_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "history",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "snooze",
            ty: ExpectedType::List,
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "source_down",
            ty: ExpectedType::Dur,
            required: false,
            expr_ok: false,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

/// Return the Phase-5 rule table for a top-level block kind, or `None`
/// to indicate "pass through without validating body" (component /
/// page / theme / alert / reminder — they have their own non-trivial
/// semantics the UI phase will cover).
fn top_schema_for(kind: &str) -> Option<&'static BlockSchema> {
    match kind {
        "secret" => Some(SECRET_SCHEMA),
        "source" => Some(SOURCE_SCHEMA),
        "action" => Some(ACTION_SCHEMA),
        "screen" => Some(SCREEN_SCHEMA),
        "display" => Some(DISPLAY_SCHEMA),
        "security" => Some(SECURITY_SCHEMA),
        "app" => Some(APP_SCHEMA),
        "alerts" => Some(ALERTS_SCHEMA),
        "alert" => Some(ALERT_SCHEMA),
        "reminder" => Some(REMINDER_SCHEMA),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Page / component keys and UI building blocks (schema section 7-8).
// ---------------------------------------------------------------------------

const PAGE_KEYS: &[KeyRule] = &[
    KeyRule { name: "title", ty: ExpectedType::Str, required: false, expr_ok: true },
    KeyRule { name: "icon", ty: ExpectedType::Icon, required: false, expr_ok: false },
    KeyRule { name: "nav", ty: ExpectedType::Int, required: false, expr_ok: false },
];

/// Properties every UI building block accepts (schema section 8.1).
const COMMON_UI_KEYS: &[KeyRule] = &[
    KeyRule { name: "width", ty: ExpectedType::Size, required: false, expr_ok: true },
    KeyRule { name: "height", ty: ExpectedType::Size, required: false, expr_ok: true },
    KeyRule { name: "min_width", ty: ExpectedType::Size, required: false, expr_ok: false },
    KeyRule { name: "max_width", ty: ExpectedType::Size, required: false, expr_ok: false },
    KeyRule { name: "min_height", ty: ExpectedType::Size, required: false, expr_ok: false },
    KeyRule { name: "max_height", ty: ExpectedType::Size, required: false, expr_ok: false },
    KeyRule { name: "grow", ty: ExpectedType::Number, required: false, expr_ok: false },
    KeyRule {
        name: "align_self",
        ty: ExpectedType::OneOf(&["start", "center", "end", "stretch"]),
        required: false,
        expr_ok: false,
    },
    KeyRule { name: "cell", ty: ExpectedType::Cell, required: false, expr_ok: false },
    KeyRule { name: "x", ty: ExpectedType::Offset, required: false, expr_ok: false },
    KeyRule { name: "y", ty: ExpectedType::Offset, required: false, expr_ok: false },
    KeyRule {
        name: "anchor",
        ty: ExpectedType::OneOf(&[
            "top_left", "top_center", "top_right",
            "left", "center", "right",
            "bottom_left", "bottom_center", "bottom_right",
        ]),
        required: false,
        expr_ok: false,
    },
    KeyRule { name: "padding", ty: ExpectedType::Padding, required: false, expr_ok: false },
    KeyRule { name: "class", ty: ExpectedType::IdentOrList, required: false, expr_ok: false },
    KeyRule { name: "background", ty: ExpectedType::Background, required: false, expr_ok: true },
    KeyRule { name: "opacity", ty: ExpectedType::Number, required: false, expr_ok: true },
    KeyRule { name: "radius", ty: ExpectedType::Size, required: false, expr_ok: false },
    KeyRule { name: "border", ty: ExpectedType::Str, required: false, expr_ok: false },
    KeyRule { name: "shadow", ty: ExpectedType::Shadow, required: false, expr_ok: false },
    KeyRule { name: "color", ty: ExpectedType::Color, required: false, expr_ok: true },
    KeyRule { name: "font", ty: ExpectedType::Font, required: false, expr_ok: true },
    KeyRule { name: "visible", ty: ExpectedType::Bool, required: false, expr_ok: true },
    KeyRule { name: "enabled", ty: ExpectedType::Bool, required: false, expr_ok: true },
    // `on_tap = action` / `on_hold = action` as the plain-assign form.
    // The block form `on_tap action { arg = val }` is handled by
    // the UI walker separately. `expr_ok` is true because a component
    // can thread an action through a param: `on_tap = {{ tap }}` where
    // `tap: action` (schema.md section 7 example).
    KeyRule { name: "on_tap", ty: ExpectedType::Ident, required: false, expr_ok: true },
    KeyRule { name: "on_hold", ty: ExpectedType::Ident, required: false, expr_ok: true },
];

/// Event-handler keys whose block form (`on_tap action { ... }`) is a
/// shortcut for calling an action with arguments.
const EVENT_HANDLER_KINDS: &[&str] = &["on_tap", "on_hold", "on_change", "on_submit"];

/// Inline-action keys accepted inside an action-call shortcut body
/// (e.g. `on_tap { goto = home; refresh = foo }`).
const INLINE_ACTION_KEYS: &[&str] =
    &["goto", "refresh", "set", "then", "else", "run", "confirm", "pin", "title"];

/// A chart `series { }` sub-block.
const CHART_SERIES_SCHEMA: &BlockSchema = &BlockSchema {
    keys: &[
        KeyRule {
            name: "values",
            ty: ExpectedType::Any,
            required: false,
            expr_ok: true,
        },
        KeyRule {
            name: "color",
            ty: ExpectedType::Any,
            required: false,
            expr_ok: true,
        },
    ],
    nested: &[],
    body_mode: BodyMode::Keyed,
};

struct UiNode {
    kind: &'static str,
    keys: &'static [KeyRule],
    accepts_children: bool,
    nested_subblocks: &'static [NestedBlock],
    slot_only: bool,
}

const UI_ROW: &UiNode = &UiNode {
    kind: "row",
    keys: &[
        KeyRule { name: "gap", ty: ExpectedType::Int, required: false, expr_ok: true },
        KeyRule {
            name: "justify",
            ty: ExpectedType::OneOf(&["start", "center", "end", "between", "around", "evenly"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "align",
            ty: ExpectedType::OneOf(&["start", "center", "end", "stretch"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "wrap", ty: ExpectedType::Bool, required: false, expr_ok: false },
    ],
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_COLUMN: &UiNode = &UiNode {
    kind: "column",
    keys: UI_ROW.keys,
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_GRID: &UiNode = &UiNode {
    kind: "grid",
    keys: &[
        KeyRule { name: "columns", ty: ExpectedType::Tracks, required: false, expr_ok: false },
        KeyRule { name: "rows", ty: ExpectedType::Tracks, required: false, expr_ok: false },
        KeyRule { name: "gap", ty: ExpectedType::Int, required: false, expr_ok: true },
        KeyRule { name: "row_gap", ty: ExpectedType::Int, required: false, expr_ok: true },
        KeyRule { name: "column_gap", ty: ExpectedType::Int, required: false, expr_ok: true },
    ],
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_STACK: &UiNode = &UiNode {
    kind: "stack",
    keys: &[],
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_SCROLL: &UiNode = &UiNode {
    kind: "scroll",
    keys: &[
        KeyRule {
            name: "direction",
            ty: ExpectedType::OneOf(&["vertical", "horizontal"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "snap",
            ty: ExpectedType::OneOf(&["none", "start", "center"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "gap", ty: ExpectedType::Int, required: false, expr_ok: true },
    ],
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_SLOT: &UiNode = &UiNode {
    kind: "slot",
    keys: &[],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: true,
};

const UI_TEXT: &UiNode = &UiNode {
    kind: "text",
    keys: &[
        KeyRule { name: "value", ty: ExpectedType::Str, required: false, expr_ok: true },
        KeyRule {
            name: "align",
            ty: ExpectedType::OneOf(&["left", "center", "right"]),
            required: false,
            expr_ok: false,
        },
        KeyRule {
            name: "overflow",
            ty: ExpectedType::OneOf(&["wrap", "clip", "dots", "scroll"]),
            required: false,
            expr_ok: false,
        },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_ICON: &UiNode = &UiNode {
    kind: "icon",
    keys: &[
        KeyRule { name: "name", ty: ExpectedType::Icon, required: false, expr_ok: true },
        KeyRule { name: "size", ty: ExpectedType::PositiveInt, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_IMAGE: &UiNode = &UiNode {
    kind: "image",
    keys: &[
        KeyRule { name: "src", ty: ExpectedType::Str, required: false, expr_ok: true },
        KeyRule {
            name: "fit",
            ty: ExpectedType::OneOf(&["contain", "cover", "fill", "none"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "recolor", ty: ExpectedType::Color, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_RECT: &UiNode = &UiNode {
    kind: "rect",
    keys: &[],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_ARC: &UiNode = &UiNode {
    kind: "arc",
    keys: &[
        KeyRule { name: "value", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "min", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "max", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "start", ty: ExpectedType::Number, required: false, expr_ok: false },
        KeyRule { name: "end", ty: ExpectedType::Number, required: false, expr_ok: false },
        KeyRule { name: "thickness", ty: ExpectedType::PositiveInt, required: false, expr_ok: false },
        KeyRule { name: "track", ty: ExpectedType::Color, required: false, expr_ok: true },
        KeyRule { name: "rounded", ty: ExpectedType::Bool, required: false, expr_ok: false },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_BAR: &UiNode = &UiNode {
    kind: "bar",
    keys: &[
        KeyRule { name: "value", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "min", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "max", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule {
            name: "direction",
            ty: ExpectedType::OneOf(&["horizontal", "vertical"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "track", ty: ExpectedType::Color, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_LINE: &UiNode = &UiNode {
    kind: "line",
    keys: &[
        KeyRule { name: "points", ty: ExpectedType::Any, required: false, expr_ok: true },
        KeyRule { name: "thickness", ty: ExpectedType::PositiveInt, required: false, expr_ok: false },
        KeyRule { name: "rounded", ty: ExpectedType::Bool, required: false, expr_ok: false },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_CHART: &UiNode = &UiNode {
    kind: "chart",
    keys: &[
        KeyRule {
            name: "type",
            ty: ExpectedType::OneOf(&["line", "bar", "area"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "min", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "max", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "points", ty: ExpectedType::Int, required: false, expr_ok: false },
        KeyRule { name: "divisions", ty: ExpectedType::Str, required: false, expr_ok: false },
    ],
    accepts_children: false,
    nested_subblocks: &[NestedBlock {
        kind: "series",
        name_mode: NameMode::Forbidden,
        schema: CHART_SERIES_SCHEMA,
    }],
    slot_only: false,
};

const UI_QR: &UiNode = &UiNode {
    kind: "qr",
    keys: &[
        KeyRule { name: "data", ty: ExpectedType::Str, required: false, expr_ok: true },
        KeyRule { name: "size", ty: ExpectedType::Size, required: false, expr_ok: false },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_SPINNER: &UiNode = &UiNode {
    kind: "spinner",
    keys: &[KeyRule {
        name: "size",
        ty: ExpectedType::Size,
        required: false,
        expr_ok: false,
    }],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_BUTTON: &UiNode = &UiNode {
    kind: "button",
    keys: &[
        KeyRule { name: "action", ty: ExpectedType::Ident, required: false, expr_ok: true },
        KeyRule { name: "label", ty: ExpectedType::Str, required: false, expr_ok: true },
        KeyRule { name: "icon", ty: ExpectedType::Icon, required: false, expr_ok: true },
    ],
    accepts_children: true,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_TOGGLE: &UiNode = &UiNode {
    kind: "toggle",
    keys: &[
        KeyRule { name: "value", ty: ExpectedType::Bool, required: false, expr_ok: true },
        KeyRule { name: "on_change", ty: ExpectedType::Ident, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_SLIDER: &UiNode = &UiNode {
    kind: "slider",
    keys: &[
        KeyRule { name: "value", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "min", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "max", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule { name: "step", ty: ExpectedType::Number, required: false, expr_ok: true },
        KeyRule {
            name: "direction",
            ty: ExpectedType::OneOf(&["horizontal", "vertical"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "on_change", ty: ExpectedType::Ident, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_SELECT: &UiNode = &UiNode {
    kind: "select",
    keys: &[
        KeyRule { name: "options", ty: ExpectedType::Any, required: false, expr_ok: true },
        KeyRule { name: "value", ty: ExpectedType::Any, required: false, expr_ok: true },
        KeyRule {
            name: "style",
            ty: ExpectedType::OneOf(&["dropdown", "roller", "buttons"]),
            required: false,
            expr_ok: false,
        },
        KeyRule { name: "on_change", ty: ExpectedType::Ident, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

const UI_KEYPAD: &UiNode = &UiNode {
    kind: "keypad",
    keys: &[
        KeyRule { name: "length", ty: ExpectedType::Int, required: false, expr_ok: false },
        KeyRule { name: "masked", ty: ExpectedType::Bool, required: false, expr_ok: false },
        KeyRule { name: "on_submit", ty: ExpectedType::Ident, required: false, expr_ok: true },
    ],
    accepts_children: false,
    nested_subblocks: &[],
    slot_only: false,
};

fn ui_node_schema(kind: &str) -> Option<&'static UiNode> {
    Some(match kind {
        "row" => UI_ROW,
        "column" => UI_COLUMN,
        "grid" => UI_GRID,
        "stack" => UI_STACK,
        "scroll" => UI_SCROLL,
        "slot" => UI_SLOT,
        "text" => UI_TEXT,
        "icon" => UI_ICON,
        "image" => UI_IMAGE,
        "rect" => UI_RECT,
        "arc" => UI_ARC,
        "bar" => UI_BAR,
        "line" => UI_LINE,
        "chart" => UI_CHART,
        "qr" => UI_QR,
        "spinner" => UI_SPINNER,
        "button" => UI_BUTTON,
        "toggle" => UI_TOGGLE,
        "slider" => UI_SLIDER,
        "select" => UI_SELECT,
        "keypad" => UI_KEYPAD,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The built-in theme tokens and their defaults (schema section 11),
/// under every theme. The daemon starts its theme map from these.
pub const BUILTIN_THEME_TOKENS: &[(&str, &str)] = &[
    ("background", "#f5f5f5"),
    ("surface", "#ffffff"),
    ("text", "#212121"),
    ("muted", "#757575"),
    ("accent", "#2196f3"),
    ("ok", "#2e7d32"),
    ("info", "#1976d2"),
    ("warning", "#ef6c00"),
    ("critical", "#d32f2f"),
    ("font_small", "roboto 12"),
    ("font", "roboto 16"),
    ("font_big", "roboto 24"),
    ("font_huge", "roboto 32"),
    ("gap", "8"),
    ("padding", "12"),
    ("radius", "8"),
    ("tap_min", "48"),
    ("shadow", "none"),
    ("icons", "builtin"),
    ("motion", "reduced"),
];

/// A theme's tokens as written (built-ins under it), by name.
type TokenMap = HashMap<String, String>;

thread_local! {
    /// Where a bare color name is looked up while checking: the token
    /// maps of the active theme's chain (one map), or of every theme
    /// when the active one is not known. A name is a color when it
    /// resolves to a hex in any of them.
    static THEME_MAPS: std::cell::RefCell<Vec<TokenMap>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `f` with `maps` as the color-name lookup.
fn with_theme_maps<T>(maps: Vec<TokenMap>, f: impl FnOnce() -> T) -> T {
    let before = THEME_MAPS.with(|t| t.replace(maps));
    let out = f();
    THEME_MAPS.with(|t| t.replace(before));
    out
}

/// The named `theme` blocks of `applied` (overrides folded in).
fn theme_blocks(applied: &Document) -> Vec<Block> {
    applied
        .items
        .iter()
        .cloned()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "theme" && b.name.is_some() => Some(b),
            _ => None,
        })
        .collect()
}

/// Built-ins, then `name`'s `extends` chain from its base down, the
/// child winning (cycle-safe).
fn chain_tokens(themes: &[Block], name: &str) -> TokenMap {
    let chain = theme_chain(themes, name);
    let mut out: TokenMap = BUILTIN_THEME_TOKENS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for t in chain.iter().rev() {
        for it in &t.items {
            if let Item::Assign(a) = it {
                if let AssignTarget::Simple { key } = &a.target {
                    if key.text != "extends" {
                        out.insert(key.text.clone(), a.value.text.trim().to_string());
                    }
                }
            }
        }
    }
    out
}

/// Theme `name` and the themes it extends, nearest first (cycle-safe).
fn theme_chain<'a>(themes: &'a [Block], name: &str) -> Vec<&'a Block> {
    let mut chain: Vec<&Block> = Vec::new();
    let mut next = Some(name.to_string());
    while let Some(n) = next.take() {
        let Some(t) = themes.iter().find(|t| t.name.as_ref().is_some_and(|x| x.text == n)) else { break };
        if chain.iter().any(|c| std::ptr::eq(*c, t)) {
            break;
        }
        chain.push(t);
        next = t.items.iter().find_map(|it| match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "extends") => {
                Some(a.value.text.trim().to_string())
            }
            _ => None,
        });
    }
    chain
}

/// The maps a page's color names resolve in: the active theme's chain
/// (`screen:theme`, else the first theme), or every theme's when the
/// active one is not defined.
fn active_theme_maps(applied: &Document, themes: &[Block]) -> Vec<TokenMap> {
    let configured = applied.items.iter().find_map(|it| match it {
        Item::Block(b) if b.kind.text == "screen" && b.name.is_none() => b.items.iter().find_map(|i| match i {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "theme") => {
                Some(a.value.text.trim().to_string())
            }
            _ => None,
        }),
        _ => None,
    });
    let active = configured.or_else(|| themes.first().and_then(|t| t.name.as_ref()).map(|n| n.text.clone()));
    match active {
        Some(a) if themes.iter().any(|t| t.name.as_ref().is_some_and(|n| n.text == a)) => vec![chain_tokens(themes, &a)],
        _ if themes.is_empty() => vec![chain_tokens(themes, "")],
        _ => themes.iter().filter_map(|t| t.name.as_ref()).map(|n| chain_tokens(themes, &n.text)).collect(),
    }
}

/// Follow a bare color name through `map` to a hex (cycle-safe).
fn resolve_color_name(name: &str, map: &TokenMap) -> Result<(), String> {
    let mut path: Vec<&str> = Vec::new();
    let mut cur = name;
    loop {
        if path.contains(&cur) {
            path.push(cur);
            return Err(format!("theme color `{name}` refers to itself: {}", path.join(" -> ")));
        }
        path.push(cur);
        let Some(v) = map.get(cur) else {
            return Err(format!("unknown theme color `{cur}`: not a built-in token or one the active theme defines"));
        };
        let v = v.trim();
        if parse_color(v).is_some() || v.contains("{{") {
            return Ok(());
        }
        // Another token's name: follow it.
        if map.contains_key(v) {
            cur = v;
            continue;
        }
        return Err(format!("theme token `{cur}` is `{v}`, not a color"));
    }
}

/// A bare name is a color when it resolves in one of the current maps.
fn check_color_name(name: &str) -> Result<(), String> {
    THEME_MAPS.with(|m| {
        let maps = m.borrow();
        let mut first = None;
        for map in maps.iter() {
            match resolve_color_name(name, map) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    first.get_or_insert(e);
                }
            }
        }
        Err(first.unwrap_or_else(|| format!("unknown theme color `{name}`")))
    })
}

/// A literal icon name is a built-in, unless the active theme has an
/// `icons` folder (a file there may add it; the renderer skips a name
/// found in neither).
fn check_icon_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if crate::icons::glyph(name).is_some() {
        return Ok(());
    }
    let folder = THEME_MAPS.with(|m| {
        m.borrow().iter().any(|map| map.get("icons").is_some_and(|v| !v.trim().is_empty() && v.trim() != "builtin"))
    });
    if folder {
        return Ok(());
    }
    let hint = match crate::icons::suggest(name) {
        Some(s) => format!("; did you mean `{s}`?"),
        None => String::new(),
    };
    Err(format!("unknown icon `{name}`: not a built-in icon (schema 8.3) and the theme sets no `icons` folder{hint}"))
}

/// `display` `width` x `height` as written, default 800x480.
fn display_pixels(doc: &Document) -> u64 {
    let mut size = (800u64, 480u64);
    for it in &doc.items {
        let Item::Block(b) = it else { continue };
        if b.kind.text != "display" {
            continue;
        }
        for i in &b.items {
            if let Item::Assign(a) = i {
                if let AssignTarget::Simple { key } = &a.target {
                    if let Ok(n) = a.value.text.trim().parse::<u64>() {
                        match key.text.as_str() {
                            "width" => size.0 = n,
                            "height" => size.1 = n,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    size.0 * size.1
}

/// A literal `background` (schema 8.1). A bare name is a theme color,
/// or a token holding another background (a gradient, an image).
fn check_background(text: &str) -> Result<(), String> {
    use crate::paint::{parse_background, Background};
    match parse_background(text)? {
        Background::None => Ok(()),
        Background::Color(c) if c.starts_with('#') => Ok(()),
        Background::Color(c) => {
            let other = THEME_MAPS.with(|m| {
                m.borrow().iter().any(|map| {
                    map.get(c).is_some_and(|v| {
                        matches!(parse_background(v), Ok(Background::Gradient(_) | Background::Image { .. } | Background::None))
                    })
                })
            });
            if other {
                Ok(())
            } else {
                check_color_name(c)
            }
        }
        Background::Gradient(g) => {
            for stop in g.stops {
                if !stop.starts_with('#') {
                    check_color_name(stop)?;
                }
            }
            Ok(())
        }
        Background::Image { path, .. } => check_image(path, "background image"),
    }
}

/// A literal `shadow`: its color a hex or a theme color; a bare name a
/// theme token holding a shadow.
fn check_shadow(text: &str) -> Result<(), String> {
    use crate::paint::{parse_shadow, ShadowValue};
    match parse_shadow(text)? {
        ShadowValue::None => Ok(()),
        ShadowValue::Shadow(sh) if sh.color.starts_with('#') => Ok(()),
        ShadowValue::Shadow(sh) => check_color_name(sh.color),
        ShadowValue::Token(name) => {
            let mut first = None;
            let found = THEME_MAPS.with(|m| {
                m.borrow().iter().any(|map| match map.get(name).map(|v| parse_shadow(v)) {
                    Some(Ok(ShadowValue::Token(_))) | None => false,
                    Some(Ok(_)) => true,
                    Some(Err(e)) => {
                        first.get_or_insert(format!("theme token `{name}`: {e}"));
                        false
                    }
                })
            });
            match (found, first) {
                (true, _) => Ok(()),
                (false, Some(e)) => Err(e),
                (false, None) => Err(format!("unknown theme shadow `{name}`: not a token holding a shadow")),
            }
        }
    }
}

/// An image file (relative to the config folder, or a theme folder's,
/// rewritten to `themes/<name>/...`) exists inside its folder and is a
/// PNG.
fn check_image(path: &str, what: &str) -> Result<(), String> {
    use std::io::Read;
    let path = path.trim();
    if !path.to_ascii_lowercase().ends_with(".png") {
        return Err(format!("{what} `{path}` must be a `.png` file"));
    }
    let Some(dir) = CONFIG_DIR.with(|d| d.borrow().clone()) else { return Ok(()) };
    let full = crate::assets::resolve(&dir, path, what)?;
    let mut head = [0u8; 24];
    let png = std::fs::File::open(&full).and_then(|mut f| f.read_exact(&mut head)).is_ok()
        && &head[..8] == b"\x89PNG\r\n\x1a\n"
        && &head[12..16] == b"IHDR";
    if !png {
        return Err(format!("{what} `{path}` is not a PNG file"));
    }
    let be = |b: &[u8]| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let (w, h) = (be(&head[16..20]), be(&head[20..24]));
    let screen = DISPLAY_PIXELS.with(|d| d.get());
    let max = crate::paint::max_image_pixels(screen);
    if w * h > max {
        return Err(format!(
            "{what} `{path}` is {w}x{h}: more than {max} pixels (twice the screen's, at most {} MiB decoded) is not drawn",
            crate::paint::IMAGE_CACHE_BYTES >> 20
        ));
    }
    Ok(())
}

/// A literal font value: a theme font token, or `<family> <size>
/// [bold]` whose `.ttf` file (relative to the config folder) exists.
fn check_font(text: &str) -> Result<(), String> {
    let text = text.trim();
    if crate::fonts::TOKENS.contains(&text) {
        return Ok(());
    }
    let font = crate::fonts::parse(text)?;
    if let crate::fonts::Family::File(path) = &font.family {
        let Some(dir) = CONFIG_DIR.with(|d| d.borrow().clone()) else { return Ok(()) };
        let full = crate::fonts::resolve_file(&dir, path)?;
        if std::fs::metadata(&full).map_or(true, |m| m.len() > crate::fonts::MAX_FILE_BYTES) {
            return Err(format!("font file `{path}` is larger than {} MiB", crate::fonts::MAX_FILE_BYTES >> 20));
        }
    }
    Ok(())
}

pub fn check(doc: &Document, source: &SourceMap) -> Vec<Diagnostic> {
    CONFIG_DIR.with(|d| *d.borrow_mut() = source.root_dir().map(|p| p.to_path_buf()));
    let mut applied = doc.clone();
    crate::apply_overrides(&mut applied);
    DISPLAY_PIXELS.with(|d| d.set(display_pixels(&applied)));
    let themes = theme_blocks(&applied);
    let maps = active_theme_maps(&applied, &themes);
    THEME_BLOCKS.with(|t| t.replace(themes));
    let diags = with_theme_maps(maps, || check_inner(doc, source));
    THEME_BLOCKS.with(|t| t.borrow_mut().clear());
    diags
}

thread_local! {
    /// The root config's folder while a document is checked: a font
    /// file is looked up there.
    static CONFIG_DIR: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    /// The screen's pixels while a document is checked (`display`
    /// `width` x `height`, default 800x480): the image size limit.
    static DISPLAY_PIXELS: std::cell::Cell<u64> = const { std::cell::Cell::new(800 * 480) };
    /// The document's themes while it is checked: a theme's own tokens
    /// resolve in its own chain.
    static THEME_BLOCKS: std::cell::RefCell<Vec<Block>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn check_inner(doc: &Document, source: &SourceMap) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let mut seen: HashMap<(String, String), Span> = HashMap::new();

    // Pre-passes: collect the names of user-defined `component`s so that
    // calls to them inside UI trees can be recognized, the name tables
    // used by the Phase 7 cross-reference resolver, and the per-kind
    // param tables used by the Phase 8 arg type-checker.
    let user_components = collect_user_components(doc);
    let names = collect_names(doc);
    let mut params = collect_params(doc, source, &mut diags);
    check_theme_component_params(doc, source, &mut params, &mut diags);

    let ctx = UiContext {
        user_components: &user_components,
        names: &names,
        params: &params,
        source,
    };

    for item in &doc.items {
        check_top_item(item, source, &ctx, &mut seen, &mut diags);
    }
    // Remotes as they will run: `remote[x]:host` and
    // `remote_kind[k]:url` overrides applied, so an override gets the
    // same checks as the key written in the block.
    let mut applied = doc.clone();
    crate::apply_overrides(&mut applied);
    for item in &applied.items {
        let Item::Block(b) = item else { continue };
        match b.kind.text.as_str() {
            "remote_kind" => remote::check_kind(b, source, &mut diags),
            "remote" if b.name.is_some() => remote::check_instance(b, &applied, source, &mut diags),
            _ => {}
        }
    }

    resolve_refs(doc, &names, &mut diags);
    check_theme_only_calls(doc, &applied, &mut diags);
    check_ssh_runs(doc, &mut diags);
    remote::check_self_use(doc, &mut diags);
    check_override_targets(doc, &mut diags);
    check_reminder_schedules(doc, &mut diags);

    // Phase 10: parse every `{{ ... }}` fragment as CEL, surface parse
    // errors, and cross-reference `sources.<name>` against the collected
    // source names.
    check_cel_in_doc(doc, &names, &mut diags);

    diags
}

/// `kind[name]:key = value` overrides one property of an existing
/// named item (config-format, "Blocks"). An override whose item is
/// not defined would otherwise be dropped silently: for
/// `action[x]:pin = true` that means a misspelled name leaves the
/// real action without its PIN.
fn check_override_targets(doc: &Document, diags: &mut Vec<Diagnostic>) {
    let mut defined: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    for item in &doc.items {
        if let Item::Block(b) = item {
            if let Some(name) = &b.name {
                defined.insert((b.kind.text.as_str(), name.text.as_str()));
            }
        }
    }
    for item in &doc.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Indexed { kind, name, .. } = &a.target else {
            continue;
        };
        if !matches!(classify_kind(&kind.text), Some(KindKind::Named)) {
            continue;
        }
        if !defined.contains(&(kind.text.as_str(), name.text.as_str())) {
            diags.push(Diagnostic::new(
                name.span,
                format!(
                    "`{0}[{1}]` overrides an item that is not defined (no `{0} {1} {{ }}`)",
                    kind.text, name.text
                ),
            ));
        }
    }
}

/// True if `cmd` runs a shell or an interpreter with a script given
/// inline (`sh -c`, `bash -ec`, `python3 -c`, `perl -e`, also behind
/// `env`, `sudo`, `timeout` and the like) and that script holds a
/// `{{ }}`. Values must reach commands as argv items or env vars,
/// never as code (requirements, "Inputs").
fn shell_script_interpolates(cmd: &str) -> bool {
    /// A program that takes a script inline: its script options (any
    /// of these letters in a cluster like `-ec`), the options that
    /// take the next word as their argument, and whether other
    /// arguments may come before the script option (`su user -c`).
    struct Runner {
        name: &'static str,
        script: &'static str,
        takes_arg: &'static [&'static str],
        args_first: bool,
    }
    const SHELL_ARGS: &[&str] = &["-o", "+o", "-O", "+O", "--rcfile", "--init-file"];
    const RUNNERS: &[Runner] = &[
        Runner { name: "sh", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "bash", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "dash", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "ash", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "zsh", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "ksh", script: "c", takes_arg: SHELL_ARGS, args_first: false },
        Runner { name: "python", script: "c", takes_arg: &["-W", "-X"], args_first: false },
        Runner { name: "python3", script: "c", takes_arg: &["-W", "-X"], args_first: false },
        Runner { name: "perl", script: "eE", takes_arg: &[], args_first: false },
        Runner { name: "ruby", script: "e", takes_arg: &["-I", "-r"], args_first: false },
        Runner { name: "node", script: "ep", takes_arg: &["-r", "--require"], args_first: false },
        Runner { name: "php", script: "r", takes_arg: &["-d", "-c"], args_first: false },
        Runner { name: "su", script: "c", takes_arg: &["-s", "-g", "-G", "-w"], args_first: true },
    ];
    const WRAPPERS: &[&str] = &[
        "env", "sudo", "doas", "nice", "ionice", "timeout", "nohup", "stdbuf", "chrt", "setsid", "busybox", "exec",
        "flock", "xargs", "docker", "podman", "kubectl", "chroot", "runuser", "systemd-run",
    ];
    let words = split_words(cmd);
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_string();
    // By its name, or an absolute path to it (`deploy/python`, a
    // kubectl target, is neither).
    let runner_of = |w: &str| RUNNERS.iter().find(|r| w == r.name || (w.starts_with('/') && base(w) == r.name));
    let mut i = 0;
    // Past wrappers and their own options and arguments, up to the
    // first word that names a runner.
    while i < words.len() && WRAPPERS.contains(&base(&words[i]).as_str()) {
        let env = base(&words[i]) == "env";
        i += 1;
        while i < words.len() && runner_of(&words[i]).is_none() {
            let w = &words[i];
            // `env -S "sh -c '...'"`: the command is one word.
            if env && (w == "-S" || w == "--split-string") {
                return words.get(i + 1).is_some_and(|line| shell_script_interpolates(line));
            }
            let skippable = w.starts_with('-')
                || w.contains('=')
                || w.contains("{{")
                || w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@'));
            if !skippable {
                break;
            }
            i += 1;
        }
    }
    let Some(runner) = words.get(i).and_then(|w| runner_of(w)) else {
        return false;
    };
    let mut j = i + 1;
    while j < words.len() && words[j] != "--" {
        let w = &words[j];
        j += 1;
        if !(w.starts_with('-') || w.starts_with('+')) {
            if runner.args_first {
                continue;
            }
            break;
        }
        // `-c`, `-ec`, `-lc`: a cluster of letters (not `--long`, not
        // `-dfoo=bar`).
        let cluster = !w.starts_with("--") && w.len() > 1 && w[1..].chars().all(|c| c.is_ascii_alphabetic());
        if cluster && w[1..].chars().any(|c| runner.script.contains(c)) {
            return words.get(j).is_some_and(|script| script.contains("{{"));
        }
        if runner.takes_arg.contains(&w.as_str()) {
            // `-o pipefail`, `-W ignore`: skip the option's argument.
            j += 1;
        }
    }
    false
}

/// Where a `{{ }}` sits in a hand-written ssh command line.
#[derive(Debug, PartialEq)]
enum SshSlot {
    /// In the remote command: ssh joins it into one line for the
    /// remote shell, so the value is code there.
    RemoteCommand,
    /// In ssh's options or its host before `--`: a value starting with
    /// `-` is read as an option (`-oProxyCommand=...` runs a command
    /// on the panel).
    OptionOrHost,
}

/// The words of `cmd` holding `{{ }}` that ssh would treat as shell
/// code or options, when `cmd` runs `ssh` or `autossh` (also behind
/// `sudo`, `timeout`, `sshpass`, `flock <file>`, ...).
fn ssh_risks(cmd: &str) -> Vec<(SshSlot, String)> {
    /// ssh (and autossh) options that take an argument.
    const TAKES_ARG: &str = "BbcDEeFIiJLlmMOoPpQRSWw";
    const WRAPPERS: &[&str] = &[
        "env", "sudo", "doas", "nice", "ionice", "timeout", "nohup", "stdbuf", "setsid", "exec", "flock", "command",
        "xargs", "runuser", "chpst", "sshpass",
    ];
    let words = split_words(cmd);
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_string();
    let is_ssh = |w: &str| matches!(base(w).as_str(), "ssh" | "autossh");
    // ssh is the program, or comes after a wrapper (with whatever
    // options and arguments the wrapper takes in between).
    let Some(at) = words.iter().position(|w| is_ssh(w)) else { return Vec::new() };
    if at > 0 && !WRAPPERS.contains(&base(&words[0]).as_str()) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut j = at + 1;
    while j < words.len() {
        let w = &words[j];
        j += 1;
        if w == "--" {
            j += 1; // the host: past `--`, never an option
            break;
        }
        if w.starts_with('-') && w.len() > 1 {
            if w.contains("{{") {
                out.push((SshSlot::OptionOrHost, w.clone()));
            }
            // `-vp 22`: the letter that takes an argument takes the
            // rest of the word, or the next word.
            let letters = &w[1..];
            if let Some(k) = letters.find(|c| TAKES_ARG.contains(c)) {
                if k + 1 == letters.len() {
                    if let Some(arg) = words.get(j) {
                        if arg.contains("{{") {
                            out.push((SshSlot::OptionOrHost, arg.clone()));
                        }
                    }
                    j += 1;
                }
            }
            continue;
        }
        if w.contains("{{") {
            out.push((SshSlot::OptionOrHost, w.clone()));
        }
        break; // the host
    }
    for w in words.iter().skip(j) {
        if w.contains("{{") {
            out.push((SshSlot::RemoteCommand, w.clone()));
        }
    }
    out
}

/// Every `run` in `doc` that is a hand-written ssh command with a
/// value where ssh would read it as code or an option. A value that
/// cannot carry either is fine: built from params typed int, number,
/// bool or duration, or held to `options` or a `pattern`.
fn check_ssh_runs(doc: &Document, diags: &mut Vec<Diagnostic>) {
    fn safe_params(b: &Block) -> std::collections::HashSet<String> {
        b.items
            .iter()
            .filter_map(|it| match it {
                Item::Param(p)
                    if matches!(p.ty.text.as_str(), "int" | "number" | "bool" | "duration")
                        || p.options.as_ref().is_some_and(|o| {
                            o.items.iter().any(|i| matches!(i, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "options" || key.text == "pattern")))
                        }) =>
                {
                    Some(p.name.text.clone())
                }
                _ => None,
            })
            .collect()
    }
    fn walk(items: &[Item], safe: &std::collections::HashSet<String>, diags: &mut Vec<Diagnostic>) {
        for it in items {
            match it {
                Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "run") && a.value.text.contains("{{") => {
                    for (slot, word) in ssh_risks(&a.value.text) {
                        let (exprs, _) = crate::cel::parse_interp(&word, a.value.span.file, a.value.span.start);
                        let names: Vec<String> = exprs.iter().flat_map(crate::cel::idents).map(|(n, _)| n).collect();
                        if !names.is_empty() && names.iter().all(|n| safe.contains(n)) {
                            continue;
                        }
                        diags.push(Diagnostic::new(
                            a.value.span,
                            match slot {
                                SshSlot::RemoteCommand => format!(
                                    "`{word}` in the command of an `ssh` is shell code on the far side: ssh hands it to the remote shell. \
                                     Use a `remote_kind` with `via = ssh`, which quotes each argument (schema 17), or hold the value to \
                                     a type, `options` or a `pattern`"
                                ),
                                SshSlot::OptionOrHost => format!(
                                    "`{word}` in ssh's options or host could be read as an option (`-oProxyCommand=...` runs a command \
                                     here); put `--` before the host, or hold the value to a type, `options` or a `pattern`"
                                ),
                            },
                        ));
                    }
                }
                Item::Block(b) => {
                    let mut inner = safe.clone();
                    inner.extend(safe_params(b));
                    walk(&b.items, &inner, diags);
                }
                Item::Repeat(r) => walk(&r.items, safe, diags),
                _ => {}
            }
        }
    }
    walk(&doc.items, &Default::default(), diags);
}

/// `cmd` split into words the way the runner does (quotes removed,
/// `{{ }}` kept whole); good enough to find a script argument.
fn split_words(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;
    let b = cmd.as_bytes();
    while i < b.len() {
        let c = cmd[i..].chars().next().unwrap_or(' ');
        if cmd[i..].starts_with("{{") {
            let end = crate::cel::expr_end(cmd, i).unwrap_or(cmd.len());
            cur.push_str(&cmd[i..end]);
            in_word = true;
            i = end;
            continue;
        }
        i += c.len_utf8();
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' | '"' => {
                in_word = true;
                while i < b.len() {
                    if cmd[i..].starts_with("{{") {
                        let end = crate::cel::expr_end(cmd, i).unwrap_or(cmd.len());
                        cur.push_str(&cmd[i..end]);
                        i = end;
                        continue;
                    }
                    let c2 = cmd[i..].chars().next().unwrap_or(c);
                    i += c2.len_utf8();
                    if c2 == c {
                        break;
                    }
                    if c == '"' && c2 == '\\' && i < b.len() {
                        let n = cmd[i..].chars().next().unwrap_or(' ');
                        cur.push(n);
                        i += n.len_utf8();
                        continue;
                    }
                    cur.push(c2);
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// Alert rule names the daemon uses for its own alerts: ctl, reminders
/// and the source watchdog. A config alert with one of them would
/// share their ids.
const RESERVED_ALERT_NAMES: &[&str] = &["ctl", "reminder", "source_down"];

/// A reminder needs exactly one of `every` and `once` (schema 10);
/// `start` only makes sense with `every`.
fn check_reminder_schedules(doc: &Document, diags: &mut Vec<Diagnostic>) {
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text == "alert" {
            if let Some(name) = b.name.as_ref().filter(|n| RESERVED_ALERT_NAMES.contains(&n.text.as_str())) {
                diags.push(Diagnostic::new(
                    name.span,
                    format!("alert name `{}` is reserved for the panel's own alerts", name.text),
                ));
            }
            continue;
        }
        if b.kind.text != "reminder" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let has = |k: &str| {
            b.items.iter().any(|i| {
                matches!(i, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == k))
            })
        };
        let msg = match (has("every"), has("once")) {
            (true, true) => "has both `every` and `once`; keep one",
            (false, false) => "needs `every` (repeating) or `once` (a date)",
            (false, true) if has("start") => "`start` only applies with `every`",
            _ => continue,
        };
        diags.push(Diagnostic::new(name.span, format!("reminder `{}` {msg}", name.text)));
    }
}

fn collect_user_components(doc: &Document) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "component" {
                if let Some(name) = &b.name {
                    set.insert(name.text.clone());
                }
            }
        }
    }
    // A component only a theme defines can be called too.
    for (_, b) in theme_components(doc) {
        if let Some(name) = &b.name {
            set.insert(name.text.clone());
        }
    }
    set
}

/// The `component` blocks inside `theme` blocks, with their theme's name.
fn theme_components(doc: &Document) -> Vec<(&str, &Block)> {
    let mut out = Vec::new();
    for item in &doc.items {
        let Item::Block(t) = item else { continue };
        let (true, Some(theme)) = (t.kind.text == "theme", &t.name) else { continue };
        for it in &t.items {
            if let Item::Block(c) = it {
                if c.kind.text == "component" && c.name.is_some() {
                    out.push((theme.text.as_str(), c));
                }
            }
        }
    }
    out
}

/// A component's params as a call sees them: name, type and whether
/// it may be left out, by name.
fn component_signature(b: &Block) -> Vec<(String, String, bool)> {
    let mut sig: Vec<(String, String, bool)> = b
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) => Some((
                p.name.text.clone(),
                p.ty.text.clone(),
                p.default.is_some() || p.options.as_ref().is_some_and(options_has_ask),
            )),
            _ => None,
        })
        .collect();
    sig.sort();
    sig
}

fn signature_text(sig: &[(String, String, bool)]) -> String {
    if sig.is_empty() {
        return "no params".to_string();
    }
    sig.iter()
        .map(|(n, t, opt)| format!("`{n}: {t}{}`", if *opt { " = ..." } else { "" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A theme's component takes the same params (name, type, optional or
/// not) as the one it replaces, so a call checked once fits every
/// theme: the config's or library's component of that name, else the
/// first theme that defines it. Also: the params of a component only
/// themes define are the ones calls are checked against.
fn check_theme_component_params(doc: &Document, source: &SourceMap, params: &mut Params, diags: &mut Vec<Diagnostic>) {
    let top: HashMap<&str, &Block> = doc
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "component" => b.name.as_ref().map(|n| (n.text.as_str(), b)),
            _ => None,
        })
        .collect();
    let mut first: HashMap<&str, (&str, &Block)> = HashMap::new();
    for (theme, b) in theme_components(doc) {
        let Some(name) = &b.name else { continue };
        let specs = collect_params_in_body(b, "component", source, diags);
        let (base, what) = match (top.get(name.text.as_str()), first.get(name.text.as_str())) {
            (Some(base), _) => (*base, "the component it replaces".to_string()),
            (None, Some((t, base))) => (*base, format!("theme `{t}`'s `{}`", name.text)),
            (None, None) => {
                first.insert(name.text.as_str(), (theme, b));
                params.components.insert(name.text.clone(), specs);
                continue;
            }
        };
        let (want, got) = (component_signature(base), component_signature(b));
        if want != got {
            diags.push(Diagnostic::new(
                name.span,
                format!(
                    "theme `{theme}`'s component `{}` takes {}, {what} takes {}; a theme's component takes the same params (name, type, optional or not)",
                    name.text,
                    signature_text(&got),
                    signature_text(&want),
                ),
            ));
        }
    }
}

/// The themes that can be active: `screen:theme` (else the first
/// theme) and the night theme.
fn themes_in_use(applied: &Document) -> Vec<String> {
    fn theme_of(items: &[Item]) -> Option<String> {
        items.iter().find_map(|i| match i {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "theme") => {
                Some(a.value.text.trim().to_string())
            }
            _ => None,
        })
    }
    let mut out = Vec::new();
    let mut configured = None;
    for it in &applied.items {
        let Item::Block(b) = it else { continue };
        if b.kind.text != "screen" || b.name.is_some() {
            continue;
        }
        configured = configured.or_else(|| theme_of(&b.items));
        for i in &b.items {
            if let Item::Block(n) = i {
                if n.kind.text == "night" {
                    out.extend(theme_of(&n.items));
                }
            }
        }
    }
    let first = applied.items.iter().find_map(|it| match it {
        Item::Block(b) if b.kind.text == "theme" => b.name.as_ref().map(|n| n.text.clone()),
        _ => None,
    });
    out.extend(configured.or(first));
    out.retain(|t| !t.contains("{{"));
    out
}

/// A component only themes define, called from a page or a config
/// component: every theme that can be active defines or inherits it,
/// else its pages would miss a part under that theme.
fn check_theme_only_calls(doc: &Document, applied: &Document, diags: &mut Vec<Diagnostic>) {
    for t in themes_in_use(applied) {
        for (name, span) in theme_only_calls_missing(doc, applied, &t) {
            diags.push(Diagnostic::new(
                span,
                format!("component `{name}` is only defined in themes, and theme `{t}` (in use) neither defines nor inherits it"),
            ));
        }
    }
}

/// The calls (component name, where) from pages and config components
/// to a component only themes define that theme `theme` neither
/// defines nor inherits.
fn theme_only_calls_missing(doc: &Document, applied: &Document, theme: &str) -> Vec<(String, Span)> {
    let top: std::collections::HashSet<&str> = doc
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "component" => b.name.as_ref().map(|n| n.text.as_str()),
            _ => None,
        })
        .collect();
    let only_themes: std::collections::HashSet<&str> = theme_components(doc)
        .into_iter()
        .filter_map(|(_, b)| b.name.as_ref().map(|n| n.text.as_str()))
        .filter(|n| !top.contains(n))
        .collect();
    let mut out = Vec::new();
    if only_themes.is_empty() {
        return out;
    }
    let themes = theme_blocks(applied);
    let chain = theme_chain(&themes, theme);
    let provides = |comp: &str| {
        chain.iter().any(|t| {
            t.items.iter().any(|it| {
                matches!(it, Item::Block(c) if c.kind.text == "component" && c.name.as_ref().is_some_and(|n| n.text == comp))
            })
        })
    };
    fn walk(items: &[Item], f: &mut dyn FnMut(&Block)) {
        for it in items {
            match it {
                Item::Block(b) => {
                    f(b);
                    walk(&b.items, f);
                }
                Item::Repeat(r) => walk(&r.items, f),
                _ => {}
            }
        }
    }
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if !matches!(b.kind.text.as_str(), "page" | "component") {
            continue;
        }
        walk(&b.items, &mut |call| {
            let name = call.kind.text.as_str();
            if only_themes.contains(name) && !provides(name) {
                out.push((name.to_string(), call.kind.span));
            }
        });
    }
    out
}

/// Whether the pages of `doc` (a checked config) can be drawn under
/// theme `theme`, for a switch at run time (`paneld ctl theme`): the
/// theme exists, and it has every component only themes define that
/// a page calls. The error says what is missing.
pub fn check_theme_switch(doc: &Document, theme: &str) -> Result<(), String> {
    let mut applied = doc.clone();
    crate::apply_overrides(&mut applied);
    let themes = theme_blocks(&applied);
    if !themes.iter().any(|t| t.name.as_ref().is_some_and(|n| n.text == theme)) {
        let mut known: Vec<&str> = themes.iter().filter_map(|t| t.name.as_ref().map(|n| n.text.as_str())).collect();
        known.sort_unstable();
        return Err(format!("no theme `{theme}` (themes: {})", known.join(", ")));
    }
    match theme_only_calls_missing(doc, &applied, theme).first() {
        Some((name, _)) => Err(format!("theme `{theme}` neither defines nor inherits component `{name}`, which the pages use")),
        None => Ok(()),
    }
}

struct UiContext<'a> {
    user_components: &'a std::collections::HashSet<String>,
    names: &'a Names,
    params: &'a Params,
    source: &'a SourceMap,
}

fn check_top_item(
    item: &Item,
    source: &SourceMap,
    ctx: &UiContext,
    seen: &mut HashMap<(String, String), Span>,
    diags: &mut Vec<Diagnostic>,
) {
    match item {
        Item::Block(b) => check_top_block(b, source, ctx, seen, diags),
        Item::Assign(a) => check_top_assign(a, diags),
        Item::Var(v) => {
            // Top-level `var name: type = default`: name/type/default
            // all live inside the AST node. Default-type agreement is
            // Phase 5+ work (needs the type-dispatch table). A dotted
            // name could not be read as `vars.<name>`.
            if v.name.text.contains('.') {
                diags.push(Diagnostic::new(v.name.span, "a `var` name cannot contain `.`"));
            }
        }
        Item::Param(p) => diags.push(Diagnostic::new(
            p.span,
            "`param` is only allowed inside `action` or `component` bodies",
        )),
        Item::Repeat(r) => diags.push(Diagnostic::new(
            r.span,
            "`repeat` is only allowed inside a page or component body",
        )),
    }
}

fn check_top_block(
    b: &Block,
    source: &SourceMap,
    ctx: &UiContext,
    seen: &mut HashMap<(String, String), Span>,
    diags: &mut Vec<Diagnostic>,
) {
    match classify_kind(&b.kind.text) {
        Some(KindKind::Named) => {
            let Some(name) = &b.name else {
                diags.push(Diagnostic::new(
                    b.kind.span,
                    format!("`{}` needs a name", b.kind.text),
                ));
                return;
            };
            // Dotted names are remote members (`device1.temp`).
            if name.text.contains('.') {
                diags.push(Diagnostic::new(
                    name.span,
                    format!("a `{}` name cannot contain `.`", b.kind.text),
                ));
            }
            let key = (b.kind.text.clone(), name.text.clone());
            if let Some(prev_span) = seen.get(&key) {
                diags.push(Diagnostic::new(
                    name.span,
                    format!(
                        "`{} {}` is already defined (first at {})",
                        b.kind.text,
                        name.text,
                        source.location(*prev_span)
                    ),
                ));
            } else {
                seen.insert(key, name.span);
            }
        }
        Some(KindKind::Category) => {
            if let Some(name) = &b.name {
                diags.push(Diagnostic::new(
                    name.span,
                    format!(
                        "`{}` is a settings block and does not take a name",
                        b.kind.text
                    ),
                ));
            }
        }
        None => {
            diags.push(Diagnostic::new(
                b.kind.span,
                format!("unknown top-level item `{}`", b.kind.text),
            ));
            return;
        }
    }

    if let Some(of) = &b.of {
        if b.kind.text != "remote" {
            diags.push(Diagnostic::new(
                of.span,
                format!("`{}: <kind>` is for `remote` only", b.name.as_ref().map_or("", |n| n.text.as_str())),
            ));
        }
    }

    // Per-key validation of the body, if we have a schema for this kind.
    if matches!(b.kind.text.as_str(), "remote_kind" | "remote") {
        // Checked with their overrides applied: see `check`.
    } else if let Some(schema) = top_schema_for(&b.kind.text) {
        validate_block_body(b, schema, source, diags);
    } else if b.kind.text == "page" {
        validate_page_body(b, ctx, diags);
    } else if b.kind.text == "component" {
        validate_component_body(b, ctx, diags);
    } else if b.kind.text == "theme" {
        validate_theme_body(b, ctx, diags);
    }
}

fn check_top_assign(a: &Assign, diags: &mut Vec<Diagnostic>) {
    match &a.target {
        AssignTarget::Simple { key } => {
            if !is_legal_top_assign(&key.text) {
                diags.push(Diagnostic::new(
                    key.span,
                    format!(
                        "unknown top-level option `{}`; expected `include`, a named item, or a settings block",
                        key.text
                    ),
                ));
            }
        }
        AssignTarget::Var { .. } => {}
        AssignTarget::Qualified { category, key } => {
            // `category:key = value` means "set `key` inside `category`'s
            // block". Validate it against the category's schema, same as
            // the opened form.
            if classify_kind(&category.text).is_none() {
                diags.push(Diagnostic::new(
                    category.span,
                    format!("unknown settings category `{}`", category.text),
                ));
                return;
            }
            check_qualified_key(
                top_schema_for(&category.text),
                &category.text,
                key,
                &a.value.text,
                a.value.span,
                diags,
            );
        }
        AssignTarget::Indexed { kind, key, .. } => {
            match classify_kind(&kind.text) {
                Some(KindKind::Named) => {
                    // `kind[name]:key = value` targets the named item's body.
                    check_qualified_key(
                        top_schema_for(&kind.text),
                        &kind.text,
                        key,
                        &a.value.text,
                        a.value.span,
                        diags,
                    );
                }
                Some(KindKind::Category) => diags.push(Diagnostic::new(
                    kind.span,
                    format!(
                        "`{0}` is a settings block, not a named item; use `{0}:key = ...`",
                        kind.text
                    ),
                )),
                None => diags.push(Diagnostic::new(
                    kind.span,
                    format!("unknown item kind `{}` in indexed assignment", kind.text),
                )),
            }
        }
    }
}

/// Validate one `category:key = v` or `kind[name]:key = v` shorthand
/// as if it were an assignment inside the block's body. Does nothing
/// if the kind has no rule table (component / page / theme / alert /
/// reminder — same silent pass-through as full-block form).
fn check_qualified_key(
    schema: Option<&'static BlockSchema>,
    kind_name: &str,
    key: &crate::parse::Ident,
    value_text: &str,
    value_span: Span,
    diags: &mut Vec<Diagnostic>,
) {
    let Some(schema) = schema else { return };
    match schema.body_mode {
        BodyMode::AnyKey(ty) => {
            check_value_type(value_text, value_span, ty, true, diags);
        }
        BodyMode::Keyed => {
            if let Some(rule) = schema.keys.iter().find(|r| r.name == key.text) {
                check_value_type(value_text, value_span, rule.ty, rule.expr_ok, diags);
            } else {
                diags.push(Diagnostic::new(
                    key.span,
                    format!(
                        "unknown key `{}` for `{kind_name}`; valid keys: {}",
                        key.text,
                        format_key_list(schema.keys)
                    ),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-key body validation
// ---------------------------------------------------------------------------

fn validate_block_body(
    block: &Block,
    schema: &BlockSchema,
    source: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    let mut seen_keys: HashMap<&str, Span> = HashMap::new();

    for item in &block.items {
        match item {
            Item::Assign(a) => validate_assign_in_body(a, schema, &mut seen_keys, diags),
            Item::Block(child) => validate_nested_block(child, schema, source, diags),
            Item::Param(p) => validate_param_in_body(p, &block.kind.text, diags),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                format!("`var` is only allowed at the top level, not inside `{}`", block.kind.text),
            )),
            Item::Repeat(r) => diags.push(Diagnostic::new(
                r.span,
                format!("`repeat` is not allowed inside `{}`", block.kind.text),
            )),
        }
    }

    // Required keys that never appeared.
    for rule in schema.keys.iter().filter(|r| r.required) {
        if !seen_keys.contains_key(rule.name) {
            diags.push(Diagnostic::new(
                block.kind.span,
                format!(
                    "`{}` block is missing required key `{}`",
                    block.kind.text, rule.name
                ),
            ));
        }
    }

    // Blocks like `secret` where exactly one of `env`/`file` must be
    // present. Special-cased by kind to avoid a bigger schema model.
    if block.kind.text == "secret" {
        let env_present = seen_keys.contains_key("env");
        let file_present = seen_keys.contains_key("file");
        if !env_present && !file_present {
            diags.push(Diagnostic::new(
                block.kind.span,
                "`secret` must set exactly one of `env` or `file`",
            ));
        } else if env_present && file_present {
            diags.push(Diagnostic::new(
                block.kind.span,
                "`secret` must set only one of `env` or `file`",
            ));
        }
    }
}

fn validate_assign_in_body(
    a: &Assign,
    schema: &BlockSchema,
    seen_keys: &mut HashMap<&'static str, Span>,
    diags: &mut Vec<Diagnostic>,
) {
    let key = match &a.target {
        AssignTarget::Simple { key } => key,
        AssignTarget::Var { .. } => {
            diags.push(Diagnostic::new(
                a.span,
                "`$var` definitions are only allowed at the top level",
            ));
            return;
        }
        AssignTarget::Qualified { .. } => {
            diags.push(Diagnostic::new(
                a.span,
                "`category:key` shorthand is only allowed at the top level",
            ));
            return;
        }
        AssignTarget::Indexed { .. } => {
            diags.push(Diagnostic::new(
                a.span,
                "`kind[name]:key` shorthand is only allowed at the top level",
            ));
            return;
        }
    };

    match schema.body_mode {
        BodyMode::AnyKey(ty) => {
            check_value_type(&a.value.text, a.value.span, ty, true, diags);
        }
        BodyMode::Keyed => {
            if let Some(rule) = schema.keys.iter().find(|r| r.name == key.text) {
                seen_keys.insert(rule.name, key.span);
                check_value_type(&a.value.text, a.value.span, rule.ty, rule.expr_ok, diags);
            } else {
                diags.push(Diagnostic::new(
                    key.span,
                    format!(
                        "unknown key `{}`; valid keys: {}",
                        key.text,
                        format_key_list(schema.keys)
                    ),
                ));
            }
        }
    }
}

fn validate_nested_block(
    child: &Block,
    parent: &BlockSchema,
    source: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) {
    let nb = match parent.nested.iter().find(|n| n.kind == child.kind.text) {
        Some(n) => n,
        None => {
            if matches!(parent.body_mode, BodyMode::AnyKey(_)) {
                diags.push(Diagnostic::new(
                    child.kind.span,
                    "nested blocks are not allowed here",
                ));
                return;
            }
            diags.push(Diagnostic::new(
                child.kind.span,
                format!(
                    "unknown nested block `{}`; allowed: {}",
                    child.kind.text,
                    format_nested_list(parent.nested)
                ),
            ));
            return;
        }
    };
    match nb.name_mode {
        NameMode::Required => {
            if child.name.is_none() {
                diags.push(Diagnostic::new(
                    child.kind.span,
                    format!("`{}` block needs a name", child.kind.text),
                ));
                return;
            }
        }
        NameMode::Forbidden => {
            if let Some(name) = &child.name {
                diags.push(Diagnostic::new(
                    name.span,
                    format!("`{}` does not take a name here", child.kind.text),
                ));
            }
        }
        NameMode::Optional => {}
    }
    validate_block_body(child, nb.schema, source, diags);
}

fn validate_param_in_body(p: &Param, parent_kind: &str, diags: &mut Vec<Diagnostic>) {
    if !matches!(parent_kind, "action" | "component") {
        diags.push(Diagnostic::new(
            p.span,
            format!(
                "`param` is only allowed inside `action` or `component`, not `{parent_kind}`"
            ),
        ));
    }
    if let Some(opts) = &p.options {
        validate_param_options(opts, diags);
    }
    // What the daemon refuses at run time (schema 6), caught here.
    let ty = match p.ty.text.as_str() {
        "int" => Some(ExpectedType::Int),
        "number" => Some(ExpectedType::Number),
        "bool" => Some(ExpectedType::Bool),
        "duration" => Some(ExpectedType::Dur),
        "color" => Some(ExpectedType::Color),
        "size" => Some(ExpectedType::Size),
        _ => None,
    };
    let option = |name: &str| {
        p.options.iter().flat_map(|o| &o.items).find_map(|it| match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == name) => Some(a),
            _ => None,
        })
    };
    let ty = match ty {
        Some(ExpectedType::Color) if parent_kind != "component" => Some(ExpectedType::HexColor),
        t => t,
    };
    if let (Some(d), Some(ty)) = (&p.default, ty) {
        check_value_type(&d.text, d.span, ty, true, diags);
    }
    if let Some(d) = p.default.as_ref().and_then(|d| crate::value::parse_number(d.text.trim()).map(|n| (n, d.span))) {
        let bound = |k: &str| option(k).and_then(|a| crate::value::parse_number(a.value.text.trim()));
        if bound("min").is_some_and(|m| d.0 < m) || bound("max").is_some_and(|m| d.0 > m) {
            diags.push(Diagnostic::new(d.1, format!("the default of `{}` is outside its `min` / `max`", p.name.text)));
        }
    }
    if let (Some(d), Some(pat)) = (&p.default, option("pattern")) {
        let pattern = crate::value::unquote_pattern(&pat.value.text);
        if !d.text.contains("{{") && crate::value::pattern_matches(&pattern, &crate::value::parse_string(&d.text)) == Ok(false) {
            diags.push(Diagnostic::new(d.span, format!("the default of `{}` does not match its `pattern`", p.name.text)));
        }
    }
    if let Some(ask) = option("ask") {
        if parent_kind != "action" {
            diags.push(Diagnostic::new(ask.span, "`ask` is for action params only"));
        } else if ask.value.text.trim() == "select" && option("options").is_none() {
            diags.push(Diagnostic::new(ask.span, format!("`ask = select` needs `options` for `{}`", p.name.text)));
        }
    }
}

/// Validate the options block of a `param`. Keys are per
/// `docs/schema.md` section 6.
fn validate_param_options(opts: &OptionsBlock, diags: &mut Vec<Diagnostic>) {
    for item in &opts.items {
        let Item::Assign(a) = item else {
            diags.push(Diagnostic::new(
                item.span(),
                "`param` options block only accepts `key = value` entries",
            ));
            continue;
        };
        let AssignTarget::Simple { key } = &a.target else {
            diags.push(Diagnostic::new(
                a.span,
                "`param` options block entries must be simple `key = value`",
            ));
            continue;
        };
        match key.text.as_str() {
            "min" | "max" => {
                check_value_type(&a.value.text, a.value.span, ExpectedType::Number, false, diags);
            }
            "pattern" => {
                check_value_type(&a.value.text, a.value.span, ExpectedType::Str, false, diags);
                let pattern = crate::value::unquote_pattern(&a.value.text);
                if let Err(e) = crate::value::pattern_matches(&pattern, "") {
                    diags.push(Diagnostic::new(a.value.span, format!("`pattern` is not a valid regex: {e}")));
                }
            }
            "options" => {
                check_value_type(&a.value.text, a.value.span, ExpectedType::List, true, diags);
            }
            "ask" => {
                check_value_type(
                    &a.value.text,
                    a.value.span,
                    ExpectedType::OneOf(&["select", "slider", "keypad", "toggle"]),
                    false,
                    diags,
                );
            }
            "label" => {
                check_value_type(&a.value.text, a.value.span, ExpectedType::Str, true, diags);
            }
            _ => diags.push(Diagnostic::new(
                key.span,
                format!(
                    "unknown `param` option `{}`; valid: min, max, pattern, options, ask, label",
                    key.text
                ),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Value type checking
// ---------------------------------------------------------------------------

fn check_value_type(
    raw: &str,
    span: Span,
    ty: ExpectedType,
    expr_ok: bool,
    diags: &mut Vec<Diagnostic>,
) {
    // Commands and free-text strings commonly interpolate `{{ ... }}`
    // (`run = curl /{{ vm }}/stop`). Treat those as implicitly
    // expression-tolerant, except inside a shell script.
    let implicit_expr_ok = matches!(ty, ExpectedType::Command | ExpectedType::Str);
    // Not by `classify`: it reads `"..."` as a quoted literal, but the
    // runner interpolates inside double quotes too (review round 8).
    let shape = classify(raw);
    match shape {
        _ if matches!(ty, ExpectedType::Command) && raw.contains("{{") && shell_script_interpolates(raw) => {
            diags.push(Diagnostic::new(
                span,
                "`{{ }}` inside an inline script (`sh -c`, `python -c`, `perl -e`, ...) pastes the value \
                 into code; pass it as an argument (`sh -c '... \"$$1\"' sh {{ x }}`) or in `env { }`"
                    .to_string(),
            ));
        }
        ValueShape::Expression(_) | ValueShape::Interp(_) => {
            if !expr_ok && !implicit_expr_ok {
                diags.push(Diagnostic::new(
                    span,
                    format!(
                        "expected {} here; `{{{{ ... }}}}` expressions are not accepted for this key",
                        type_name(ty)
                    ),
                ));
            }
        }
        ValueShape::Literal(text) => {
            if let Err(msg) = check_literal(text, ty) {
                diags.push(Diagnostic::new(span, msg));
            }
        }
    }
}

fn check_literal(text: &str, ty: ExpectedType) -> Result<(), String> {
    match ty {
        ExpectedType::Any => Ok(()),
        ExpectedType::Bool => parse_bool(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a bool (`true` or `false`), got `{text}`")),
        ExpectedType::Int => parse_int(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected an integer, got `{text}`")),
        ExpectedType::Number => parse_number(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a number, got `{text}`")),
        ExpectedType::Str | ExpectedType::Path | ExpectedType::Command => {
            if text.is_empty() {
                let noun = match ty {
                    ExpectedType::Str => "string",
                    ExpectedType::Path => "path",
                    ExpectedType::Command => "command",
                    _ => unreachable!(),
                };
                Err(format!("expected a non-empty {noun}"))
            } else {
                Ok(())
            }
        }
        ExpectedType::Dur => parse_duration(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a duration (`30s`, `2m`, `1h`, ...), got `{text}`")),
        ExpectedType::Date => crate::value::parse_date(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a date (`YYYY-MM-DD`), got `{text}`")),
        ExpectedType::Size => parse_size(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a size (`120`, `50%`, `grow`, `content`), got `{text}`")),
        ExpectedType::Bytes => parse_bytes(text)
            .map(|_| ())
            .ok_or_else(|| format!("expected a byte count (`64KiB`, `1MiB`, ...), got `{text}`")),
        ExpectedType::Color => {
            if parse_color(text).is_some() {
                Ok(())
            } else if is_bare_ident(text) && !text.contains('.') {
                check_color_name(text)
            } else {
                Err(format!(
                    "expected a hex color (`#rgb`, `#rrggbb`, `#rrggbbaa`) or a theme color name, got `{text}`"
                ))
            }
        }
        ExpectedType::Icon => check_icon_name(text),
        ExpectedType::Font => check_font(text),
        ExpectedType::Background => check_background(text),
        ExpectedType::Shadow => check_shadow(text),
        ExpectedType::Image => check_image(text, "background image"),
        ExpectedType::HexColor => parse_color(text)
            .map(|_| ())
            .ok_or_else(|| {
                format!("expected a hex color (`#rgb`, `#rrggbb`, `#rrggbbaa`), got `{text}`")
            }),
        ExpectedType::Ident => {
            if is_bare_ident(text) {
                Ok(())
            } else {
                Err(format!("expected an identifier, got `{text}`"))
            }
        }
        ExpectedType::List => {
            if text.starts_with('[') && text.ends_with(']') {
                Ok(())
            } else {
                Err(format!("expected a list `[...]`, got `{text}`"))
            }
        }
        ExpectedType::IdentOrList => {
            if is_bare_ident(text) || (text.starts_with('[') && text.ends_with(']')) {
                Ok(())
            } else {
                Err(format!(
                    "expected an identifier or a list of identifiers, got `{text}`"
                ))
            }
        }
        ExpectedType::Padding => {
            let n = text.split_whitespace().count();
            if matches!(n, 1 | 2 | 4) && text.split_whitespace().all(|p| p.parse::<u32>().is_ok()) {
                Ok(())
            } else {
                Err(format!(
                    "expected 1, 2 (vertical horizontal) or 4 (top right bottom left) sizes in px, got `{text}`"
                ))
            }
        }
        ExpectedType::Cell => {
            let n: Option<Vec<u8>> = text.split_whitespace().map(|p| p.parse::<u8>().ok()).collect();
            match n.as_deref() {
                Some([_, _]) => Ok(()),
                Some([_, _, cs, rs]) if *cs > 0 && *rs > 0 => Ok(()),
                _ => Err(format!(
                    "expected `col row` or `col row colspan rowspan` (from 0, spans from 1, at most 255), got `{text}`"
                )),
            }
        }
        ExpectedType::Tracks => {
            // A lone number is a count.
            let count = text.parse::<u32>().ok();
            if count.is_some_and(|n| (1..=1024).contains(&n)) {
                return Ok(());
            }
            let inner = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
            let tracks: Vec<&str> =
                inner.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
            let track_ok = |t: &str| {
                t == "content"
                    || t.strip_suffix("fr").is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=100).contains(&n)))
                    || t.parse::<u32>().is_ok_and(|n| n <= 10_000)
            };
            if count.is_none() && !tracks.is_empty() && tracks.iter().all(|t| track_ok(t)) {
                Ok(())
            } else {
                Err(format!(
                    "expected a count (`3`) or a list of tracks (`[120, 1fr, content]`), got `{text}`"
                ))
            }
        }
        ExpectedType::Offset => {
            let ok = match text.strip_suffix('%') {
                Some(p) => p.trim().parse::<i16>().is_ok_and(|p| p.abs() <= 1000),
                None => text.parse::<i32>().is_ok(),
            };
            if ok {
                Ok(())
            } else {
                Err(format!("expected an offset in px (`8`, `-8`) or a percentage (`50%`, `-10%`), got `{text}`"))
            }
        }
        ExpectedType::PositiveInt => match parse_int(text) {
            Some(n) if n >= 1 => Ok(()),
            _ => Err(format!("expected an integer of at least 1, got `{text}`")),
        },
        ExpectedType::OneOf(allowed) => {
            if allowed.contains(&text) {
                Ok(())
            } else {
                Err(format!(
                    "expected one of {}, got `{text}`",
                    allowed
                        .iter()
                        .map(|a| format!("`{a}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
    }
}

fn is_bare_ident(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    if !(b[0].is_ascii_alphabetic() || b[0] == b'_') {
        return false;
    }
    // `a.b`: a remote member.
    s.split('.').all(|seg| {
        let b = seg.as_bytes();
        !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
    })
}

fn type_name(ty: ExpectedType) -> &'static str {
    match ty {
        ExpectedType::Bool => "a bool",
        ExpectedType::Int => "an integer",
        ExpectedType::Number => "a number",
        ExpectedType::Str => "a string",
        ExpectedType::Path => "a path",
        ExpectedType::Command => "a command",
        ExpectedType::Dur => "a duration",
        ExpectedType::Date => "a date",
        ExpectedType::Size => "a size",
        ExpectedType::Bytes => "a byte count",
        ExpectedType::Color | ExpectedType::HexColor => "a color",
        ExpectedType::Ident => "an identifier",
        ExpectedType::List => "a list",
        ExpectedType::IdentOrList => "an identifier or list of identifiers",
        ExpectedType::OneOf(_) => "a keyword",
        ExpectedType::Padding => "1, 2 or 4 sizes in px",
        ExpectedType::Cell => "a grid cell",
        ExpectedType::Tracks => "a count or a list of tracks",
        ExpectedType::Offset => "an offset",
        ExpectedType::PositiveInt => "an integer of at least 1",
        ExpectedType::Icon => "an icon name",
        ExpectedType::Font => "a font",
        ExpectedType::Background => "a background",
        ExpectedType::Shadow => "a shadow",
        ExpectedType::Image => "a PNG path",
        ExpectedType::Any => "a value",
    }
}

fn format_key_list(keys: &[KeyRule]) -> String {
    if keys.is_empty() {
        return "(none)".to_string();
    }
    keys.iter()
        .map(|r| format!("`{}`", r.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_nested_list(nested: &[NestedBlock]) -> String {
    if nested.is_empty() {
        return "(none)".to_string();
    }
    nested
        .iter()
        .map(|n| format!("`{}`", n.kind))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Page / component / UI tree walker
// ---------------------------------------------------------------------------

/// Validate the body of a `page home { ... }` block. The body is a mix
/// of page-level keys (`title`, `icon`, `nav`) and UI nodes.
fn validate_page_body(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    for item in &b.items {
        match item {
            Item::Assign(a) => validate_page_key(a, diags),
            Item::Block(child) => validate_ui_block(child, ctx, false, diags),
            Item::Repeat(r) => validate_ui_repeat(r, ctx, false, diags),
            Item::Param(p) => diags.push(Diagnostic::new(
                p.span,
                "`param` is not allowed in a `page` body (use a `component`)",
            )),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                "`var` declarations are only allowed at the top level",
            )),
        }
    }
}

fn validate_page_key(a: &Assign, diags: &mut Vec<Diagnostic>) {
    let AssignTarget::Simple { key } = &a.target else {
        diags.push(Diagnostic::new(
            a.span,
            "qualified assignments are only allowed at the top level",
        ));
        return;
    };
    // Event-handler assigns are legal on a page too.
    if EVENT_HANDLER_KINDS.contains(&key.text.as_str()) {
        check_value_type(&a.value.text, a.value.span, ExpectedType::Ident, false, diags);
        return;
    }
    if let Some(rule) = PAGE_KEYS.iter().find(|r| r.name == key.text) {
        check_value_type(&a.value.text, a.value.span, rule.ty, rule.expr_ok, diags);
    } else {
        diags.push(Diagnostic::new(
            key.span,
            format!(
                "unknown page key `{}`; valid: {}",
                key.text,
                format_key_list(PAGE_KEYS)
            ),
        ));
    }
}

/// Validate the body of a `component foo { ... }` block. Body = params
/// (zero or more) + exactly one root UI node.
fn validate_component_body(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    let mut root_count = 0;
    for item in &b.items {
        match item {
            Item::Param(p) => {
                validate_param_in_body(p, "component", diags);
            }
            Item::Block(child) => {
                root_count += 1;
                validate_ui_block(child, ctx, true, diags);
            }
            Item::Repeat(r) => {
                root_count += 1;
                validate_ui_repeat(r, ctx, true, diags);
            }
            Item::Assign(a) => diags.push(Diagnostic::new(
                a.span,
                "a `component` body only accepts `param` and a single UI root node",
            )),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                "`var` declarations are only allowed at the top level",
            )),
        }
    }
    if root_count == 0 {
        diags.push(Diagnostic::new(
            b.kind.span,
            format!("`component {}` is missing its UI root node", b.name.as_ref().map(|n| n.text.as_str()).unwrap_or("?")),
        ));
    } else if root_count > 1 {
        diags.push(Diagnostic::new(
            b.kind.span,
            "a `component` has exactly one UI root node; wrap siblings in a `column` or `row`",
        ));
    }
}

// ---------------------------------------------------------------------------
// Phase 9: theme body
// ---------------------------------------------------------------------------
//
// A `theme <name> { }` body (schema.md section 11 and themes.md):
//
// - Known tokens, typed: colors, fonts, sizes, `motion`, `shadow`,
//   `extends` (theme ref), `background_image`, `icons`.
// - Free extra tokens: a theme may add any token; they are read as
//   `theme.<name>` in expressions. These are accepted with no shape
//   check (expression-tolerant).
// - `style <selector> { ... }` sub-blocks: the selector shape is
//   validated (`.<class>`, `<node>[:state]`, `*`); body is any key
//   = value, expression-tolerant.
// - `component <name> { ... }` sub-blocks: a theme-local replacement
//   component. Reuses the same body shape as a top-level component
//   (params + exactly one UI root node). The replacement name is
//   NOT required to match an existing component -- a theme may ship
//   its own layout for a new slot too.
// - A `param` or `repeat` outside a component sub-block is an error.

/// Required theme tokens that must appear (directly or via a parent
/// theme reached through `extends`). We only enforce "exists at some
/// point in this theme or via `extends`" at a later phase; here we
/// just type-check whatever is present. The list is used to drive
/// shape checks and informative diagnostics.
// Note: `background` is intentionally missing here. It's handled by
// the explicit arm below with `ExpectedType::Any` because the
// themes.md example lets a theme override `background` with either a
// hex color (the Colors-group default) or a path to an image. Keeping
// it only in the explicit arm avoids the two lists disagreeing when
// one is edited and the other isn't.
const THEME_COLOR_TOKENS: &[&str] = &[
    "surface",
    "text",
    "muted",
    "accent",
    "ok",
    "info",
    "warning",
    "critical",
];

const THEME_FONT_TOKENS: &[&str] = crate::fonts::TOKENS;

const THEME_SIZE_TOKENS: &[&str] = &["gap", "padding", "radius", "tap_min"];

fn theme_token_type(key: &str) -> Option<ExpectedType> {
    match key {
        "extends" => Some(ExpectedType::Ident),
        "motion" => Some(ExpectedType::OneOf(&["full", "reduced", "none"])),
        "shadow" => Some(ExpectedType::Shadow),
        // schema.md calls `background_image` an optional path. The
        // themes.md example also writes `background = images/bg.jpg`
        // which overrides the Color token with a path; we accept
        // either shape by typing both as Any.
        "background" => Some(ExpectedType::Background),
        "background_image" => Some(ExpectedType::Image),
        "icons" => Some(ExpectedType::Path),
        _ => {
            if THEME_COLOR_TOKENS.contains(&key) {
                Some(ExpectedType::Color)
            } else if THEME_FONT_TOKENS.contains(&key) {
                Some(ExpectedType::Font)
            } else if THEME_SIZE_TOKENS.contains(&key) {
                Some(ExpectedType::Size)
            } else {
                None
            }
        }
    }
}

/// Validate the body of a `theme name { ... }`. Items are theme
/// tokens (assigns), `style <selector> { }` sub-blocks, and theme-
/// local replacement `component <name> { }` sub-blocks.
fn validate_theme_body(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    // Names in a theme resolve in that theme's own chain.
    if let Some(name) = &b.name {
        let map = THEME_BLOCKS.with(|t| chain_tokens(&t.borrow(), &name.text));
        return with_theme_maps(vec![map], || validate_theme_items(b, ctx, diags));
    }
    validate_theme_items(b, ctx, diags);
}

fn validate_theme_items(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    for item in &b.items {
        match item {
            Item::Assign(a) => validate_theme_assign(a, diags),
            Item::Block(child) => validate_theme_sub_block(child, ctx, diags),
            Item::Param(p) => diags.push(Diagnostic::new(
                p.span,
                "`param` is only allowed inside a `component` body",
            )),
            Item::Repeat(r) => diags.push(Diagnostic::new(
                r.span,
                "`repeat` is not allowed inside a `theme` body",
            )),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                "`var` declarations are only allowed at the top level",
            )),
        }
    }
}

fn validate_theme_assign(a: &Assign, diags: &mut Vec<Diagnostic>) {
    let AssignTarget::Simple { key } = &a.target else {
        diags.push(Diagnostic::new(
            a.span,
            "qualified assignments are only allowed at the top level",
        ));
        return;
    };
    // How a dialog's backdrop looks: `transparent`, a color, `blur <px>`.
    if key.text == "dialog_backdrop" {
        let v = a.value.text.trim();
        let ok = v == "transparent"
            || v.strip_prefix("blur").is_some_and(|px| px.trim().trim_end_matches("px").parse::<u32>().is_ok_and(|n| n <= 20))
            || (v.starts_with('#') && matches!(v.len(), 7 | 9) && v[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        if !ok {
            diags.push(Diagnostic::new(
                a.value.span,
                format!("`dialog_backdrop` is `transparent`, `#rrggbb[aa]` or `blur <px>` (0 to 20), not `{v}`"),
            ));
        }
        return;
    }
    // Known token: type-check against its declared shape, with
    // expression tolerance (a theme may interpolate another token).
    if let Some(ty) = theme_token_type(&key.text) {
        check_value_type(&a.value.text, a.value.span, ty, true, diags);
        return;
    }
    // Unknown token name: accepted as a free extra token (schema.md
    // section 11: "A theme may add any other token"). We still
    // sanity-check that the key is a bare ident (the parser already
    // ensured this) and that the value is non-empty.
    check_value_type(&a.value.text, a.value.span, ExpectedType::Any, true, diags);
}

fn validate_theme_sub_block(child: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    match child.kind.text.as_str() {
        "style" => validate_theme_style_block(child, diags),
        "component" => validate_theme_component_block(child, ctx, diags),
        _ => diags.push(Diagnostic::new(
            child.kind.span,
            format!(
                "unknown theme sub-block `{}`; allowed: `style`, `component`",
                child.kind.text
            ),
        )),
    }
}

/// A `style <selector> { ... }` sub-block. The selector token was
/// captured as a raw string by the lexer; validate its shape here,
/// then accept any key = value entry in the body.
fn validate_theme_style_block(b: &Block, diags: &mut Vec<Diagnostic>) {
    // A `:state` body holds styles LVGL keeps per state; size, grow,
    // visibility and enabled would apply in every state.
    if b.name.as_ref().is_some_and(|n| n.text.contains(':')) {
        for it in &b.items {
            if let Item::Assign(a) = it {
                if let AssignTarget::Simple { key } = &a.target {
                    if matches!(key.text.as_str(), "width" | "height" | "grow" | "visible" | "enabled") {
                        diags.push(Diagnostic::new(
                            key.span,
                            format!("`{}` cannot change with a state; set it on the node or in a style without `:state`", key.text),
                        ));
                    }
                }
            }
        }
    }
    let Some(selector) = &b.name else {
        diags.push(Diagnostic::new(
            b.kind.span,
            "`style` needs a selector (`.card`, `button:pressed`, `*`, ...)",
        ));
        return;
    };
    if let Err(msg) = validate_style_selector(&selector.text) {
        diags.push(Diagnostic::new(selector.span, msg));
    }
    // Each entry is key = value (expression-tolerant). Nested blocks
    // are not permitted inside a style body.
    for item in &b.items {
        match item {
            Item::Assign(a) => {
                let AssignTarget::Simple { .. } = &a.target else {
                    diags.push(Diagnostic::new(
                        a.span,
                        "a `style` body only contains simple `key = value`",
                    ));
                    continue;
                };
                let ty = match &a.target {
                    AssignTarget::Simple { key } if key.text == "font" => ExpectedType::Font,
                    _ => ExpectedType::Any,
                };
                check_value_type(&a.value.text, a.value.span, ty, true, diags);
            }
            Item::Block(child) => diags.push(Diagnostic::new(
                child.kind.span,
                "a `style` body does not contain nested blocks",
            )),
            Item::Param(p) => diags.push(Diagnostic::new(
                p.span,
                "`param` is not allowed in a `style` body",
            )),
            Item::Repeat(r) => diags.push(Diagnostic::new(
                r.span,
                "`repeat` is not allowed in a `style` body",
            )),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                "`var` declarations are only allowed at the top level",
            )),
        }
    }
}

/// Shape check for a style selector. Accepts `*` (every node), `.ident`
/// (class selector), and `ident[:state]` (node kind, with an optional
/// pseudo-state). Multi-selector lists like `button, link` are not
/// supported in v1 and left to a later phase; a tolerant diagnostic
/// says so.
fn validate_style_selector(sel: &str) -> Result<(), String> {
    let s = sel.trim();
    if s.is_empty() {
        return Err("empty `style` selector".to_string());
    }
    if s == "*" {
        return Ok(());
    }
    if s.contains(',') {
        return Err(format!(
            "selector lists are not supported yet; split `{s}` into separate `style` blocks"
        ));
    }
    // `.class[:state]` form.
    if let Some(rest) = s.strip_prefix('.') {
        let (class, state) = rest.split_once(':').map_or((rest, None), |(c, st)| (c, Some(st)));
        if let Some(state) = state {
            if !matches!(state, "pressed" | "checked" | "disabled" | "focused" | "hover" | "running" | "failed") {
                return Err(format!(
                    "unknown pseudo-state `:{state}`; valid: `:pressed`, `:checked`, `:disabled`, `:focused`, `:hover`, `:running`, `:failed`"
                ));
            }
        }
        let rest = class;
        if !is_bare_ident(rest) {
            return Err(format!(
                "expected a class name after `.` (identifier), got `{s}`"
            ));
        }
        return Ok(());
    }
    // `node[:state]` form.
    if let Some((node, state)) = s.split_once(':') {
        if !is_bare_ident(node) {
            return Err(format!(
                "expected a node kind before `:` (identifier), got `{s}`"
            ));
        }
        if !matches!(state, "pressed" | "checked" | "disabled" | "focused" | "hover" | "running" | "failed") {
            return Err(format!(
                "unknown pseudo-state `:{state}`; valid: `:pressed`, `:checked`, `:disabled`, `:focused`, `:hover`, `:running`, `:failed`"
            ));
        }
        return Ok(());
    }
    if !is_bare_ident(s) {
        return Err(format!("invalid style selector `{s}`"));
    }
    Ok(())
}

/// A theme-local replacement `component <name> { ... }`. Same body shape
/// as a top-level component.
fn validate_theme_component_block(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    if b.name.is_none() {
        diags.push(Diagnostic::new(
            b.kind.span,
            "a theme `component` sub-block needs a name",
        ));
        return;
    }
    validate_component_body(b, ctx, diags);
}

/// Validate one UI block. `inside_component` controls whether `slot`
/// may appear.
fn validate_ui_block(
    b: &Block,
    ctx: &UiContext,
    inside_component: bool,
    diags: &mut Vec<Diagnostic>,
) {
    // Action-call shortcut (block form of `on_tap`, `on_hold`, ...).
    if EVENT_HANDLER_KINDS.contains(&b.kind.text.as_str()) {
        validate_event_handler_block(b, ctx, diags);
        return;
    }

    // User-defined component call.
    if ctx.user_components.contains(&b.kind.text) {
        validate_component_call(b, ctx, inside_component, diags);
        return;
    }

    // Built-in UI building block.
    let node = match ui_node_schema(&b.kind.text) {
        Some(n) => n,
        None => {
            diags.push(Diagnostic::new(
                b.kind.span,
                format!(
                    "unknown UI node or component `{}`",
                    b.kind.text
                ),
            ));
            return;
        }
    };

    if node.slot_only && !inside_component {
        diags.push(Diagnostic::new(
            b.kind.span,
            "`slot` is only allowed inside a `component` body",
        ));
    }
    if let Some(name) = &b.name {
        diags.push(Diagnostic::new(
            name.span,
            format!("`{}` does not take a name", b.kind.text),
        ));
    }

    for item in &b.items {
        validate_ui_item_in_body(item, node, ctx, inside_component, diags);
    }
    if node.kind == "grid" {
        check_grid_cells(b, diags);
    }
}

/// The number of tracks a literal grid `columns` / `rows` makes (a
/// count, or a list's length); `None` for an expression or a bad value.
fn literal_track_count(b: &Block, key: &str) -> Option<usize> {
    let a = b.items.iter().find_map(|it| match it {
        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key) => Some(a),
        _ => None,
    })?;
    let ValueShape::Literal(text) = classify(&a.value.text) else { return None };
    let text = text.trim();
    if let Ok(n) = text.parse::<usize>() {
        return Some(n);
    }
    let inner = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
    Some(inner.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()).count())
}

/// A child's literal `cell` must lie inside the grid's literal columns
/// (2 by default) and, when set, rows: the renderer would otherwise
/// move it.
fn check_grid_cells(grid: &Block, diags: &mut Vec<Diagnostic>) {
    let has = |key: &str| {
        grid.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key)))
    };
    let cols = if has("columns") { literal_track_count(grid, "columns") } else { Some(2) };
    let rows = literal_track_count(grid, "rows");
    fn children<'a>(items: &'a [Item], out: &mut Vec<&'a Block>) {
        for it in items {
            match it {
                Item::Block(b) => out.push(b),
                Item::Repeat(r) => children(&r.items, out),
                _ => {}
            }
        }
    }
    let mut kids = Vec::new();
    children(&grid.items, &mut kids);
    for child in kids {
        for it in &child.items {
            let Item::Assign(a) = it else { continue };
            if !matches!(&a.target, AssignTarget::Simple { key } if key.text == "cell") {
                continue;
            }
            let ValueShape::Literal(text) = classify(&a.value.text) else { continue };
            let n: Option<Vec<usize>> = text.split_whitespace().map(|p| p.parse().ok()).collect();
            let (c, r, cs, rs) = match n.as_deref() {
                Some(&[c, r]) => (c, r, 1, 1),
                Some(&[c, r, cs, rs]) => (c, r, cs, rs),
                _ => continue,
            };
            if let Some(cols) = cols.filter(|&n| c + cs > n) {
                diags.push(Diagnostic::new(
                    a.value.span,
                    format!("cell `{}` is outside the grid's {cols} columns (counted from 0)", text.trim()),
                ));
            } else if let Some(rows) = rows.filter(|&n| r + rs > n) {
                diags.push(Diagnostic::new(
                    a.value.span,
                    format!("cell `{}` is outside the grid's {rows} rows (counted from 0)", text.trim()),
                ));
            }
        }
    }
}

fn validate_ui_item_in_body(
    item: &Item,
    node: &UiNode,
    ctx: &UiContext,
    inside_component: bool,
    diags: &mut Vec<Diagnostic>,
) {
    match item {
        Item::Assign(a) => validate_ui_assign(a, node, diags),
        Item::Block(child) => {
            // Allowed as nested sub-block (`chart.series`)?
            if let Some(nb) = node
                .nested_subblocks
                .iter()
                .find(|n| n.kind == child.kind.text)
            {
                match nb.name_mode {
                    NameMode::Required if child.name.is_none() => {
                        diags.push(Diagnostic::new(
                            child.kind.span,
                            format!("`{}` block needs a name", child.kind.text),
                        ));
                    }
                    NameMode::Forbidden if child.name.is_some() => {
                        diags.push(Diagnostic::new(
                            child.name.as_ref().unwrap().span,
                            format!("`{}` does not take a name here", child.kind.text),
                        ));
                    }
                    _ => {}
                }
                validate_block_body(child, nb.schema, ctx.source, diags);
                return;
            }
            // Event-handler shortcut even in a terminal node? Yes, every
            // UI node accepts `on_tap { ... }` etc.
            if EVENT_HANDLER_KINDS.contains(&child.kind.text.as_str()) {
                validate_event_handler_block(child, ctx, diags);
                return;
            }
            // Otherwise, it's a child UI node.
            if !node.accepts_children && !ctx.user_components.contains(&child.kind.text) {
                diags.push(Diagnostic::new(
                    child.kind.span,
                    format!(
                        "`{}` does not accept child UI nodes",
                        node.kind
                    ),
                ));
                return;
            }
            validate_ui_block(child, ctx, inside_component, diags);
        }
        Item::Repeat(r) => {
            // `repeat` expands into child UI nodes, so a terminal node
            // (text, icon, etc.) cannot host one.
            if !node.accepts_children {
                diags.push(Diagnostic::new(
                    r.span,
                    format!(
                        "`{}` does not accept child UI nodes (`repeat` would produce them)",
                        node.kind
                    ),
                ));
                return;
            }
            validate_ui_repeat(r, ctx, inside_component, diags);
        }
        Item::Param(p) => diags.push(Diagnostic::new(
            p.span,
            "`param` is only allowed at the start of a `component` body",
        )),
        Item::Var(v) => diags.push(Diagnostic::new(
            v.span,
            "`var` declarations are only allowed at the top level",
        )),
    }
}

fn validate_ui_assign(a: &Assign, node: &UiNode, diags: &mut Vec<Diagnostic>) {
    let AssignTarget::Simple { key } = &a.target else {
        diags.push(Diagnostic::new(
            a.span,
            "qualified assignments are only allowed at the top level",
        ));
        return;
    };

    // Node-specific keys win over common keys.
    if let Some(rule) = node.keys.iter().find(|r| r.name == key.text) {
        check_value_type(&a.value.text, a.value.span, rule.ty, rule.expr_ok, diags);
        return;
    }
    if let Some(rule) = COMMON_UI_KEYS.iter().find(|r| r.name == key.text) {
        check_value_type(&a.value.text, a.value.span, rule.ty, rule.expr_ok, diags);
        return;
    }
    // Event-handler simple-assign form is covered by COMMON_UI_KEYS
    // (`on_tap`, `on_hold`). `on_change` / `on_submit` are node-specific
    // and already handled above when the node declares them.
    diags.push(Diagnostic::new(
        key.span,
        format!(
            "unknown property `{}` on `{}`",
            key.text, node.kind
        ),
    ));
}

fn validate_ui_repeat(
    r: &crate::parse::Repeat,
    ctx: &UiContext,
    inside_component: bool,
    diags: &mut Vec<Diagnostic>,
) {
    for item in &r.items {
        match item {
            Item::Block(child) => validate_ui_block(child, ctx, inside_component, diags),
            Item::Repeat(inner) => validate_ui_repeat(inner, ctx, inside_component, diags),
            Item::Assign(a) => diags.push(Diagnostic::new(
                a.span,
                "`repeat` body only contains UI nodes, not property assignments",
            )),
            Item::Param(p) => diags.push(Diagnostic::new(
                p.span,
                "`param` is not allowed inside a `repeat` body",
            )),
            Item::Var(v) => diags.push(Diagnostic::new(
                v.span,
                "`var` declarations are only allowed at the top level",
            )),
        }
    }
}

/// Validate the body of an `on_tap action { ... }` or inline
/// `on_tap { goto = home }` shortcut. For a named shortcut, keys are
/// cross-checked against the referenced action's `param` table; for an
/// inline (nameless) shortcut, only the `INLINE_ACTION_KEYS` subset is
/// accepted.
fn validate_event_handler_block(b: &Block, ctx: &UiContext, diags: &mut Vec<Diagnostic>) {
    // Named action-call: validate args against that action's params.
    if let Some(action_name) = &b.name {
        // Only validate if the action exists; otherwise the Phase 7
        // cross-ref resolver will already have flagged the unknown
        // action name on the block header.
        if let Some(specs) = ctx.params.actions.get(action_name.text.as_str()) {
            validate_arg_body(b, specs, &b.kind.text, &action_name.text, ctx, diags);
        }
        return;
    }

    // Nameless inline shortcut.
    for item in &b.items {
        match item {
            Item::Assign(a) => {
                let AssignTarget::Simple { key } = &a.target else {
                    diags.push(Diagnostic::new(
                        a.span,
                        "an action-call body only contains simple `key = value`",
                    ));
                    continue;
                };
                if !INLINE_ACTION_KEYS.contains(&key.text.as_str()) {
                    diags.push(Diagnostic::new(
                        key.span,
                        format!(
                            "unknown inline action key `{}`; valid: {}",
                            key.text,
                            INLINE_ACTION_KEYS
                                .iter()
                                .map(|k| format!("`{k}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
            }
            Item::Block(child) => {
                // Inline actions can hold `set { var = val }` or
                // `env { KEY = val }` sub-blocks, matching the full
                // action grammar.
                match child.kind.text.as_str() {
                    "set" => validate_block_body(child, SET_BLOCK, ctx.source, diags),
                    "env" => validate_block_body(child, ENV_BLOCK, ctx.source, diags),
                    _ => diags.push(Diagnostic::new(
                        child.kind.span,
                        format!(
                            "`{}` is not valid inside an inline action; use `set` or `env`",
                            child.kind.text
                        ),
                    )),
                }
            }
            _ => diags.push(Diagnostic::new(
                item.span(),
                "an action-call body only contains `key = value` entries",
            )),
        }
    }
}

/// Validate a user-component call block (`stat_tile { label = CPU; ... }`).
/// Items are a mix of param args (simple assigns matching a param name),
/// common UI properties applied to the component's root (section 7),
/// event-handler assigns, nested event-handler blocks, and child UI
/// nodes going into the component's `slot`.
fn validate_component_call(
    b: &Block,
    ctx: &UiContext,
    inside_component: bool,
    diags: &mut Vec<Diagnostic>,
) {
    if let Some(name) = &b.name {
        diags.push(Diagnostic::new(
            name.span,
            format!("`{}` does not take a name", b.kind.text),
        ));
    }

    let specs = ctx
        .params
        .components
        .get(b.kind.text.as_str())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    validate_arg_body(b, specs, "component", &b.kind.text, ctx, diags);
    // Still walk child UI nodes so misuses inside them surface (and
    // event-handler blocks get their own typed arg check).
    for item in &b.items {
        match item {
            Item::Block(child) => {
                if EVENT_HANDLER_KINDS.contains(&child.kind.text.as_str()) {
                    // Already validated via validate_arg_body's arg
                    // walk (block args are not arg values); fall through
                    // to the event-handler validator for its body.
                    validate_event_handler_block(child, ctx, diags);
                } else {
                    // A child UI node going into the component's slot.
                    validate_ui_block(child, ctx, inside_component, diags);
                }
            }
            Item::Repeat(r) => validate_ui_repeat(r, ctx, inside_component, diags),
            _ => {}
        }
    }
}

/// Shared validator for the arg body of a user-component call or a named
/// action-call shortcut. `kind_label` is "component" or an event-handler
/// name (`on_tap`, ...); `call_name` is the component / action name.
fn validate_arg_body(
    b: &Block,
    specs: &[ParamSpec],
    kind_label: &str,
    call_name: &str,
    ctx: &UiContext,
    diags: &mut Vec<Diagnostic>,
) {
    let is_component = kind_label == "component";
    let mut seen: HashMap<String, Span> = HashMap::new();
    for item in &b.items {
        // A call's body is its args. (A component call's child nodes
        // go to its slot and are checked by the caller.)
        if !is_component && !matches!(item, Item::Assign(_)) {
            diags.push(Diagnostic::new(item.span(), "an action call only takes `key = value` args"));
            continue;
        }
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            diags.push(Diagnostic::new(
                a.span,
                "an action/component call only contains simple `key = value` args",
            ));
            continue;
        };
        if seen.contains_key(&key.text) {
            diags.push(Diagnostic::new(
                key.span,
                format!("arg `{}` is already set", key.text),
            ));
            continue;
        }

        // On a component call, common UI properties and event handlers
        // apply to the root node (section 7). A simple event-handler
        // assign is validated as an action-ref literal.
        if is_component {
            if let Some(rule) = COMMON_UI_KEYS.iter().find(|r| r.name == key.text) {
                seen.insert(key.text.clone(), key.span);
                check_value_type(&a.value.text, a.value.span, rule.ty, rule.expr_ok, diags);
                continue;
            }
            if EVENT_HANDLER_KINDS.contains(&key.text.as_str()) {
                seen.insert(key.text.clone(), key.span);
                check_value_type(&a.value.text, a.value.span, ExpectedType::Ident, true, diags);
                continue;
            }
        }

        let Some(spec) = specs.iter().find(|s| s.name == key.text) else {
            diags.push(Diagnostic::new(
                key.span,
                format!(
                    "unknown arg `{}` for {}; valid args: {}",
                    key.text,
                    call_label(kind_label, call_name),
                    format_param_list(specs)
                ),
            ));
            continue;
        };
        seen.insert(spec.name.clone(), key.span);
        check_param_arg(&a.value.text, a.value.span, spec, ctx.names, diags);
    }

    // Report missing required args (no default, no `ask`).
    for spec in specs.iter() {
        if spec.has_default || spec.asked {
            continue;
        }
        if !seen.contains_key(&spec.name) {
            diags.push(Diagnostic::new(
                b.kind.span,
                format!(
                    "{} is missing required arg `{}` ({})",
                    call_label(kind_label, call_name),
                    spec.name,
                    param_type_name(spec.ty)
                ),
            ));
        }
    }
}

fn call_label(kind_label: &str, call_name: &str) -> String {
    if kind_label == "component" {
        format!("component `{call_name}`")
    } else {
        format!("action `{call_name}` (called from `{kind_label}`)")
    }
}

fn format_param_list(specs: &[ParamSpec]) -> String {
    if specs.is_empty() {
        return "(none)".to_string();
    }
    specs
        .iter()
        .map(|s| format!("`{}`", s.name))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Phase 7: cross-reference resolution
// ---------------------------------------------------------------------------
//
// After the structural + per-key checks, walk the AST once more to
// resolve name references:
//
// - `on_tap` / `on_hold` / `on_change` / `on_submit` / `button.action`
//   / `alert.action` / `action.then` / `action.else` = action name (or
//   `none`).
// - `action.refresh` = source name, or a list of them.
// - `alert.from` = source name.
// - `screen.theme`, `screen.night.theme` = theme name.
// - `app.start_page` and any `goto = ...` = page name (or `back`).
// - `security.pin` = secret name.
//
// Values that are `{{ ... }}` expressions are skipped; CEL will type
// them later. The identifier `none` short-circuits action refs,
// `back` short-circuits page refs. If a name is written literally and
// isn't in the collected table, we emit a diagnostic.

#[derive(Clone, Default)]
struct Names {
    sources: std::collections::HashSet<String>,
    actions: std::collections::HashSet<String>,
    pages: std::collections::HashSet<String>,
    themes: std::collections::HashSet<String>,
    secrets: std::collections::HashSet<String>,
}

fn collect_names(doc: &Document) -> Names {
    let mut n = Names::default();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        let Some(name) = &b.name else { continue };
        match b.kind.text.as_str() {
            "source" => {
                n.sources.insert(name.text.clone());
            }
            "action" => {
                n.actions.insert(name.text.clone());
            }
            "page" => {
                n.pages.insert(name.text.clone());
            }
            "theme" => {
                n.themes.insert(name.text.clone());
            }
            "secret" => {
                n.secrets.insert(name.text.clone());
            }
            _ => {}
        }
    }
    remote::add_names(doc, &mut n);
    n
}

#[derive(Copy, Clone)]
enum RefKind {
    Action,
    Source,
    SourceList,
    Page,
    Theme,
    Secret,
}

// ---------------------------------------------------------------------------
// Phase 8: param tables for component and action calls
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum ParamType {
    /// A regular typed value (string, int, number, bool, duration, color,
    /// size, list, any).
    Expected(ExpectedType),
    /// `param foo: action` -- the arg is an action reference (bare
    /// identifier or `{{ expr }}`); bare identifiers are cross-referenced
    /// against the collected action names, with `none` as a short-circuit.
    ActionRef,
    /// `param foo: source` -- source reference, same shape as above
    /// without the `none` short-circuit.
    SourceRef,
}

#[derive(Clone, Debug)]
struct ParamSpec {
    name: String,
    ty: ParamType,
    /// The param has a `= default`, so it is optional at the call site.
    has_default: bool,
    /// The param has an `ask = ...` option; the runtime prompts for it
    /// when the caller omits the arg (actions only, per schema.md
    /// section 6), so it is treated as optional too.
    asked: bool,
    /// Span of the `param` keyword, for "declared at" cross-references.
    #[allow(dead_code)]
    span: Span,
}

#[derive(Default)]
struct Params {
    components: HashMap<String, Vec<ParamSpec>>,
    actions: HashMap<String, Vec<ParamSpec>>,
}

/// Map the identifier after `:` in `param foo: TYPE` to a [`ParamType`].
/// Returns `None` for an unknown type (the caller reports it).
fn param_type_from_ident(s: &str) -> Option<ParamType> {
    use ExpectedType as E;
    Some(match s {
        "string" => ParamType::Expected(E::Str),
        "int" => ParamType::Expected(E::Int),
        "number" => ParamType::Expected(E::Number),
        "bool" => ParamType::Expected(E::Bool),
        "duration" => ParamType::Expected(E::Dur),
        "color" => ParamType::Expected(E::Color),
        "size" => ParamType::Expected(E::Size),
        "list" => ParamType::Expected(E::List),
        "any" => ParamType::Expected(E::Any),
        "action" => ParamType::ActionRef,
        "source" => ParamType::SourceRef,
        // A remote kind's param: the name of a `secret` item (schema 17).
        "secret" => ParamType::Expected(E::Ident),
        _ => return None,
    })
}

/// Describe a [`ParamType`] in human words for diagnostics. The
/// article-less `action name` / `source name` match the shape of
/// `type_name`'s "an integer" / "a number" so the caller can inline
/// either directly without stacking two articles.
fn param_type_name(ty: ParamType) -> &'static str {
    match ty {
        ParamType::Expected(e) => type_name(e),
        ParamType::ActionRef => "action name",
        ParamType::SourceRef => "source name",
    }
}

/// Collect `param` declarations from every top-level `component` and
/// `action` body. Unknown type names and duplicate param names in a
/// single declaration list are flagged here; a param without `: type`
/// (parser-level error) is skipped.
fn collect_params(doc: &Document, source: &SourceMap, diags: &mut Vec<Diagnostic>) -> Params {
    let mut params = Params::default();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        let (bucket, parent) = match b.kind.text.as_str() {
            "component" => (&mut params.components, "component"),
            "action" => (&mut params.actions, "action"),
            _ => continue,
        };
        let Some(name) = &b.name else { continue };
        let specs = collect_params_in_body(b, parent, source, diags);
        bucket.insert(name.text.clone(), specs);
    }
    remote::add_params(doc, source, &mut params);
    params
}

fn collect_params_in_body(
    b: &Block,
    parent: &str,
    source: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) -> Vec<ParamSpec> {
    let mut specs: Vec<ParamSpec> = Vec::new();
    let mut seen: HashMap<String, Span> = HashMap::new();
    for item in &b.items {
        let Item::Param(p) = item else { continue };
        if let Some(prev) = seen.get(&p.name.text) {
            diags.push(Diagnostic::new(
                p.name.span,
                format!(
                    "`param {}` is already declared (first at {})",
                    p.name.text,
                    source.location(*prev)
                ),
            ));
            continue;
        }
        // Reject component params whose name collides with a common UI
        // property or an event-handler key: on a component call the
        // common-key lookup wins, so the param would be unreachable
        // and silently return a type-mismatch for a value the user
        // typed against the param's declared type (schema.md section
        // 7 says common props on a call apply to the component's
        // root).
        if parent == "component" {
            if COMMON_UI_KEYS.iter().any(|r| r.name == p.name.text) {
                diags.push(Diagnostic::new(
                    p.name.span,
                    format!(
                        "`param {}` conflicts with the common UI property `{0}`; pick a different name",
                        p.name.text
                    ),
                ));
                continue;
            }
            if EVENT_HANDLER_KINDS.contains(&p.name.text.as_str()) {
                diags.push(Diagnostic::new(
                    p.name.span,
                    format!(
                        "`param {}` conflicts with the event-handler key `{0}`; pick a different name",
                        p.name.text
                    ),
                ));
                continue;
            }
        }
        if p.ty.text == "secret" && parent != "remote_kind" {
            diags.push(Diagnostic::new(p.ty.span, "`secret` params are for remote kinds only"));
        }
        let ty = match param_type_from_ident(&p.ty.text) {
            Some(t) => t,
            None => {
                diags.push(Diagnostic::new(
                    p.ty.span,
                    format!(
                        "unknown param type `{}`; valid: string, int, number, bool, duration, color, size, list, action, source, any",
                        p.ty.text
                    ),
                ));
                // Fall back to `any` so later checks don't double-fire.
                ParamType::Expected(ExpectedType::Any)
            }
        };
        // A theme color name means something in a component's UI only.
        let ty = match ty {
            ParamType::Expected(ExpectedType::Color) if parent != "component" => {
                ParamType::Expected(ExpectedType::HexColor)
            }
            t => t,
        };
        let asked = p
            .options
            .as_ref()
            .map(options_has_ask)
            .unwrap_or(false);
        seen.insert(p.name.text.clone(), p.name.span);
        specs.push(ParamSpec {
            name: p.name.text.clone(),
            ty,
            has_default: p.default.is_some(),
            asked,
            span: p.span,
        });
    }
    specs
}

fn options_has_ask(opts: &OptionsBlock) -> bool {
    for item in &opts.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        if key.text == "ask" {
            return true;
        }
    }
    false
}

/// Validate a call-site arg value against the declared param type. For
/// `action` / `source` params, cross-references bare identifiers against
/// the collected names; `{{ expr }}` is accepted as opaque.
fn check_param_arg(
    value_text: &str,
    value_span: Span,
    spec: &ParamSpec,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    match spec.ty {
        ParamType::Expected(ty) => {
            check_value_type(value_text, value_span, ty, true, diags);
        }
        ParamType::ActionRef => {
            // Bare ident is a reference. `{{ expr }}` is accepted
            // opaquely (CEL phase will type it).
            match classify(value_text) {
                ValueShape::Expression(_) | ValueShape::Interp(_) => {}
                ValueShape::Literal(text) => {
                    let t = text.trim();
                    if t == "none" {
                        return;
                    }
                    if !is_bare_ident(t) {
                        diags.push(Diagnostic::new(
                            value_span,
                            format!(
                                "expected an action name for `{}` (bare identifier or `{{{{ expr }}}}`), got `{text}`",
                                spec.name
                            ),
                        ));
                        return;
                    }
                    if !names.actions.contains(t) {
                        diags.push(Diagnostic::new(
                            value_span,
                            format!("unknown action `{t}`"),
                        ));
                    }
                }
            }
        }
        ParamType::SourceRef => {
            match classify(value_text) {
                ValueShape::Expression(_) | ValueShape::Interp(_) => {}
                ValueShape::Literal(text) => {
                    let t = text.trim();
                    if !is_bare_ident(t) {
                        diags.push(Diagnostic::new(
                            value_span,
                            format!(
                                "expected a source name for `{}` (bare identifier or `{{{{ expr }}}}`), got `{text}`",
                                spec.name
                            ),
                        ));
                        return;
                    }
                    if !names.sources.contains(t) {
                        diags.push(Diagnostic::new(
                            value_span,
                            format!("unknown source `{t}`"),
                        ));
                    }
                }
            }
        }
    }
}

fn resolve_refs(doc: &Document, names: &Names, diags: &mut Vec<Diagnostic>) {
    for item in &doc.items {
        match item {
            Item::Block(b) if b.kind.text == "remote_kind" => remote::resolve_kind_refs(b, names, diags),
            Item::Block(b) => resolve_refs_in_block(b, &b.kind.text, names, diags),
            // Top-level qualified / indexed shorthand carries the same
            // ref-containing keys as the opened form, so route them
            // through the shared dispatcher.
            Item::Assign(a) => match &a.target {
                AssignTarget::Qualified { category, key } => {
                    resolve_ref_shorthand(&category.text, key, &a.value, names, diags);
                }
                AssignTarget::Indexed { kind, key, .. } => {
                    resolve_ref_shorthand(&kind.text, key, &a.value, names, diags);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn resolve_ref_shorthand(
    parent_kind: &str,
    key: &crate::parse::Ident,
    value: &crate::parse::RawValue,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    let Some(kind) = assign_ref_kind(parent_kind, &key.text) else {
        return;
    };
    if matches!(
        crate::value::classify(&value.text),
        crate::value::ValueShape::Expression(_) | crate::value::ValueShape::Interp(_)
    ) {
        return;
    }
    match kind {
        RefKind::SourceList => check_source_list(&value.text, value.span, names, diags),
        _ => check_ref_name(&value.text, value.span, kind, names, diags),
    }
}

fn resolve_refs_in_block(
    b: &Block,
    parent_kind: &str,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    // Block-header refs: `on_tap action_name { ... }`, `then foo { ... }`,
    // etc. The `name` identifier (when present) is a ref.
    if let Some(name) = &b.name {
        if let Some(kind) = block_name_ref_kind(parent_kind, &b.kind.text) {
            check_ref_name(&name.text, name.span, kind, names, diags);
        }
    }

    for item in &b.items {
        resolve_refs_in_item(item, &b.kind.text, names, diags);
    }
}

fn resolve_refs_in_item(
    item: &Item,
    parent_kind: &str,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    match item {
        Item::Assign(a) => resolve_refs_in_assign(a, parent_kind, names, diags),
        Item::Block(child) => resolve_refs_in_block(child, parent_kind, names, diags),
        Item::Repeat(r) => {
            for inner in &r.items {
                resolve_refs_in_item(inner, parent_kind, names, diags);
            }
        }
        Item::Param(p) => {
            if let Some(opts) = &p.options {
                for inner in &opts.items {
                    resolve_refs_in_item(inner, "param", names, diags);
                }
            }
        }
        Item::Var(_) => {}
    }
}

fn resolve_refs_in_assign(
    a: &Assign,
    parent_kind: &str,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    let AssignTarget::Simple { key } = &a.target else {
        return;
    };
    let kind = match assign_ref_kind(parent_kind, &key.text) {
        Some(k) => k,
        None => return,
    };
    // A handler expression (`on_tap = {{ tap }}`): drawn once, so the
    // event's value is not there yet; and every action it can name
    // must exist.
    if let (RefKind::Action, crate::value::ValueShape::Expression(body)) = (kind, crate::value::classify(&a.value.text)) {
        let open = a.value.text.find("{{").unwrap_or(0) as u32;
        if let (Some(e), _) = crate::cel::parse_fragment(body, a.value.span.file, a.value.span.start + open + 2) {
            for (name, span) in crate::cel::idents(&e) {
                if matches!(name.as_str(), "event" | "value") {
                    diags.push(Diagnostic::new(span, format!(
                        "`{name}` is not known yet when `{}` picks its action (when the page is drawn); pick it from vars or params",
                        key.text
                    )));
                }
            }
            for (lit, span) in crate::cel::string_literals(&e) {
                if lit != "none" && !names.actions.contains(&lit) {
                    diags.push(Diagnostic::new(span, format!("unknown action `{lit}`")));
                }
            }
        }
        return;
    }
    // Expressions and interpolated text are opaque at this layer.
    if matches!(
        crate::value::classify(&a.value.text),
        crate::value::ValueShape::Expression(_) | crate::value::ValueShape::Interp(_)
    ) {
        return;
    }
    match kind {
        RefKind::SourceList => check_source_list(&a.value.text, a.value.span, names, diags),
        _ => check_ref_name(&a.value.text, a.value.span, kind, names, diags),
    }
}

/// Keys whose assigned value is a reference, by parent block kind.
fn assign_ref_kind(parent_kind: &str, key: &str) -> Option<RefKind> {
    // UI-tree event handlers and `goto` are context-independent.
    match key {
        "on_tap" | "on_hold" | "on_change" | "on_submit" => return Some(RefKind::Action),
        "goto" => return Some(RefKind::Page),
        _ => {}
    }
    match (parent_kind, key) {
        ("action", "then") | ("action", "else") => Some(RefKind::Action),
        ("action", "refresh") => Some(RefKind::SourceList),
        ("alert", "from") => Some(RefKind::Source),
        ("alert", "action") => Some(RefKind::Action),
        ("screen", "theme") => Some(RefKind::Theme),
        ("night", "theme") => Some(RefKind::Theme),
        ("theme", "extends") => Some(RefKind::Theme),
        ("app", "start_page") => Some(RefKind::Page),
        ("security", "pin") => Some(RefKind::Secret),
        ("button", "action") => Some(RefKind::Action),
        _ => None,
    }
}

/// Block-header refs: for `kind name { ... }` inside a parent, what
/// kind of name is `name`?
fn block_name_ref_kind(parent_kind: &str, child_kind: &str) -> Option<RefKind> {
    match child_kind {
        "on_tap" | "on_hold" | "on_change" | "on_submit" => return Some(RefKind::Action),
        _ => {}
    }
    match (parent_kind, child_kind) {
        ("action", "then") | ("action", "else") => Some(RefKind::Action),
        _ => None,
    }
}

fn check_ref_name(
    text: &str,
    span: Span,
    kind: RefKind,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    let t = text.trim();
    // Short-circuits defined by the spec.
    match kind {
        RefKind::Action if t == "none" => return,
        RefKind::Page if t == "back" => return,
        _ => {}
    }
    if t.is_empty() {
        return;
    }
    if !is_bare_ident(t) {
        // The per-key validator already flagged shape issues; don't
        // double-report.
        return;
    }
    let (table, label) = match kind {
        RefKind::Action => (&names.actions, "action"),
        RefKind::Source => (&names.sources, "source"),
        RefKind::SourceList => unreachable!("handled in check_source_list"),
        RefKind::Page => (&names.pages, "page"),
        RefKind::Theme => (&names.themes, "theme"),
        RefKind::Secret => (&names.secrets, "secret"),
    };
    if !table.contains(t) {
        let hint = match kind {
            RefKind::Theme => format!(
                " (no `theme {t} {{ }}` in the config and no `{}{}` beside it)",
                crate::assets::theme_prefix(t),
                crate::assets::THEME_FILE
            ),
            _ => String::new(),
        };
        diags.push(Diagnostic::new(
            span,
            format!("unknown {label} `{t}`{hint}"),
        ));
    }
}

fn check_source_list(
    text: &str,
    span: Span,
    names: &Names,
    diags: &mut Vec<Diagnostic>,
) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    // Either a single ident or a `[a, b, c]` literal.
    let items = match crate::value::split_list(t) {
        Some(items) => items,
        None => vec![t.to_string()],
    };
    // Collect missing names, dedup while preserving order so one
    // diagnostic lists them all at the value's span.
    let mut missing: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim();
        if item.is_empty() || !is_bare_ident(item) {
            continue;
        }
        if !names.sources.contains(item) && !missing.iter().any(|m| m == item) {
            missing.push(item.to_string());
        }
    }
    if missing.is_empty() {
        return;
    }
    let msg = if missing.len() == 1 {
        format!("unknown source `{}` in `refresh`", missing[0])
    } else {
        let list = missing
            .iter()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown sources in `refresh`: {list}")
    };
    diags.push(Diagnostic::new(span, msg));
}

// ---------------------------------------------------------------------------
// Phase 10: CEL parse + source cross-reference
// ---------------------------------------------------------------------------
//
// Walk the whole AST and, for every raw-value text or `{{ ... }}`
// expression on a `repeat`, parse the embedded CEL. Surface parse
// errors with file:line:col, and cross-reference `sources.<name>`
// against the collected source names.

fn check_cel_in_doc(doc: &Document, names: &Names, diags: &mut Vec<Diagnostic>) {
    for item in &doc.items {
        check_cel_in_item(item, names, false, diags);
    }
}

/// `in_env`: inside an `env { }` block, where (like in `run`) schema
/// 16 allows `secrets`.
fn check_cel_in_item(item: &Item, names: &Names, in_env: bool, diags: &mut Vec<Diagnostic>) {
    match item {
        Item::Assign(a) => {
            // `run = ...`, or overridden as `action[x]:run` /
            // `source[x]:run`: the one key besides `env` that may
            // use secrets.
            // In a remote kind, the request keys go to the command's
            // stdin, not on screen (schema 17).
            let is_run = match &a.target {
                AssignTarget::Simple { key } => matches!(key.text.as_str(), "run" | "body" | "send" | "url"),
                AssignTarget::Indexed { kind, key, .. } => {
                    key.text == "run" && matches!(kind.text.as_str(), "action" | "source")
                }
                _ => false,
            };
            check_cel_in_raw_value(&a.value, names, in_env || is_run, diags)
        }
        Item::Block(b) => {
            let in_env = in_env || b.kind.text == "env" || b.kind.text == "headers";
            for child in &b.items {
                check_cel_in_item(child, names, in_env, diags);
            }
        }
        Item::Repeat(r) => {
            // The repeat source is already a parsed Expr (body + full span).
            let body_offset = r.source.span.start + 2; // past `{{`
            let (expr, mut ds) =
                crate::cel::parse_fragment(&r.source.body, r.source.span.file, body_offset);
            diags.append(&mut ds);
            if let Some(e) = &expr {
                crate::cel::walk_source_refs(e, &names.sources, diags);
                crate::cel::check_calls(e, diags);
                flag_secrets(e, false, diags);
            }
            for inner in &r.items {
                check_cel_in_item(inner, names, in_env, diags);
            }
        }
        Item::Param(p) => {
            if let Some(default) = &p.default {
                check_cel_in_raw_value(default, names, false, diags);
            }
            if let Some(opts) = &p.options {
                for inner in &opts.items {
                    check_cel_in_item(inner, names, false, diags);
                }
            }
        }
        Item::Var(v) => check_cel_in_raw_value(&v.default, names, false, diags),
    }
}

fn check_cel_in_raw_value(
    v: &crate::parse::RawValue,
    names: &Names,
    secrets_ok: bool,
    diags: &mut Vec<Diagnostic>,
) {
    match classify(&v.text) {
        ValueShape::Literal(_) => {}
        ValueShape::Expression(body) => {
            // Translate the body's byte offset back to the enclosing
            // file. The lexer trims only ` ` and `\t` off the value
            // span, but `classify` runs `.trim()` which also strips
            // `\r` and other whitespace from the text before matching
            // `{{...}}`. Re-locating `{{` in the un-further-trimmed
            // `v.text` and adding 2 (for the `{{`) therefore maps to
            // the first byte of the body in the file regardless of
            // which flavor of leading whitespace classify's trim
            // dropped. If the shape is Expression, `find("{{")` is
            // guaranteed to succeed; the `.unwrap_or(0)` is defensive.
            let open_idx = v.text.find("{{").unwrap_or(0);
            let body_offset = v.span.start + open_idx as u32 + 2;
            let (expr, mut ds) =
                crate::cel::parse_fragment(body, v.span.file, body_offset);
            diags.append(&mut ds);
            if let Some(e) = expr {
                crate::cel::walk_source_refs(&e, &names.sources, diags);
                crate::cel::check_calls(&e, diags);
                flag_secrets(&e, secrets_ok, diags);
            }
        }
        ValueShape::Interp(_) => {
            // Multi-fragment: walk each `{{ ... }}` in the trimmed text.
            // The trimmed text begins at `v.span.start` (the lexer
            // already trimmed leading / trailing whitespace from the
            // span), so each fragment's local offset maps directly.
            let (exprs, mut ds) =
                crate::cel::parse_interp(&v.text, v.span.file, v.span.start);
            diags.append(&mut ds);
            for e in &exprs {
                crate::cel::walk_source_refs(e, &names.sources, diags);
                crate::cel::check_calls(e, diags);
                flag_secrets(e, secrets_ok, diags);
            }
        }
    }
}

/// Schema 16: `secrets` only in `run` and `env`. Anywhere else a
/// secret could end up on screen, in a message or in the state file.
fn flag_secrets(e: &crate::cel::Expr, secrets_ok: bool, diags: &mut Vec<Diagnostic>) {
    if secrets_ok {
        return;
    }
    if let Some(span) = crate::cel::find_ident(e, "secrets") {
        diags.push(Diagnostic::new(
            span,
            "`secrets` can only be used in `run` and `env`",
        ));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn tmp_file(label: &str, text: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("testdata");
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let dir = base.join(format!("paneld-sch-{label}-{pid}-{n}"));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("root.conf");
        fs::write(&p, text).unwrap();
        p
    }

    fn check_text(text: &str) -> Vec<String> {
        let p = tmp_file("one", text);
        let (map, doc, mut diags) = load(&p).unwrap();
        diags.extend(check(&doc, &map));
        let out: Vec<_> = diags
            .iter()
            .map(|d| format!("{}: {}", map.location(d.span), d.message))
            .collect();
        let _ = fs::remove_dir_all(p.parent().unwrap());
        out
    }

    fn assert_msg(diags: &[String], needle: &str) {
        assert!(
            diags.iter().any(|m| m.contains(needle)),
            "no diagnostic contained `{needle}`; diags: {diags:?}"
        );
    }

    /// Each misplaced or malformed item, and the message it gets.
    #[test]
    fn every_structural_error_has_its_message() {
        let cases: &[(&str, &str)] = &[
            ("source a.b { run = x }\n", "a `source` name cannot contain `.`"),
            ("action a { var v: int = 1 }\n", "`var` is only allowed at the top level, not inside `action`"),
            ("action a { repeat x in {{ [1] }} { } }\n", "`repeat` is not allowed inside `action`"),
            ("action a { $v = 1 }\n", "`$var` definitions are only allowed at the top level"),
            ("action a { app:x = 1 }\n", "`category:key` shorthand is only allowed at the top level"),
            ("action a { theme[x]:k = 1 }\n", "`kind[name]:key` shorthand is only allowed at the top level"),
            ("action a { env { x { } } }\n", "nested blocks are not allowed here"),
            ("action a { set foo { } }\n", "`set` does not take a name here"),
            ("source s { run = x\n param p: int }\n", "`param` is only allowed inside `action` or `component`, not `source`"),
            ("action a { param p: int { x { } } }\n", "`param` options block only accepts `key = value` entries"),
            ("action a { param p: int { app:x = 1 } }\n", "`param` options block entries must be simple `key = value`"),
            ("app { timeout = soon }\n", "expected a duration"),
            ("app { max_running = x }\n", "an integer"),
            ("app { max_output = lots }\n", "a byte count"),
            ("action a { goto = 1x }\n", "expected an identifier, got `1x`"),
            ("action a { refresh = 1x }\n", "expected an identifier or a list of identifiers, got `1x`"),
            ("page p { param x: int }\n", "`param` is not allowed in a `page` body (use a `component`)"),
            ("page p { var v: int = 1 }\n", "`var` declarations are only allowed at the top level"),
            ("page p { app:x = 1 }\n", "qualified assignments are only allowed at the top level"),
            ("component c { title = x\n text { value = y } }\n", "a `component` body only accepts `param` and a single UI root node"),
            ("theme t { param p: int }\n", "`param` is only allowed inside a `component` body"),
            ("theme t { repeat x in {{ [1] }} { } }\n", "`repeat` is not allowed inside a `theme` body"),
            ("theme t { style .c { x { } } }\n", "a `style` body does not contain nested blocks"),
            ("theme t { style .c { param p: int } }\n", "`param` is not allowed in a `style` body"),
            ("theme t { style .c { repeat x in {{ [1] }} { } } }\n", "`repeat` is not allowed in a `style` body"),
            ("theme t { style .1 { radius = 1 } }\n", "expected a class name after `.`"),
            ("theme t { style 1:pressed { radius = 1 } }\n", "expected a node kind before `:`"),
            ("theme t { style a b { radius = 1 } }\n", "invalid style selector"),
            ("page p { text foo { value = x } }\n", "`text` does not take a name"),
            ("component c { column { param p: int } }\n", "`param` is only allowed at the start of a `component` body"),
            ("page p { repeat x in {{ [1] }} { value = 1 } }\n", "`repeat` body only contains UI nodes, not property assignments"),
            ("page p { repeat x in {{ [1] }} { param q: int } }\n", "`param` is not allowed inside a `repeat` body"),
            ("action a { }\npage p { button { on_tap a { x { } } } }\n", "an action call only takes `key = value` args"),
            ("page p { button { on_tap { app:x = 1 } } }\n", "an action-call body only contains simple `key = value`"),
            ("page p { button { on_tap { goto { } } } }\n", "is not valid inside an inline action; use `set` or `env`"),
            ("component c { param v: int\n text { value = x } }\npage p { c { v = 1\n app:x = 2 } }\n", "an action/component call only contains simple `key = value` args"),
        ];
        let mut missing = Vec::new();
        for (src, needle) in cases {
            let d = check_text(src);
            if !d.iter().any(|m| m.contains(needle)) {
                missing.push(format!("`{}` -> wanted `{needle}`, got {d:?}", src.trim()));
            }
        }
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    #[test]
    fn a_value_in_a_hand_written_ssh_command_is_refused() {
        let flagged = |run: &str| {
            let d = check_text(&format!(
                "action a {{ param x: string\n param n: int\n param u: string {{ options = [a, b] }}\n param p: string {{ pattern = \"[a-z]+\" }}\n run = {run} }}\n"
            ));
            d.iter().find(|m| m.contains("in the command of an `ssh`") || m.contains("in ssh's options or host")).cloned()
        };
        // In the remote command.
        for run in [
            "ssh host systemctl restart {{ x }}",
            "ssh -p 22 -i /k -o BatchMode=yes admin@h echo {{ x }}",
            "sudo -n ssh -- h rm {{ x }}",
            "sudo -u panel timeout 5 ssh h rm {{ x }}",
            "/usr/bin/ssh h {{ x }}",
            "sshpass -f /k ssh h rm {{ x }}",
            "autossh -M 0 h rm {{ x }}",
            "command ssh h rm {{ x }}",
            "flock /tmp/l ssh h rm {{ x }}",
        ] {
            assert!(flagged(run).is_some_and(|m| m.contains("shell code")), "{run}");
        }
        // In the host or options: read as an option.
        for run in ["ssh {{ x }} uptime", "ssh -l {{ x }} h uptime", "ssh -vl{{ x }} h uptime", "ssh -o{{ x }} h uptime"] {
            assert!(flagged(run).is_some_and(|m| m.contains("read as an option")), "{run}");
        }
        // Fine: no value, a host after `--`, values that cannot carry
        // code, and commands that are not ssh.
        for run in [
            "ssh h uptime",
            "ssh -- {{ x }} uptime",
            "ssh h renice -n {{ n }} 1",
            "ssh -vp 22 h systemctl restart {{ u }}",
            "ssh h systemctl restart {{ p }}",
            "echo ssh h {{ x }}",
            "scp {{ x }} h:/tmp",
        ] {
            assert_eq!(flagged(run), None, "{run}");
        }
    }

    #[test]
    fn dialog_backdrop_takes_transparent_a_color_or_a_blur() {
        for ok in ["transparent", "#102030", "#10203080", "blur 8", "blur 12px"] {
            let d = check_text(&format!("theme t {{ dialog_backdrop = {ok} }}\n"));
            assert!(d.is_empty(), "{ok}: {d:?}");
        }
        for bad in ["red", "#12", "blur", "blur 21", "fog"] {
            let d = check_text(&format!("theme t {{ dialog_backdrop = {bad} }}\n"));
            assert!(d.iter().any(|m| m.contains("`dialog_backdrop` is")), "{bad}: {d:?}");
        }
    }

    #[test]
    fn a_param_pattern_must_compile_and_its_default_match() {
        let d = check_text("action a { param p: string { pattern = \"[a-z\" }\n run = x }\n");
        assert_msg(&d, "`pattern` is not a valid regex");
        let d = check_text("action a { param p: string = Bad { pattern = \"[a-z]+\" }\n run = x }\n");
        assert_msg(&d, "the default of `p` does not match its `pattern`");
        assert!(check_text("action a { param p: string = ok { pattern = \"[a-z]+\" }\n run = x }\n").is_empty());
    }

    #[test]
    fn a_handler_expression_names_known_actions_and_no_event_value() {
        let d = check_text(
            "action bump { }\npage p {\n button { on_tap = {{ vars.x ? 'bmup' : 'bump' }} }\n\
             slider { on_change = {{ value > 5 ? 'bump' : 'none' }} }\n button { on_tap = {{ true ? 'bump' : 'none' }} }\n}\n",
        );
        assert_msg(&d, "unknown action `bmup`");
        assert_msg(&d, "`value` is not known yet when `on_change` picks its action");
        assert_eq!(d.len(), 2, "{d:?}");
    }

    #[test]
    fn a_state_style_takes_only_what_can_change_with_the_state() {
        let d = check_text("theme t { style .card:checked { visible = false; background = #000 }\n style button:pressed { width = 400 } }\n");
        assert_msg(&d, "`visible` cannot change with a state");
        assert_msg(&d, "`width` cannot change with a state");
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(check_text("theme t { style .card { visible = false; width = 10 } }\n").is_empty());
    }

    #[test]
    fn function_calls_are_checked_at_load() {
        let d = check_text(
            "page p {\n text { value = {{ rond(1.5) }} }\n text { value = {{ round(1, 2, 3) }} }\n\
             text { value = A {{ [1].sizes() }} }\n text { value = {{ [1].map(1, 2) }} }\n text { value = {{ 'a'.split() }} }\n text { value = {{ format(1536, 'bytes') + duration(5) }} }\n\
             text { color = {{ mix(color('accent'), '#000', clamp(0.2, 0, 1)) }} }\n}\n",
        );
        assert_msg(&d, ":2:");
        assert_msg(&d, "unknown function `rond`; known: string, int");
        assert_msg(&d, "`round` takes 1 or 2 argument(s), not 3");
        assert_msg(&d, "unknown method `.sizes()`");
        assert_msg(&d, "`.map()` takes a name first");
        assert_msg(&d, "`.split()` takes 1 argument(s), not 0");
        assert_eq!(d.len(), 5, "the known functions pass: {d:?}");
    }

    #[test]
    fn clean_doc_has_no_diagnostics() {
        let text = r#"
secret adguard { env = ADGUARD_AUTH }
source cpu_temp {
    poll  = 30s
    run   = cat /sys/x
    parse = number
    value = {{ out / 1000.0 }}
}
screen { idle = 2m }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn unknown_top_level_item_is_flagged() {
        let diags = check_text("wat home { }\n");
        assert_msg(&diags, "unknown top-level item `wat`");
    }

    #[test]
    fn named_kind_without_name_is_flagged() {
        let diags = check_text("source { run = cat /x }\n");
        assert_msg(&diags, "`source` needs a name");
    }

    #[test]
    fn category_kind_with_name_is_flagged() {
        let diags = check_text("screen home { idle = 2m }\n");
        assert_msg(&diags, "settings block");
    }

    #[test]
    fn duplicate_named_item_is_flagged() {
        let diags = check_text("source cpu { run = a }\nsource cpu { run = b }\n");
        assert_msg(&diags, "`source cpu` is already defined");
    }

    #[test]
    fn unknown_top_level_option_is_flagged() {
        let diags = check_text("whatever = 3\n");
        assert_msg(&diags, "unknown top-level option `whatever`");
    }

    #[test]
    fn param_at_top_level_is_flagged() {
        let diags = check_text("param x: int = 1\n");
        assert_msg(&diags, "`param` is only allowed");
    }

    #[test]
    fn repeat_at_top_level_is_flagged() {
        let diags = check_text("repeat x in {{ 1 }} { }\n");
        assert_msg(&diags, "`repeat` is only allowed");
    }

    #[test]
    fn category_qualifier_with_unknown_category_is_flagged() {
        let diags = check_text("foo:bar = 1\n");
        assert_msg(&diags, "unknown settings category `foo`");
    }

    #[test]
    fn indexed_qualifier_on_category_suggests_colon_form() {
        let diags = check_text("screen[home]:idle = 2m\n");
        assert_msg(&diags, "settings block");
        assert_msg(&diags, "screen:key");
    }

    #[test]
    fn indexed_qualifier_with_unknown_kind_is_flagged() {
        let diags = check_text("widget[foo]:x = 1\n");
        assert_msg(&diags, "unknown item kind `widget`");
    }

    #[test]
    fn top_level_var_declaration_is_accepted() {
        let diags = check_text("var tab: int = 0\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // --- Phase 5: per-key validation ---

    #[test]
    fn source_without_run_is_flagged() {
        let diags = check_text("source cpu { poll = 30s }\n");
        assert_msg(&diags, "missing required key `run`");
    }

    #[test]
    fn source_unknown_key_is_flagged() {
        let diags = check_text("source cpu { run = cat; nope = 1 }\n");
        assert_msg(&diags, "unknown key `nope`");
    }

    #[test]
    fn source_bad_parse_mode_is_flagged() {
        let diags = check_text("source cpu { run = cat; parse = xml }\n");
        assert_msg(&diags, "expected one of");
        assert_msg(&diags, "got `xml`");
    }

    #[test]
    fn source_bad_duration_is_flagged() {
        let diags = check_text("source cpu { run = cat; poll = soon }\n");
        assert_msg(&diags, "expected a duration");
    }

    #[test]
    fn source_bool_valid() {
        let diags = check_text("source cpu { run = cat; stream = true }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn source_bool_invalid() {
        let diags = check_text("source cpu { run = cat; stream = yes }\n");
        assert_msg(&diags, "expected a bool");
    }

    #[test]
    fn source_env_subblock_accepted() {
        let diags = check_text("source cpu { run = cat; env { KEY = value } }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn source_unknown_subblock_flagged() {
        let diags = check_text("source cpu { run = cat; wat { x = 1 } }\n");
        assert_msg(&diags, "unknown nested block `wat`");
    }

    #[test]
    fn screen_night_subblock_accepted() {
        let diags = check_text(
            "theme dim { }\nscreen { idle = 2m; night { from = 22:00; to = 07:00; theme = dim } }\n",
        );
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn screen_night_unknown_key_flagged() {
        let diags = check_text("screen { night { nope = 1 } }\n");
        assert_msg(&diags, "unknown key `nope`");
    }

    #[test]
    fn screen_bad_first_tap_flagged() {
        let diags = check_text("screen { first_tap = never }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn display_rotation_valid_values() {
        let diags = check_text("display { rotation = 90 }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn display_touch_none_and_bad_rotation() {
        let diags = check_text("display { touch = none; rotation = 270 }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
        let diags = check_text("display { rotation = 45 }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn display_bad_backend_flagged() {
        let diags = check_text("display { backend = qt }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn app_log_level_valid() {
        let diags = check_text("app { log = debug }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn app_bad_log_level_flagged() {
        let diags = check_text("app { log = verbose }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn app_max_output_bytes_valid() {
        let diags = check_text("app { max_output = 2MiB }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn app_bad_bytes_flagged() {
        let diags = check_text("app { max_output = 2gigs }\n");
        assert_msg(&diags, "expected a byte count");
    }

    #[test]
    fn secret_env_only_valid() {
        let diags = check_text("secret s { env = X }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn secret_both_env_and_file_flagged() {
        let diags = check_text("secret s { env = X; file = /r }\n");
        assert_msg(&diags, "must set only one of `env` or `file`");
    }

    #[test]
    fn secret_neither_env_nor_file_flagged() {
        let diags = check_text("secret s { }\n");
        assert_msg(&diags, "must set exactly one of `env` or `file`");
    }

    #[test]
    fn action_param_options_accepted() {
        let text = r#"
action pause {
    param minutes: int { ask = select; options = [5, 30]; label = Minutes }
    run = sh -c 'echo'
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn action_param_bad_ask_flagged() {
        let text = r#"
action a {
    param x: int { ask = nope }
    run = sh -c 'echo'
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn action_param_unknown_option_flagged() {
        let text = r#"
action a {
    param x: int { squiggle = yes }
    run = sh -c 'echo'
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown `param` option `squiggle`");
    }

    #[test]
    fn expression_value_ok_where_allowed() {
        let text = r#"
source cpu {
    run = cat
    value = {{ out / 1000.0 }}
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn expression_not_ok_for_duration() {
        let diags = check_text("source cpu { run = cat; poll = {{ x }} }\n");
        assert_msg(&diags, "expressions are not accepted");
    }

    #[test]
    fn qualified_top_level_assignment_is_validated() {
        let diags = check_text("screen:idle = soon\n");
        assert_msg(&diags, "expected a duration");
    }

    #[test]
    fn qualified_top_level_unknown_key_is_flagged() {
        let diags = check_text("screen:bogus = 1\n");
        assert_msg(&diags, "unknown key `bogus` for `screen`");
    }

    #[test]
    fn qualified_top_level_valid_passes() {
        let diags = check_text("screen:idle = 2m\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn indexed_top_level_assignment_is_validated() {
        let diags = check_text("source[cpu]:parse = xml\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn indexed_top_level_unknown_key_is_flagged() {
        let diags = check_text("source[cpu]:nope = 1\n");
        assert_msg(&diags, "unknown key `nope` for `source`");
    }

    #[test]
    fn qualified_bypass_on_unknown_category_still_flags_the_category() {
        let diags = check_text("wat:idle = 2m\n");
        assert_msg(&diags, "unknown settings category `wat`");
    }

    #[test]
    fn empty_command_value_error_has_correct_grammar() {
        let diags = check_text("source cpu { run = \"\" }\n");
        // Should NOT say "a non-empty a command"; must read naturally.
        assert!(
            diags.iter().all(|m| !m.contains("non-empty a ")),
            "double-article grammar bug returned: {diags:?}"
        );
    }

    #[test]
    fn component_and_page_bodies_are_not_validated_yet() {
        // Phase 6 added page/component body validation. Theme bodies
        // remain deferred to a later phase.
        let text = r#"
component foo {
    param label: string
    row { text { value = Y } }
}
page home {
    grid { columns = 3 }
}
theme light {
    accent = #44aaff
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // --- Phase 6: alert + reminder ---

    #[test]
    fn alert_valid_condition_mode() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
alert pi_hot {
    when     = {{ sources.cpu_temp.value > 70 }}
    for      = 2m
    severity = warning
    title    = Pi hot
    text     = {{ round(sources.cpu_temp.value, 1) }} C
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn alert_bad_severity_flagged() {
        let diags = check_text("alert a { severity = catastrophic }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn alert_bad_duration_flagged() {
        let diags = check_text("alert a { for = soon }\n");
        assert_msg(&diags, "expected a duration");
    }

    #[test]
    fn alert_unknown_key_flagged() {
        let diags = check_text("alert a { squiggle = yes }\n");
        assert_msg(&diags, "unknown key `squiggle`");
    }

    #[test]
    fn reminder_valid() {
        let text = r#"
reminder restore_test {
    every = 1 month
    title = Test a restore
    link  = https://example.org/x
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn secrets_allowed_in_an_overridden_run() {
        let src = "secret tok { env = T }\naction a { run = /bin/true }\naction[a]:run = curl -H {{ secrets.tok }} x\n";
        assert!(check_text(src).is_empty(), "{:?}", check_text(src));
        let bad = "secret tok { env = T }\naction a { run = /bin/true }\naction[a]:done = {{ secrets.tok }}\n";
        assert!(!check_text(bad).is_empty());
    }

    #[test]
    fn param_defaults_and_ask_are_checked() {
        assert_msg(&check_text("action a { param m: int = abc }\n"), "expected an integer");
        assert_msg(&check_text("action a { param m: int = 20 { max = 10 } }\n"), "outside its `min` / `max`");
        assert_msg(&check_text("action a { param m: int { ask = select } }\n"), "needs `options`");
        assert_msg(&check_text("component c { param m: int { ask = slider }\n text { value = x } }\n"), "action params only");
        assert!(check_text("action a { param m: int = 5 { min = 1; max = 10; ask = select; options = [5, 10] } }\n").is_empty());
    }

    #[test]
    fn interpolating_into_a_shell_script_is_flagged() {
        assert_msg(&check_text("action a { run = sh -c 'echo {{ vars.x }} > /tmp/f' }\n"), "pastes the value");
        assert_msg(&check_text("screen { dim = /bin/bash -c 'echo {{ level }}' }\n"), "pastes the value");
        // As an argument after the script, or with no shell: fine.
        assert!(check_text("action a { run = sh -c 'echo \"$$1\" > /tmp/f' sh {{ vars.x }} }\n").is_empty());
        // (Not an inline script: ssh's `-c` is a cipher; `--` keeps the
        // host from being read as an option.)
        assert!(check_text("action a { run = ssh -c aes128-ctr -- {{ vars.x }} }\n").is_empty());
        // Review round 7: ways around the first version.
        for cmd in [
            "sh -ec 'echo {{ vars.x }}'",
            "bash -lc 'echo {{ vars.x }}'",
            "/usr/bin/env sh -c 'echo {{ vars.x }}'",
            "sudo -u pi bash -c 'echo {{ vars.x }}'",
            "timeout 5 sh -c 'echo {{ vars.x }}'",
            "busybox sh -c 'echo {{ vars.x }}'",
            "python3 -c 'print({{ vars.x }})'",
            "perl -e 'print {{ vars.x }}'",
        ] {
            assert_msg(&check_text(&format!("action a {{ run = {cmd} }}\n")), "pastes the value");
        }
        // Review round 8: double quotes, option arguments, `{{ }}` in a
        // wrapper's args, more wrappers.
        for cmd in [
            "sh -c \"echo {{ vars.x }} > /tmp/out\"",
            "sh -o pipefail -c 'echo {{ vars.x }}'",
            "python3 -W ignore -c 'print({{ vars.x }})'",
            "sudo -u {{ vars.x }} sh -c 'echo {{ vars.x }}'",
            "docker exec web sh -c 'echo {{ vars.x }}'",
            "su -c 'echo {{ vars.x }}'",
            "xargs sh -c 'echo {{ vars.x }}'",
        ] {
            assert_msg(&check_text(&format!("action a {{ run = {cmd} }}\n")), "pastes the value");
        }
        // Review round 9: options without an argument, more script
        // options, `env -S`, and no runner by a relative basename.
        for cmd in [
            "python3 -I -c 'print({{ vars.x }})'",
            "python3 -O -c 'print({{ vars.x }})'",
            "perl -E 'say {{ vars.x }}'",
            "node -p '{{ vars.x }}'",
            "bash --rcfile /dev/null -c 'echo {{ vars.x }}'",
            "su - pi -c 'echo {{ vars.x }}'",
            "env -S \"sh -c 'echo {{ vars.x }}'\"",
        ] {
            assert_msg(&check_text(&format!("action a {{ run = {cmd} }}\n")), "pastes the value");
        }
        assert!(check_text("action a { run = kubectl exec deploy/python -c {{ vars.x }} -- ls }\n").is_empty());
        assert!(check_text("action a { run = php -dfoo=bar {{ vars.x }} }\n").is_empty());
        assert!(check_text("action a { run = env FOO=1 mycmd {{ vars.x }} }\n").is_empty());
        assert!(check_text("action a { run = sudo systemctl restart {{ vars.x }} }\n").is_empty());
        assert!(check_text("action a { run = python3 /opt/x.py {{ vars.x }} }\n").is_empty());
        assert!(check_text("action a { run = curl -H {{ vars.x }} http://x }\n").is_empty());
    }

    #[test]
    fn reminder_schedule_and_dates_checked() {
        assert!(check_text("reminder r { once = 2026-12-01 }\n").is_empty());
        assert!(check_text("reminder r { every = 1w\n start = 2026-12-01 }\n").is_empty());
        assert_msg(&check_text("reminder r { once = 2026-02-30 }\n"), "expected a date");
        assert_msg(&check_text("reminder r { title = x }\n"), "needs `every`");
        assert_msg(&check_text("reminder r { every = 1d\n once = 2026-12-01 }\n"), "both `every` and `once`");
        assert_msg(&check_text("reminder r { once = 2026-12-01\n start = 2026-11-01 }\n"), "`start` only applies");
        assert_msg(&check_text("alert reminder { when = {{ true }} }\n"), "is reserved");
    }

    #[test]
    fn reminder_bad_duration_flagged() {
        let diags = check_text("reminder r { every = soon }\n");
        assert_msg(&diags, "expected a duration");
    }

    // --- Phase 6: page body ---

    #[test]
    fn page_valid_keys_and_ui_tree() {
        let text = r#"
page home {
    title = Home
    icon  = home
    nav   = 1
    grid { columns = 3; text { value = Hello } }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn page_unknown_key_flagged() {
        let diags = check_text("page home { wat = 1 }\n");
        assert_msg(&diags, "unknown page key `wat`");
    }

    #[test]
    fn page_bad_nav_type_flagged() {
        let diags = check_text("page home { nav = many }\n");
        assert_msg(&diags, "expected an integer");
    }

    // --- Phase 6: UI building blocks ---

    #[test]
    fn ui_text_valid_properties() {
        let text = r#"
page home {
    text { value = Hi; align = center; overflow = dots; color = #44aaff }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn ui_text_unknown_property_flagged() {
        let diags = check_text("page home { text { value = Hi; wibble = 1 } }\n");
        assert_msg(&diags, "unknown property `wibble` on `text`");
    }

    #[test]
    fn ui_text_bad_align_flagged() {
        let diags = check_text("page home { text { align = sideways } }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn ui_grid_accepts_children() {
        let text = r#"
page home {
    grid {
        columns = 3
        text { value = A }
        text { value = B }
    }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn ui_text_rejects_children() {
        let diags = check_text("page home { text { value = Hi; column { } } }\n");
        assert_msg(&diags, "does not accept child UI nodes");
    }

    #[test]
    fn ui_chart_series_accepted() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
page home {
    chart {
        type = line
        series { values = {{ sources.cpu_temp.history }}; color = #44aaff }
    }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn unknown_ui_node_flagged() {
        let diags = check_text("page home { wobble { } }\n");
        assert_msg(&diags, "unknown UI node or component `wobble`");
    }

    #[test]
    fn user_component_call_is_recognized() {
        let text = r#"
component stat_tile {
    param label: string
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn slot_outside_component_is_flagged() {
        let diags = check_text("page home { slot { } }\n");
        assert_msg(&diags, "`slot` is only allowed inside a `component`");
    }

    #[test]
    fn slot_inside_component_is_accepted() {
        let text = r#"
component card {
    column { slot { } }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_missing_root_is_flagged() {
        let diags = check_text("component foo { param x: int }\n");
        assert_msg(&diags, "missing its UI root node");
    }

    #[test]
    fn component_multiple_roots_is_flagged() {
        let text = r#"
component foo {
    text { value = A }
    text { value = B }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "exactly one UI root node");
    }

    // --- Phase 6: event handlers ---

    #[test]
    fn on_tap_simple_assign_is_accepted() {
        let text = r#"
action refresh_all { run = cat }
page home {
    text { value = Hi; on_tap = refresh_all }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn on_tap_named_action_call_is_accepted() {
        let text = r#"
action vm_stop { param vm: string; run = cat }
page home {
    text { value = Hi; on_tap vm_stop { vm = {{ item.id }} } }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn on_tap_inline_action_valid_keys() {
        let text = r#"
page settings { }
page home {
    text { value = Hi; on_tap { goto = settings } }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn layout_values_are_checked_as_render_reads_them() {
        let ok = "page home {\n\
                  grid { columns = [120, 1fr, 2fr, content]; rows = 2; row_gap = 4; column_gap = 8\n\
                         rect { cell = 1 0 2 2; padding = 4 8 } rect { cell = 0 1; padding = 1 2 3 4 } }\n\
                  stack { rect { anchor = bottom_right; x = -8; y = 50% } }\n\
                  arc { value = 1; start = 180; end = 0; thickness = 12 }\n\
                  text { overflow = dots; align_self = end } }\n";
        let diags = check_text(ok);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
        let bad = |prop: &str| check_text(&format!("page home {{ grid {{ rect {{ {prop} }} }} }}\n"));
        assert_msg(&bad("padding = 1 2 3"), "expected 1, 2 (vertical horizontal) or 4");
        assert_msg(&bad("padding = 50%"), "expected 1, 2 (vertical horizontal) or 4");
        assert_msg(&bad("cell = 1"), "expected `col row` or `col row colspan rowspan`");
        assert_msg(&bad("cell = 0 0 0 1"), "expected `col row` or `col row colspan rowspan`");
        assert_msg(&bad("x = grow"), "expected an offset");
        assert_msg(&bad("anchor = middle"), "expected one of `top_left`");
        let grid = |cols: &str| check_text(&format!("page home {{ grid {{ columns = {cols} }} }}\n"));
        assert_msg(&grid("[100, 1x]"), "expected a count (`3`) or a list of tracks");
        assert_msg(&grid("0"), "expected a count (`3`) or a list of tracks");
        assert!(grid("1fr 1fr").is_empty());
        assert_msg(&check_text("page home { arc { thickness = 50% } }\n"), "expected an integer of at least 1");
        assert_msg(&check_text("page home { line { thickness = 0 } }\n"), "expected an integer of at least 1");
        assert!(check_text("page home { stack { rect { x = -10%; y = -8 } } }\n").is_empty());
        let cells = |grid: &str| check_text(&format!("page home {{ grid {{ {grid} }} }}\n"));
        assert_msg(&cells("rect { cell = 2 0 }"), "outside the grid's 2 columns");
        assert_msg(&cells("columns = [1fr, 1fr, 1fr]; rect { cell = 1 0 3 1 }"), "outside the grid's 3 columns");
        assert_msg(&cells("columns = 3; rows = 2; repeat i in {{ [1] }} { rect { cell = 0 1 1 2 } }"), "outside the grid's 2 rows");
        assert!(cells("columns = 4; rect { cell = 1 5 3 1 }").is_empty(), "rows grow when not set");
        assert_msg(&check_text("page home { row { gap = 50% } }\n"), "expected an integer");
        assert_msg(&check_text("page home { grid { row_gap = grow } }\n"), "expected an integer");
        assert!(check_text("page home { row { gap = {{ theme.gap }} } }\n").is_empty());
    }

    #[test]
    fn on_tap_inline_action_unknown_key_flagged() {
        let diags = check_text("page home { text { on_tap { wobble = 1 } } }\n");
        assert_msg(&diags, "unknown inline action key `wobble`");
    }

    // --- Phase 6: repeat ---

    #[test]
    fn on_tap_action_param_expression_is_accepted() {
        // From schema.md section 7: a component takes `tap: action` and
        // forwards it with `on_tap = {{ tap }}`.
        let text = r#"
component stat_tile {
    param tap: action = none
    column { on_tap = {{ tap }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn on_change_expression_on_toggle_is_accepted() {
        let text = r#"
component my_toggle {
    param flip: action = none
    toggle { value = true; on_change = {{ flip }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn repeat_in_terminal_ui_node_is_flagged() {
        let diags = check_text(
            "page home { text { value = Hi; repeat x in {{ xs }} { text { value = {{ x }} } } } }\n",
        );
        assert_msg(&diags, "`text` does not accept child UI nodes");
    }

    #[test]
    fn invalid_hex_color_literal_is_flagged() {
        let diags = check_text("page home { text { color = banana } }\n");
        assert_msg(&diags, "unknown theme color `banana`");
        let diags = check_text("page home { text { color = #12 } }\n");
        assert_msg(&diags, "expected a hex color");
    }

    #[test]
    fn icon_names_are_built_ins_unless_the_theme_has_an_icons_folder() {
        let ok = "page home { icon = wifi\n icon { name = home; size = 48; color = accent }\n\
                  icon { name = {{ 'any' + 'thing' }}; size = {{ 20 }} }\n\
                  button { icon = power; label = Off } }\n";
        let diags = check_text(ok);
        assert!(diags.is_empty(), "{diags:?}");
        assert_msg(&check_text("page home { icon { name = hom } }\n"), "unknown icon `hom`");
        assert_msg(&check_text("page home { icon { name = hom } }\n"), "did you mean `home`?");
        assert_msg(&check_text("page home { button { icon = setings } }\n"), "did you mean `settings`?");
        assert_msg(&check_text("page home { icon = rocket\n text { value = x } }\n"), "unknown icon `rocket`");
        assert_msg(&check_text("page home { icon { name = home; size = 0 } }\n"), "at least 1");
        // A theme icons folder may add names: not known until render.
        let diags = check_text("theme t { icons = icons }\npage home { icon = rocket\n icon { name = rocket } }\n");
        assert!(diags.is_empty(), "{diags:?}");
        // `icons = builtin` (the default) adds none.
        assert_msg(&check_text("theme t { icons = builtin }\npage home { icon { name = rocket } }\n"), "unknown icon");
    }

    #[test]
    fn theme_color_names_are_colors() {
        // Built-in tokens, and any token a theme defines (here `track`,
        // also through `theme[x]:key`).
        let text = "theme t { track = #333333 }\ntheme[t]:glow = #ff000080\n\
                    page home { text { value = Hi; color = accent }\n\
                    image { src = a.png; recolor = muted }\n\
                    bar { value = 1; track = track }\n arc { value = 1; track = glow } }\n";
        let diags = check_text(text);
        assert!(diags.is_empty(), "{diags:?}");
        // A theme token may name another token.
        assert!(check_text("theme t { accent = critical }\n").is_empty());
        // A token of another document is not known here.
        assert_msg(&check_text("page home { text { color = track } }\n"), "unknown theme color `track`");
        // An action param's value goes to a command: hex only.
        let diags = check_text("action a { param tint: color = accent\n run = echo {{ tint }} }\n");
        assert_msg(&diags, "expected a hex color");
        let diags = check_text("component c { param tint: color = accent\n text { value = x; color = {{ tint }} } }\n");
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn theme_color_names_resolve_in_the_active_chain_to_a_hex() {
        let head = "screen:theme = b\ntheme a { track = #ff0000 }\ntheme b { extends = c; accent = gap }\ntheme c { glow = #00ff00 }\n";
        let page = |c: &str| check_text(&format!("{head}page home {{ text {{ value = x; color = {c} }} }}\n"));
        // Non-color built-ins and tokens of an inactive theme are not colors.
        assert_msg(&page("gap"), "theme token `gap` is `8`, not a color");
        assert_msg(&page("icons"), "theme token `icons` is `builtin`, not a color");
        assert_msg(&page("track"), "unknown theme color `track`");
        // `accent = gap` in the active theme: reported on that line too.
        let diags = page("glow");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_msg(&diags, "theme token `gap` is `8`, not a color");
        // A cycle, also through two names.
        assert_msg(&check_text("theme t { accent = accent }\n"), "refers to itself: accent -> accent");
        assert_msg(&check_text("theme t { ok = info; info = ok }\npage p { text { value = x; color = ok } }\n"), "refers to itself");
        // An undefined active theme: any theme that resolves the name.
        assert!(check_text("theme a { track = #ff0000 }\nscreen:theme = gone\npage p { text { value = x; color = track } }\n")
            .iter()
            .all(|d| !d.contains("color")));
    }

    #[test]
    fn theme_token_color_expression_still_accepted() {
        let text = r#"
page home {
    text { value = Hi; color = {{ theme.critical }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn repeat_in_page_body_accepted() {
        let text = r#"
source vms { run = cat; parse = json }
page home {
    grid {
        repeat item in {{ sources.vms.value }} {
            text { value = {{ item.name }} }
        }
    }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // --- Phase 7: cross-reference resolution ---

    #[test]
    fn unknown_action_in_on_tap_is_flagged() {
        let diags = check_text("page home { text { on_tap = ghost } }\n");
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn on_tap_none_is_accepted() {
        let diags = check_text("page home { text { on_tap = none } }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn goto_back_is_accepted() {
        let diags = check_text("page home { text { on_tap { goto = back } } }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn goto_unknown_page_is_flagged() {
        let diags = check_text("page home { text { on_tap { goto = somewhere } } }\n");
        assert_msg(&diags, "unknown page `somewhere`");
    }

    #[test]
    fn start_page_known_is_accepted() {
        let diags = check_text("page home { }\napp { start_page = home }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn start_page_unknown_is_flagged() {
        let diags = check_text("app { start_page = ghost }\n");
        assert_msg(&diags, "unknown page `ghost`");
    }

    #[test]
    fn screen_theme_known_is_accepted() {
        let diags = check_text("theme dark { }\nscreen { theme = dark }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn screen_theme_unknown_is_flagged() {
        let diags = check_text("screen { theme = ghost }\n");
        assert_msg(&diags, "unknown theme `ghost`");
    }

    #[test]
    fn alert_from_known_source_is_accepted() {
        let text = r#"
source ntfy { run = cat; stream = true }
alert a { from = ntfy; severity = warning; title = Up }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn alert_from_unknown_source_is_flagged() {
        let diags = check_text("alert a { from = ghost; severity = info; title = x }\n");
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn action_refresh_single_source_resolved() {
        let text = r#"
source vms { run = cat }
action pause { run = cat; refresh = vms }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn action_refresh_source_list_each_resolved() {
        let text = r#"
source vms { run = cat }
source cpu { run = cat }
action pause { run = cat; refresh = [vms, cpu] }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn action_refresh_unknown_source_in_list_flagged() {
        let text = r#"
source vms { run = cat }
action pause { run = cat; refresh = [vms, ghost] }
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn action_then_simple_action_resolved() {
        let text = r#"
action a { run = cat }
action b { run = cat; then = a }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn action_then_block_form_accepted() {
        let text = r#"
action vm_stop { param vm: string; run = cat }
action pause { run = cat; then vm_stop { vm = foo } }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn action_then_unknown_action_flagged() {
        let diags = check_text("action a { run = cat; then = ghost }\n");
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn alert_action_known_resolved() {
        let text = r#"
action restart { run = cat }
alert a { severity = info; title = x; action = restart }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn security_pin_unknown_secret_flagged() {
        let diags = check_text("security { pin = ghost }\n");
        assert_msg(&diags, "unknown secret `ghost`");
    }

    #[test]
    fn security_pin_known_secret_resolved() {
        let diags = check_text(
            "secret pin_hash { env = PIN_HASH }\nsecurity { pin = pin_hash }\n",
        );
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn on_change_unknown_action_flagged() {
        let diags = check_text(
            "page home { toggle { value = true; on_change = ghost } }\n",
        );
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn button_action_unknown_flagged() {
        let diags = check_text("page home { button { action = ghost } }\n");
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn on_tap_named_action_unknown_name_flagged() {
        let diags = check_text("page home { text { on_tap ghost { } } }\n");
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn expression_reference_is_not_checked_against_names() {
        // `{{ ... }}` is opaque at this layer; CEL will check it.
        let text = r#"
page home { text { on_tap = {{ some_expr }} } }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn inline_action_set_block_accepted() {
        let diags = check_text(
            "page home { text { on_tap { set { tab = home } } } }\n",
        );
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn qualified_top_level_ref_is_resolved() {
        let diags = check_text("screen:theme = ghost\n");
        assert_msg(&diags, "unknown theme `ghost`");
    }

    #[test]
    fn qualified_top_level_known_ref_passes() {
        let diags = check_text("theme dark { }\nscreen:theme = dark\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn qualified_top_level_app_start_page_ref_is_resolved() {
        let diags = check_text("app:start_page = ghost\n");
        assert_msg(&diags, "unknown page `ghost`");
    }

    #[test]
    fn qualified_top_level_security_pin_ref_is_resolved() {
        let diags = check_text("security:pin = ghost\n");
        assert_msg(&diags, "unknown secret `ghost`");
    }

    #[test]
    fn indexed_top_level_alert_action_ref_is_resolved() {
        let diags = check_text("alert[a]:action = ghost\n");
        assert_msg(&diags, "unknown action `ghost`");
    }

    #[test]
    fn indexed_top_level_alert_from_ref_is_resolved() {
        let diags = check_text("alert[a]:from = ghost\n");
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn secrets_only_allowed_in_run_and_env() {
        let ok = check_text(
            "secret tok { env = T }\n\
             source s { run = curl -H {{ secrets.tok }} x\n env { K = {{ secrets.tok }} } }\n\
             action a { run = curl {{ secrets.tok }}\n env { K = {{ secrets.tok }} } }\n",
        );
        assert!(ok.is_empty(), "{ok:?}");

        for bad in [
            "secret tok { env = T }\npage home { text { value = {{ secrets.tok }} } }\n",
            "secret tok { env = T }\nalert a { when = {{ true }}; title = x {{ secrets.tok }} }\n",
            "secret tok { env = T }\naction a { confirm = {{ secrets.tok }} }\n",
            "secret tok { env = T }\nvar v: string = {{ secrets.tok }}\n",
        ] {
            assert_msg(&check_text(bad), "`secrets` can only be used in `run` and `env`");
        }
    }

    #[test]
    fn override_of_undefined_item_is_flagged() {
        let diags = check_text(
            "action reboot_rack { run = /bin/true }\naction[reboot_rak]:pin = true\n",
        );
        assert_msg(
            &diags,
            "`action[reboot_rak]` overrides an item that is not defined (no `action reboot_rak { }`)",
        );
    }

    #[test]
    fn override_of_defined_item_is_clean() {
        let diags = check_text(
            "action reboot_rack { run = /bin/true }\naction[reboot_rack]:pin = true\n",
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn night_theme_known_passes() {
        let diags = check_text(
            "theme dim { }\nscreen { night { from = 22:00; theme = dim } }\n",
        );
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn night_theme_unknown_is_flagged() {
        let diags = check_text("screen { night { theme = ghost } }\n");
        assert_msg(&diags, "unknown theme `ghost`");
    }

    #[test]
    fn refresh_list_collapses_duplicates_into_one_diagnostic() {
        let text = r#"
source vms { run = cat }
action pause { run = cat; refresh = [vms, ghost1, ghost2, ghost1] }
"#;
        let diags = check_text(text);
        // One diagnostic lists both ghost1 and ghost2, no duplicates.
        let refresh_diags: Vec<_> = diags
            .iter()
            .filter(|m| m.contains("refresh"))
            .collect();
        assert_eq!(refresh_diags.len(), 1, "diags: {diags:?}");
        assert!(refresh_diags[0].contains("ghost1"));
        assert!(refresh_diags[0].contains("ghost2"));
    }

    // --- Phase 8: component / action arg type-checking ---

    #[test]
    fn component_call_with_matching_args_is_accepted() {
        let text = r#"
component stat_tile {
    param label: string
    param value: number
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU; value = 42 }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_unknown_arg_is_flagged() {
        let text = r#"
component stat_tile {
    param label: string
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU; bogus = 1 }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown arg `bogus` for component `stat_tile`");
    }

    #[test]
    fn component_call_missing_required_arg_is_flagged() {
        let text = r#"
component stat_tile {
    param label: string
    param value: number
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU }
}
"#;
        let diags = check_text(text);
        assert_msg(
            &diags,
            "component `stat_tile` is missing required arg `value`",
        );
    }

    #[test]
    fn component_call_param_with_default_is_optional() {
        let text = r#"
component stat_tile {
    param label: string
    param warn: number = 1e9
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_wrong_arg_type_is_flagged() {
        let text = r#"
component stat_tile {
    param value: number
    text { value = {{ value }} }
}
page home {
    stat_tile { value = banana }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "expected a number, got `banana`");
    }

    #[test]
    fn component_call_arg_expression_is_accepted() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
component stat_tile {
    param value: number
    text { value = {{ value }} }
}
page home {
    stat_tile { value = {{ sources.cpu_temp.value }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_common_ui_property_on_root_is_accepted() {
        // schema.md section 7: common node properties set on the call
        // apply to the component's root node.
        let text = r#"
component stat_tile {
    param label: string
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU; width = 120; class = card }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_action_ref_arg_resolved() {
        let text = r#"
action refresh_all { run = cat }
component button_x {
    param tap: action
    text { value = Go; on_tap = {{ tap }} }
}
page home {
    button_x { tap = refresh_all }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn button_action_takes_an_expression() {
        let text = r#"
action go { run = cat }
component big {
    param tap: action = none
    button { label = Go; action = {{ tap }} }
}
page home {
    big { tap = go }
    button { label = Pick; action = {{ vars.x ? 'go' : 'gone' }} }
}
var x: bool = false
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown action `gone`");
        assert_eq!(diags.len(), 1, "{diags:?}");
    }

    #[test]
    fn component_call_unknown_action_ref_arg_is_flagged() {
        let text = r#"
component button_x {
    param tap: action
    text { value = Go; on_tap = {{ tap }} }
}
page home {
    button_x { tap = refresh_all }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown action `refresh_all`");
    }

    #[test]
    fn component_call_action_ref_none_is_accepted() {
        let text = r#"
component button_x {
    param tap: action = none
    text { value = Go; on_tap = {{ tap }} }
}
page home {
    button_x { tap = none }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_source_ref_arg_resolved() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
component gauge {
    param src: source
    text { value = {{ src.value }} }
}
page home {
    gauge { src = cpu_temp }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn component_call_unknown_source_ref_arg_is_flagged() {
        let text = r#"
component gauge {
    param src: source
    text { value = {{ src.value }} }
}
page home {
    gauge { src = ghost }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn unknown_param_type_is_flagged() {
        let text = r#"
component stat_tile {
    param label: banana
    text { value = {{ label }} }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown param type `banana`");
    }

    #[test]
    fn named_action_call_arg_type_mismatch_is_flagged() {
        let text = r#"
action vm_stop { param vm: int; run = cat }
page home {
    text { value = Hi; on_tap vm_stop { vm = banana } }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "expected an integer, got `banana`");
    }

    #[test]
    fn named_action_call_unknown_arg_is_flagged() {
        let text = r#"
action vm_stop { param vm: string; run = cat }
page home {
    text { value = Hi; on_tap vm_stop { bogus = 1 } }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown arg `bogus` for action `vm_stop`");
    }

    #[test]
    fn named_action_call_missing_required_arg_is_flagged() {
        let text = r#"
action vm_stop { param vm: string; run = cat }
page home {
    text { value = Hi; on_tap vm_stop { } }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "is missing required arg `vm`");
    }

    #[test]
    fn named_action_call_asked_param_is_optional() {
        // schema.md section 6: a param with `ask = ...` is prompted at
        // runtime when not passed, so it's optional at the call site.
        let text = r#"
action restart { param delay: int { ask = keypad }; run = cat }
page home {
    text { value = Hi; on_tap restart { } }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn duplicate_param_in_component_is_flagged() {
        let text = r#"
component stat_tile {
    param label: string
    param label: int
    text { value = {{ label }} }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "`param label` is already declared");
    }

    #[test]
    fn component_call_duplicate_arg_is_flagged() {
        let text = r#"
component stat_tile {
    param label: string
    text { value = {{ label }} }
}
page home {
    stat_tile { label = CPU; label = GPU }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "arg `label` is already set");
    }

    #[test]
    fn component_call_child_ui_node_still_walked() {
        // Children inside a user-component call go into the component's
        // slot. The arg check must not swallow typos inside them.
        let text = r#"
component card {
    column { slot { } }
}
page home {
    card { text { bad_prop = 1 } }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown property `bad_prop`");
    }

    #[test]
    fn component_call_event_handler_arg_applies_to_root() {
        // on_tap on a component call applies to its root node.
        let text = r#"
action refresh_all { run = cat }
component card {
    param label: string
    text { value = {{ label }} }
}
page home {
    card { label = CPU; on_tap = refresh_all }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // --- Phase 9: theme body ---

    #[test]
    fn theme_with_known_tokens_is_accepted() {
        let text = r#"
theme dark {
    background = #202020
    accent     = #44aaff
    text       = #e8e8e8
    font       = roboto 16
    gap        = 8
    motion     = reduced
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn font_values_are_checked_where_they_appear() {
        let ok = check_text(
            "theme t { font = inter 18 bold; font_big = sans 30; style .x { font = noto_sans 12 } }\n\
             page home { text { value = a; font = open_sans 20 } text { value = b; font = font_big } \
             text { value = c; font = {{ theme.font_huge }} } }\n",
        );
        assert!(ok.is_empty(), "unexpected: {ok:?}");
        assert_msg(&check_text("theme t { font = robto 16 }\n"), "did you mean `roboto`");
        assert_msg(&check_text("theme t { font_small = 12 }\n"), "expected a font");
        assert_msg(&check_text("theme t { style .x { font = inter 999 } }\n"), "from 6 to 200");
        assert_msg(&check_text("page home { text { value = a; font = montserrat 16 italic } }\n"), "`italic`");
        // The diagnostic carries file and line.
        let d = check_text("page home {\n  text { value = a; font = arial 16 }\n}\n");
        assert!(d.iter().any(|m| m.contains("root.conf:2:") && m.contains("unknown font family `arial`")), "{d:?}");
    }

    #[test]
    fn a_ttf_font_must_exist_next_to_the_config() {
        let p = tmp_file("ttf", "page home { text { value = a; font = fonts/Inter.ttf 18 } }\n");
        let run = |p: &std::path::Path| {
            let (map, doc, mut diags) = load(p).unwrap();
            diags.extend(check(&doc, &map));
            diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
        };
        assert_msg(&run(&p), "font file `fonts/Inter.ttf` not found");
        let dir = p.parent().unwrap();
        fs::create_dir_all(dir.join("fonts")).unwrap();
        fs::write(dir.join("fonts/Inter.ttf"), b"x").unwrap();
        assert!(run(&p).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn theme_color_token_bad_value_is_flagged() {
        let diags = check_text("theme dark { accent = banana }\n");
        assert_msg(&diags, "unknown theme color `banana`");
        let diags = check_text("theme dark { accent = 12 }\n");
        assert_msg(&diags, "expected a hex color");
    }

    #[test]
    fn theme_motion_bad_keyword_is_flagged() {
        let diags = check_text("theme dark { motion = sometimes }\n");
        assert_msg(&diags, "expected one of");
    }

    #[test]
    fn theme_extra_token_is_accepted() {
        // schema.md section 11: "A theme may add any other token".
        let text = r#"
theme dark {
    my_token = 42
    other    = whatever
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_style_with_class_selector_is_accepted() {
        let text = r#"
theme dark {
    style .card { radius = 16; shadow = 12 }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_style_with_pseudostate_selector_is_accepted() {
        let text = r#"
theme dark {
    style button:pressed { scale = 0.96; background = {{ theme.accent }} }
    style button:running { background = {{ theme.muted }} }
    style .card:failed { background = #330000 }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_style_with_bare_node_selector_is_accepted() {
        let diags = check_text("theme dark { style button { radius = 8 } }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_style_with_wildcard_selector_is_accepted() {
        let diags = check_text("theme dark { style * { padding = 4 } }\n");
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_style_unknown_pseudostate_is_flagged() {
        let diags = check_text("theme dark { style button:wobble { radius = 8 } }\n");
        assert_msg(&diags, "unknown pseudo-state");
    }

    #[test]
    fn theme_style_selector_list_is_flagged() {
        let diags = check_text("theme dark { style button, link { radius = 8 } }\n");
        assert_msg(&diags, "selector lists are not supported yet");
    }

    #[test]
    fn theme_style_body_rejects_nested_block() {
        let diags = check_text("theme dark { style .card { foo { } } }\n");
        assert_msg(&diags, "does not contain nested blocks");
    }

    #[test]
    fn theme_replacement_component_is_accepted() {
        let text = r#"
theme dark { }
theme glass {
    extends = dark
    component stat_tile {
        param label: string
        param value: number
        column {
            text { value = {{ label }} }
            text { value = {{ string(round(value, 1)) }} }
        }
    }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn theme_replacement_component_without_name_is_flagged() {
        let diags = check_text("theme dark { component { column { text { value = Hi } } } }\n");
        assert_msg(&diags, "needs a name");
    }

    #[test]
    fn theme_unknown_sub_block_is_flagged() {
        let diags = check_text("theme dark { wobble { } }\n");
        assert_msg(&diags, "unknown theme sub-block `wobble`");
    }

    #[test]
    fn theme_param_outside_component_is_flagged() {
        let diags = check_text("theme dark { param x: int }\n");
        assert_msg(&diags, "`param` is only allowed inside a `component` body");
    }

    #[test]
    fn theme_extends_simple_assign_is_accepted() {
        let text = r#"
theme dark { }
theme glass { extends = dark }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn style_outside_theme_still_parses_but_flags() {
        // `style` at top level isn't recognized as a top-level item
        // kind. The parser handles `style <selector> { ... }`
        // identically everywhere; the schema check flags it.
        let diags = check_text("style .card { radius = 8 }\n");
        assert_msg(&diags, "unknown top-level item `style`");
    }

    #[test]
    fn style_as_an_assignment_key_still_works() {
        // The contextual lexer must not break `style = foo` or
        // `style:x = ...` (unlikely syntax, but we want robustness).
        let diags = check_text("page home { text { value = Hi } }\nstyle = ignore_me\n");
        // The second line is a top-level simple assign with an
        // unknown key; the schema flags that, but it's not a lex
        // error -- the selector mode was correctly suppressed.
        assert_msg(&diags, "unknown top-level option `style`");
    }

    // --- Phase 10: CEL parse + source cross-reference ---

    #[test]
    fn cel_expression_parses_cleanly() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
page home {
    text { value = {{ sources.cpu_temp.value * 1.8 + 32 }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn cel_unknown_source_ref_is_flagged() {
        let text = r#"
page home {
    text { value = {{ sources.ghost.value }} }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn cel_parse_error_is_flagged() {
        let diags = check_text("page home { text { value = {{ 1 + + 2 }} } }\n");
        let has_cel = diags.iter().any(|d| d.contains("expression"));
        assert!(has_cel, "expected a CEL diagnostic, got: {diags:?}");
    }

    #[test]
    fn cel_in_interpolation_is_parsed() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
page home {
    text { value = Temp is {{ sources.cpu_temp.value }} C }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn cel_unknown_source_in_interpolation_is_flagged() {
        let diags = check_text(
            "page home { text { value = Temp is {{ sources.ghost.value }} C } }\n",
        );
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn cel_in_repeat_source_is_checked() {
        let text = r#"
page home {
    grid {
        repeat item in {{ sources.ghost.value }} {
            text { value = {{ item.name }} }
        }
    }
}
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn cel_ternary_and_call_combined() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
component stat_tile {
    param warn: number
    text { value = {{ sources.cpu_temp.value > warn ? "hot" : "ok" }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn cel_bare_identifier_not_flagged() {
        // CEL cross-ref only flags `sources.<name>`. Bare roots like
        // `item`, `theme`, a component param -- all legal and left
        // to a later type-check phase.
        let text = r#"
component stat_tile {
    param label: string
    text { value = {{ label }} }
}
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    // --- Review follow-ups ---

    #[test]
    fn component_param_shadowing_common_ui_key_is_flagged() {
        // schema.md section 7: common UI properties on a component call
        // apply to the root node. A param whose name collides with one
        // of those keys would be unreachable, so flag it at decl time.
        let diags = check_text(
            "component t { param width: string = \"\"\n text { value = Hi } }\n",
        );
        assert_msg(
            &diags,
            "`param width` conflicts with the common UI property `width`",
        );
    }

    #[test]
    fn component_param_shadowing_event_handler_is_flagged() {
        // `on_change` is in EVENT_HANDLER_KINDS but not in COMMON_UI_KEYS,
        // so the event-handler-specific branch fires here. (`on_tap`
        // and `on_hold` also appear in COMMON_UI_KEYS, so they'd be
        // reported as common-UI-property shadowing instead.)
        let diags = check_text(
            "component t { param on_change: action = none\n text { value = Hi } }\n",
        );
        assert_msg(
            &diags,
            "`param on_change` conflicts with the event-handler key `on_change`",
        );
    }

    #[test]
    fn action_param_shadowing_common_ui_key_is_accepted() {
        // Actions aren't called with common UI props, so the shadow
        // trap doesn't apply there.
        let diags = check_text(
            "action a { param width: int; run = cat }\n",
        );
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn cel_sources_index_string_cross_ref() {
        let text = r#"
page home { text { value = {{ sources["ghost"].value }} } }
"#;
        let diags = check_text(text);
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn cel_sources_index_known_source_is_accepted() {
        let text = r#"
source cpu_temp { run = cat; parse = number }
page home { text { value = {{ sources["cpu_temp"].value }} } }
"#;
        let diags = check_text(text);
        assert!(diags.is_empty(), "unexpected: {diags:?}");
    }

    #[test]
    fn interp_double_backslash_before_opener_is_literal_backslash() {
        // `\\{{ ... }}` is a literal `\` followed by an expression,
        // NOT an escaped `{{`. The CEL walk must parse the `{{...}}`.
        let text = r#"
page home { text { value = \\{{ sources.ghost.value }} } }
"#;
        let diags = check_text(text);
        // The expression inside should be parsed; the ghost ref flagged.
        assert_msg(&diags, "unknown source `ghost`");
    }

    #[test]
    fn interp_single_backslash_escapes_opener() {
        // `\{{ x }}` is a literal `{{ x }}`, no CEL parse.
        let text = r#"
page home { text { value = \{{ sources.ghost.value }} } }
"#;
        let diags = check_text(text);
        // ghost ref must NOT fire -- the whole thing is literal.
        assert!(
            !diags.iter().any(|d| d.contains("unknown source")),
            "unexpected ghost diagnostic: {diags:?}"
        );
    }

    // --- Theme folders and theme components (ADR 0021) ---

    /// A config folder with `files` (path, text); checks `root.conf`.
    /// Returns the diagnostics (`file:line:col: message`, file names
    /// relative to the folder) and the loaded document.
    const PNG_HEAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\0\x10\0\0\0\x10";

    /// 1000x800: past the limit of an 800x480 screen, not of 1024x600.
    const BIG_PNG_HEAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR\0\0\x03\xe8\0\0\x03\x20";

    fn check_folder(label: &str, files: &[(&str, &str)]) -> (Vec<String>, Document) {
        let root = tmp_file(label, files[0].1);
        let dir = root.parent().unwrap().to_path_buf();
        for (path, text) in &files[1..] {
            let p = dir.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            // `<png>`: the start of a PNG file, what the check reads.
            let bytes: &[u8] = match *text {
                "<png>" => PNG_HEAD,
                "<bigpng>" => BIG_PNG_HEAD,
                _ => text.as_bytes(),
            };
            fs::write(p, bytes).unwrap();
        }
        let (map, doc, mut diags) = load(&root).unwrap();
        diags.extend(check(&doc, &map));
        let canon = fs::canonicalize(&dir).unwrap();
        let out = diags
            .iter()
            .map(|d| {
                let loc = map.location(d.span);
                let file = loc.path.strip_prefix(&canon).unwrap_or(&loc.path).display().to_string();
                format!("{file}:{}:{}: {}", loc.line, loc.column, d.message)
            })
            .collect();
        let _ = fs::remove_dir_all(&dir);
        (out, doc)
    }

    fn theme_value(doc: &Document, theme: &str, key: &str) -> String {
        doc.items
            .iter()
            .find_map(|it| match it {
                Item::Block(b) if b.kind.text == "theme" && b.name.as_ref().is_some_and(|n| n.text == theme) => {
                    b.items.iter().find_map(|i| match i {
                        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key) => {
                            Some(a.value.text.clone())
                        }
                        _ => None,
                    })
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {theme}.{key}"))
    }

    const GLASS: &str = "theme glass {\n    extends = base\n    font = fonts/f.ttf 18\n    icons = icons\n    \
        background_image = images/bg.png\n    accent = #44aaff\n    style .card { font = fonts/f.ttf 12 }\n    \
        component tile {\n        param label: string\n        param value: number = 0\n        \
        column { text { value = {{ label }} }\n image { src = images/{{ label }}.png }\n image { src = images/bg2.png } }\n    }\n}\n";

    #[test]
    fn a_theme_folder_is_loaded_with_its_asset_paths() {
        let (diags, doc) = check_folder(
            "tdir",
            &[
                ("root.conf", "screen:theme = glass\ncomponent tile { param label: string\n param value: number = 0\n text { value = {{ label }} } }\npage home { tile { label = a } }\n"),
                ("themes/glass/theme.conf", GLASS),
                ("themes/glass/fonts/f.ttf", "x"),
                ("themes/glass/images/bg.png", "<png>"),
                ("themes/base/theme.conf", "theme base { surface = #222222 }\n"),
            ],
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(theme_value(&doc, "glass", "font"), "themes/glass/fonts/f.ttf 18");
        assert_eq!(theme_value(&doc, "glass", "icons"), "themes/glass/icons");
        assert_eq!(theme_value(&doc, "glass", "background_image"), "themes/glass/images/bg.png");
        assert_eq!(theme_value(&doc, "glass", "accent"), "#44aaff");
        assert_eq!(theme_value(&doc, "base", "surface"), "#222222");
        let text = format!("{doc:?}");
        // A literal `src` is the theme folder's; one from an expression
        // (a param from a page) the config folder's.
        assert!(text.contains("\"themes/glass/images/bg2.png\""), "literal src rewritten");
        assert!(text.contains("\"images/{{ label }}.png\""), "expression src left alone");
        assert!(text.contains("themes/glass/fonts/f.ttf 12"), "style font rewritten");
    }

    #[test]
    fn backgrounds_and_shadows_are_checked_with_their_files() {
        let (diags, doc) = check_folder(
            "tbg",
            &[
                (
                    "root.conf",
                    "screen:theme = glass\n\
                     page home {\n\
                     rect { background = linear 180 #000 accent #ffffff00 }\n\
                     rect { background = radial surface #00000000; shadow = 8 #00000080 0 4 }\n\
                     rect { background = images/a.png contain; shadow = shadow }\n\
                     rect { background = sunset; shadow = lifted }\n\
                     rect { background = {{ 'linear 90 #000 #fff' }} }\n\
                     rect { background = linear #000 #fff }\n\
                     rect { background = radial #000 nope }\n\
                     rect { background = images/none.png }\n\
                     rect { background = images/fake.png }\n\
                     rect { background = ../x.png }\n\
                     rect { background = images/a.png zoom }\n\
                     rect { shadow = 99 }\n\
                     rect { shadow = 8 nope }\n\
                     rect { shadow = flat }\n\
                     rect { background = images/big.png }\n\
                     }\n",
                ),
                ("images/a.png", "<png>"),
                ("images/fake.png", "not a png"),
                ("images/big.png", "<bigpng>"),
                (
                    "themes/glass/theme.conf",
                    "theme glass {\n    background_image = img/bg.png\n    sunset = radial #ff8800 #220044\n    \
                     lifted = 12 accent 0 6\n    shadow = 4\n    flat = #000\n    \
                     style .card { background = img/card.png fill }\n}\n",
                ),
                ("themes/glass/img/bg.png", "<png>"),
                ("themes/glass/img/card.png", "<png>"),
            ],
        );
        let text = format!("{doc:?}");
        assert!(text.contains("themes/glass/img/card.png fill"), "style background rewritten");
        let want = [
            "root.conf:8:21: a linear gradient is `linear <angle> <color> <color> ...`",
            "root.conf:9:21: unknown theme color `nope`",
            "root.conf:10:21: background image `images/none.png` not found",
            "root.conf:11:21: background image `images/fake.png` is not a PNG file",
            "root.conf:12:21: background image `../x.png` must be a relative path inside the config folder",
            "root.conf:13:21: background image fit `zoom`",
            "root.conf:14:17: shadow size 99 is out of range",
            "root.conf:15:17: unknown theme color `nope`",
            "root.conf:16:17: theme token `flat`: expected a shadow",
            "root.conf:17:21: background image `images/big.png` is 1000x800: more than 768000 pixels",
        ];
        for w in want {
            assert!(diags.iter().any(|d| d.starts_with(w)), "missing `{w}` in {diags:#?}");
        }
        assert_eq!(diags.len(), want.len(), "{diags:#?}");

        // A theme folder's `background_image` must be there.
        let (diags, _) = check_folder(
            "tbg2",
            &[("root.conf", "screen:theme = glass\n"), ("themes/glass/theme.conf", "theme glass { background_image = bg.png }\n")],
        );
        assert!(diags.iter().any(|d| d.contains("background image `themes/glass/bg.png` not found")), "{diags:?}");

        // A larger screen draws a larger image.
        let (diags, _) = check_folder(
            "tbg3",
            &[
                ("root.conf", "display { width = 1024; height = 600 }\npage home { rect { background = big.png } }\n"),
                ("big.png", "<bigpng>"),
            ],
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn a_theme_folder_wins_only_when_the_config_does_not_define_it() {
        let (diags, doc) = check_folder(
            "tinl",
            &[
                ("root.conf", "screen:theme = glass\ntheme glass { accent = #000001 }\n"),
                ("themes/glass/theme.conf", "theme glass { accent = #000002 }\n"),
            ],
        );
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(theme_value(&doc, "glass", "accent"), "#000001");
    }

    #[test]
    fn a_broken_theme_folder_is_reported_in_its_file() {
        // A font file missing from the theme folder, a font kept
        // outside it, a bad token, and items a theme file cannot hold.
        let (diags, _) = check_folder(
            "tbad",
            &[
                ("root.conf", "screen:theme = glass\nscreen { night { theme = gone } }\n"),
                ("themes/glass/theme.conf", "theme glass {\n  font = fonts/none.ttf 18\n  font_big = ../f.ttf 20\n  accent = banana\n}\npage x { }\ntheme other { }\n"),
            ],
        );
        let has = |needle: &str| assert!(diags.iter().any(|d| d.contains(needle)), "no `{needle}` in {diags:?}");
        has("themes/glass/theme.conf:2:10: font file `themes/glass/fonts/none.ttf` not found");
        has("themes/glass/theme.conf:3:14: font file `../f.ttf` must be a relative path");
        has("themes/glass/theme.conf:4:");
        has("themes/glass/theme.conf:6:1: `themes/glass/theme.conf` holds only `theme glass { }`; `page` belongs in the config");
        has("themes/glass/theme.conf:7:7: `themes/glass/theme.conf` holds one `theme glass { }`");
        has("root.conf:2:26: unknown theme `gone` (no `theme gone { }` in the config and no `themes/gone/theme.conf` beside it)");

        let (diags, _) = check_folder(
            "tname",
            &[("root.conf", "screen:theme = glass\n"), ("themes/glass/theme.conf", "theme glas { }\n")],
        );
        assert!(diags.iter().any(|d| d.contains("root.conf:1:16: `themes/glass/theme.conf` does not define `theme glass { }`")), "{diags:?}");
    }

    #[test]
    fn a_broken_theme_folder_nothing_names_is_a_warning() {
        let root = tmp_file("tunused", "screen:theme = good\npage home { }\n");
        let dir = root.parent().unwrap().to_path_buf();
        for (name, text) in [("good", "theme good { accent = #000001 }\n"), ("bad", "theme bad {\n accent = banana\n}\n"), ("fine", "theme fine { }\n")] {
            fs::create_dir_all(dir.join("themes").join(name)).unwrap();
            fs::write(dir.join("themes").join(name).join("theme.conf"), text).unwrap();
        }
        let (map, mut doc, mut diags) = load(&root).unwrap();
        diags.extend(check(&doc, &map));
        let (errors, warnings) = crate::split_warnings(&mut doc, &map, diags);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(map.location(warnings[0].span).path.ends_with("themes/bad/theme.conf"));
        assert!(warnings[0].message.contains("theme `bad` is named nowhere: left out"), "{}", warnings[0].message);
        assert!(check_theme_switch(&doc, "bad").is_err());
        assert!(check_theme_switch(&doc, "fine").is_ok());
        // Named, the same theme is an error.
        fs::write(&root, "screen:theme = bad\n").unwrap();
        let (map, mut doc, mut diags) = load(&root).unwrap();
        diags.extend(check(&doc, &map));
        let (errors, _) = crate::split_warnings(&mut doc, &map, diags);
        assert!(errors.iter().any(|d| d.message.contains("banana")), "{errors:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_names_a_known_theme() {
        assert_msg(&check_text("theme a { extends = gone }\n"), "unknown theme `gone`");
        assert_msg(&check_text("theme a { }\ntheme[a]:extends = gone\n"), "unknown theme `gone`");
    }

    #[test]
    fn a_theme_component_takes_the_same_params() {
        // Replacing the library's `shell` (no params) with one that
        // has one; replacing a config component with other params.
        let text = "component tile { param label: string\n text { value = {{ label }} } }\n\
            theme t {\n component shell { param x: int\n column { slot { } } }\n\
            component tile { param label: number\n text { value = x } }\n}\n";
        let d = check_text(text);
        assert_msg(&d, "root.conf:4:12: theme `t`'s component `shell` takes `x: int`, the component it replaces takes no params");
        assert_msg(&d, "component `tile` takes `label: number`, the component it replaces takes `label: string`");
        // Same params, a different default: fine. Optional or not counts.
        assert!(check_text("component tile { param a: int = 1\n text { value = x } }\ntheme t { component tile { param a: int = 2\n row { } } }\n").is_empty());
        assert_msg(
            &check_text("component tile { param a: int = 1\n text { value = x } }\ntheme t { component tile { param a: int\n row { } } }\n"),
            "takes `a: int`, the component it replaces takes `a: int = ...`",
        );
        // Two themes adding the same component agree with each other.
        assert_msg(
            &check_text("theme a { component pill { param n: int\n row { } } }\ntheme b { component pill { row { } } }\n"),
            "theme `b`'s component `pill` takes no params, theme `a`'s `pill` takes `n: int`",
        );
        // Calls to a theme-only component check against its params.
        assert_msg(
            &check_text("theme a { component pill { param n: int\n row { } } }\npage home { pill { n = abc } }\n"),
            "expected an integer, got `abc`",
        );
        assert!(check_text("theme a { component pill { param n: int\n row { } } }\npage home { pill { n = 3 } }\n").is_empty());
    }

    #[test]
    fn a_switch_at_run_time_is_checked() {
        let p = tmp_file("switch", "theme day { component pill { row { } } }\ntheme dusk { extends = day }\nscreen:theme = day\npage home { pill { } }\n");
        let (map, doc, mut diags) = load(&p).unwrap();
        diags.extend(check(&doc, &map));
        assert!(diags.is_empty(), "{diags:?}");
        assert!(check_theme_switch(&doc, "dusk").is_ok());
        assert_eq!(check_theme_switch(&doc, "dark").unwrap_err(), "theme `dark` neither defines nor inherits component `pill`, which the pages use");
        assert_eq!(check_theme_switch(&doc, "gone").unwrap_err(), "no theme `gone` (themes: dark, day, dusk, light)");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_theme_only_component_is_in_every_theme_in_use() {
        let head = "theme day { component pill { row { } } }\ntheme dusk { extends = day }\ntheme dark { }\n";
        let page = "page home { pill { } }\n";
        assert!(check_text(&format!("{head}screen {{ theme = dusk }}\n{page}")).is_empty());
        assert_msg(
            &check_text(&format!("{head}screen {{ theme = day; night {{ theme = dark }} }}\n{page}")),
            "root.conf:5:13: component `pill` is only defined in themes, and theme `dark` (in use) neither defines nor inherits it",
        );
        // Used only inside the theme's own components: no matter.
        assert!(check_text("theme day { component pill { row { } }\n component shell { column { pill { }\n slot { } } } }\ntheme dark { }\nscreen { theme = day; night { theme = dark } }\n").is_empty());
    }
}
