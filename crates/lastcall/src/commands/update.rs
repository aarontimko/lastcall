//! `lastcall update [--check]`: replace this binary with a newer GitHub release, after a
//! SHA-256 check (kickoff deliverable 2; docs/spec/00-spec.md §8 Phase 9, §10 2026-09-04).
//!
//! Three rules shape the whole file.
//!
//! 1. **No HTTP crate.** Every network call is a `curl` subprocess built by [`curl_args`]
//!    and run by [`fetch`]. That keeps the shipped dependency tree free of a TLS stack, and
//!    it is what makes the tests offline: an integration test puts `tests/probe/curl.sh`
//!    first on `PATH` and no byte leaves the machine.
//! 2. **Verify, then replace.** The digest is checked against the release's `SHA256SUMS`
//!    before anything is renamed over the running binary, the temp file is created `O_EXCL`
//!    beside the canonical executable (same filesystem, so `rename` cannot fail `EXDEV`),
//!    and a `Drop` guard removes it on every error path.
//! 3. **The environment is not a redirect.** `LASTCALL_UPDATE_BASE_URL` is honoured only by
//!    the explicit command, only for loopback, and it announces itself on stderr. The TUI's
//!    daily check never reads it at all.
//!
//! This module is also the binary's only `std::env` reader outside `commands/mod.rs` and
//! `tui/term.rs`, its only `Command::new` outside the `$EDITOR` spawn, and one of the two
//! places that create files outside a repository (`docs/dev/tui.md` "Gate greps").

use std::cmp::Ordering;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// The version this binary was built as; the left-hand side of every comparison.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// `<owner>/<repo>` for every URL this module builds.
pub const REPO: &str = "aarontimko/lastcall";

/// Where the release metadata comes from.
pub const API_BASE: &str = "https://api.github.com";

/// Where the release assets come from.
pub const DOWNLOAD_BASE: &str = "https://github.com";

/// The loopback-only test hook (rule 2.9). Never read by [`daily_check`].
pub const BASE_URL_VAR: &str = "LASTCALL_UPDATE_BASE_URL";

/// The once-a-day stamp under the state dir.
pub const STAMP_FILE: &str = "update-check.json";

/// How long a stamp throttles the background lookup.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// How many releases the prerelease path asks for.
pub const PRERELEASE_PAGE: usize = 10;

// ---------------------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------------------

/// A message for stderr and the exit code that goes with it. Every message is printed as
/// `update: <message>`, which is why none of them repeats the word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub message: String,
    pub code: u8,
}

impl Fail {
    /// Exit 2: the command could not do the job (bad host, no curl, a refusal, an
    /// unreachable API). Nothing was replaced.
    fn stop(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    /// Exit 1: the download happened and did not verify. Reserved for `checksum mismatch`,
    /// which is the one failure that means "somebody or something served the wrong bytes".
    fn mismatch() -> Self {
        Self {
            message: "checksum mismatch".to_owned(),
            code: 1,
        }
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

// ---------------------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------------------

/// A hand-rolled `major.minor.patch[-pre]`. No `semver` crate: the three numbers and an
/// opaque prerelease tag are the whole grammar this program's tags use, and a dependency
/// that only the updater needs is a dependency every user carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// `Some("rc.1")` for `0.1.0-rc.1`. A release has `None`, and a release always beats a
    /// prerelease of the same three numbers.
    pub pre: Option<String>,
}

impl Version {
    /// Parse `0.1.0`, `v0.1.0` or `0.1.0-rc.1`. Anything else is `None` (an unparsable tag
    /// is skipped, never guessed at).
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        // Build metadata is not precedence; drop it before anything else looks at the text.
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Whether this is a prerelease by its own version string.
    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some()
    }

    /// Whether the two share `major.minor.patch`, whatever their prerelease tags are.
    pub fn same_triple(&self, other: &Version) -> bool {
        (self.major, self.minor, self.patch) == (other.major, other.minor, other.patch)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // Semver's one counter-intuitive rule: 0.1.0 is newer than 0.1.0-rc.1.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Releases
// ---------------------------------------------------------------------------------------

/// One release as the API describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The tag exactly as GitHub spells it (`v0.1.1`); the download URL uses it verbatim.
    pub tag: String,
    pub version: Version,
    /// GitHub's own flag, OR'd with the version's prerelease tag: either is enough.
    pub prerelease: bool,
}

/// Every release in an API answer. The answer is one object (`releases/latest`) or an array
/// (`releases?per_page=…`); a tag that is not semver is skipped rather than guessed at.
pub fn releases_from_json(value: &serde_json::Value) -> Vec<Release> {
    let items: Vec<&serde_json::Value> = match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    items
        .into_iter()
        .filter(|item| item.get("draft") != Some(&serde_json::Value::Bool(true)))
        .filter_map(|item| {
            let tag = item.get("tag_name")?.as_str()?.to_owned();
            let version = Version::parse(&tag)?;
            let flagged = item.get("prerelease").and_then(|p| p.as_bool()) == Some(true);
            Some(Release {
                prerelease: flagged || version.is_prerelease(),
                tag,
                version,
            })
        })
        .collect()
}

/// The newest release worth offering to `current`, or `None`.
///
/// A prerelease is offered only to a binary that is itself a prerelease of the same
/// `major.minor.patch` — otherwise `0.2.0-rc.1` would push itself onto every stable user
/// the day it is tagged.
pub fn choose(current: &Version, releases: &[Release]) -> Option<Release> {
    releases
        .iter()
        .filter(|r| !r.prerelease || (current.is_prerelease() && current.same_triple(&r.version)))
        .filter(|r| r.version > *current)
        .max_by(|a, b| a.version.cmp(&b.version))
        .cloned()
}

// ---------------------------------------------------------------------------------------
// Assets
// ---------------------------------------------------------------------------------------

/// The host triple this binary was built for, when the release matrix builds one.
/// `None` on any other host, which is an `no release asset for …` rather than a panic.
pub fn host_target() -> Option<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        Some("aarch64-unknown-linux-gnu")
    } else {
        None
    }
}

