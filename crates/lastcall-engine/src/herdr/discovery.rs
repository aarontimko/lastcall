//! Session discovery (docs/spec/00-spec.md §6.6) in herdr's own precedence (§5.1,
//! `src/session.rs:80-90`, `:169-181`):
//!
//! 1. `HERDR_SOCKET_PATH` — authoritative when set (we are inside a herdr pane).
//! 2. A pinned session name — config `herdr.session`, or the `HERDR_SESSION` env var when
//!    the config has no pin (additive to §6.6, same meaning) —
//!    `<config_dir>/sessions/<name>/herdr.sock`.
//! 3. The default socket `<config_dir>/herdr.sock`, if it answers `ping`.
//! 4. Enumerate `<config_dir>/sessions/*/herdr.sock` and keep the ones that answer `ping`:
//!    exactly one → use it; several → [`Discovery::Ambiguous`] and the caller shows the notice.
//!
//! `config_dir` is `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`. **All paths come from the
//! injected [`Env`]**, so a test can never reach the real socket: an `Env::empty` discovers
//! nothing.

use std::path::{Path, PathBuf};

use crate::env::Env;

/// How a socket path was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySource {
    /// `HERDR_SOCKET_PATH`.
    EnvSocketPath,
    /// `herdr.session` in config.
    ConfigSession(String),
    /// `HERDR_SESSION`.
    EnvSession(String),
    /// `<config_dir>/herdr.sock`.
    Default,
    /// The single live entry of `<config_dir>/sessions/*/herdr.sock`.
    EnumeratedSession(String),
}

/// The outcome of discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// Connect here.
    Socket {
        path: PathBuf,
        source: DiscoverySource,
    },
    /// Nothing found; `tried` lists what was considered (for the standalone notice).
    None { tried: Vec<PathBuf> },
    /// Several live sessions and no pin: connect to none (§6.6).
    Ambiguous { names: Vec<String> },
}

impl Discovery {
    /// The one-line notice for the non-socket outcomes.
    pub fn notice(&self) -> Option<String> {
        match self {
            Discovery::Socket { .. } => None,
            Discovery::None { tried } => Some(if tried.is_empty() {
                "no herdr session found (no HERDR_SOCKET_PATH and no config dir); running standalone"
                    .to_string()
            } else {
                format!(
                    "no herdr session found (tried {}); running standalone",
                    tried
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
            Discovery::Ambiguous { names } => Some(format!(
                "multiple herdr sessions ({}); set herdr.session in config to pick one; running standalone",
                names.join(", ")
            )),
        }
    }
}

/// `$XDG_CONFIG_HOME/herdr` or `~/.config/herdr`.
pub fn herdr_config_dir(env: &Env) -> Option<PathBuf> {
    env.xdg_config_home().map(|dir| dir.join("herdr"))
}

/// `<config_dir>/sessions/<name>/herdr.sock`.
pub fn session_socket_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join("sessions").join(name).join("herdr.sock")
}

/// `<config_dir>/herdr.sock`.
pub fn default_socket_path(config_dir: &Path) -> PathBuf {
    config_dir.join("herdr.sock")
}

/// The steps that need no probing, resolved purely from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    /// Steps 1–2: an authoritative path, used without probing.
    Authoritative {
        path: PathBuf,
        source: DiscoverySource,
    },
    /// Steps 3–4: probe the default socket, then enumerate sessions under `config_dir`.
    Probe { config_dir: PathBuf },
    /// No config dir can be derived (no `XDG_CONFIG_HOME`, no `HOME`).
    Nothing,
}

/// Resolve the authoritative steps. `pinned_session` is `herdr.session` from config.
pub fn candidate(env: &Env, pinned_session: Option<&str>) -> Candidate {
    if let Some(path) = env.var("HERDR_SOCKET_PATH") {
        return Candidate::Authoritative {
            path: PathBuf::from(path),
            source: DiscoverySource::EnvSocketPath,
        };
    }
    let config_dir = herdr_config_dir(env);
    let pin = pinned_session
        .filter(|s| !s.trim().is_empty())
        .map(|s| (s.to_string(), true))
        .or_else(|| {
            env.var("HERDR_SESSION")
                .filter(|s| !s.trim().is_empty())
                .map(|s| (s.to_string(), false))
        });
    match (pin, config_dir) {
        (Some((name, from_config)), Some(dir)) => Candidate::Authoritative {
            path: session_socket_path(&dir, &name),
            source: if from_config {
                DiscoverySource::ConfigSession(name)
            } else {
                DiscoverySource::EnvSession(name)
            },
        },
        (_, Some(dir)) => Candidate::Probe { config_dir: dir },
        (_, None) => Candidate::Nothing,
    }
}

/// The session names under `<config_dir>/sessions/` that have a `herdr.sock` entry, sorted.
pub fn enumerate_sessions(config_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(config_dir.join("sessions")) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let sock = entry.path().join("herdr.sock");
            sock.exists().then_some((name, sock))
        })
        .collect();
    found.sort();
    found
}

