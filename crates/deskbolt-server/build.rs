fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo sets the manifest directory");
        println!("cargo:rerun-if-changed=macos-host.plist");
        println!("cargo:rustc-link-arg-bin=deskbolt-server=-Wl,-sectcreate,__TEXT,__info_plist,{manifest}/macos-host.plist");
    }
}
