//! `lastcall update` end to end, with no network anywhere (kickoff deliverable 2.8).
//!
//! Every scene runs the **built binary** against a served directory laid out like the
//! release API, reached through `tests/probe/curl.sh` installed as `curl` first on the
//! child's `PATH`. Nothing here resolves a hostname: with `LASTCALL_TEST_RELEASE_DIR`
//! unset the probe exits 99, which is its own scene below.
//!
//! The served "binary" is a two-line shell script rather than a second real build: one
//! `cargo test` compiles one version of `lastcall`, so a test that wants to see
//! `lastcall 9.9.9` printed by the replaced file has to serve something that is not this
//! binary. What the update path promises is bytes-in-equals-bytes-out plus `0o755`, and a
//! script proves both while also being runnable.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lastcall_testkit::tmp::TempDir;
use sha2::{Digest, Sha256};

/// The host triple the release matrix builds, mirroring `commands/update.rs::host_target`.
/// Repeated rather than shared because the binary's modules are not a library.
fn host_target() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        panic!("this host is not in the release matrix; the update tests cannot run here");
    }
}

/// What the served release contains: a script that announces a version this build cannot.
const ASSET_BODY: &str = "#!/bin/sh\necho lastcall 9.9.9\n";

const NEWER: &str = "9.9.9";

/// Whether the crate this build came from is itself a prerelease.
///
/// It decides which API path `update` reads: `releases/latest` never answers with a
/// prerelease, so a prerelease binary pages the list instead (`commands/update.rs::lookup`).
/// The crate version crosses that line during a release (`0.1.0-rc.1`, then `0.1.0`), so
/// every scene here serves **both** shapes and every assertion asks this rather than naming
/// one of them.
fn this_build_is_a_prerelease() -> bool {
    env!("CARGO_PKG_VERSION").contains('-')
}

/// The API path this build's `update` will actually ask for.
fn api_path() -> &'static str {
    if this_build_is_a_prerelease() {
        "/releases?per_page=10"
    } else {
        "/releases/latest"
    }
}

fn probe_curl() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/probe/curl.sh"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("the file is there")
        .permissions()
        .mode()
        & 0o777
}

/// One scene: a private `PATH` whose `curl` is the probe, a served release directory, and a
/// copy of the built binary to replace.
struct Scene {
    dir: TempDir,
    serve: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

impl Scene {
    /// A scene whose served `latest.json` offers `tag` and whose `SHA256SUMS` is correct.
    fn new(tag: &str) -> Scene {
        let dir = TempDir::new("lc-update");
        let serve = dir.mkdir("serve");
        let bindir = dir.mkdir("bin");
        let probe_dir = dir.mkdir("probe");
        std::os::unix::fs::symlink(probe_curl(), probe_dir.join("curl")).expect("probe curl");
        let bin = bindir.join("lastcall");
        std::fs::copy(env!("CARGO_BIN_EXE_lastcall"), &bin).expect("copy the binary");
        let scene = Scene {
            serve,
            bin,
            log: dir.path().join("curl.log"),
            dir,
        };
        scene.serve_release(tag, false);
        scene.write(scene.asset_name(), ASSET_BODY.to_owned());
        scene.write(
            "SHA256SUMS",
            format!(
                "{}  {}\n",
                sha256_hex(ASSET_BODY.as_bytes()),
                scene.asset_name()
            ),
        );
        scene
    }

    fn asset_name(&self) -> String {
        format!("lastcall-{NEWER}-{}", host_target())
    }

    /// Serve one release as **both** API shapes: the single object `releases/latest`
    /// answers with, and the one-element array `releases?per_page=N` answers with. Which
    /// one the binary asks for depends on whether this build is a prerelease, and the two
    /// files say the same thing so no scene has to care.
    fn serve_release(&self, tag: &str, prerelease: bool) {
        let one = format!(r#"{{"tag_name":"{tag}","prerelease":{prerelease}}}"#);
        self.write("latest.json", one.clone());
        self.write("list.json", format!("[{one}]"));
    }

    /// The served file this build's `update` will read, for the `.status` / `.headers`
    /// sidecars the probe looks for beside it.
    fn api_file(&self) -> &'static str {
        if this_build_is_a_prerelease() {
            "list.json"
        } else {
            "latest.json"
        }
    }

    fn write(&self, name: impl AsRef<Path>, body: String) {
        std::fs::write(self.serve.join(name), body).expect("serve a file");
    }

    fn probe_dir(&self) -> PathBuf {
        self.dir.path().join("probe")
    }

    /// Put a stand-in of our own where the probe `curl` is, for the failures the probe does
    /// not model. The tracked probe is reached through a **symlink** here, so it is removed
    /// first: writing through the link would rewrite the repository's own file.
    fn install_curl(&self, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.probe_dir().join("curl");
        std::fs::remove_file(&path).expect("the probe symlink");
        std::fs::write(&path, script).expect("the stand-in curl");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    }

