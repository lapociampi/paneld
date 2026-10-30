//! Build script: compile the vendored LVGL 9 source with paneld's own
//! `lv_conf.h`, link SDL2 for the PC build, and generate FFI bindings
//! with bindgen (ADR 0011).
//!
//! Layout:
//!   vendor/lvgl/            vendored v9.6.0 (git submodule)
//!   crates/lvgl-sys/
//!     include/lv_conf.h     paneld's build configuration
//!     wrapper.h             bindgen entry point
//!     src/lib.rs            `include!(generated bindings)`
//!     build.rs              this file

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("lvgl-sys is a workspace crate")
        .to_path_buf();
    let lvgl_dir = workspace_root.join("vendor/lvgl");

    if !lvgl_dir.join("lvgl.h").exists() {
        panic!(
            "vendor/lvgl is missing. Run `git submodule update --init \
             --recursive` from the workspace root to fetch LVGL 9."
        );
    }

    println!("cargo:rerun-if-changed=include/lv_conf.h");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");
    // Submodule rev changes: let `.gitmodules` act as a signal.
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join(".gitmodules").display()
    );

    compile_lvgl(&manifest_dir, &lvgl_dir);
    generate_bindings(&manifest_dir, &lvgl_dir);
    link_sdl2();
}

fn compile_lvgl(manifest_dir: &Path, lvgl_dir: &Path) {
    let mut build = cc::Build::new();
    build
        .flag_if_supported("-std=c11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-missing-field-initializers")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-cast-function-type")
        .flag_if_supported("-fvisibility=hidden")
        .define("LV_CONF_INCLUDE_SIMPLE", None)
        .define("LV_LVGL_H_INCLUDE_SIMPLE", None)
        // Required for `strnlen` and friends in the clib stdlib shim.
        .define("_GNU_SOURCE", None)
        .include(manifest_dir.join("include"))
        .include(lvgl_dir)
        .include(lvgl_dir.join("src"))
        .include(lvgl_dir.join("include"));

    // SDL2 headers (for the drivers/sdl sources).
    for inc in sdl2_include_paths() {
        build.include(inc);
    }

    // Walk every C file under src/, filtering only files we know
    // shouldn't be compiled (optional backends gated by features we
    // don't enable).
    let srcs = collect_c_sources(&lvgl_dir.join("src"));
    for src in &srcs {
        build.file(src);
    }

    build.compile("lvgl");

    // Make the include paths available for consumers (the `ui` crate
    // needs them for bindgen-generated types to type-check).
    println!("cargo:include={}", lvgl_dir.display());
    println!("cargo:lvgl_dir={}", lvgl_dir.display());
}

fn collect_c_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| e == "c") {
            out.push(p);
        }
    }
}

fn generate_bindings(manifest_dir: &Path, lvgl_dir: &Path) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out_dir.join("bindings.rs");

    let mut builder = bindgen::Builder::default()
        .header(
            manifest_dir
                .join("wrapper.h")
                .to_string_lossy()
                .into_owned(),
        )
        .clang_arg(format!("-I{}", manifest_dir.join("include").display()))
        .clang_arg(format!("-I{}", lvgl_dir.display()))
        .clang_arg(format!("-I{}", lvgl_dir.join("src").display()))
        .clang_arg(format!("-I{}", lvgl_dir.join("include").display()))
        .clang_arg("-DLV_CONF_INCLUDE_SIMPLE")
        .clang_arg("-DLV_LVGL_H_INCLUDE_SIMPLE")
        // Narrow the generated surface to the LVGL public API; keep
        // our bindings small (ADR 0011).
        .allowlist_function("lv_.*")
        .allowlist_type("lv_.*")
        .allowlist_type("_lv_.*")
        .allowlist_var("LV_.*")
        .allowlist_var("_LV_.*")
        .allowlist_var("lv_font_.*")
        .size_t_is_usize(true)
        .derive_default(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for inc in sdl2_include_paths() {
        builder = builder.clang_arg(format!("-I{}", inc.display()));
    }

    let bindings = builder
        .generate()
        .expect("bindgen failed to generate LVGL bindings");
    bindings
        .write_to_file(&bindings_path)
        .expect("writing LVGL bindings");
}

fn sdl2_include_paths() -> Vec<PathBuf> {
    // Try pkg-config first.
    if let Ok(lib) = pkg_config_sdl2() {
        return lib;
    }
    // Fall back to the common Linux path.
    vec![PathBuf::from("/usr/include/SDL2"), PathBuf::from("/usr/include")]
}

fn pkg_config_sdl2() -> Result<Vec<PathBuf>, ()> {
    let out = std::process::Command::new("pkg-config")
        .args(["--cflags-only-I", "sdl2"])
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let paths = s
        .split_whitespace()
        .filter_map(|f| f.strip_prefix("-I").map(PathBuf::from))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        Err(())
    } else {
        Ok(paths)
    }
}

fn link_sdl2() {
    // On a PC build we always link SDL2. The device targets will add
    // libdrm / libinput here instead (ADR 0012); for now we only
    // speak PC.
    if pkg_config::Config::new().probe("sdl2").is_err() {
        println!("cargo:rustc-link-lib=SDL2");
    }
}
