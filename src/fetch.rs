//! Distfile fetching and Manifest verification.

use crate::config::{Config, UseInfo};
use crate::depspec;
use crate::repo::Ebuild;
use crate::util::{blake2b_str, info, manifest_hashes, warn};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Distfile {
    pub name: String,
    pub uris: Vec<String>,
    pub size: Option<u64>,
    pub blake2b: Option<String>,
    pub sha512: Option<String>,
}

fn manifest(dir: &Path) -> HashMap<String, (u64, HashMap<String, String>)> {
    let mut m = HashMap::new();
    let Ok(t) = std::fs::read_to_string(dir.join("Manifest")) else { return m };
    for l in t.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 3 && f[0] == "DIST" {
            let mut hashes = HashMap::new();
            for pair in f[3..].chunks(2) {
                if pair.len() == 2 {
                    hashes.insert(pair[0].to_string(), pair[1].to_lowercase());
                }
            }
            if let Ok(size) = f[2].parse() {
                m.insert(f[1].to_string(), (size, hashes));
            }
        }
    }
    m
}

pub fn restrict_has(e: &Ebuild, use_: &UseInfo, what: &str) -> bool {
    let r = depspec::parse(e.meta.get("RESTRICT")).unwrap_or_default();
    depspec::flat_leaves(&depspec::reduce(&r, &use_.enabled)).iter().any(|x| x == what)
}

pub fn properties_has(e: &Ebuild, use_: &UseInfo, what: &str) -> bool {
    let r = depspec::parse(e.meta.get("PROPERTIES")).unwrap_or_default();
    depspec::flat_leaves(&depspec::reduce(&r, &use_.enabled)).iter().any(|x| x == what)
}

pub fn distfiles(cfg: &Config, e: &Ebuild, use_: &UseInfo) -> Result<Vec<Distfile>, String> {
    let nodes = depspec::parse(e.meta.get("SRC_URI"))?;
    let pairs = depspec::src_uri_files(&depspec::reduce(&nodes, &use_.enabled));
    let man = manifest(e.path.parent().unwrap());
    let tpm = cfg.repos.third_party_mirrors();
    let mirror_restricted = restrict_has(e, use_, "mirror") || restrict_has(e, use_, "fetch");
    let primary = restrict_has(e, use_, "primaryuri");
    let gentoo: Vec<String> = cfg.var("GENTOO_MIRRORS").split_whitespace().map(|s| s.trim_end_matches('/').to_string()).collect();

    let mut by_name: Vec<Distfile> = Vec::new();
    for (uri, name) in pairs {
        let mut upstream = Vec::new();
        if let Some(rest) = uri.strip_prefix("mirror://") {
            let (mname, path) = rest.split_once('/').unwrap_or((rest, ""));
            if mname == "gentoo" {
                // only the gentoo mirrors
            } else if let Some(bases) = tpm.get(mname) {
                for b in bases {
                    upstream.push(format!("{}/{}", b.trim_end_matches('/'), path));
                }
            } else {
                warn(&format!("{}: unknown mirror '{mname}'", e.cpv()));
            }
        } else if uri.contains("://") {
            upstream.push(uri.clone());
        }
        let mut mirrors = Vec::new();
        if !mirror_restricted || uri.starts_with("mirror://gentoo/") {
            let h = blake2b_str(&name);
            for g in &gentoo {
                mirrors.push(format!("{g}/distfiles/{}/{name}", &h[..2]));
                mirrors.push(format!("{g}/distfiles/{name}"));
            }
        }
        let uris = if primary { [upstream, mirrors].concat() } else { [mirrors, upstream].concat() };
        if let Some(d) = by_name.iter_mut().find(|d| d.name == name) {
            for u in uris {
                if !d.uris.contains(&u) {
                    d.uris.push(u);
                }
            }
            continue;
        }
        let (size, blake2b, sha512) = match man.get(&name) {
            Some((s, h)) => (Some(*s), h.get("BLAKE2B").cloned(), h.get("SHA512").cloned()),
            None => (None, None, None),
        };
        by_name.push(Distfile { name, uris, size, blake2b, sha512 });
    }
    Ok(by_name)
}