/// Full discovery. `probe` answers "does this socket answer `ping`?" — inject the transport's
/// ping in production and a closure in tests.
pub async fn discover<F>(env: &Env, pinned_session: Option<&str>, probe: F) -> Discovery
where
    F: AsyncFn(PathBuf) -> bool,
{
    let config_dir = match candidate(env, pinned_session) {
        Candidate::Authoritative { path, source } => return Discovery::Socket { path, source },
        Candidate::Nothing => return Discovery::None { tried: Vec::new() },
        Candidate::Probe { config_dir } => config_dir,
    };
    let mut tried = Vec::new();
    let default = default_socket_path(&config_dir);
    tried.push(default.clone());
    if default.exists() && probe(default.clone()).await {
        return Discovery::Socket {
            path: default,
            source: DiscoverySource::Default,
        };
    }
    let mut live = Vec::new();
    for (name, sock) in enumerate_sessions(&config_dir) {
        tried.push(sock.clone());
        if probe(sock.clone()).await {
            live.push((name, sock));
        }
    }
    match live.len() {
        0 => Discovery::None { tried },
        1 => {
            let (name, path) = live.remove(0);
            Discovery::Socket {
                path,
                source: DiscoverySource::EnumeratedSession(name),
            }
        }
        _ => Discovery::Ambiguous {
            names: live.into_iter().map(|(name, _)| name).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;
    use std::collections::HashSet;

    async fn never(_: PathBuf) -> bool {
        false
    }

    #[tokio::test]
    async fn discovery_env_socket_path_is_authoritative_and_unprobed() {
        let env = Env::empty("/w")
            .with_home("/home/u")
            .with_var("HERDR_SOCKET_PATH", "/run/herdr.sock")
            .with_var("HERDR_SESSION", "ignored");
        let d = discover(&env, Some("also-ignored"), |_: PathBuf| async {
            panic!("must not probe")
        })
        .await;
        assert_eq!(
            d,
            Discovery::Socket {
                path: PathBuf::from("/run/herdr.sock"),
                source: DiscoverySource::EnvSocketPath
            }
        );
    }

    #[tokio::test]
    async fn discovery_config_session_pin_beats_env_session() {
        let env = Env::empty("/w")
            .with_home("/home/u")
            .with_var("HERDR_SESSION", "envname");
        let d = discover(&env, Some("work"), never).await;
        assert_eq!(
            d,
            Discovery::Socket {
                path: PathBuf::from("/home/u/.config/herdr/sessions/work/herdr.sock"),
                source: DiscoverySource::ConfigSession("work".into())
            }
        );
        let d = discover(&env, None, never).await;
        assert_eq!(
            d,
            Discovery::Socket {
                path: PathBuf::from("/home/u/.config/herdr/sessions/envname/herdr.sock"),
                source: DiscoverySource::EnvSession("envname".into())
            }
        );
    }

    #[tokio::test]
    async fn discovery_uses_xdg_config_home_for_the_config_dir() {
        let env = Env::empty("/w")
            .with_home("/home/u")
            .with_var("XDG_CONFIG_HOME", "/xdg");
        assert_eq!(herdr_config_dir(&env), Some(PathBuf::from("/xdg/herdr")));
        let d = discover(&env, Some("s"), never).await;
        assert!(
            matches!(d, Discovery::Socket { ref path, .. } if path == Path::new("/xdg/herdr/sessions/s/herdr.sock"))
        );
    }

    #[tokio::test]
    async fn discovery_empty_env_finds_nothing_and_probes_nothing() {
        let env = Env::empty("/w");
        let d = discover(&env, None, |_: PathBuf| async { panic!("must not probe") }).await;
        assert_eq!(d, Discovery::None { tried: Vec::new() });
        assert!(d.notice().unwrap().contains("standalone"));
    }

    #[tokio::test]
    async fn discovery_default_socket_wins_when_live() {
        let dir = TempDir::new("lc-disc");
        let config_dir = dir.mkdir("herdr");
        dir.write("herdr/herdr.sock", "");
        dir.write("herdr/sessions/a/herdr.sock", "");
        let env =
            Env::empty("/w").with_var("XDG_CONFIG_HOME", dir.path().to_string_lossy().to_string());
        let d = discover(&env, None, |_: PathBuf| async { true }).await;
        assert_eq!(
            d,
            Discovery::Socket {
                path: default_socket_path(&config_dir),
                source: DiscoverySource::Default
            }
        );
    }

    #[tokio::test]
    async fn discovery_enumerates_sessions_and_keeps_the_live_one() {
        let dir = TempDir::new("lc-disc");
        let config_dir = dir.mkdir("herdr");
        dir.write("herdr/herdr.sock", ""); // exists but dead
        dir.write("herdr/sessions/dead/herdr.sock", "");
        dir.write("herdr/sessions/live/herdr.sock", "");
        dir.mkdir("herdr/sessions/nosock");
        let env =
            Env::empty("/w").with_var("XDG_CONFIG_HOME", dir.path().to_string_lossy().to_string());
        let live_path = session_socket_path(&config_dir, "live");
        let probed = std::sync::Mutex::new(HashSet::new());
        let d = discover(&env, None, |p: PathBuf| {
            let hit = p == live_path;
            probed.lock().unwrap().insert(p);
            async move { hit }
        })
        .await;
        assert_eq!(
            d,
            Discovery::Socket {
                path: live_path.clone(),
                source: DiscoverySource::EnumeratedSession("live".into())
            }
        );
        let probed = probed.into_inner().unwrap();
        assert!(probed.contains(&default_socket_path(&config_dir)));
        assert!(probed.contains(&session_socket_path(&config_dir, "dead")));
        assert!(probed.contains(&live_path));
        assert!(
            !probed
                .iter()
                .any(|p| p.to_string_lossy().contains("nosock"))
        );
    }

    #[tokio::test]
    async fn discovery_multiple_live_sessions_without_pin_is_ambiguous() {
        let dir = TempDir::new("lc-disc");
        dir.write("herdr/sessions/one/herdr.sock", "");
        dir.write("herdr/sessions/two/herdr.sock", "");
        let env =
            Env::empty("/w").with_var("XDG_CONFIG_HOME", dir.path().to_string_lossy().to_string());
        let d = discover(&env, None, |_: PathBuf| async { true }).await;
        assert_eq!(
            d,
            Discovery::Ambiguous {
                names: vec!["one".into(), "two".into()]
            }
        );
        let notice = d.notice().unwrap();
        assert!(notice.contains("herdr.session"), "{notice}");
        assert!(notice.contains("one, two"), "{notice}");
    }

    #[tokio::test]
    async fn discovery_nothing_live_lists_what_was_tried() {
        let dir = TempDir::new("lc-disc");
        let config_dir = dir.mkdir("herdr");
        dir.write("herdr/sessions/x/herdr.sock", "");
        let env =
            Env::empty("/w").with_var("XDG_CONFIG_HOME", dir.path().to_string_lossy().to_string());
        let d = discover(&env, None, never).await;
        assert_eq!(
            d,
            Discovery::None {
                tried: vec![
                    default_socket_path(&config_dir),
                    session_socket_path(&config_dir, "x")
                ]
            }
        );
    }
}
