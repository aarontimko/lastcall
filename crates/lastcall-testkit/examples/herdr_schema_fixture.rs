//! Write `fixtures/herdr/schema/consumed-surface.json` from a herdr binary (kickoff 11a).
//!
//! Run it through `just herdr-schema-fixture`, which passes the **pinned release asset**
//! (`herdr_version` in the `justfile`, v0.9.0 at Phase 9b): the fixture is generated from the
//! release tag, never from master and never by hand. `herdr api schema --json` needs no running server, so this touches no socket.
//!
//! ```text
//! cargo run -p lastcall-testkit --example herdr_schema_fixture -- <herdr-bin> <out.json>
//! ```

use std::path::PathBuf;
use std::process::Command;

use lastcall_testkit::herdr_schema;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let bin = PathBuf::from(args.next().ok_or("usage: <herdr-bin> <out.json>")?);
    let out = PathBuf::from(args.next().ok_or("usage: <herdr-bin> <out.json>")?);

    let version = Command::new(&bin).arg("--version").output()?;
    if !version.status.success() {
        return Err(format!("{} --version failed", bin.display()).into());
    }
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();

    let schema = Command::new(&bin)
        .args(["api", "schema", "--json"])
        .output()?;
    if !schema.status.success() {
        return Err(format!(
            "`{} api schema --json` exited {:?}: {}",
            bin.display(),
            schema.status.code(),
            String::from_utf8_lossy(&schema.stderr).trim()
        )
        .into());
    }

    let parsed: serde_json::Value = serde_json::from_slice(&schema.stdout)?;
    let projection = herdr_schema::project(&parsed)?;
    let rendered = herdr_schema::render(&projection);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&out, &rendered)?;

    eprintln!(
        "herdr-schema-fixture: {version}, protocol {}, schema_version {} -> {} ({} bytes, \
         {} methods, {} events, {} defs)",
        projection["protocol"],
        projection["schema_version"],
        out.display(),
        rendered.len(),
        projection["methods"]
            .as_object()
            .map_or(0, serde_json::Map::len),
        projection["events"]
            .as_object()
            .map_or(0, serde_json::Map::len),
        projection["defs"]
            .as_object()
            .map_or(0, serde_json::Map::len),
    );
    Ok(())
}