fn verify(path: &Path, d: &Distfile) -> Result<(), String> {
    let (size, b2, sha) = manifest_hashes(path).map_err(|e| e.to_string())?;
    if let Some(s) = d.size {
        if s != size {
            return Err(format!("size {size}, Manifest says {s}"));
        }
    }
    if let Some(h) = &d.blake2b {
        if *h != b2 {
            return Err("BLAKE2B checksum mismatch".into());
        }
    }
    if let Some(h) = &d.sha512 {
        if *h != sha {
            return Err("SHA512 checksum mismatch".into());
        }
    }
    Ok(())
}

pub fn distdir(cfg: &Config) -> PathBuf {
    let d = cfg.var("DISTDIR");
    PathBuf::from(if d.is_empty() { "/var/cache/distfiles" } else { d })
}

fn download(url: &str, dest: &Path, name: &str) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .user_agent("hoard/0.1 (SimplePackagemAnagingSystem)")
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::Status(c, _) => format!("HTTP {c}"),
        e => e.to_string(),
    })?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());
    let mut r = resp.into_reader();
    let mut f = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    let mut last = std::time::Instant::now();
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    loop {
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        got += n as u64;
        if tty && last.elapsed().as_millis() > 200 {
            last = std::time::Instant::now();
            match total {
                Some(t) if t > 0 => eprint!("\r    {name}: {} / {} ({}%)   ", crate::util::human_size(got), crate::util::human_size(t), got * 100 / t),
                _ => eprint!("\r    {name}: {}   ", crate::util::human_size(got)),
            }
        }
    }
    if tty {
        eprint!("\r\x1b[K");
    }
    f.sync_all().ok();
    Ok(())
}

