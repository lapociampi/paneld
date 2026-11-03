---
title: "Config format"
description: "The hypr-style syntax of paneld config files."
date: 2026-10-29
updated: 2026-11-03
tags: [paneld, config]
series: paneld
order: 3
---

v1. A hypr-style format ([ADR 0006](decisions/0006-config-format.md)), inspired by hyprlang, Hyprland's classic config language. What goes in it (every section, building block and property) is in [Schema](schema.md).

## Example

```
# rack.conf
include = theme.conf
include = pages/*.conf

$pve = https://10.0.0.2:8006

secret adguard { env = ADGUARD_AUTH }

source cpu_temp {
    poll  = 30s
    run   = cat /sys/class/thermal/thermal_zone0/temp
    parse = number
    value = {{ out / 1000.0 }}
}

source vms {
    poll  = 1m
    run   = sh -c 'curl -sf $pve/api2/json/nodes/pve/qemu | jq .data'
    parse = json
}

action vm_stop {
    param vm: string
    pin     = true
    confirm = Stop VM {{ vm }}?
    run     = curl -sf -X POST $pve/api2/json/nodes/pve/qemu/{{ vm }}/status/stop
    refresh = vms
}

component stat_tile {
    param label: string
    param value: number
    param warn: number = 1e9

    column {
        padding    = 8
        background = {{ theme.surface }}
        text { value = {{ label }} }
        text {
            value = {{ string(round(value, 1)) }}
            color = {{ value > warn ? theme.warning : theme.text }}
        }
    }
}

page home {
    grid {
        columns = 3
        stat_tile { label = CPU; value = {{ sources.cpu_temp.value }}; warn = 70 }
        repeat item in {{ sources.vms.value }} {
            stat_tile {
                label = {{ item.name }}
                value = {{ item.cpu * 100.0 }}
                on_hold vm_stop { vm = {{ string(item.vmid) }} }
            }
        }
    }
}
```

## Files

- One main file, `/etc/paneld/paneld.conf` by default, `--config <path>` to change it.
- `include = <path>` loads another file at that point. Paths are relative to the including file; `*` globs are allowed and load in sorted order. An include cycle or a missing non-glob file is an error.
- Files are read in order, top to bottom, includes inline. Order matters only for options and `$vars` (below).

## Lines

- `key = value`. Spaces around `=` and at both ends of the value are ignored.
- `;` separates several assignments on one line: `text { value = Hi; color = {{ theme.accent }} }`.
- A `\` at the end of a line continues the value on the next one.
- `#` starts a comment to the end of the line; `##` is a literal `#`.
- A value is plain text to the end of the line (or `;` or `}`); double quotes keep leading and trailing spaces and allow `;`, `}` and `#` inside.

## Blocks

| Form | Use |
| --- | --- |
| `kind { ... }` | A settings category (`screen { }`) or a UI node (`column { }`, `stat_tile { }`) |
| `kind name { ... }` | A named item: `secret`, `source`, `action`, `component`, `page`, `theme`, `alert`, `reminder` |
| `category:key = value` | One option without opening the block: `screen:idle = 2m` |
| `kind[name]:key = value` | One property of a named item: `theme[dark]:accent = #44aaff` |

- Inside a UI node, child nodes are its children, in order.
- **Options** (`key = value`) can be set more than once; the last one in load order wins. This is how a shipped defaults file and personal overrides combine.
- **Named items** are defined once; a second `source cpu_temp { }` is an error. Single properties of an existing item can still be overridden with `kind[name]:key`.
- **Repeatable keywords** add an entry each time instead of replacing: `include`, `param`.

## Variables

- `$name = value` defines a constant; `$name` anywhere later is replaced by its text before anything else is parsed. `$$` is a literal `$`.
- A variable must be defined before it is used (in load order).
- For hosts, URLs, sizes, colors shared between files. Secrets are not variables: they use `secret` items (never written in the config).

## Expressions

- `{{ ... }}` holds a CEL expression ([ADR 0004](decisions/0004-cel.md)).
- A value that is only one expression keeps the expression's type (number, bool, color, list): `value = {{ sources.cpu_temp.value }}`.
- An expression mixed with text makes a string: `label = CPU {{ round(x, 1) }} C`.
- `\{{` is a literal `{{`.
- `\\` is a literal backslash, so `\\{{ ... }}` is a backslash followed by a real expression (not an escaped opener). To write a literal backslash directly before an expression without the opener firing, use `\\\{{` (literal backslash, then an escaped `{{`).
- Expressions are compiled and type-checked at load.

## Value types

| Type | Written |
| --- | --- |
| number | `12`, `0.5`, `1e9` |
| bool | `true`, `false` |
| string | text, or `"..."` |
| duration | `500ms`, `30s`, `2m`, `1h`, `1d`, `1w`, `1 month`, `1y` (a month is 30 days, a year 365) |
| size | `120` (px), `50%`, `grow` |
| bytes | `512`, `64KiB`, `1MiB` |
| color | `#rgb`, `#rrggbb`, `#rrggbbaa`, or a theme color by name |
| list | `[5, 30]` |
| command | see below |
| action | an action name, or `none` |

## Commands

- `run = curl -sf -u {{ secrets.adguard }} http://localhost:8080/control/status` is split into argv like a shell would split it (single and double quotes, backslash escapes), but no shell ever runs it.
- `$vars` and `{{ }}` are filled in inside each argument separately, so a value can never become a second argument or a shell command.
- Pipes and redirections need an explicit shell: `run = sh -c 'curl -sf ... | jq .data'`. Values reach that script as arguments after it (`sh -c 'echo "$$1"' sh {{ x }}`) or as environment variables, never pasted into it: `paneld check` rejects `{{ }}` inside a `-c` script.

## Structure

- `repeat <name> in {{ list }} { ... }` repeats its body once per item, with the item as `<name>`.
- `visible = {{ bool }}` on any node shows or hides it.
- These are the only control flow ([ADR 0003](decisions/0003-ui-model.md)).

## Errors

- Every error has file, line and column, from the parser, the schema check and the CEL compiler alike.
- `paneld check [--config <path>]` checks a config without running it ([ADR 0007](decisions/0007-ctl.md)).
- On reload, an invalid config is rejected as a whole: the last good one stays on screen with an error banner.
- If no valid config has ever loaded, a built-in error page shows the error and the config path.

## Editor support

None ready-made (the price of an own format). Planned: a syntax-highlighting grammar; a language server only if it proves worth it. Until then `paneld check` is the tool.
