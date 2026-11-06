# paneld config for VS Code

Syntax highlighting for paneld config files ([Config format](../../docs/config-format.md)): blocks and their names, keys, values, `$vars`, comments, and the CEL inside `{{ }}`.

## Install

Put the folder in VS Code's extensions folder, from the repo root, and reload the window:

| VS Code | Command |
| --- | --- |
| Linux, or a Linux build under WSLg | `ln -s "$PWD/editors/vscode" ~/.vscode/extensions/paneld-config` |
| Remote (SSH, WSL, containers) | `ln -s "$PWD/editors/vscode" ~/.vscode-server/extensions/paneld-config` |
| Windows or macOS, local | copy the folder to `%USERPROFILE%\.vscode\extensions\paneld-config` or `~/.vscode/extensions/paneld-config` |

Files named `paneld.conf` and `*.conf` under a `paneld` folder open as paneld. For other config folders, add them in the settings:

```
"files.associations": { "**/touchscreen/**/*.conf": "paneld" }
```

The grammar (`syntaxes/paneld.tmLanguage.json`) is plain TextMate, so other editors that read TextMate grammars (Sublime Text, JetBrains IDEs through their TextMate bundles support) can use it too.

## Test

```
bun install
bun test
```

The test runs the grammar over every `.conf` in the repo (and every `.conf` under `$PANELD_CONFIGS` when it is set) and the config examples in `docs/`, and checks that no rule runs past its line.
