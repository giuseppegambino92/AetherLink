fn main() {
    // iOS links the *staticlib* into the Xcode app, where the Swift side provides the
    // `aether_swift_*` callbacks (and plugin `init_plugin_*` entry points). The cdylib that
    // Cargo also emits for the crate-type list is never used on iOS; let it leave those
    // symbols to be resolved at load time so the build does not fail.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-undefined,dynamic_lookup");
    }
    tauri_build::build();
}