/// What the `no release asset for <target>` message names: the real triple when there is
/// one, and the host's own arch and OS when the matrix does not build for it.
pub fn host_target_label() -> String {
    host_target()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS))
}

/// The asset name for a release: `lastcall-<version>-<target>`. The version is the **tag's**
/// (`lastcall-0.1.0-rc.1-…`), so an rc and its final release never collide in `SHA256SUMS`.
pub fn asset_name(version: &Version, target: &str) -> String {
    format!("lastcall-{version}-{target}")
}

/// The digest `SHA256SUMS` records for `name`, lowercased.
///
/// `sha256sum` writes `<hex>  <name>` (two spaces, text mode) or `<hex> *<name>` (binary
/// mode), and hand-written files use one space; all three parse here.
pub fn sha256sums_lookup(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hex) = parts.next() else { continue };
        let Some(entry) = parts.next() else { continue };
        if parts.next().is_some() {
            // A name with a space in it is not something this release ever produces, and
            // guessing where the digest ends is how a verifier is talked into the wrong file.
            continue;
        }
        if entry.trim_start_matches('*') == name {
            return Some(hex.to_ascii_lowercase());
        }
    }
    None
}

/// SHA-256 of a file, lowercase hex.
fn sha256_file(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------

/// The refusal a package-manager-owned path earns, or `None`.
///
/// Nothing on disk records how a binary was installed, so the path is the only evidence
/// there is. Both the `current_exe` path and its `canonicalize`d form are checked by the
/// caller: `/usr/local/bin/lastcall` is a symlink into the Cellar on an Intel Mac.
pub fn refusal_for(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    let text = text.as_ref();
    let under = |prefix: &str| text == prefix.trim_end_matches('/') || text.starts_with(prefix);
    if under("/opt/homebrew/") || under("/usr/local/Cellar/") || under("/home/linuxbrew/") {
        return Some("installed by Homebrew — run: brew upgrade lastcall".to_owned());
    }
    if text.contains("/.cargo/bin/") {
        return Some(
            "installed by cargo — run: cargo install --git https://github.com/aarontimko/lastcall --force"
                .to_owned(),
        );
    }
    if under("/nix/store/") {
        return Some("installed by nix — update the flake or profile that provides it".to_owned());
    }
    None
}

// ---------------------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------------------

/// Where this run looks for releases. `Default` is GitHub; `Test` is the loopback base URL
/// the install smoke and the integration tests serve from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Default,
    /// Always ends in `/`.
    Test(String),
}

/// Whether a `LASTCALL_UPDATE_BASE_URL` value may be used: `http://127.0.0.1:<port>/` or
/// `http://localhost:<port>/` and nothing else.
///
/// An environment variable that can point a self-updater at an arbitrary host is a remote
/// code path whose checksum "verifies" (the checksum comes from the same host), and a herdr
/// layout can put environment into a pane. Loopback cannot be another machine.
pub fn loopback_base(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = authority.rsplit_once(':')?;
    if host != "127.0.0.1" && host != "localhost" {
        return None;
    }
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut url = format!("http://{host}:{port}{path}");
    if !url.ends_with('/') {
        url.push('/');
    }
    Some(url)
}

