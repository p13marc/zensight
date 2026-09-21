fn main() {
    zenkey_build::Config::new()
        .registry_dir("registry")
        // Both ledgers default to `<registry_dir>/<name>.lock` and would be
        // picked up implicitly. They are named anyway, because their absence
        // is silent: a missing `conditional.lock` is an *empty* ledger, not an
        // error, so a rename or a stray `.gitignore` would quietly stop
        // excusing anything — and the emitted-surface check would then start
        // failing for a reason that has nothing to do with the code.
        .ledger("registry/deprecated.lock")
        .conditional_ledger("registry/conditional.lock")
        .generate()
        .unwrap();

    // The bundled view definitions (#1259): every `registry/views/<producer>.toml`
    // becomes an entry of `views::VIEWS`, as text. No parsing here — the
    // vocabulary is checked by `views::tests` and the deeper lint by the GUI,
    // which is the only crate that links the script engine.
    views_table();
}

fn views_table() {
    use std::fmt::Write as _;
    let dir = std::path::Path::new("registry/views");
    println!("cargo:rerun-if-changed=registry/views");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    (p.extension().is_some_and(|x| x == "toml"))
                        .then(|| p.file_stem()?.to_str().map(str::to_string))
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = String::from(
        "/// The bundled `views.toml` documents, by producer name (#1259).\n\
         pub static VIEWS: &[(&str, &str)] = &[\n",
    );
    for name in &names {
        let path = dir.join(format!("{name}.toml"));
        println!("cargo:rerun-if-changed={}", path.display());
        let abs = std::fs::canonicalize(&path).expect("views.toml path");
        let _ = writeln!(out, "    ({name:?}, include_str!({:?})),", abs.display());
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("zensight_views.rs");
    std::fs::write(dest, out).expect("write zensight_views.rs");
}
