//! The write path — the **only** code in lastcall that opens a file in the user's working
//! tree for writing (docs/spec/00-spec.md invariant 4, §6.3 "restore hunk / file",
//! Phase 7 deliverable 1).
//!
//! Everything here is mechanism; the compare-and-swap that authorises a write lives in
//! [`crate::ops`]. The rules this module enforces on its own:
//!
//! - **Never follow a symlink.** The leaf is opened with `O_NOFOLLOW` *and* every component
//!   of the parent chain is `lstat`ed from the root down first (review F9): `O_NOFOLLOW` on
//!   the leaf alone would let a parent swapped for a symlink carry the write outside the
//!   tree, and `hash_path`'s own `symlink_metadata` follows that parent too, so the CAS
//!   would pass.
//! - **Temp file, then `rename`.** `.<name>.lastcall-restore-<pid>-<n>` in the same
//!   directory, `O_CREAT | O_EXCL | O_NOFOLLOW`, written and `sync_all`'d, renamed over the
//!   path. The rename is atomic and replaces a symlink at the leaf rather than writing
//!   through it. On any error after the temp file exists it is removed.
//! - **Bytes go through git's smudge/eol conversion** (review F2). Our blobs are canonical
//!   git blobs — `hash-object -w` ran with cwd = root, so `text=auto` and `filter=lfs`
//!   already cleaned them (§6.4) — and writing them raw would rewrite a CRLF file LF or
//!   drop an LFS pointer over the user's binary. [`materialise`] runs
//!   `cat-file --filters --path=<rel>`; [`filter_attr`] refuses a path whose `filter`
//!   attribute names a driver the store cannot promise to run.
//! - **`nix`, never `libc`** (workspace rule, `Cargo.toml`): `O_NOFOLLOW` comes from
//!   `nix::fcntl::OFlag` and the liveness probe from `nix::sys::signal::kill`.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::git::{Mode, Oid};
use crate::store::{RootKind, Store};

/// The basename shape a restore's temp file takes: `.<name>.lastcall-restore-<pid>-<n>`.
/// One constant so [`crate::scan`] drops it from the candidate set and [`sweep`] finds the
/// ghosts (review F8).
pub const RESTORE_TEMP_GLOB: &str = ".*.lastcall-restore-*";

/// The middle segment of [`RESTORE_TEMP_GLOB`].
const MARK: &[u8] = b".lastcall-restore-";

/// Whether `name` — a **basename**, not a path — is a restore temp file.
///
/// The scan drops these so a second lastcall process scanning mid-write never shows one as
/// an added row, and so a ghost left by a crash is invisible rather than acceptable.
pub fn is_restore_temp(name: &[u8]) -> bool {
    name.first() == Some(&b'.') && find(&name[1..], MARK).is_some()
}