impl Base {
    /// `releases/latest` or `releases?per_page=N` for this base.
    pub fn api_url(&self, path: &str) -> String {
        match self {
            Base::Default => format!("{API_BASE}/repos/{REPO}/{path}"),
            Base::Test(base) => format!("{base}repos/{REPO}/{path}"),
        }
    }

    /// One release asset. The tail is always `download/<tag>/<asset>`, which is what the
    /// probe `curl` and the smoke's `http.server` layout key on.
    pub fn download_url(&self, tag: &str, asset: &str) -> String {
        match self {
            Base::Default => format!("{DOWNLOAD_BASE}/{REPO}/releases/download/{tag}/{asset}"),
            Base::Test(base) => format!("{base}{REPO}/releases/download/{tag}/{asset}"),
        }
    }
}

/// Read `LASTCALL_UPDATE_BASE_URL` for the **explicit** command, announcing what it found.
/// A value that is not loopback is ignored with a warning rather than honoured.
fn base_from_env() -> Base {
    let Some(raw) = std::env::var_os(BASE_URL_VAR) else {
        return Base::Default;
    };
    let raw = raw.to_string_lossy().into_owned();
    if raw.is_empty() {
        return Base::Default;
    }
    match loopback_base(&raw) {
        Some(url) => {
            eprintln!("update: test base URL {url}");
            Base::Test(url)
        }
        None => {
            eprintln!(
                "update: ignoring {BASE_URL_VAR}={raw} (only http://127.0.0.1:<port>/ or http://localhost:<port>/)"
            );
            Base::Default
        }
    }
}

// ---------------------------------------------------------------------------------------
// curl
// ---------------------------------------------------------------------------------------

/// What one `curl` run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub status: u16,
    /// The last response's header lines (`-D -`), empty for an asset download.
    pub headers: Vec<String>,
    /// The response body, empty when the bytes went to a file.
    pub body: String,
}

impl Fetched {
    /// One header by name, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

/// The argument vector for one fetch. `dest` picks the shape: `None` is the JSON call
/// (short timeout, headers on stdout), `Some` is an asset (herdr's stall detector, bytes to
/// a file). Separated from [`fetch`] so the numbers are asserted without running anything.
pub fn curl_args(url: &str, dest: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-sSL".into(),
        "-A".into(),
        format!("lastcall/{CURRENT}"),
        "-w".into(),
        "%{http_code}".into(),
    ];
    match dest {
        None => {
            args.push("--max-time".into());
            args.push("20".into());
            // Headers on stdout: a 403 is only distinguishable from "no release" by
            // `X-RateLimit-Reset`, and telling a user "no release" when GitHub said
            // "slow down" is the wrong answer twice.
            args.push("-D".into());
            args.push("-".into());
        }
        Some(path) => {
            args.push("--max-time".into());
            args.push("120".into());
            args.push("--speed-limit".into());
            args.push("1024".into());
            args.push("--speed-time".into());
            args.push("30".into());
            args.push("-o".into());
            args.push(path.to_string_lossy().into_owned());
        }
    }
    args.push(url.to_owned());
    args
}

/// Split `curl -D -` output into the **last** response's header lines and the body. A
/// redirect chain writes one block per hop; only the last one describes what arrived.
/// Output with no `HTTP/` prefix at all is all body, which is what `-o` leaves behind.
pub fn split_headers(raw: &str) -> (Vec<String>, String) {
    let mut rest = raw;
    let mut last: Vec<String> = Vec::new();
    while rest.starts_with("HTTP/") {
        let (block, tail) = match (rest.find("\r\n\r\n"), rest.find("\n\n")) {
            (Some(i), _) => (&rest[..i], &rest[i + 4..]),
            (None, Some(i)) => (&rest[..i], &rest[i + 2..]),
            (None, None) => (rest, ""),
        };
        last = block.lines().map(str::to_owned).collect();
        rest = tail;
    }
    (last, rest.to_owned())
}

/// The one network call in the program. `dest` writes the body to a file instead of
/// returning it.
pub fn fetch(url: &str, dest: Option<&Path>) -> Result<Fetched, Fail> {
    let args = curl_args(url, dest);
    let output = match Command::new("curl").args(&args).output() {
        Ok(output) => output,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Fail::stop("curl not found"));
        }
        Err(e) => return Err(Fail::stop(format!("could not run curl: {e}"))),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // `-w '%{http_code}'` puts exactly three digits last, whatever else happened.
    let cut = stdout.len().saturating_sub(3);
    let status: u16 = stdout[cut..].parse().map_err(|_| {
        let why = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Fail::stop(if why.is_empty() {
            format!("could not reach {url}")
        } else {
            format!("could not reach {url}: {why}")
        })
    })?;
    let (headers, body) = split_headers(&stdout[..cut]);
    Ok(Fetched {
        status,
        headers,
        body,
    })
}

