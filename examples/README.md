# Examples

Four complete configs, each with its own look. They run on any Linux machine as is:

```
cargo run --release -- -c examples/homelab/paneld.conf
cargo run --release -- -c examples/smart-home/paneld.conf
cargo run --release -- -c examples/system/paneld.conf
cargo run --release -- -c examples/stream-deck/paneld.conf
```

The homelab, smart home and stream deck configs read demo data from their `demo/` folders and say, above every source and action, what a real setup runs instead; swap them in one at a time and check with `paneld check -c <config>`. The system monitor reads the machine it runs on.

## Homelab rack

[`homelab/`](homelab/): services, VMs, storage and the network of a home server rack. Shows a theme folder whose `shell` component replaces the frame with a side bar (`themes/graphite/`), alerts raised from source values, actions behind a PIN (`1234` in the demo) and a confirmation, an action that asks for a parameter, `repeat` over JSON lists, and gauges built from `arc` and `stack`.

| | |
| --- | --- |
| ![Overview](../docs/images/homelab-overview.png) | ![VMs](../docs/images/homelab-vms.png) |
| ![Storage](../docs/images/homelab-storage.png) | ![Network](../docs/images/homelab-network.png) |
| ![PIN before stopping a VM](../docs/images/homelab-pin.png) | |

## Smart home

[`smart-home/`](smart-home/): a wall panel with the time, weather, scenes, lights, heating and solar power. Shows a light theme with a large clock (`now`), toggles and a slider bound to `var`s, a thermostat on an `arc`, bar charts, night settings, and a ready `remote_kind` for Home Assistant's REST API (`HA_TOKEN` from the environment).

| | |
| --- | --- |
| ![Home](../docs/images/smart-home-home.png) | ![Lights](../docs/images/smart-home-lights.png) |
| ![Heating](../docs/images/smart-home-climate.png) | ![Energy](../docs/images/smart-home-energy.png) |

## System monitor

[`system/`](system/): CPU, memory, disk, network and processes of the machine paneld runs on, read by one small POSIX script (`stats.sh`) per poll. Shows a theme in a plain included file (`theme.conf`), sources that poll only while their page is on screen (`active = visible`), and charts fed by a source's `history`.

| | |
| --- | --- |
| ![Overview](../docs/images/system-overview.png) | ![CPU](../docs/images/system-cpu.png) |
| ![Network](../docs/images/system-network.png) | ![Processes](../docs/images/system-processes.png) |

## Stream deck

[`stream-deck/`](stream-deck/): a control surface for streaming, on a touchscreen of its own beside the PC: OBS scenes, go live and record, a four-channel mixer, the music, and a pad of macro keys. Shows how far a theme goes without a browser (`themes/studio/`): a backdrop image, its own icons and scene pictures (drawn as SVG in `src/`, rendered by `make-assets.sh`), gradients, glows that fade in with the state they show (two versions of a node, swapped by `visible` with `enter = fade`), eased meters and charts, page fades, keys that sink when pressed, a blurred backdrop behind dialogs, and an alert banner that slides in. OBS is driven over its own WebSocket with `obs-cmd`, the PC's hotkeys and media keys over `ssh` (the `pc` remote).

| | |
| --- | --- |
| ![Live](../docs/images/stream-deck-live.png) | ![Mixer](../docs/images/stream-deck-mixer.png) |
| ![Music](../docs/images/stream-deck-music.png) | ![Macros](../docs/images/stream-deck-macros.png) |

The screenshots are made with `paneld shot`, which renders a config headless to a PNG.
