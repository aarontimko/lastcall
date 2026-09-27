//! `lastcall config [--json]`: print the effective resolved config and notices. The
//! `[keys]` table is validated the way the TUI will read it, so a bad binding fails here
//! (stderr, exit 2) rather than at the next `lastcall` launch.

use std::process::ExitCode;

use lastcall::tui::input::Keymap;
use lastcall_engine::config::{self, Loaded, Resolved};
use lastcall_engine::env::Env;

pub fn run(json: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let env = Env::from_process();
    let loaded = config::load(&env)?;
    if let Err(e) = Keymap::from_config(&loaded.config.keys) {
        eprintln!("lastcall: {e}");
        return Ok(ExitCode::from(2));
    }
    let resolved = loaded.resolve(env.cwd());
    if json {
        print_json(&loaded, &resolved)?;
    } else {
        print_human(&loaded, &resolved)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_json(loaded: &Loaded, resolved: &Resolved) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&json_dump(loaded, resolved))?
    );
    Ok(())
}

/// What `lastcall config --json` prints. The config's field names are the JSON keys a
/// script reads, so a renamed field is a renamed key here.
fn json_dump(loaded: &Loaded, resolved: &Resolved) -> serde_json::Value {
    serde_json::json!({
        "config_path": loaded.source.path(),
        "source": loaded.source,
        "state_dir": loaded.state_dir,
        "config": loaded.config,
        "resolved": resolved,
    })
}

fn print_human(loaded: &Loaded, resolved: &Resolved) -> Result<(), Box<dyn std::error::Error>> {
    println!("config:    {}", loaded.source.label());
    println!("state_dir: {}", loaded.state_dir.display());
    println!("parent_dirs (effective this run):");
    for dir in &resolved.parent_dirs {
        println!("  {}", dir.display());
    }
    if resolved.notices.is_empty() {
        println!("notices:   (none)");
    } else {
        println!("notices:");
        for notice in &resolved.notices {
            println!("  {notice}");
        }
    }
    println!("--- effective config (toml) ---");
    print!("{}", toml::to_string_pretty(&loaded.config)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_engine::config::{Config, ConfigSource};
    use std::path::{Path, PathBuf};

    /// Phase 14 I: the JSON dump names the two renamed keys by their new names, and
    /// neither old name appears anywhere in it.
    #[test]
    fn config_json_names_the_renamed_keys_by_their_new_names() {
        let loaded = Loaded {
            config: Config::default(),
            source: ConfigSource::Defaults {
                searched: Vec::new(),
            },
            state_dir: PathBuf::from("/state"),
        };
        let resolved = loaded.resolve(Path::new("/launch"));
        let out = json_dump(&loaded, &resolved);
        let config = out["config"].as_object().expect("config is an object");
        assert!(config.contains_key("watch_ignore_globs"), "{out}");
        assert!(config.contains_key("include_gitignored"), "{out}");
        let text = out.to_string();
        assert!(!text.contains("\"review_ignored\""), "{text}");
        assert!(!text.contains("\"ignore_globs\""), "{text}");
    }
}