/// Ask the API which releases exist. A 403 is reported as the rate limit it is.
fn lookup(base: &Base, current: &Version) -> Result<Vec<Release>, Fail> {
    // `releases/latest` never answers with a prerelease, so a prerelease binary has to read
    // the list to find the rc that follows it.
    let path = if current.is_prerelease() {
        format!("releases?per_page={PRERELEASE_PAGE}")
    } else {
        "releases/latest".to_owned()
    };
    let url = base.api_url(&path);
    let answer = fetch(&url, None)?;
    match answer.status {
        200 => {}
        403 | 429 => {
            let reset = answer
                .header("x-ratelimit-reset")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|secs| {
                    lastcall_engine::ledger::iso8601(UNIX_EPOCH + Duration::from_secs(secs))
                });
            return Err(Fail::stop(match reset {
                Some(at) => format!("GitHub API rate limit — try after {at}"),
                None => "GitHub API rate limit — try again later".to_owned(),
            }));
        }
        404 => return Err(Fail::stop(format!("no releases yet ({url})"))),
        other => return Err(Fail::stop(format!("GitHub API answered {other} ({url})"))),
    }
    let value: serde_json::Value = serde_json::from_str(&answer.body)
        .map_err(|e| Fail::stop(format!("could not read the release list: {e}")))?;
    Ok(releases_from_json(&value))
}

// ---------------------------------------------------------------------------------------
// The replace
// ---------------------------------------------------------------------------------------

/// Removes whatever it holds when the scope ends: the download's temp files go away on
/// every path out of [`replace`], the successful one included (where the rename has already
/// taken the asset away and the removal is a no-op).
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Prove the binary's directory is writable before spending anyone's bandwidth.
fn probe_writable(dir: &Path) -> Result<(), Fail> {
    let probe = dir.join(".lastcall-update-probe");
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&probe);
    let _ = std::fs::remove_file(&probe);
    opened
        .map(|_| ())
        .map_err(|_| Fail::stop(format!("cannot write {}: permission denied", dir.display())))
}

/// Claim a temp name with `O_EXCL`, so two `lastcall update`s cannot write one file.
fn claim(path: &Path) -> Result<(), Fail> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => Fail::stop(format!(
                "{} already exists (another update, or a crashed one: delete it)",
                path.display()
            )),
            _ => Fail::stop(format!("cannot write {}: {e}", path.display())),
        })
}

