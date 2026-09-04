//! `lastcall status [--json] [--root <path>…]`: open the engine, scan, print the report.
//!
//! Exit 0 on success (a scan failure for one root is a notice, not an exit code); exit 1
//! when the engine cannot open at all (unwritable state dir, git too old, no git) or a
//! `--root` is not inside any watched root. `--root` scans only the roots it names.

use std::path::PathBuf;
use std::process::ExitCode;

use lastcall_engine::config;
use lastcall_engine::engine::Engine;
use lastcall_engine::env::Env;
use lastcall_engine::status::StatusReport;

pub fn run(json: bool, roots: Vec<PathBuf>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let env = Env::from_process();
    let loaded = config::load(&env)?;
    let resolved = loaded.resolve(env.cwd());
    let mut engine = match Engine::open(&loaded, &resolved, &env, crate::commands::engine_options())
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("lastcall: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    let only = (!roots.is_empty()).then_some(roots.as_slice());
    let report = match StatusReport::build(&mut engine, only) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lastcall: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.render_human());
    }
    Ok(ExitCode::SUCCESS)
}
