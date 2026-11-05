// Physical controllers come from GameController.framework (src/hid/gc_mac.m).
fn main() {
    println!("cargo:rerun-if-changed=src/hid/gc_mac.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new().file("src/hid/gc_mac.m").flag("-fobjc-arc").flag("-fmodules").compile("ounce_gamecontroller");
        println!("cargo:rustc-link-lib=framework=GameController");
    }
}
