//! Compiles the vendored lwIP with `cc` and generates its bindings with `bindgen`.
//!
//! The two have to agree on three things — the target, the include path, and the
//! `lwipopts.h` that decides which declarations exist at all — so both are configured
//! from the same values below.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let lwip = manifest.join("lwip/src");
    let port = manifest.join("port");

    let mut sources = Vec::new();
    // lwIP's own layering: the protocol-independent core, then one directory per IP
    // version, then the ethernet link layer. Files whose feature is off in `lwipopts.h`
    // compile to nothing and are dropped by the linker.
    for dir in ["core", "core/ipv4", "core/ipv6"] {
        let mut in_dir: Vec<_> = fs::read_dir(lwip.join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "c"))
            .collect();
        in_dir.sort();
        sources.extend(in_dir);
    }
    sources.push(lwip.join("netif/ethernet.c"));
    sources.push(port.join("lwip_port.c"));

    let mut build = cc::Build::new();
    build
        .include(lwip.join("include"))
        .include(&port)
        .files(&sources)
        // One section per function and per object, so the linker can drop the protocols
        // this build compiles but never calls. Same treatment the Rust side gets.
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        // lwIP is warning-clean on its own terms; it is vendored code, not ours.
        .warnings(false);
    // lwIP's assertions are on unless LWIP_NOASSERT is defined.
    if !cfg!(feature = "assert") {
        build.define("LWIP_NOASSERT", "1");
    }
    build.compile("lwip");

    // bindgen parses the same headers with clang, so it needs the toolchain's own
    // freestanding headers (lwIP includes <string.h>, <inttypes.h>, <limits.h>).
    let mut bindings = bindgen::Builder::default()
        .header(port.join("wrapper.h").to_str().unwrap())
        .clang_arg(format!("-I{}", lwip.join("include").display()))
        .clang_arg(format!("-I{}", port.display()))
        .clang_arg(format!("--target={}", env::var("TARGET").unwrap()))
        .use_core()
        .ctypes_prefix("::core::ffi")
        // The generated file is read by humans about as often as the C is; keep the
        // doc comments lwIP's headers carry.
        .generate_comments(true)
        .layout_tests(false)
        .derive_default(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    if let Some(sysroot) = sysroot(&build) {
        bindings = bindings.clang_arg(format!("--sysroot={}", sysroot.display()));
    }
    bindings
        .generate()
        .expect("bindgen failed on lwIP's headers")
        .write_to_file(out.join("bindings.rs"))
        .unwrap();

    println!("cargo:rerun-if-changed=port");
    println!("cargo:rerun-if-changed=lwip/src");
}

/// The C compiler's sysroot, i.e. where its libc headers live.
///
/// `cc` picks the cross compiler (`arm-none-eabi-gcc` for this target); asking it where
/// its own headers are keeps clang and gcc reading the same ones, without a hardcoded
/// path to whichever toolchain happens to be installed.
fn sysroot(build: &cc::Build) -> Option<PathBuf> {
    let compiler = build.try_get_compiler().ok()?;
    let out = Command::new(compiler.path()).arg("-print-sysroot").output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (path != Path::new("") && path.is_dir()).then_some(path)
}
