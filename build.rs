fn main() {
    println!("cargo:rustc-check-cfg=cfg(have_libinput_plugin_system)");
    println!("cargo:rustc-check-cfg=cfg(have_anland_audio)");
    if pkg_config::Config::new()
        .atleast_version("1.30.0")
        .probe("libinput")
        .is_ok()
    {
        println!("cargo:rustc-cfg=have_libinput_plugin_system")
    }

    // Anland backend C sources are only compiled when the `anland` feature
    // (and thus the optional `cc` dependency) is enabled.
    #[cfg(feature = "anland")]
    build_anland();
}

#[cfg(feature = "anland")]
fn build_anland() {
    // The display-producer + socket IPC was ported to Rust (see ffi.rs); only
    // the audio/camera PipeWire bridges remain C, compiled when libpipewire is
    // available (sets the `have_anland_audio` cfg).
    let pipewire = pkg_config::Config::new().probe("libpipewire-0.3");
    let Ok(lib) = &pipewire else { return };

    let mut build = cc::Build::new();
    build
        .file("src/backend/anland/c/anland_audio.c")
        .file("src/backend/anland/c/anland_camera.c")
        .include("src/backend/anland/c")
        .warnings(false);

    for path in &lib.include_paths {
        build.include(path);
    }
    for lib in &lib.libs {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    println!("cargo:rustc-cfg=have_anland_audio");
    build.compile("anland_av");
}
