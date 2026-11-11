//! `hoard self-update`: replace a hand-installed hoard binary with the newest
//! GitHub release. A hoard installed as a package (sys-apps/spas) is left to
//! the package manager.

use crate::util::{ask, bold, hex, info, warn};
use crate::version::Version;
use std::io::Read;
use std::path::{Path, PathBuf};

const REPO: &str = "tortr-rs/SimplePackagemAnagingSystem";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(&format!("hoard/{} (SimplePackagemAnagingSystem)", crate::VERSION))
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .build()
}

fn get(url: &str) -> Result<Vec<u8>, String> {
    let resp = agent().get(url).set("Accept", "application/vnd.github+json").call().map_err(|e| match e {
        ureq::Error::Status(c, _) => format!("{url}: HTTP {c}"),
        e => format!("{url}: {e}"),
    })?;
    let mut v = Vec::new();
    resp.into_reader().take(256 << 20).read_to_end(&mut v).map_err(|e| format!("{url}: {e}"))?;
    Ok(v)
}

/// The package that installed `exe`, if any.
fn owner(exe: &Path) -> Option<String> {
    let vdb = crate::vdb::Vdb::load(Path::new("/var/db/pkg"));
    let mut canon = crate::merge::Canon::new();
    let want = canon.get(exe);
    for p in &vdb.pkgs {
        if p.contents().iter().any(|e| canon.get(Path::new(e.path())) == want) {
            return Some(p.cpv());
        }
    }
    None
}

fn sha256(d: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(d))
}

pub fn run(check_only: bool, yes: bool) -> Result<i32, String> {
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| format!("can't find my own binary: {e}"))?;
    if let Some(cpv) = owner(&exe) {
        return Err(format!(
            "{} was installed by {cpv}; update it like any package: hoard sync && hoard upgrade",
            exe.display()
        ));
    }

    let rel: serde_json::Value = serde_json::from_slice(&get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?)
        .map_err(|e| format!("bad reply from GitHub: {e}"))?;
    let tag = rel["tag_name"].as_str().ok_or("GitHub reply has no tag_name")?;
    let latest_s = tag.trim_start_matches('v');
    let latest = Version::parse(latest_s).ok_or(format!("can't parse release version '{tag}'"))?;
    let current = Version::parse(crate::VERSION).ok_or("can't parse my own version")?;
    if latest <= current {
        info(&format!("hoard {} is the newest release", crate::VERSION));
        return Ok(0);
    }
    info(&format!("hoard {} is available (you have {})", bold(latest_s), crate::VERSION));
    if check_only {
        return Ok(0);
    }

    let dir = exe.parent().ok_or("binary has no parent directory")?.to_path_buf();
    let probe = dir.join(".hoard.probe");
    if std::fs::write(&probe, "").is_err() {
        return Err(format!("can't write to {}: run it with sudo", dir.display()));
    }
    let _ = std::fs::remove_file(&probe);

    let arch = std::env::consts::ARCH;
    let base = format!("hoard-{latest_s}-{arch}-linux-gnu");
    let asset = |name: &str| -> Option<String> {
        rel["assets"].as_array()?.iter().find(|a| a["name"] == name)?["browser_download_url"].as_str().map(String::from)
    };
    let tar_url = asset(&format!("{base}.tar.xz"))
        .ok_or(format!("release {tag} has no {arch} binary; build it from source or use the sys-apps/spas package"))?;
    let sums_url = asset("SHA256SUMS").ok_or(format!("release {tag} has no SHA256SUMS"))?;

    if !yes && !ask(&format!("Replace {} with hoard {latest_s}?", exe.display())) {
        return Ok(1);
    }
    info(&format!("Downloading {base}.tar.xz"));
    let tarball = get(&tar_url)?;
    let sums = String::from_utf8_lossy(&get(&sums_url)?).to_string();
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, n)| n.trim().trim_start_matches('*') == format!("{base}.tar.xz"))
        .map(|(h, _)| h.to_lowercase())
        .ok_or(format!("SHA256SUMS has no entry for {base}.tar.xz"))?;
    if sha256(&tarball) != want {
        return Err(format!("{base}.tar.xz does not match SHA256SUMS; nothing was changed"));
    }

    // pull the binary out of the tarball
    let mut ar = tar::Archive::new(xz2::read::XzDecoder::new(&tarball[..]));
    let mut bin = None;
    for ent in ar.entries().map_err(|e| e.to_string())? {
        let mut ent = ent.map_err(|e| e.to_string())?;
        if ent.path().map(|p| p == Path::new(&base).join("hoard")).unwrap_or(false) {
            let mut v = Vec::new();
            ent.read_to_end(&mut v).map_err(|e| e.to_string())?;
            bin = Some(v);
            break;
        }
    }
    let bin = bin.ok_or(format!("{base}.tar.xz has no {base}/hoard"))?;

    let new: PathBuf = dir.join(".hoard.new");
    std::fs::write(&new, &bin).map_err(|e| format!("{}: {e}", new.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    // make sure it runs and is the version we expect before swapping it in
    let out = std::process::Command::new(&new).arg("--version").output();
    let ok = matches!(&out, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).contains(latest_s));
    if !ok {
        let _ = std::fs::remove_file(&new);
        return Err(format!("the downloaded hoard {latest_s} doesn't run on this system; nothing was changed"));
    }
    std::fs::rename(&new, &exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    info(&format!("{} is now hoard {}", exe.display(), bold(latest_s)));
    warn("the release is checked against its SHA256SUMS from the same GitHub release, which proves the download is intact but not who built it");
    Ok(0)
}
