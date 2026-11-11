//! Repository syncing (git, rsync) and GLEP 74 Manifest tree verification.

use crate::repo::Repo;
use crate::util::{error, info, manifest_hashes, warn};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const RSYNC_OPTS: &[&str] = &[
    "--recursive", "--links", "--safe-links", "--perms", "--times", "--omit-dir-times", "--compress", "--force",
    "--whole-file", "--delete", "--stats", "--human-readable", "--timeout=180", "--exclude=/distfiles",
    "--exclude=/local", "--exclude=/packages", "--exclude=/.git",
];

fn run(cmd: &mut Command) -> Result<(), String> {
    let st = cmd.env("PATH", crate::ebuild_env::PATH).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("{:?} failed", cmd.get_program())) }
}

fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "yes" | "true" | "1")
}

fn key_path(r: &Repo) -> PathBuf {
    PathBuf::from(r.settings.get("sync-openpgp-key-path").cloned().unwrap_or_else(|| "/usr/share/openpgp-keys/gentoo-release.asc".into()))
}

/// Should commits of this git repository be signature-checked?
/// repos.conf decides; ::gentoo defaults to yes (its mirror signs every commit).
pub fn git_verify_wanted(r: &Repo, main_repo: &str) -> bool {
    match r.settings.get("sync-git-verify-commit-signature") {
        Some(v) => truthy(v),
        None => r.name == main_repo,
    }
}

/// Run `f` with a throwaway GnuPG home that trusts only `key`.
fn with_keyring<T>(key: &Path, f: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
    if !key.exists() {
        return Err(format!("signing key {} is missing (emerge sec-keys/openpgp-keys-gentoo-release)", key.display()));
    }
    let home = std::env::temp_dir().join(format!("spas-gpg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    let imp = Command::new("gpg")
        .arg("--homedir")
        .arg(&home)
        .args(["--batch", "--no-tty", "--import"])
        .arg(key)
        .output()
        .map_err(|e| format!("gpg: {e}"));
    let res = match imp {
        Ok(o) if o.status.success() => f(&home),
        Ok(_) => Err("gpg could not import the signing key".into()),
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_dir_all(&home);
    res
}

fn git(r: &Repo) -> Command {
    let mut c = Command::new("git");
    // the checkout may belong to another user; we only read it here
    c.args(["-c", "safe.directory=*", "-C"]).arg(&r.location).env("PATH", crate::ebuild_env::PATH);
    c
}

fn home_of(uid: u32) -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*pw).pw_dir).to_string_lossy().to_string();
        Some(PathBuf::from(dir))
    }
}