    /// Run the copied binary. `release_dir` false leaves `LASTCALL_TEST_RELEASE_DIR` unset,
    /// which is how the probe's exit 99 is reached.
    fn run(&self, args: &[&str], release_dir: bool) -> Output {
        let path = format!(
            "{}:{}",
            self.probe_dir().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .env("LASTCALL_PROBE_CURL_LOG", &self.log)
            .env_remove("LASTCALL_UPDATE_BASE_URL")
            .env_remove("LASTCALL_TEST_RELEASE_DIR");
        if release_dir {
            cmd.env("LASTCALL_TEST_RELEASE_DIR", &self.serve);
        }
        cmd.output().expect("the binary runs")
    }

    fn urls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("url: ").map(str::to_owned))
            .collect()
    }

    fn bin_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.bin).expect("the binary is still there")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The happy path: verified, replaced, runnable, and `0o755` whatever the server's mode was.
#[test]
fn update_replaces_the_binary_with_the_verified_asset() {
    let scene = Scene::new("v9.9.9");
    let before = scene.bin_bytes();
    assert_ne!(before, ASSET_BODY.as_bytes(), "the copy is the real binary");

    let out = scene.run(&["update"], true);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        format!("lastcall {} → {NEWER}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        scene.bin_bytes(),
        ASSET_BODY.as_bytes(),
        "the target is byte-for-byte the served asset"
    );
    assert_eq!(mode_of(&scene.bin), 0o755);
    let replaced = Command::new(&scene.bin)
        .output()
        .expect("the new file runs");
    assert_eq!(stdout(&replaced).trim_end(), "lastcall 9.9.9");

    // Three fetches and nothing else: the release, the asset, the checksums.
    let urls = scene.urls();
    assert_eq!(urls.len(), 3, "{urls:?}");
    assert!(urls[0].ends_with(api_path()), "{urls:?}");
    assert!(
        urls[1].ends_with(&format!("/download/v9.9.9/{}", scene.asset_name())),
        "{urls:?}"
    );
    assert!(urls[2].ends_with("/download/v9.9.9/SHA256SUMS"), "{urls:?}");
    // No temp file survives a success.
    let leftovers: Vec<PathBuf> = std::fs::read_dir(scene.bin.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".lastcall-update"))
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// A wrong `SHA256SUMS` for the very same asset: exit 1, `checksum mismatch`, and the
/// binary the user is running is untouched byte-for-byte.
#[test]
fn update_refuses_a_checksum_mismatch_and_leaves_the_binary_alone() {
    let scene = Scene::new("v9.9.9");
    scene.write(
        "SHA256SUMS",
        format!("{}  {}\n", "0".repeat(64), scene.asset_name()),
    );
    let before = scene.bin_bytes();

    let out = scene.run(&["update"], true);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim_end(), "update: checksum mismatch");
    assert_eq!(scene.bin_bytes(), before, "not one byte moved");
    // The `Drop` guard cleaned the download up on the way out.
    let leftovers: Vec<PathBuf> = std::fs::read_dir(scene.bin.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".lastcall-update"))
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// `--check` says what it found and writes nothing at all.
#[test]
fn update_check_reports_without_writing() {
    let scene = Scene::new("v9.9.9");
    let before = scene.bin_bytes();
    let out = scene.run(&["update", "--check"], true);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        format!("{NEWER} available — run: lastcall update")
    );
    assert_eq!(scene.bin_bytes(), before);
    assert_eq!(scene.urls().len(), 1, "the API only: {:?}", scene.urls());

    // An older release than this binary is "up to date", and still nothing is written.
    let old = Scene::new("v0.0.1");
    let out = old.run(&["update", "--check"], true);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        format!("up to date ({})", env!("CARGO_PKG_VERSION"))
    );
    // …and `update` itself stops there too, before it fetches an asset.
    let out = old.run(&["update"], true);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        old.urls().len(),
        2,
        "one API call per run: {:?}",
        old.urls()
    );
}

/// A prerelease from another line is never offered, whatever this build is.
///
/// `choose` takes a prerelease only for a binary that is itself a prerelease **of the same
/// `major.minor.patch`**, so a `9.9.9` prerelease is refused by a stable build (it is a
/// prerelease) and by an rc build of `0.1.0` (it is a different line) alike. The unit tests
/// cover the semver rule itself; this proves the served answer travels through the command
/// the same way.
#[test]
fn update_does_not_offer_a_prerelease_from_another_line() {
    let scene = Scene::new("v9.9.9");
    scene.serve_release("v9.9.9", true);
    let out = scene.run(&["update", "--check"], true);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim_end(),
        format!("up to date ({})", env!("CARGO_PKG_VERSION"))
    );
}

