fn main() {
    println!("cargo:rustc-link-arg-bins=--nmagic");
    // Alignment probe. `TEXT_SHIFT=<n>` moves the whole of .text up by n bytes, by
    // overriding cortex-m-rt's `PROVIDE(_stext = <end of vector table>)` from a script
    // fragment linked ahead of it. Nothing about the code changes, only where it lands,
    // which is how a real throughput difference is told apart from a code-placement
    // artifact (see README).
    println!("cargo:rerun-if-env-changed=TEXT_SHIFT");
    if let Ok(shift) = std::env::var("TEXT_SHIFT") {
        let shift: u32 = shift.parse().expect("TEXT_SHIFT must be a byte count");
        assert!(shift % 4 == 0, "TEXT_SHIFT must be a multiple of 4");
        // 0x0800_01ac is where .text starts otherwise: the end of the F429's vector table.
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        std::fs::write(out.join("probe.x"), format!("_stext = {:#x};\n", 0x0800_01acu32 + shift)).unwrap();
        println!("cargo:rustc-link-search={}", out.display());
        println!("cargo:rustc-link-arg-bins=-Tprobe.x");
    }

    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    println!("cargo:rustc-link-arg-bins=-Tteleprobe.x");

}
