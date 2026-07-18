fn main() {
    zenkey_build::Config::new()
        .registry_dir("registry")
        .generate()
        .unwrap();
}