/// A binary under `~/.cargo/bin` refuses before it fetches anything. Homebrew and nix use
/// absolute prefixes no test may create, so those two are unit-tested on the path strings
/// (`commands::update::tests::update_refuses_package_manager_paths_on_the_path_strings`);
/// this is the one refusal a temp directory can actually reproduce.
#[test]
fn update_refuses_a_cargo_installed_binary_before_any_fetch() {
    let scene = Scene::new("v9.9.9");
    let cargo_bin = scene.dir.mkdir(".cargo/bin");
    let installed = cargo_bin.join("lastcall");
    std::fs::copy(&scene.bin, &installed).expect("copy");
    let before = std::fs::read(&installed).expect("read");

    let path = format!(
        "{}:{}",
        scene.probe_dir().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(&installed)
        .arg("update")
        .env("PATH", path)
        .env("HOME", scene.dir.path())
        .env("LASTCALL_PROBE_CURL_LOG", &scene.log)
        .env("LASTCALL_TEST_RELEASE_DIR", &scene.serve)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("installed by cargo"), "{said}");
    assert!(said.contains("cargo install --git"), "{said}");
    assert_eq!(scene.urls(), Vec::<String>::new(), "nothing was fetched");
    assert_eq!(std::fs::read(&installed).unwrap(), before);
}

/// The rate limit is reported as the rate limit, never as "no release".
#[test]
fn update_reports_the_github_rate_limit_with_its_reset() {
    let scene = Scene::new("v9.9.9");
    let api = format!("{}.status", scene.api_file());
    std::fs::write(scene.serve.join(api), "403").expect("status");
    std::fs::write(
        scene.serve.join(format!("{}.headers", scene.api_file())),
        "X-RateLimit-Reset: 1757600000\n",
    )
    .expect("headers");
    let out = scene.run(&["update", "--check"], true);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("GitHub API rate limit"), "{said}");
    assert!(said.contains("2025-09-11T"), "the reset is a time: {said}");
}

/// `LASTCALL_UPDATE_BASE_URL` is loopback or nothing, and it announces itself either way.
#[test]
fn update_base_url_is_announced_for_loopback_and_ignored_otherwise() {
    let scene = Scene::new("v9.9.9");
    let path = format!(
        "{}:{}",
        scene.probe_dir().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |value: &str| -> Output {
        let _ = std::fs::remove_file(&scene.log);
        Command::new(&scene.bin)
            .args(["update", "--check"])
            .env("PATH", &path)
            .env("HOME", scene.dir.path())
            .env("LASTCALL_PROBE_CURL_LOG", &scene.log)
            .env("LASTCALL_TEST_RELEASE_DIR", &scene.serve)
            .env("LASTCALL_UPDATE_BASE_URL", value)
            .output()
            .expect("runs")
    };

    let out = run("http://127.0.0.1:8099/");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("update: test base URL http://127.0.0.1:8099/"),
        "{}",
        stderr(&out)
    );
    assert!(
        scene.urls()[0].starts_with("http://127.0.0.1:8099/"),
        "{:?}",
        scene.urls()
    );

    // Not another host, and (verifier (a) F14) not a path tail either: the served layout
    // starts at the root.
    for value in [
        "https://releases.example.com/",
        "http://127.0.0.1:8099/serve/",
        "http://127.0.0.1:8099/../",
    ] {
        let out = run(value);
        assert!(out.status.success(), "{}: {}", value, stderr(&out));
        assert!(
            stderr(&out).contains("ignoring"),
            "{}: {}",
            value,
            stderr(&out)
        );
        assert!(
            scene.urls()[0].starts_with("https://api.github.com/"),
            "the ignored value falls back to GitHub, not to the attacker: {:?}",
            scene.urls()
        );
    }
}

/// Verifier (a) F4: curl's own verdict comes before its output. A transfer that stalls
/// under `--speed-time` exits 28 having written a partial file; reading that as a response
/// made the program say `checksum mismatch`, which tells a user on a slow link that the
/// release they are downloading is corrupt.
#[test]
fn update_reports_a_failed_curl_rather_than_a_checksum_mismatch() {
    let scene = Scene::new("v9.9.9");
    let before = scene.bin_bytes();
    scene.install_curl(
        r#"#!/bin/sh
# The release list answers normally; the asset download dies half way through.
dest=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) dest="$2"; shift 2 ;;
        *) url="$1"; shift ;;
    esac
done
case "$url" in
    */releases/latest)
        printf 'HTTP/1.1 200 probe\r\n\r\n{"tag_name":"v9.9.9","prerelease":false}200'
        exit 0
        ;;
esac
[ -n "$dest" ] && printf '#!/bin/' >"$dest"
printf '200'
exit 28
"#,
    );
    let out = scene.run(&["update"], true);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("curl exited 28"), "{said}");
    assert!(!said.contains("checksum mismatch"), "{said}");
    assert_eq!(scene.bin_bytes(), before, "not one byte moved");
    let leftovers: Vec<String> = std::fs::read_dir(scene.bin.parent().expect("a directory"))
        .expect("the binary's directory")
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|name| name.starts_with(".lastcall-update-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// The isolation's own test: with no served directory the probe exits 99, so a scene that
/// reaches for the network fails loudly instead of spending anyone's rate limit.
#[test]
fn update_without_a_served_directory_fails_loudly() {
    let scene = Scene::new("v9.9.9");
    let out = scene.run(&["update", "--check"], false);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let said = stderr(&out);
    assert!(said.contains("curl exited 99"), "{said}");
    assert!(
        said.contains("LASTCALL_TEST_RELEASE_DIR is unset"),
        "{said}"
    );
}
