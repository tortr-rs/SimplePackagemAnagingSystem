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