/// Whether the root-relative path `rel` ends in a restore temp basename.
pub fn is_restore_temp_path(rel: &[u8]) -> bool {
    let base = match rel.iter().rposition(|b| *b == b'/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    };
    is_restore_temp(base)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// What went wrong while writing. Every variant except [`WriteError::Refuse`] is a real
/// failure (`OpsError::Io`); `Refuse` carries the reason string a `Refused::Unhashable`
/// should quote.
#[derive(Debug)]
pub enum WriteError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Refuse(String),
}

impl WriteError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Parents
// ---------------------------------------------------------------------------------------

/// `lstat` every component of `rel`'s parent chain, from `root` down.
///
/// A symlink anywhere in it refuses (review F9). A component that does not exist ends the
/// walk successfully — the caller decides whether a missing parent is a recreation
/// ([`create_parents`]) or a failure (the live CAS will already have said `Absent`).
pub fn check_parent_chain(root: &Path, rel: &[u8]) -> Result<(), WriteError> {
    let mut here = root.to_path_buf();
    let components: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    // The last component is the leaf itself; `O_NOFOLLOW` and `rename` cover it.
    for comp in &components[..components.len().saturating_sub(1)] {
        if comp.is_empty() || *comp == b"." {
            continue;
        }
        here.push(OsStr::from_bytes(comp));
        match std::fs::symlink_metadata(&here) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(WriteError::Refuse("parent is a symlink".into()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(WriteError::io(&here, e)),
        }
    }
    Ok(())
}

/// Create every missing directory of `rel`'s parent chain, component by component, under
/// the same `lstat` rule as [`check_parent_chain`] (review F9: `rm -r dir` is the common
/// shape of a deletion, so a deletion restore has to put the directory back).
pub fn create_parents(root: &Path, rel: &[u8]) -> Result<(), WriteError> {
    let mut here = root.to_path_buf();
    let components: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    for comp in &components[..components.len().saturating_sub(1)] {
        if comp.is_empty() || *comp == b"." {
            continue;
        }
        here.push(OsStr::from_bytes(comp));
        match std::fs::symlink_metadata(&here) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(WriteError::Refuse("parent is a symlink".into()));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(WriteError::Refuse(format!(
                    "{} is not a directory",
                    here.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&here).map_err(|e| WriteError::io(&here, e))?;
            }
            Err(e) => return Err(WriteError::io(&here, e)),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The directory listing that decides "absent" (D4, review F6)
// ---------------------------------------------------------------------------------------

/// The entry in `rel`'s parent directory that would collide with `rel`'s own name, if any.
///
/// Byte-exact always; **and** case-folded when the root's filesystem folds case. A
/// byte-only rule would call `f1` absent while `F1` sits beside it, and `rename(temp, "f1")`
/// on APFS would then fold onto `F1` and clobber the case-renamed file rather than refuse
/// (review F6; D4).
///
/// An unreadable or missing parent is "nothing collides" — the caller recreates it.
pub fn collision(root: &Path, rel: &[u8], case_insensitive: bool) -> Option<Vec<u8>> {
    let rel_path = Path::new(OsStr::from_bytes(rel));
    let name = rel_path.file_name()?.as_bytes().to_vec();
    let parent = root.join(rel_path.parent().unwrap_or(Path::new("")));
    let entries = std::fs::read_dir(&parent).ok()?;
    let mut folded: Option<Vec<u8>> = None;
    for entry in entries.flatten() {
        let found = entry.file_name().into_vec();
        if found == name {
            return Some(found);
        }
        if case_insensitive && folded.is_none() && found.eq_ignore_ascii_case(&name) {
            folded = Some(found);
        }
    }
    folded
}

// ---------------------------------------------------------------------------------------
// Bytes: the store's canonical blob → what belongs on disk
// ---------------------------------------------------------------------------------------

/// The `filter` attribute in force for `rel`, when it names a driver.
///
/// `check-attr` runs through the store's git (cwd = root, `GIT_DIR` = store,
/// `GIT_WORK_TREE` = root) exactly as `hash-object` does, so it sees the worktree's
/// `.gitattributes` and the `info/attributes` the store copies in at every open.
/// `unspecified`/`unset` mean no driver.
pub fn filter_attr(store: &Store, rel: &[u8]) -> Option<String> {
    if store.kind() != RootKind::Git {
        return None;
    }
    let out = store
        .git()
        .run(&[
            OsStr::new("check-attr"),
            OsStr::new("-z"),
            OsStr::new("filter"),
            OsStr::new("--"),
            OsStr::from_bytes(rel),
        ])
        .ok()?;
    // `-z`: `<path>\0filter\0<value>\0`.
    let value = out.split(|b| *b == 0).nth(2)?;
    let value = String::from_utf8_lossy(value).into_owned();
    match value.as_str() {
        "" | "unspecified" | "unset" => None,
        _ => Some(value),
    }
}

/// The bytes that belong on disk at `rel` for the canonical blob `oid`.
///
/// Git roots go through `cat-file --filters --path=<rel>`, which applies the same
/// convert-to-worktree pass `git checkout` would: a `text=auto` file comes back with its
/// CRLF endings, an LFS pointer comes back as the object. Draft roots have a raw-byte
/// content model (§6.4) and take the blob as it is.
pub fn materialise(store: &Store, oid: &Oid, rel: &[u8]) -> Result<Vec<u8>, WriteError> {
    if store.kind() != RootKind::Git {
        return store
            .cat_blob(oid)
            .map_err(|e| WriteError::Refuse(e.to_string()));
    }
    let mut path_arg = OsString::from("--path=");
    path_arg.push(OsStr::from_bytes(rel));
    store
        .git()
        .run(&[
            OsString::from("cat-file"),
            OsString::from("--filters"),
            path_arg,
            OsString::from(oid.as_str()),
        ])
        .map_err(|e| WriteError::Refuse(format!("cat-file --filters: {e}")))
}

// ---------------------------------------------------------------------------------------
// The write itself
// ---------------------------------------------------------------------------------------

/// The permission bits to give the restored file: the live file's, with the executable bit
/// forced to what `mode` says when the root honours `core.filemode`.
fn perm_bits(full: &Path, mode: Option<Mode>, filemode: bool) -> u32 {
    let mut bits = std::fs::symlink_metadata(full)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    if filemode {
        if mode == Some(Mode::Executable) {
            bits |= 0o111;
        } else if mode == Some(Mode::Regular) {
            bits &= !0o111;
        }
    }
    bits
}

/// Remove any `.<name>.lastcall-restore-<pid>-*` beside `rel` whose pid is no longer alive.
///
/// A SIGKILL or a power loss between create and rename leaves a temp file forever, and the
/// scan hides it — so the next restore of that name sweeps it (review F8). `kill(pid, None)`
/// sends no signal; `ESRCH` is the answer that matters. A pid we cannot judge is left alone.
pub fn sweep(root: &Path, rel: &[u8]) {
    let rel_path = Path::new(OsStr::from_bytes(rel));
    let Some(name) = rel_path.file_name().map(|n| n.as_bytes().to_vec()) else {
        return;
    };
    let parent = root.join(rel_path.parent().unwrap_or(Path::new("")));
    let mut prefix = vec![b'.'];
    prefix.extend_from_slice(&name);
    prefix.extend_from_slice(MARK);
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return;
    };
    for entry in entries.flatten() {
        let found = entry.file_name().into_vec();
        if !found.starts_with(&prefix) {
            continue;
        }
        let tail = &found[prefix.len()..];
        let Some(dash) = tail.iter().position(|b| *b == b'-') else {
            continue;
        };
        let Ok(pid) = String::from_utf8_lossy(&tail[..dash]).parse::<i32>() else {
            continue;
        };
        if pid <= 0 || pid == std::process::id() as i32 {
            continue;
        }
        if let Err(nix::errno::Errno::ESRCH) =
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Create the temp file beside `rel` with `O_CREAT | O_EXCL | O_NOFOLLOW`.
fn create_temp(root: &Path, rel: &[u8]) -> Result<(PathBuf, std::fs::File), WriteError> {
    use std::os::unix::fs::OpenOptionsExt;
    let rel_path = Path::new(OsStr::from_bytes(rel));
    let name = rel_path
        .file_name()
        .ok_or_else(|| WriteError::Refuse("path has no file name".into()))?
        .as_bytes()
        .to_vec();
    let parent = root.join(rel_path.parent().unwrap_or(Path::new("")));
    let pid = std::process::id();
    for n in 0u32..64 {
        let mut base = vec![b'.'];
        base.extend_from_slice(&name);
        base.extend_from_slice(MARK);
        base.extend_from_slice(format!("{pid}-{n}").as_bytes());
        let temp = parent.join(OsStr::from_bytes(&base));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .mode(0o600)
            .open(&temp)
        {
            Ok(f) => return Ok((temp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(WriteError::io(&temp, e)),
        }
    }
    Err(WriteError::Refuse(
        "no free restore temp name in this directory".into(),
    ))
}

/// Write `bytes` at `rel` through the temp-file-and-rename path.
///
/// `before_rename` runs after the temp file is on disk and synced, immediately before the
/// `rename` — that is where the *second* live compare-and-swap goes (§6.3; the microsecond
/// window it leaves is the §11 residual, and the post-restore rescan is its mitigation).
/// Returning `Err` from it aborts the write and takes the temp file with it.
pub fn write_bytes(
    store: &Store,
    rel: &[u8],
    bytes: &[u8],
    mode: Option<Mode>,
    before_rename: &mut dyn FnMut() -> Result<(), WriteError>,
) -> Result<(), WriteError> {
    use std::io::Write;
    let root = store.root();
    let full = root.join(OsStr::from_bytes(rel));
    sweep(root, rel);
    let (temp, mut file) = create_temp(root, rel)?;
    let result = (|| -> Result<(), WriteError> {
        file.write_all(bytes)
            .map_err(|e| WriteError::io(&temp, e))?;
        file.sync_all().map_err(|e| WriteError::io(&temp, e))?;
        drop(file);
        let bits = perm_bits(&full, mode, store.filemode());
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(bits))
            .map_err(|e| WriteError::io(&temp, e))?;
        before_rename()?;
        std::fs::rename(&temp, &full).map_err(|e| WriteError::io(&temp, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Replace `rel` with a symlink to `target` (§6.4: restore of a symlink is unlink + symlink,
/// never a write through the link). `before` is the second CAS, as in [`write_bytes`].
pub fn write_symlink(
    store: &Store,
    rel: &[u8],
    target: &[u8],
    before: &mut dyn FnMut() -> Result<(), WriteError>,
) -> Result<(), WriteError> {
    let full = store.root().join(OsStr::from_bytes(rel));
    before()?;
    match std::fs::remove_file(&full) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(WriteError::io(&full, e)),
    }
    std::os::unix::fs::symlink(OsStr::from_bytes(target), &full)
        .map_err(|e| WriteError::io(&full, e))
}

/// Remove `rel` (the baseline is absent: the file was added since the baseline). `before`
/// is the second CAS.
pub fn remove(
    store: &Store,
    rel: &[u8],
    before: &mut dyn FnMut() -> Result<(), WriteError>,
) -> Result<(), WriteError> {
    let full = store.root().join(OsStr::from_bytes(rel));
    before()?;
    match std::fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WriteError::io(&full, e)),
    }
}

/// `chmod` `rel` to `mode` and write no bytes (the D1 mode hunk). A root that does not
/// honour `core.filemode` does nothing at all.
pub fn set_mode(store: &Store, rel: &[u8], mode: Option<Mode>) -> Result<(), WriteError> {
    if !store.filemode() {
        return Ok(());
    }
    let full = store.root().join(OsStr::from_bytes(rel));
    let bits = perm_bits(&full, mode, true);
    std::fs::set_permissions(&full, std::fs::Permissions::from_mode(bits))
        .map_err(|e| WriteError::io(&full, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lastcall_testkit::tmp::TempDir;

    #[test]
    fn restore_temp_names_are_recognised_by_the_shared_rule() {
        assert!(is_restore_temp(b".f1.lastcall-restore-4242-0"));
        assert!(is_restore_temp_path(b"d/sub/.f1.lastcall-restore-1-0"));
        assert!(!is_restore_temp(b"f1"));
        assert!(!is_restore_temp(b".f1"));
        // Not a dotfile: a real (if odd) tracked path keeps its row.
        assert!(!is_restore_temp(b"f1.lastcall-restore-1-0"));
        assert!(!is_restore_temp_path(b"d/f1.lastcall-restore-1-0"));
        assert_eq!(RESTORE_TEMP_GLOB, ".*.lastcall-restore-*");
    }

    #[test]
    fn restore_parent_chain_refuses_a_symlinked_directory() {
        let tmp = TempDir::new("lc-restore-parents");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/f"), b"x").unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();
        assert!(check_parent_chain(root, b"real/f").is_ok());
        match check_parent_chain(root, b"link/f") {
            Err(WriteError::Refuse(r)) => assert_eq!(r, "parent is a symlink"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // A missing parent is not a refusal: `create_parents` is the deletion path's answer.
        assert!(check_parent_chain(root, b"gone/f").is_ok());
    }

    #[test]
    fn restore_create_parents_makes_the_chain_and_still_refuses_a_symlink() {
        let tmp = TempDir::new("lc-restore-mkparents");
        let root = tmp.path();
        create_parents(root, b"a/b/c/f").unwrap();
        assert!(root.join("a/b/c").is_dir());
        std::os::unix::fs::symlink("a", root.join("l")).unwrap();
        match create_parents(root, b"l/x/f") {
            Err(WriteError::Refuse(r)) => assert_eq!(r, "parent is a symlink"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn restore_collision_is_byte_exact_and_case_folded_only_when_asked() {
        let tmp = TempDir::new("lc-restore-collide");
        let root = tmp.path();
        std::fs::write(root.join("F1"), b"x").unwrap();
        assert_eq!(collision(root, b"F1", false).as_deref(), Some(&b"F1"[..]));
        assert_eq!(collision(root, b"f1", false), None);
        assert_eq!(collision(root, b"f1", true).as_deref(), Some(&b"F1"[..]));
        assert_eq!(collision(root, b"other", true), None);
        // A missing parent collides with nothing.
        assert_eq!(collision(root, b"nope/f1", true), None);
    }

    #[test]
    fn restore_sweep_removes_only_a_dead_pids_temp_file_for_that_name() {
        let tmp = TempDir::new("lc-restore-sweep");
        let root = tmp.path();
        let ours = format!(".f1.lastcall-restore-{}-0", std::process::id());
        // pid 1 is always alive; a very high pid is not (and is not ours).
        let dead = ".f1.lastcall-restore-2147480000-0";
        let alive = ".f1.lastcall-restore-1-0";
        let other_name = ".f2.lastcall-restore-2147480000-0";
        for n in [ours.as_str(), dead, alive, other_name] {
            std::fs::write(root.join(n), b"").unwrap();
        }
        sweep(root, b"f1");
        assert!(!root.join(dead).exists(), "a dead pid's ghost is swept");
        assert!(root.join(alive).exists(), "a live pid's temp file is left");
        assert!(root.join(&ours).exists(), "our own in-flight name is left");
        assert!(root.join(other_name).exists(), "another name is left");
    }
}
