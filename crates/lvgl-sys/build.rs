//! Build script: compile the vendored LVGL 9 source with paneld's own
//! `lv_conf.h`, link SDL2 for the PC build, and generate FFI bindings
//! with bindgen (ADR 0011).
//!
//! Layout:
//!   vendor/lvgl/            vendored v9.6.0 (git submodule)
//!   vendor/libdrm/          libdrm 2.4.134 (git submodule), for `drm`
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
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join(".gitmodules").display()
    );

    let sdl = env::var_os("CARGO_FEATURE_SDL").is_some();
    let drm = env::var_os("CARGO_FEATURE_DRM").is_some();

    // libdrm from source when a checkout is at hand (a static musl
    // build for the Pi has no system libdrm to link): `$PANELD_LIBDRM_SRC`,
    // else `vendor/libdrm`. Without one, the system's libdrm is used.
    println!("cargo:rerun-if-env-changed=PANELD_LIBDRM_SRC");
    let given = env::var_os("PANELD_LIBDRM_SRC").map(PathBuf::from);
    if let Some(g) = given.as_ref().filter(|d| !d.join("xf86drm.c").exists()) {
        println!("cargo:warning=PANELD_LIBDRM_SRC={} has no xf86drm.c; ignored", g.display());
    }
    let libdrm_src = given
        .filter(|d| d.join("xf86drm.c").exists())
        .or_else(|| Some(workspace_root.join("vendor/libdrm")))
        .filter(|d| d.join("xf86drm.c").exists());
    let libdrm_inc = match (&libdrm_src, drm) {
        (Some(src), true) => Some(compile_libdrm(src)),
        _ => None,
    };

    compile_lvgl(&manifest_dir, &lvgl_dir, sdl, drm, libdrm_inc.as_deref());
    generate_bindings(&manifest_dir, &lvgl_dir, sdl, drm);
    link_runtime_libs(sdl, drm && libdrm_inc.is_none());
}

/// Compile libdrm's core (the five files its own build makes
/// `libdrm.so` from) into a static library, with the `config.h` its
/// meson build would write for Linux and the modifier name table its
/// `gen_table_fourcc.py` would generate. Returns the include folders.
fn compile_libdrm(src: &Path) -> Vec<PathBuf> {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("libdrm");
    std::fs::create_dir_all(&out).unwrap();
    // `#ifdef MAJOR_IN_MKDEV` is tested, so it is left undefined.
    std::fs::write(
        out.join("config.h"),
        "#define HAVE_SYS_SYSCTL_H 0\n#define HAVE_SYS_SELECT_H 1\n#define HAVE_ALLOCA_H 1\n\
         #define MAJOR_IN_SYSMACROS 1\n#define HAVE_OPEN_MEMSTREAM 1\n#define HAVE_SECURE_GETENV 1\n\
         #define UDEV 0\n#define HAVE_VISIBILITY 1\n#define HAVE_LIBDRM_ATOMIC_PRIMITIVES 1\n\
         #define HAVE_LIB_ATOMIC_OPS 0\n",
    )
    .unwrap();
    let fourcc = std::fs::read_to_string(src.join("include/drm/drm_fourcc.h")).expect("libdrm drm_fourcc.h");
    println!("cargo:rerun-if-changed={}", src.join("include/drm/drm_fourcc.h").display());
    std::fs::write(out.join("generated_static_table_fourcc.h"), fourcc_table(&fourcc)).unwrap();
    let inc = vec![out.clone(), src.to_path_buf(), src.join("include/drm")];
    let mut build = cc::Build::new();
    build
        .flag_if_supported("-std=gnu11")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-missing-field-initializers")
        .flag_if_supported("-fvisibility=hidden")
        .flag("-include")
        .flag(out.join("config.h").to_string_lossy().into_owned())
        .define("_GNU_SOURCE", None);
    for i in &inc {
        build.include(i);
    }
    for f in ["xf86drm.c", "xf86drmHash.c", "xf86drmRandom.c", "xf86drmSL.c", "xf86drmMode.c"] {
        build.file(src.join(f));
        println!("cargo:rerun-if-changed={}", src.join(f).display());
    }
    build.compile("drm");
    inc
}