fn git_head(r: &Repo) -> Option<String> {
    let o = git(r).args(["rev-parse", "HEAD"]).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Check the OpenPGP signature of the checked-out commit.
pub fn verify_git_head(r: &Repo) -> Result<(), String> {
    let key = key_path(r);
    with_keyring(&key, |home| {
        let out = git(r).env("GNUPGHOME", home).args(["verify-commit", "--raw", "HEAD"]).output().map_err(|e| format!("git: {e}"))?;
        let status = String::from_utf8_lossy(&out.stderr);
        if out.status.success() && status.contains("[GNUPG:] VALIDSIG") {
            return Ok(());
        }
        if status.contains("EXPKEYSIG") {
            warn(&format!("the key in {} has expired; update it", key.display()));
        }
        if !status.contains("[GNUPG:]") {
            return Err(format!("{}: the newest commit is not signed", r.name));
        }
        Err(format!("{}: the newest commit has no valid signature from {}", r.name, key.display()))
    })
}

fn sync_git(r: &Repo, verify: bool) -> Result<(), String> {
    if !r.location.join(".git").exists() {
        if r.sync_uri.is_empty() {
            return Err(format!("{}: no sync-uri", r.name));
        }
        if let Some(parent) = r.location.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let depth = r.settings.get("sync-depth").cloned().unwrap_or_else(|| "1".into());
        let mut c = Command::new("git");
        c.arg("clone");
        if depth != "0" {
            c.args(["--depth", &depth]);
        }
        run(c.arg(&r.sync_uri).arg(&r.location))?;
        if verify {
            if let Err(e) = verify_git_head(r) {
                let _ = std::fs::remove_dir_all(&r.location);
                return Err(format!("{e}; the fresh clone was removed"));
            }
        }
        return Ok(());
    }
    let before = git_head(r);
    // run as whoever owns the checkout, so file ownership stays the same
    let owner = std::fs::metadata(&r.location).map(|m| (m.uid(), m.gid())).ok();
    let as_owner = |c: &mut Command| {
        if let Some((u, g)) = owner {
            if u != 0 && crate::util::is_root() {
                c.uid(u).gid(g);
                if let Some(h) = home_of(u) {
                    c.env("HOME", h);
                }
            }
        }
    };
    let mut c = Command::new("git");
    c.arg("-C").arg(&r.location).args(["pull", "--ff-only"]);
    as_owner(&mut c);
    run(&mut c)?;
    if !verify {
        warn(&format!(
            "{} is not signature-checked (set sync-git-verify-commit-signature = yes and sync-openpgp-key-path in repos.conf if its commits are signed)",
            r.name
        ));
        return Ok(());
    }
    if let Err(e) = verify_git_head(r) {
        if let Some(old) = before.filter(|b| Some(b) != git_head(r).as_ref()) {
            let mut c = Command::new("git");
            c.arg("-C").arg(&r.location).args(["reset", "-q", "--hard", &old]);
            as_owner(&mut c);
            let _ = run(&mut c);
            return Err(format!("{e}; rolled back to the previous commit"));
        }
        return Err(e);
    }
    info(&format!("{}: commit signature is good", r.name));
    Ok(())
}

pub fn sync_repo(r: &Repo, main_repo: &str) -> Result<(), String> {
    info(&format!("Syncing {} ({})", crate::util::bold(&r.name), r.location.display()));
    match r.sync_type.as_str() {
        "git" => sync_git(r, git_verify_wanted(r, main_repo)),
        "rsync" => {
            if r.sync_uri.is_empty() {
                return Err(format!("{}: no sync-uri", r.name));
            }
            std::fs::create_dir_all(&r.location).map_err(|e| e.to_string())?;
            run(Command::new("rsync").args(RSYNC_OPTS).arg(format!("{}/", r.sync_uri.trim_end_matches('/'))).arg(&r.location))?;
            if r.settings.get("sync-rsync-verify-metamanifest").map(|s| s != "no").unwrap_or(true) {
                verify_tree(r)?;
            }
            Ok(())
        }
        "" => {
            info(&format!("{}: no sync-type, skipping", r.name));
            Ok(())
        }
        t => Err(format!("{}: sync-type '{t}' is not supported", r.name)),
    }
}

/// Verify the OpenPGP signature on the top Manifest and every file below it.
pub fn verify_tree(r: &Repo) -> Result<(), String> {
    let key = r
        .settings
        .get("sync-openpgp-key-path")
        .cloned()
        .unwrap_or_else(|| "/usr/share/openpgp-keys/gentoo-release.asc".into());
    info(&format!("Verifying {} (OpenPGP signature and Manifest tree)", r.name));
    let top = r.location.join("Manifest");
    verify_signature(&top, Path::new(&key))?;
    let mut expected: HashMap<String, (u64, HashMap<String, String>)> = HashMap::new();
    let mut ignore: Vec<String> = Vec::new();
    let text = std::fs::read_to_string(&top).map_err(|e| format!("{}: {e}", top.display()))?;
    load_manifest(&r.location, "", &strip_pgp(&text), &mut expected, &mut ignore)?;
    // every expected file must match, and nothing unexpected may exist
    let mut bad = Vec::new();
    let mut seen = 0usize;
    walk_check(&r.location, "", &expected, &ignore, &mut bad, &mut seen);
    for (p, _) in &expected {
        if !r.location.join(p).exists() {
            bad.push(format!("missing: {p}"));
        }
    }
    if !bad.is_empty() {
        for b in bad.iter().take(20) {
            error(b);
        }
        return Err(format!("{} failed verification ({} problems); do not trust this tree", r.name, bad.len()));
    }
    info(&format!("{}: {seen} files verified", r.name));
    Ok(())
}

pub fn verify_signature(manifest: &Path, key: &Path) -> Result<(), String> {
    with_keyring(key, |home| {
        let out = Command::new("gpg")
            .arg("--homedir")
            .arg(home)
            .args(["--batch", "--no-tty", "--status-fd", "1", "--verify"])
            .arg(manifest)
            .output()
            .map_err(|e| format!("gpg: {e}"))?;
        let status = String::from_utf8_lossy(&out.stdout);
        if status.contains("[GNUPG:] VALIDSIG") && out.status.success() {
            return Ok(());
        }
        if status.contains("EXPKEYSIG") {
            warn("the signing key has expired; update sec-keys/openpgp-keys-gentoo-release");
        }
        Err(format!("bad OpenPGP signature on {}", manifest.display()))
    })
}

fn strip_pgp(text: &str) -> String {
    if !text.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
        return text.to_string();
    }
    let mut out = String::new();
    let mut in_body = false;
    for l in text.lines() {
        if !in_body {
            if l.is_empty() {
                in_body = true;
            }
            continue;
        }
        if l.starts_with("-----BEGIN PGP SIGNATURE-----") {
            break;
        }
        out.push_str(l.strip_prefix("- ").unwrap_or(l));
        out.push('\n');
    }
    out
}