/// Make sure every distfile is present and verified.
pub fn fetch(cfg: &Config, e: &Ebuild, use_: &UseInfo, files: &[Distfile]) -> Result<(), String> {
    let dir = distdir(cfg);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let fetch_restricted = restrict_has(e, use_, "fetch");
    for d in files {
        let path = dir.join(&d.name);
        // parallel jobs may want the same file: one downloads, the others wait
        let _lock = crate::util::lock(&dir.join(".locks").join(format!("{}.lock", d.name)))?;
        if d.size.is_none() {
            return Err(format!("{}: no Manifest entry for {}", e.cpv(), d.name));
        }
        if path.is_file() {
            match verify(&path, d) {
                Ok(()) => continue,
                Err(err) => {
                    warn(&format!("{}: existing file is bad ({err}), fetching again", d.name));
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        if fetch_restricted {
            return Err(format!(
                "{} has a fetch restriction: download {} yourself and put it in {}",
                e.cpv(),
                d.name,
                dir.display()
            ));
        }
        let tmp = dir.join(format!("{}.__download__", d.name));
        let mut ok = false;
        info(&format!("Downloading {}", d.name));
        for u in &d.uris {
            match download(u, &tmp, &d.name) {
                Ok(()) => match verify(&tmp, d) {
                    Ok(()) => {
                        std::fs::rename(&tmp, &path).map_err(|err| err.to_string())?;
                        ok = true;
                        break;
                    }
                    Err(err) => warn(&format!("{u}: {err}")),
                },
                Err(err) => {
                    // 404s on the mirrors are normal, don't flood the output
                    if !err.starts_with("HTTP 404") {
                        warn(&format!("{u}: {err}"));
                    }
                }
            }
        }
        let _ = std::fs::remove_file(&tmp);
        if !ok {
            return Err(format!("could not fetch {} from any location", d.name));
        }
    }
    Ok(())
}

/// Where `hoard manifest` keeps downloads: DISTDIR if we may write there,
/// else ~/.cache/hoard/distfiles.
fn manifest_distdir(cfg: &Config) -> PathBuf {
    let d = distdir(cfg);
    let probe = d.join(format!(".hoard-probe-{}", std::process::id()));
    if std::fs::create_dir_all(&d).is_ok() && std::fs::write(&probe, "").is_ok() {
        let _ = std::fs::remove_file(&probe);
        return d;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".cache/hoard/distfiles")
}

/// `hoard manifest`: write the DIST entries of a package directory's Manifest,
/// for every file any of its ebuilds can download (all USE conditionals).
/// Entries already in the Manifest are kept; new files are downloaded and hashed.
pub fn write_manifest(cfg: &Config, ebuilds: &[std::rc::Rc<Ebuild>]) -> Result<(), String> {
    let pkgdir = ebuilds.first().and_then(|e| e.path.parent()).ok_or("no ebuilds")?.to_path_buf();
    let old = manifest(&pkgdir);
    let tpm = cfg.repos.third_party_mirrors();
    let gentoo: Vec<String> = cfg.var("GENTOO_MIRRORS").split_whitespace().map(|s| s.trim_end_matches('/').to_string()).collect();
    let mut want: Vec<(String, Vec<String>)> = Vec::new();
    for e in ebuilds {
        let nodes = depspec::parse(e.meta.get("SRC_URI")).map_err(|err| format!("{}: SRC_URI: {err}", e.cpv()))?;
        for (uri, name) in depspec::src_uri_files(&nodes) {
            let mut uris = Vec::new();
            if let Some(rest) = uri.strip_prefix("mirror://") {
                let (mname, path) = rest.split_once('/').unwrap_or((rest, ""));
                if mname == "gentoo" {
                    uris.extend(gentoo.iter().map(|g| format!("{g}/distfiles/{path}")));
                } else if let Some(bases) = tpm.get(mname) {
                    uris.extend(bases.iter().map(|b| format!("{}/{}", b.trim_end_matches('/'), path)));
                } else {
                    return Err(format!("{}: unknown mirror '{mname}'", e.cpv()));
                }
            } else if uri.contains("://") {
                uris.push(uri.clone());
            }
            match want.iter_mut().find(|(n, _)| *n == name) {
                Some((_, u)) => u.extend(uris),
                None => want.push((name, uris)),
            }
        }
    }
    want.sort_by(|a, b| a.0.cmp(&b.0));
    let dir = manifest_distdir(cfg);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut lines = Vec::new();
    for (name, uris) in &want {
        if let Some((size, h)) = old.get(name) {
            if let (Some(b2), Some(sha)) = (h.get("BLAKE2B"), h.get("SHA512")) {
                lines.push(format!("DIST {name} {size} BLAKE2B {b2} SHA512 {sha}"));
                continue;
            }
        }
        let path = dir.join(name);
        if !path.is_file() {
            let tmp = dir.join(format!("{name}.__download__"));
            info(&format!("Downloading {name}"));
            let mut ok = false;
            for u in uris {
                match download(u, &tmp, name) {
                    Ok(()) => {
                        std::fs::rename(&tmp, &path).map_err(|err| err.to_string())?;
                        ok = true;
                        break;
                    }
                    Err(err) => warn(&format!("{u}: {err}")),
                }
            }
            let _ = std::fs::remove_file(&tmp);
            if !ok {
                return Err(format!("could not fetch {name} from any location"));
            }
        }
        let (size, b2, sha) = manifest_hashes(&path).map_err(|err| format!("{name}: {err}"))?;
        lines.push(format!("DIST {name} {size} BLAKE2B {b2} SHA512 {sha}"));
    }
    // keep whatever else a thick Manifest has (EBUILD/AUX/MISC lines)
    let text = std::fs::read_to_string(pkgdir.join("Manifest")).unwrap_or_default();
    lines.extend(text.lines().filter(|l| !l.starts_with("DIST ") && !l.trim().is_empty()).map(String::from));
    let out = pkgdir.join("Manifest");
    let tmp = pkgdir.join(".Manifest.hoard");
    std::fs::write(&tmp, if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" }).map_err(|err| format!("{}: {err}", tmp.display()))?;
    // as root in someone's checkout: keep the file theirs
    if let Ok(md) = std::fs::metadata(&pkgdir) {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(&tmp, Some(md.uid()), Some(md.gid()));
    }
    std::fs::rename(&tmp, &out).map_err(|err| err.to_string())?;
    info(&format!("Wrote {} ({} distfile{})", out.display(), want.len(), if want.len() == 1 { "" } else { "s" }));
    Ok(())
}
