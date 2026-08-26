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
}