fn download_and_replace(base: &Base, release: &Release, canonical: &Path) -> Result<(), Fail> {
    let Some(target) = host_target() else {
        return Err(Fail::stop(format!(
            "no release asset for {}",
            host_target_label()
        )));
    };
    let dir = canonical
        .parent()
        .ok_or_else(|| Fail::stop("the running binary has no directory"))?;
    let asset = asset_name(&release.version, target);
    let pid = std::process::id();
    let tmp_asset = dir.join(format!(".lastcall-update-{pid}"));
    let tmp_sums = dir.join(format!(".lastcall-update-{pid}.sums"));
    let _guard = TempFiles(vec![tmp_asset.clone(), tmp_sums.clone()]);
    claim(&tmp_asset)?;
    claim(&tmp_sums)?;

    let asset_url = base.download_url(&release.tag, &asset);
    let got = fetch(&asset_url, Some(&tmp_asset))?;
    if got.status == 404 {
        return Err(Fail::stop(format!("no release asset for {target}")));
    }
    if got.status != 200 {
        return Err(Fail::stop(format!(
            "downloading {asset} answered {} ({asset_url})",
            got.status
        )));
    }
    let sums_url = base.download_url(&release.tag, "SHA256SUMS");
    let sums = fetch(&sums_url, Some(&tmp_sums))?;
    if sums.status != 200 {
        return Err(Fail::stop(format!(
            "SHA256SUMS answered {} ({sums_url})",
            sums.status
        )));
    }
    let text = std::fs::read_to_string(&tmp_sums)
        .map_err(|e| Fail::stop(format!("cannot read the downloaded SHA256SUMS: {e}")))?;
    let want = sha256sums_lookup(&text, &asset)
        .ok_or_else(|| Fail::stop(format!("SHA256SUMS does not list {asset}")))?;
    let have = sha256_file(&tmp_asset)
        .map_err(|e| Fail::stop(format!("cannot read the downloaded asset: {e}")))?;
    if have != want {
        return Err(Fail::mismatch());
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp_asset, std::fs::Permissions::from_mode(0o755)).map_err(|e| {
        Fail::stop(format!(
            "cannot set the mode on {}: {e}",
            tmp_asset.display()
        ))
    })?;
    // Both paths are canonical and in one directory, so this is a same-filesystem rename:
    // the running process keeps the old inode exactly as it does on Linux, and no signature
    // is involved on either OS (ruling P3).
    std::fs::rename(&tmp_asset, canonical)
        .map_err(|e| Fail::stop(format!("cannot replace {}: {e}", canonical.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------------------

fn try_run(check: bool) -> Result<(), Fail> {
    let current = Version::parse(CURRENT)
        .ok_or_else(|| Fail::stop(format!("this binary's version ({CURRENT}) is not semver")))?;
    let exe = std::env::current_exe()
        .map_err(|e| Fail::stop(format!("cannot find the running binary: {e}")))?;
    // Canonical first, before anything else looks at the path: on macOS `current_exe`
    // answers with the symlink, and a temp file beside the symlink is on another filesystem
    // as often as not.
    let canonical = std::fs::canonicalize(&exe)
        .map_err(|e| Fail::stop(format!("cannot resolve {}: {e}", exe.display())))?;
    if let Some(refusal) = refusal_for(&exe).or_else(|| refusal_for(&canonical)) {
        return Err(Fail::stop(refusal));
    }
    let dir = canonical
        .parent()
        .ok_or_else(|| Fail::stop("the running binary has no directory"))?;
    if !check {
        probe_writable(dir)?;
    }
    let base = base_from_env();
    let releases = lookup(&base, &current)?;
    let Some(release) = choose(&current, &releases) else {
        println!("up to date ({current})");
        return Ok(());
    };
    if check {
        println!("{} available — run: lastcall update", release.version);
        return Ok(());
    }
    download_and_replace(&base, &release, &canonical)?;
    println!("lastcall {current} → {}", release.version);
    Ok(())
}

pub fn run(check: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match try_run(check) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(fail) => {
            eprintln!("update: {fail}");
            Ok(ExitCode::from(fail.code))
        }
    }
}

// ---------------------------------------------------------------------------------------
// The TUI's once-a-day check
// ---------------------------------------------------------------------------------------

/// `<state_dir>/update-check.json`. `checked_at` is Unix seconds (the file is state, not a
/// contract); `seen_version` is the binary that wrote it, so the stamp a replaced binary
/// inherits is stale by construction and does not keep offering the version it now is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub checked_at: u64,
    /// The newest release the last lookup saw, or `None` when there was none.
    #[serde(default)]
    pub latest: Option<String>,
    pub seen_version: String,
}

fn read_stamp(path: &Path) -> Option<Stamp> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_stamp(path: &Path, stamp: &Stamp) -> io::Result<()> {
    // The ledger's idiom: temp beside the target, then rename. Two `tui`s racing here write
    // the same content, so last writer wins and either answer is right.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(stamp).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

/// The background check (rule 2.6). Returns the newer version to announce, or `None`.
///
/// Never called on the launch path: the TUI spawns it on a detached thread once the launch
/// hold has ended, so nothing here can delay the first frame or hold up a quit. `quitting`
/// is polled before the stamp is written, so a check that finishes during teardown leaves
/// the state dir alone. Every failure is `None` and one `info` line: a background check that
/// shouts at a user who did not ask for it is a bug.
pub fn daily_check(state_dir: &Path, quitting: &dyn Fn() -> bool) -> Option<String> {
    let current = Version::parse(CURRENT)?;
    let path = state_dir.join(STAMP_FILE);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let newer = |latest: &Option<String>| -> Option<String> {
        let version = Version::parse(latest.as_deref()?)?;
        (version > current).then(|| version.to_string())
    };
    if let Some(stamp) = read_stamp(&path) {
        let fresh = now.saturating_sub(stamp.checked_at) < CHECK_INTERVAL.as_secs();
        if fresh && stamp.seen_version == CURRENT {
            // Inside the day: the answer is whatever the last lookup wrote, so a restart
            // keeps showing the notice without spending a request.
            return newer(&stamp.latest);
        }
    }
    // Deliberately `Base::Default`: the daily check never reads LASTCALL_UPDATE_BASE_URL
    // (rule 2.9). A test reaches it by putting the probe `curl` on PATH, not by redirecting
    // the URL.
    let releases = match lookup(&Base::Default, &current) {
        Ok(releases) => releases,
        Err(fail) => {
            tracing::info!(reason = %fail, "update check failed");
            return None;
        }
    };
    let latest = choose(&current, &releases).map(|r| r.version.to_string());
    if quitting() {
        return None;
    }
    if let Err(e) = write_stamp(
        &path,
        &Stamp {
            checked_at: now,
            latest: latest.clone(),
            seen_version: CURRENT.to_owned(),
        },
    ) {
        tracing::info!(error = %e, "could not write the update stamp");
    }
    latest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).expect(text)
    }

    fn release(tag: &str, prerelease: bool) -> Release {
        Release {
            version: v(tag),
            tag: tag.to_owned(),
            prerelease: prerelease || v(tag).is_prerelease(),
        }
    }

    #[test]
    fn update_semver_parses_and_orders_including_prereleases() {
        assert_eq!(
            v("0.1.0"),
            Version {
                major: 0,
                minor: 1,
                patch: 0,
                pre: None
            }
        );
        assert_eq!(
            v("v0.1.0"),
            v("0.1.0"),
            "a leading v is the tag's, not the version's"
        );
        assert_eq!(v("0.1.0-rc.1").pre.as_deref(), Some("rc.1"));
        assert_eq!(
            v("1.2.3+build7"),
            v("1.2.3"),
            "build metadata is not precedence"
        );
        assert_eq!(Version::parse("0.1"), None);
        assert_eq!(Version::parse("0.1.0.1"), None);
        assert_eq!(Version::parse("0.1.x"), None);
        assert_eq!(Version::parse("0.1.0-"), None);
        assert_eq!(Version::parse("latest"), None);

        assert!(v("0.1.1") > v("0.1.0"));
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        // The rule that catches everyone: the release beats its own candidates.
        assert!(v("0.1.0") > v("0.1.0-rc.2"));
        assert!(v("0.1.0-rc.2") > v("0.1.0-rc.1"));
        assert_eq!(v("0.1.0"), v("0.1.0"));
        assert_eq!(v("0.1.0-rc.1").to_string(), "0.1.0-rc.1");
        assert_eq!(v("0.1.0").to_string(), "0.1.0");
    }

    #[test]
    fn update_choose_never_offers_a_prerelease_to_a_stable_binary() {
        let stable = v("0.1.0");
        let list = vec![
            release("v0.1.1", false),
            release("v0.2.0-rc.1", true),
            release("v0.1.0", false),
        ];
        assert_eq!(choose(&stable, &list).unwrap().tag, "v0.1.1");
        // Nothing but prereleases ahead of it: a stable binary is up to date.
        assert_eq!(choose(&stable, &[release("v0.2.0-rc.1", true)]), None);
        // An rc binary takes the next rc of its own triple, and the release that follows it.
        let rc = v("0.2.0-rc.1");
        assert_eq!(
            choose(&rc, &[release("v0.2.0-rc.2", true)]).unwrap().tag,
            "v0.2.0-rc.2"
        );
        assert_eq!(
            choose(&rc, &[release("v0.2.0", false)]).unwrap().tag,
            "v0.2.0"
        );
        // …but not an rc of a *different* triple.
        assert_eq!(choose(&rc, &[release("v0.3.0-rc.1", true)]), None);
        // Never a downgrade, never a sideways move.
        assert_eq!(choose(&v("0.1.1"), &[release("v0.1.0", false)]), None);
        assert_eq!(choose(&v("0.1.1"), &[release("v0.1.1", false)]), None);
        // The newest wins whatever order the API listed them in.
        let many = vec![
            release("v0.1.1", false),
            release("v0.3.0", false),
            release("v0.2.0", false),
        ];
        assert_eq!(choose(&stable, &many).unwrap().tag, "v0.3.0");
    }

    #[test]
    fn update_releases_from_json_reads_one_object_or_a_list() {
        let one = serde_json::json!({"tag_name": "v0.1.1", "prerelease": false});
        assert_eq!(releases_from_json(&one), vec![release("v0.1.1", false)]);
        let list = serde_json::json!([
            {"tag_name": "v0.2.0-rc.1", "prerelease": true},
            {"tag_name": "not-a-version"},
            {"tag_name": "v0.1.1", "prerelease": false, "draft": true},
            {"tag_name": "v0.1.0"},
        ]);
        let got = releases_from_json(&list);
        assert_eq!(
            got.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(),
            vec!["v0.2.0-rc.1", "v0.1.0"],
            "an unparsable tag and a draft are skipped, never guessed at"
        );
        assert!(got[0].prerelease);
        assert!(!got[1].prerelease);
        // GitHub's flag alone is enough, even for a tag that looks stable.
        let flagged = serde_json::json!({"tag_name": "v0.1.1", "prerelease": true});
        assert!(releases_from_json(&flagged)[0].prerelease);
    }

    #[test]
    fn update_asset_name_and_sha256sums_parsing() {
        assert_eq!(
            asset_name(&v("0.1.0"), "aarch64-apple-darwin"),
            "lastcall-0.1.0-aarch64-apple-darwin"
        );
        assert_eq!(
            asset_name(&v("0.1.0-rc.1"), "x86_64-unknown-linux-gnu"),
            "lastcall-0.1.0-rc.1-x86_64-unknown-linux-gnu",
            "the rc and the release never collide in one SHA256SUMS"
        );
        let sums = "\
ab  lastcall-0.1.0-aarch64-apple-darwin
cd *lastcall-0.1.0-x86_64-apple-darwin
ef lastcall-0.1.0-x86_64-unknown-linux-gnu
";
        assert_eq!(
            sha256sums_lookup(sums, "lastcall-0.1.0-aarch64-apple-darwin").as_deref(),
            Some("ab"),
            "two spaces, sha256sum's text mode"
        );
        assert_eq!(
            sha256sums_lookup(sums, "lastcall-0.1.0-x86_64-apple-darwin").as_deref(),
            Some("cd"),
            "binary mode's * belongs to the name, not the digest"
        );
        assert_eq!(
            sha256sums_lookup(sums, "lastcall-0.1.0-x86_64-unknown-linux-gnu").as_deref(),
            Some("ef"),
            "one space, hand written"
        );
        assert_eq!(sha256sums_lookup(sums, "lastcall-0.1.0-missing"), None);
        assert_eq!(sha256sums_lookup("", "x"), None);
        assert_eq!(
            sha256sums_lookup("AB  x", "x").as_deref(),
            Some("ab"),
            "the comparison is against a lowercase digest"
        );
        assert_eq!(
            sha256sums_lookup("ab  two words", "two words"),
            None,
            "a name with a space is not parsed by guesswork"
        );
    }

    #[test]
    fn update_refuses_package_manager_paths_on_the_path_strings() {
        for path in [
            "/opt/homebrew/bin/lastcall",
            "/usr/local/Cellar/lastcall/0.1.0/bin/lastcall",
            "/home/linuxbrew/.linuxbrew/bin/lastcall",
        ] {
            let refusal = refusal_for(Path::new(path)).expect(path);
            assert!(refusal.contains("brew upgrade lastcall"), "{refusal}");
        }
        let cargo = refusal_for(Path::new("/home/a/.cargo/bin/lastcall")).expect("cargo");
        assert!(cargo.contains("cargo install --git"), "{cargo}");
        assert!(cargo.contains("--force"), "{cargo}");
        let nix =
            refusal_for(Path::new("/nix/store/abc-lastcall-0.1.0/bin/lastcall")).expect("nix");
        assert!(nix.contains("nix"), "{nix}");
        // Everything else updates itself.
        assert_eq!(refusal_for(Path::new("/usr/local/bin/lastcall")), None);
        assert_eq!(refusal_for(Path::new("/home/a/bin/lastcall")), None);
        assert_eq!(refusal_for(Path::new("/home/a/.local/bin/lastcall")), None);
        // Not a prefix match on a lookalike directory.
        assert_eq!(refusal_for(Path::new("/opt/homebrewery/lastcall")), None);
        assert_eq!(refusal_for(Path::new("/nix/storage/lastcall")), None);
    }

    #[test]
    fn update_base_url_is_loopback_only() {
        assert_eq!(
            loopback_base("http://127.0.0.1:8080/").as_deref(),
            Some("http://127.0.0.1:8080/")
        );
        assert_eq!(
            loopback_base("http://localhost:9").as_deref(),
            Some("http://localhost:9/"),
            "a missing trailing slash is added, not rejected"
        );
        assert_eq!(
            loopback_base("http://127.0.0.1:8080/serve").as_deref(),
            Some("http://127.0.0.1:8080/serve/")
        );
        // Everything that could be another machine.
        for raw in [
            "https://127.0.0.1:8080/",
            "http://127.0.0.2:8080/",
            "http://example.com:80/",
            "http://localhost/",
            "http://127.0.0.1:/",
            "http://127.0.0.1:80x/",
            "http://user@127.0.0.1:8080/",
            "127.0.0.1:8080",
            "",
        ] {
            assert_eq!(loopback_base(raw), None, "{raw} must not be honoured");
        }
    }

    #[test]
    fn update_urls_keep_the_tails_the_probe_and_the_smoke_key_on() {
        let default = Base::Default;
        assert_eq!(
            default.api_url("releases/latest"),
            "https://api.github.com/repos/aarontimko/lastcall/releases/latest"
        );
        assert_eq!(
            default.download_url("v0.1.1", "SHA256SUMS"),
            "https://github.com/aarontimko/lastcall/releases/download/v0.1.1/SHA256SUMS"
        );
        let test = Base::Test("http://127.0.0.1:8080/".to_owned());
        assert!(
            test.api_url("releases/latest")
                .ends_with("/releases/latest")
        );
        assert!(
            test.api_url("releases?per_page=10")
                .ends_with("/releases?per_page=10")
        );
        assert!(
            test.download_url("v9.9.9", "lastcall-9.9.9-x")
                .ends_with("/download/v9.9.9/lastcall-9.9.9-x")
        );
        assert!(
            test.api_url("releases/latest")
                .starts_with("http://127.0.0.1:8080/")
        );
    }

    #[test]
    fn update_curl_args_are_herdrs_numbers_and_nothing_else() {
        let json = curl_args("https://api/x", None);
        assert_eq!(json.last().unwrap(), "https://api/x");
        assert!(json.contains(&"-sSL".to_owned()));
        assert!(json.contains(&format!("lastcall/{CURRENT}")), "{json:?}");
        assert!(json.contains(&"%{http_code}".to_owned()));
        assert!(json.windows(2).any(|w| w == ["--max-time", "20"]));
        assert!(json.windows(2).any(|w| w == ["-D", "-"]));
        assert!(!json.iter().any(|a| a == "-o"));

        let asset = curl_args("https://dl/x", Some(Path::new("/tmp/x")));
        assert!(asset.windows(2).any(|w| w == ["--max-time", "120"]));
        assert!(asset.windows(2).any(|w| w == ["--speed-limit", "1024"]));
        assert!(asset.windows(2).any(|w| w == ["--speed-time", "30"]));
        assert!(asset.windows(2).any(|w| w == ["-o", "/tmp/x"]));
        assert!(!asset.iter().any(|a| a == "-D"), "no headers on an asset");
    }

    #[test]
    fn update_split_headers_keeps_the_last_response_and_finds_the_rate_limit() {
        let raw = "HTTP/1.1 302 Found\r\nlocation: /next\r\n\r\nHTTP/1.1 403 Forbidden\r\nX-RateLimit-Reset: 1757600000\r\n\r\n{\"message\":\"rate\"}";
        let (headers, body) = split_headers(raw);
        assert_eq!(body, "{\"message\":\"rate\"}");
        let fetched = Fetched {
            status: 403,
            headers,
            body,
        };
        assert_eq!(fetched.header("x-ratelimit-reset"), Some("1757600000"));
        assert_eq!(fetched.header("X-RATELIMIT-RESET"), Some("1757600000"));
        assert_eq!(fetched.header("location"), None, "the last block only");
        // A body with no header block at all is all body.
        let (headers, body) = split_headers("{\"tag_name\":\"v1.0.0\"}");
        assert!(headers.is_empty());
        assert_eq!(body, "{\"tag_name\":\"v1.0.0\"}");
        // LF-only header blocks parse too (the probe writes them).
        let (headers, body) = split_headers("HTTP/1.1 200 OK\nx: y\n\nbody");
        assert_eq!(
            headers,
            vec!["HTTP/1.1 200 OK".to_owned(), "x: y".to_owned()]
        );
        assert_eq!(body, "body");
    }

    #[test]
    fn update_stamp_round_trips_and_the_host_target_is_one_of_the_matrix() {
        let stamp = Stamp {
            checked_at: 1_757_600_000,
            latest: Some("0.1.1".to_owned()),
            seen_version: "0.1.0".to_owned(),
        };
        let text = serde_json::to_string(&stamp).unwrap();
        assert_eq!(serde_json::from_str::<Stamp>(&text).unwrap(), stamp);
        // `latest` is absent when the last lookup found nothing newer.
        let none: Stamp =
            serde_json::from_str(r#"{"checked_at":1,"seen_version":"0.1.0"}"#).unwrap();
        assert_eq!(none.latest, None);
        // Whatever host the suite runs on, the label is never empty and the four matrix
        // targets are the only ones that get an asset.
        assert!(!host_target_label().is_empty());
        if let Some(target) = host_target() {
            assert!(
                [
                    "aarch64-apple-darwin",
                    "x86_64-apple-darwin",
                    "x86_64-unknown-linux-gnu",
                    "aarch64-unknown-linux-gnu",
                ]
                .contains(&target),
                "{target}"
            );
            assert_eq!(host_target_label(), target);
        }
    }
}
