#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(gles)");

    // gpui_apple's build script generates its Metal header from these sources.
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!(
        "cargo::metadata=src_dir={}",
        manifest_dir.join("src").display()
    );
    // The `.rc` and manifest, for a dependent that embeds them in its own executables
    // (DEP_SKATHI_GPUI_WINDOWS_RESOURCES).
    println!(
        "cargo::metadata=windows_resources={}",
        manifest_dir.join("resources").join("windows").display()
    );

    // Leak detection does bookkeeping on every entity handle, so `test-support`
    // does not enable it: that feature must stay cheap enough to compile into
    // every build. CI sets `GPUI_LEAK_DETECTION`; the `leak-detection` feature
    // turns it on explicitly.
    println!("cargo::rustc-check-cfg=cfg(gpui_leak_detection)");
    println!("cargo::rerun-if-env-changed=GPUI_LEAK_DETECTION");
    let requested_by_environment =
        std::env::var_os("GPUI_LEAK_DETECTION").is_some_and(|value| !value.is_empty());
    if requested_by_environment || std::env::var_os("CARGO_FEATURE_LEAK_DETECTION").is_some() {
        println!("cargo::rustc-cfg=gpui_leak_detection");
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Hot reloading is limited to debug builds so the feature can't ship in release binaries,
    // and to the platforms whose patch transport is implemented.
    println!("cargo::rustc-check-cfg=cfg(gpui_hot_reload)");
    if std::env::var_os("CARGO_FEATURE_HOT_RELOAD").is_some()
        && std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some()
        && matches!(target_os.as_str(), "macos" | "linux" | "windows" | "ios")
    {
        println!("cargo::rustc-cfg=gpui_hot_reload");
    }

    if target_os == "windows" {
        #[cfg(feature = "windows-manifest")]
        embed_resource();
    }
}

#[cfg(feature = "windows-manifest")]
fn embed_resource() {
    let resource_dir = std::path::Path::new("resources/windows");
    let manifest = resource_dir.join("gpui.manifest.xml");
    let rc_file = resource_dir.join("gpui.rc");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rerun-if-changed={}", rc_file.display());
    embed_resource::compile(rc_file, embed_resource::ParamsIncludeDirs([resource_dir]))
        .manifest_required()
        .unwrap();
}