/// What libdrm's `gen_table_fourcc.py` makes of `drm_fourcc.h`: the
/// names `drmGetFormatModifierName` and `drmGetFormatModifierVendor`
/// return.
fn fourcc_table(header: &str) -> String {
    const VENDORS: &[&str] = &["ARM", "APPLE", "SAMSUNG", "QCOM", "VIVANTE", "NVIDIA", "BROADCOM", "ALLWINNER"];
    // `#define <prefix><ident>` followed by whitespace (a macro with
    // arguments does not count).
    let defined = |prefix: &str| -> Vec<String> {
        header
            .lines()
            .filter_map(|l| l.strip_prefix("#define ")?.strip_prefix(prefix))
            .filter_map(|rest| {
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
                rest[end..].starts_with(char::is_whitespace).then(|| rest[..end].to_string())
            })
            .filter(|name| !name.is_empty())
            .collect()
    };
    let mut out = String::from(
        "static const struct drmFormatModifierInfo drm_format_modifier_table[] = {\n\
         \x20   { DRM_MODIFIER_INVALID(NONE, INVALID) },\n\
         \x20   { DRM_MODIFIER_LINEAR(NONE, LINEAR) },\n",
    );
    for m in defined("I915_FORMAT_MOD_") {
        out.push_str(&format!("    {{ DRM_MODIFIER_INTEL({m}, {m}) }},\n"));
    }
    for entry in defined("DRM_FORMAT_MOD_") {
        let Some((vendor, m)) = entry.split_once('_') else { continue };
        if !VENDORS.contains(&vendor) || entry.starts_with("VENDOR_") {
            continue;
        }
        if vendor == "ARM" && matches!(m, "TYPE_AFBC" | "TYPE_MISC" | "TYPE_AFRC") {
            continue;
        }
        out.push_str(&format!("    {{ DRM_MODIFIER({vendor}, {m}, {m}) }},\n"));
    }
    out.push_str("};\nstatic const struct drmFormatModifierVendorInfo drm_format_modifier_vendor_table[] = {\n");
    for v in defined("DRM_FORMAT_MOD_VENDOR_") {
        out.push_str(&format!("    {{ DRM_FORMAT_MOD_VENDOR_{v}, \"{v}\" }},\n"));
    }
    out.push_str("};\n");
    out
}

fn compile_lvgl(manifest_dir: &Path, lvgl_dir: &Path, sdl: bool, drm: bool, libdrm_inc: Option<&[PathBuf]>) {
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
        .define("_GNU_SOURCE", None)
        .include(manifest_dir.join("include"))
        .include(lvgl_dir)
        .include(lvgl_dir.join("src"))
        .include(lvgl_dir.join("include"));

    // Flip the backend defines to match our Cargo features. These
    // override the matching #defines in lv_conf.h so a feature-less
    // build drops the whole subsystem's code.
    build.define("PANELD_USE_SDL", if sdl { "1" } else { "0" });
    build.define("PANELD_USE_DRM", if drm { "1" } else { "0" });
    // `lv_conf.h` honors these overrides below.

    if sdl {
        for inc in sdl2_include_paths() {
            build.include(inc);
        }
    }
    if drm {
        match libdrm_inc {
            Some(dirs) => {
                for inc in dirs {
                    build.include(inc);
                }
            }
            None => {
                for inc in libdrm_include_paths() {
                    build.include(inc);
                }
            }
        }
    }

    let srcs = collect_c_sources(&lvgl_dir.join("src"));
    for src in &srcs {
        // Skip whole-backend driver TUs when their feature is off,
        // so a missing SDL2 header on a device build never blocks
        // compilation.
        let path = src.to_string_lossy();
        if !sdl && path.contains("/drivers/sdl/") {
            continue;
        }
        if !drm && path.contains("lv_linux_drm.c") {
            continue;
        }
        build.file(src);
    }

    build.compile("lvgl");

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

fn generate_bindings(manifest_dir: &Path, lvgl_dir: &Path, sdl: bool, drm: bool) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_path = out_dir.join("bindings.rs");

    let wrapper = if sdl { "wrapper.h" } else { "wrapper_nosdl.h" };
    let mut builder = bindgen::Builder::default()
        .header(manifest_dir.join(wrapper).to_string_lossy().into_owned())
        .clang_arg(format!("-I{}", manifest_dir.join("include").display()))
        .clang_arg(format!("-I{}", lvgl_dir.display()))
        .clang_arg(format!("-I{}", lvgl_dir.join("src").display()))
        .clang_arg(format!("-I{}", lvgl_dir.join("include").display()))
        .clang_arg("-DLV_CONF_INCLUDE_SIMPLE")
        .clang_arg("-DLV_LVGL_H_INCLUDE_SIMPLE")
        .clang_arg(format!("-DPANELD_USE_SDL={}", if sdl { 1 } else { 0 }))
        .clang_arg(format!("-DPANELD_USE_DRM={}", if drm { 1 } else { 0 }))
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

    if sdl {
        for inc in sdl2_include_paths() {
            builder = builder.clang_arg(format!("-I{}", inc.display()));
        }
    }

    let bindings = builder
        .generate()
        .expect("bindgen failed to generate LVGL bindings");
    bindings
        .write_to_file(&bindings_path)
        .expect("writing LVGL bindings");
}

fn sdl2_include_paths() -> Vec<PathBuf> {
    if let Ok(lib) = pkg_config_cflags("sdl2") {
        return lib;
    }
    vec![PathBuf::from("/usr/include/SDL2"), PathBuf::from("/usr/include")]
}

fn libdrm_include_paths() -> Vec<PathBuf> {
    if let Ok(lib) = pkg_config_cflags("libdrm") {
        return lib;
    }
    vec![PathBuf::from("/usr/include/libdrm"), PathBuf::from("/usr/include/drm")]
}

fn pkg_config_cflags(pkg: &str) -> Result<Vec<PathBuf>, ()> {
    let out = std::process::Command::new("pkg-config")
        .args(["--cflags-only-I", pkg])
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

fn link_runtime_libs(sdl: bool, drm: bool) {
    if sdl && pkg_config::Config::new().probe("sdl2").is_err() {
        println!("cargo:rustc-link-lib=SDL2");
    }
    if drm && pkg_config::Config::new().probe("libdrm").is_err() {
        println!("cargo:rustc-link-lib=drm");
    }
}
