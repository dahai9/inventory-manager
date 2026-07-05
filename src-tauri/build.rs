fn main() {
    println!("cargo:rerun-if-env-changed=INVENTORY_LICENSE_PUBLIC_KEY");
    if std::env::var("PROFILE").as_deref() == Ok("release")
        && std::env::var("INVENTORY_LICENSE_PUBLIC_KEY").is_err()
    {
        panic!("release builds must set INVENTORY_LICENSE_PUBLIC_KEY to the Ed25519 public key used for activation");
    }
    tauri_build::build()
}
