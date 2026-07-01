fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let local_onnx = std::env::var("CARGO_FEATURE_LOCAL_ONNX").is_ok();

    if target_os == "linux" && local_onnx {
        println!("cargo:rerun-if-changed=glibc_shim.c");
        cc::Build::new().file("glibc_shim.c").compile("isoc23_shim");
    }
}
