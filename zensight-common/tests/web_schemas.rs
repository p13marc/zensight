//! The browser client's types are generated from the fleet type table, not
//! hand-written (#706): `web/schemas/*.json` are the JSON Schemas of the four
//! parallax wire types as every producer serves them on
//! `@rpc/<producer>/describe`, and `web/src/types.gen.ts` is compiled from
//! them by `npm run gen`. This test is the drift guard in the direction the
//! TypeScript toolchain cannot see: a `#[serde]` change here that is not
//! reflected in the checked-in schema fails `cargo test`, the same way
//! `registry.lock` pins the key grammar.
//!
//! Regenerate with `UPDATE_WEB_SCHEMAS=1 cargo test -p zensight-common
//! --test web_schemas`, then `cd web && npm run gen`.

use std::path::PathBuf;

use zensight_common::schema::SCHEMAS;

/// `(type name in the table, file under web/schemas/)`. The file name is the
/// TypeScript identifier the generator will emit, so it is plain.
const EXPORTED: &[(&str, &str)] = &[
    ("Vec<StreamDescriptor>", "StreamDescriptors.json"),
    ("StreamStatus", "StreamStatus.json"),
    ("Command<StreamControl>", "StreamCommand.json"),
    ("MediaReceiverReport", "MediaReceiverReport.json"),
];

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("web")
        .join("schemas")
}

#[test]
fn the_browser_clients_schemas_match_the_type_table() {
    let update = std::env::var_os("UPDATE_WEB_SCHEMAS").is_some();
    let dir = schemas_dir();
    let mut drift = Vec::new();
    for (type_name, file) in EXPORTED {
        let schema = SCHEMAS
            .get(type_name)
            .unwrap_or_else(|| panic!("{type_name} is not in the type table"));
        let document = schema
            .json_document()
            .unwrap_or_else(|| panic!("{type_name} is not a json-schema entry"));
        let mut rendered = serde_json::to_string_pretty(document).expect("serializes");
        rendered.push('\n');
        let path = dir.join(file);
        if update {
            std::fs::create_dir_all(&dir).expect("web/schemas exists");
            std::fs::write(&path, &rendered).expect("schema written");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(on_disk) if on_disk == rendered => {}
            Ok(_) => drift.push(format!("{file} differs from the {type_name} schema")),
            Err(e) => drift.push(format!("{file}: {e}")),
        }
    }
    assert!(
        drift.is_empty(),
        "web/schemas is behind the type table:\n  {}\nRegenerate: UPDATE_WEB_SCHEMAS=1 cargo test -p \
         zensight-common --test web_schemas && (cd web && npm run gen)",
        drift.join("\n  ")
    );
}