/// GLEP 74 path encoding: \xHH, \uHHHH, \UHHHHHHHH
fn decode_path(s: &str) -> String {
    let mut out = String::new();
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' && i + 1 < c.len() {
            let n = match c[i + 1] {
                'x' => 2,
                'u' => 4,
                'U' => 8,
                _ => 0,
            };
            if n > 0 && i + 2 + n <= c.len() {
                let hex: String = c[i + 2..i + 2 + n].iter().collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                    i += 2 + n;
                    continue;
                }
            }
        }
        out.push(c[i]);
        i += 1;
    }
    out
}

fn join(dir: &str, p: &str) -> String {
    if dir.is_empty() { p.to_string() } else { format!("{dir}/{p}") }
}

fn load_manifest(
    root: &Path,
    dir: &str,
    text: &str,
    expected: &mut HashMap<String, (u64, HashMap<String, String>)>,
    ignore: &mut Vec<String>,
) -> Result<(), String> {
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.is_empty() {
            continue;
        }
        match f[0] {
            "IGNORE" if f.len() >= 2 => ignore.push(join(dir, &decode_path(f[1]))),
            "MANIFEST" | "DATA" | "EBUILD" | "MISC" | "AUX" if f.len() >= 3 => {
                let mut rel = decode_path(f[1]);
                if f[0] == "AUX" {
                    rel = format!("files/{rel}");
                }
                let path = join(dir, &rel);
                let size: u64 = f[2].parse().map_err(|_| format!("bad Manifest line: {l}"))?;
                let mut hashes = HashMap::new();
                for pair in f[3..].chunks(2) {
                    if pair.len() == 2 {
                        hashes.insert(pair[0].to_string(), pair[1].to_lowercase());
                    }
                }
                if f[0] == "MANIFEST" {
                    let full = root.join(&path);
                    check_file(&full, size, &hashes).map_err(|e| format!("{path}: {e}"))?;
                    let mut sub = String::new();
                    if path.ends_with(".gz") {
                        let file = std::fs::File::open(&full).map_err(|e| e.to_string())?;
                        flate2::read::GzDecoder::new(file).read_to_string(&mut sub).map_err(|e| format!("{path}: {e}"))?;
                    } else {
                        sub = std::fs::read_to_string(&full).map_err(|e| e.to_string())?;
                    }
                    let subdir = Path::new(&path).parent().map(|p| p.display().to_string()).unwrap_or_default();
                    load_manifest(root, &subdir, &strip_pgp(&sub), expected, ignore)?;
                    // the sub-Manifest itself is accounted for
                    expected.insert(path, (size, hashes));
                } else {
                    expected.insert(path, (size, hashes));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_file(p: &Path, size: u64, hashes: &HashMap<String, String>) -> Result<(), String> {
    let (s, b2, sha) = manifest_hashes(p).map_err(|e| e.to_string())?;
    if s != size {
        return Err(format!("size {s}, expected {size}"));
    }
    if hashes.get("BLAKE2B").map(|h| *h != b2).unwrap_or(false) {
        return Err("BLAKE2B mismatch".into());
    }
    if hashes.get("SHA512").map(|h| *h != sha).unwrap_or(false) {
        return Err("SHA512 mismatch".into());
    }
    if !hashes.contains_key("BLAKE2B") && !hashes.contains_key("SHA512") {
        return Err("no usable hash".into());
    }
    Ok(())
}

fn walk_check(
    root: &Path,
    rel: &str,
    expected: &HashMap<String, (u64, HashMap<String, String>)>,
    ignore: &[String],
    bad: &mut Vec<String>,
    seen: &mut usize,
) {
    let dir: PathBuf = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let r = join(rel, &name);
        if ignore.iter().any(|i| r == *i || r.starts_with(&format!("{i}/"))) {
            continue;
        }
        // GLEP 74: names starting with a dot are not covered by Manifests
        if name.starts_with('.') || (rel.is_empty() && name == "Manifest") {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
        if md.is_dir() {
            walk_check(root, &r, expected, ignore, bad, seen);
        } else {
            match expected.get(&r) {
                Some((size, hashes)) => {
                    if let Err(err) = check_file(&e.path(), *size, hashes) {
                        bad.push(format!("{r}: {err}"));
                    }
                    *seen += 1;
                }
                None => bad.push(format!("unexpected file: {r}")),
            }
        }
    }
}
