//! Integration test: parse and resolve the example from
//! `docs/config-format.md` end to end. No diagnostics should fire.

use paneld_config::load;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, io::Write};

static N: AtomicUsize = AtomicUsize::new(0);

fn tmp_dir(label: &str) -> PathBuf {
    // CARGO_TARGET_TMPDIR is set for integration tests; it points at a
    // workspace-local target subdirectory, which is where we want
    // tests to write (not /tmp).
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let p = base.join(format!("paneld-example-{label}-{pid}-{n}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(&p).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    p
}

#[test]
fn config_format_example_loads_cleanly() {
    let d = tmp_dir("docs");

    let main = write(
        &d,
        "rack.conf",
        r#"$pve = https://10.0.0.2:8006

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
"#,
    );

    let (map, doc, diags) = load(&main).unwrap();
    let msgs: Vec<String> = diags
        .iter()
        .map(|d| format!("{}: {}", map.location(d.span), d.message))
        .collect();
    assert!(
        diags.is_empty(),
        "expected no diagnostics, got:\n  {}",
        msgs.join("\n  ")
    );
    // Six top-level items: $pve, secret, 2 sources, action, component, page.
    // (The `$pve` definition is itself one Assign item at the top.)
    assert_eq!(
        doc.items.len(),
        7,
        "unexpected item count: {}",
        doc.items.len()
    );

    // Clean up.
    let _ = fs::remove_dir_all(&d);
}
