//! Binary packages from the official Gentoo binhost (GLEP 78 .gpkg.tar).

use crate::config::{Config, UseInfo};
use crate::repo::Ebuild;
use crate::util::{info, warn};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

pub const RELEASE_KEY: &str = "/usr/share/openpgp-keys/gentoo-release.asc";

#[derive(Debug)]
pub struct BinPkg {
    pub build_id: u64,
    pub path: String,
    pub base: String,
    pub size: u64,
    pub md5: String,
    pub sha1: String,
    pub use_: HashSet<String>,
    pub iuse: HashSet<String>,
    pub fields: HashMap<String, String>,
}

impl BinPkg {
    pub fn get(&self, k: &str) -> &str {
        self.fields.get(k).map(|s| s.as_str()).unwrap_or("")
    }
    pub fn url(&self) -> String {
        format!("{}/{}", self.base, self.path)
    }
    pub fn file_name(&self) -> String {
        self.path.rsplit('/').next().unwrap_or(&self.path).to_string()
    }
}

pub struct Binhost {
    pkgs: HashMap<String, Vec<Rc<BinPkg>>>,
}

/// The official binhost for this machine's profile, or PORTAGE_BINHOST if set.
pub fn binhost_url(cfg: &Config) -> Option<String> {
    if let Some(u) = cfg.var("PORTAGE_BINHOST").split_whitespace().next() {
        return Some(u.trim_end_matches('/').to_string());
    }
    let variant = match cfg.arch.as_str() {
        "amd64" => "x86-64",
        "arm64" => "arm64",
        "x86" => "i686",
        _ => return None,
    };
    Some(format!("https://distfiles.gentoo.org/releases/{}/binpackages/23.0/{variant}", cfg.arch))
}

fn http_date(t: u64) -> String {
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days.rem_euclid(7)) as usize];
    let mn = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m - 1) as usize];
    format!("{wd}, {d:02} {mn} {y} {:02}:{:02}:{:02} GMT", secs / 3600, secs / 60 % 60, secs % 60)
}

const SYS_CACHE: &str = "/var/cache/hoard/binhost";

fn age_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_secs())
}

/// Where to keep the index: the system cache for root; for users, root's copy
/// while it is fresh, otherwise their own.
fn cache_file(name: &str) -> PathBuf {
    let sys = PathBuf::from(SYS_CACHE);
    if crate::util::is_root() {
        let _ = std::fs::create_dir_all(&sys);
        return sys.join(name);
    }
    if age_secs(&sys.join(name)).map(|a| a <= 3600).unwrap_or(false) {
        return sys.join(name);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let p = PathBuf::from(home).join(".cache/hoard/binhost");
    let _ = std::fs::create_dir_all(&p);
    p.join(name)
}

impl Binhost {
    /// Load the Packages index, refreshing the cached copy when it is older than an hour.
    pub fn load(cfg: &Config, force_refresh: bool) -> Result<Binhost, String> {
        let base = binhost_url(cfg).ok_or_else(|| format!("no official binhost for ARCH={}", cfg.arch))?;
        let cache = cache_file(&format!("Packages-{}", &crate::util::blake2b_str(&base)[..12]));
        let age = age_secs(&cache);
        if force_refresh || age.map(|a| a > 3600).unwrap_or(true) {
            if let Err(e) = refresh(&base, &cache) {
                if cache.exists() {
                    warn(&format!("could not refresh the binhost index ({e}); using the cached copy"));
                } else {
                    return Err(format!("could not download the binhost index: {e}"));
                }
            }
        }
        let text = std::fs::read_to_string(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
        Ok(parse_index(&text, &base))
    }

    /// A binary that is exactly this ebuild with exactly this USE configuration.
    pub fn find(&self, e: &Ebuild, u: &UseInfo) -> Option<Rc<BinPkg>> {
        let list = self.pkgs.get(&e.cpv())?;
        let iuse: HashSet<String> = u.iuse.iter().cloned().collect();
        let want: HashSet<&String> = u.enabled.iter().filter(|f| iuse.contains(*f)).collect();
        let slot = if e.meta.get("SLOT").is_empty() { "0" } else { e.meta.get("SLOT") };
        list.iter()
            .filter(|b| {
                let repo = b.get("REPO");
                (repo.is_empty() || repo == e.repo_name)
                    && b.iuse == iuse
                    && b.use_.iter().filter(|f| iuse.contains(*f)).collect::<HashSet<_>>() == want
                    && (if b.get("SLOT").is_empty() { "0" } else { b.get("SLOT") }) == slot
                    && crate::repo::SUPPORTED_EAPIS.contains(&b.get("EAPI"))
            })
            .max_by_key(|b| b.build_id)
            .cloned()
    }
}

fn refresh(base: &str, cache: &Path) -> Result<(), String> {
    info("Fetching the binhost package index...");
    let mut req = ureq::get(&format!("{base}/Packages")).set("User-Agent", "hoard/0.1 (SimplePackagemAnagingSystem)");
    if let Ok(t) = std::fs::metadata(cache).and_then(|m| m.modified()) {
        if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
            req = req.set("If-Modified-Since", &http_date(d.as_secs()));
        }
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(c, _) => format!("HTTP {c}"),
        e => e.to_string(),
    })?;
    if resp.status() == 304 {
        // still current: just mark it fresh
        let f = std::fs::File::options().append(true).open(cache).map_err(|e| e.to_string())?;
        f.set_modified(std::time::SystemTime::now()).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let mut body = String::new();
    resp.into_reader().take(256 << 20).read_to_string(&mut body).map_err(|e| e.to_string())?;
    if !body.contains("\nCPV: ") {
        return Err("the index does not look like a Packages file".into());
    }
    let tmp = cache.with_extension("new");
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, cache).map_err(|e| e.to_string())?;
    Ok(())
}

fn parse_index(text: &str, base: &str) -> Binhost {
    let mut pkgs: HashMap<String, Vec<Rc<BinPkg>>> = HashMap::new();
    for block in text.split("\n\n") {
        let mut f: HashMap<String, String> = HashMap::new();
        for line in block.lines() {
            if let Some((k, v)) = line.split_once(':') {
                f.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let Some(cpv) = f.get("CPV").cloned() else { continue };
        let Some(path) = f.get("PATH").cloned() else { continue };
        if !path.ends_with(".gpkg.tar") {
            continue;
        }
        let set = |k: &str| -> HashSet<String> {
            f.get(k).map(|s| s.split_whitespace().map(|x| x.trim_start_matches(['+', '-']).to_string()).collect()).unwrap_or_default()
        };
        let b = BinPkg {

            build_id: f.get("BUILD_ID").and_then(|s| s.parse().ok()).unwrap_or(0),
            path,
            base: base.to_string(),
            size: f.get("SIZE").and_then(|s| s.parse().ok()).unwrap_or(0),
            md5: f.get("MD5").cloned().unwrap_or_default(),
            sha1: f.get("SHA1").cloned().unwrap_or_default(),
            use_: set("USE"),
            iuse: set("IUSE"),
            fields: f,
        };
        pkgs.entry(cpv).or_default().push(Rc::new(b));
    }
    Binhost { pkgs }
}

// ---------------------------------------------------------------- fetching and unpacking

pub fn pkgdir(cfg: &Config) -> PathBuf {
    let p = cfg.var("PKGDIR");
    PathBuf::from(if p.is_empty() { "/var/cache/binpkgs" } else { p })
}

fn check_index_hashes(p: &Path, b: &BinPkg) -> Result<(), String> {
    use sha1_smol_like::sha1_hex;
    let data = std::fs::read(p).map_err(|e| e.to_string())?;
    if b.size != 0 && data.len() as u64 != b.size {
        return Err(format!("size {} but the index says {}", data.len(), b.size));
    }
    if !b.md5.is_empty() {
        use md5::Digest;
        if crate::util::hex(&md5::Md5::digest(&data)) != b.md5.to_lowercase() {
            return Err("MD5 mismatch".into());
        }
    }
    if !b.sha1.is_empty() && sha1_hex(&data) != b.sha1.to_lowercase() {
        return Err("SHA1 mismatch".into());
    }
    Ok(())
}

/// Download the package into PKGDIR (or reuse a verified copy).
pub fn fetch(cfg: &Config, b: &BinPkg) -> Result<PathBuf, String> {
    let dest = pkgdir(cfg).join(&b.path);
    if dest.is_file() && check_index_hashes(&dest, b).is_ok() {
        return Ok(dest);
    }
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
    info(&format!("Downloading {}", b.file_name()));
    let resp = ureq::get(&b.url())
        .set("User-Agent", "hoard/0.1 (SimplePackagemAnagingSystem)")
        .call()
        .map_err(|e| format!("{}: {e}", b.url()))?;
    let tmp = dest.with_extension("partial");
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    std::io::copy(&mut resp.into_reader(), &mut f).map_err(|e| e.to_string())?;
    drop(f);
    if let Err(e) = check_index_hashes(&tmp, b) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", b.file_name()));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

fn decompressor<'a>(name: &str, f: std::fs::File) -> Result<Box<dyn Read + 'a>, String> {
    Ok(if name.ends_with(".xz") {
        Box::new(xz2::read::XzDecoder::new(f))
    } else if name.ends_with(".zst") {
        Box::new(zstd::stream::read::Decoder::new(f).map_err(|e| e.to_string())?)
    } else if name.ends_with(".bz2") {
        Box::new(bzip2::read::BzDecoder::new(f))
    } else if name.ends_with(".gz") {
        Box::new(flate2::read::GzDecoder::new(f))
    } else if name.ends_with(".tar") {
        Box::new(f)
    } else {
        return Err(format!("unsupported compression in {name}"));
    })
}

/// Relative path inside `prefix/`, refusing anything that could escape the destination.
fn inner(path: &Path, prefix: &str) -> Option<PathBuf> {
    let mut comps = path.components();
    match comps.next() {
        Some(Component::Normal(c)) if c == prefix => {}
        _ => return None,
    }
    let rest: PathBuf = comps.collect();
    if rest.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(rest)
}

fn unpack_tar(r: impl Read, prefix: &str, dest: &Path) -> Result<(), String> {
    let mut ar = tar::Archive::new(r);
    ar.set_preserve_permissions(true);
    ar.set_preserve_ownerships(true);
    ar.set_preserve_mtime(true);
    ar.set_unpack_xattrs(true);
    ar.set_overwrite(true);
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut dirs: Vec<(PathBuf, u32)> = Vec::new();
    for ent in ar.entries().map_err(|e| e.to_string())? {
        let mut ent = ent.map_err(|e| e.to_string())?;
        let path = ent.path().map_err(|e| e.to_string())?.into_owned();
        let Some(rel) = inner(&path, prefix) else {
            if path.components().count() <= 1 {
                continue; // the top directory itself
            }
            return Err(format!("unsafe path in package: {}", path.display()));
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&rel);
        // never write through a symlink that the archive itself created
        let mut cur = dest.to_path_buf();
        for c in rel.parent().into_iter().flat_map(|p| p.components()) {
            cur.push(c);
            if std::fs::symlink_metadata(&cur).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                return Err(format!("unsafe path in package (through symlink): {}", rel.display()));
            }
        }
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let kind = ent.header().entry_type();
        if kind.is_hard_link() {
            let link = ent.link_name().map_err(|e| e.to_string())?.ok_or("hard link without target")?;
            let src = inner(&link, prefix).ok_or_else(|| format!("unsafe hard link {}", link.display()))?;
            let _ = std::fs::remove_file(&target);
            std::fs::hard_link(dest.join(src), &target).map_err(|e| e.to_string())?;
            continue;
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            dirs.push((target.clone(), ent.header().mode().unwrap_or(0o755)));
        }
        ent.unpack(&target).map_err(|e| format!("{}: {e}", rel.display()))?;
    }
    // directory modes last, so read-only directories don't block their contents
    use std::os::unix::fs::PermissionsExt;
    for (d, mode) in dirs.into_iter().rev() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode & 0o7777));
    }
    Ok(())
}

/// Verify a downloaded .gpkg.tar and unpack it: metadata into `meta`, files into `image`.
pub fn unpack(gpkg: &Path, work: &Path, meta: &Path, image: &Path, require_sig: bool) -> Result<(), String> {
    let parts = work.join("gpkg");
    let _ = std::fs::remove_dir_all(&parts);
    std::fs::create_dir_all(&parts).map_err(|e| e.to_string())?;
    // outer tar: plain, members under one directory
    let f = std::fs::File::open(gpkg).map_err(|e| e.to_string())?;
    let mut outer = tar::Archive::new(f);
    let mut names = Vec::new();
    for ent in outer.entries().map_err(|e| e.to_string())? {
        let mut ent = ent.map_err(|e| e.to_string())?;
        let path = ent.path().map_err(|e| e.to_string())?.into_owned();
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if path.components().count() != 2 || name.contains('/') || name.starts_with('.') {
            continue;
        }
        let mut out = std::fs::File::create(parts.join(&name)).map_err(|e| e.to_string())?;
        std::io::copy(&mut ent, &mut out).map_err(|e| e.to_string())?;
        names.push(name);
    }
    if !names.iter().any(|n| n == "gpkg-1") {
        return Err("not a gpkg-1 package".into());
    }
    // signed Manifest -> hashes of every member
    let manifest = parts.join("Manifest");
    if manifest.exists() {
        crate::sync::verify_signature(&manifest, Path::new(RELEASE_KEY))?;
        let text = std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?;
        let mut listed = HashSet::new();
        for l in text.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 3 && f[0] == "DATA" {
                listed.insert(f[1].to_string());
                let p = parts.join(f[1]);
                let (size, b2, sha) = crate::util::manifest_hashes(&p).map_err(|e| format!("{}: {e}", f[1]))?;
                if size.to_string() != f[2] {
                    return Err(format!("{}: size mismatch", f[1]));
                }
                for pair in f[3..].chunks(2) {
                    let ok = match pair {
                        ["BLAKE2B", h] => h.to_lowercase() == b2,
                        ["SHA512", h] => h.to_lowercase() == sha,
                        _ => true,
                    };
                    if !ok {
                        return Err(format!("{}: checksum mismatch", f[1]));
                    }
                }
            }
        }
        for n in &names {
            if n != "Manifest" && !listed.contains(n) {
                return Err(format!("{n} is not covered by the signed Manifest"));
            }
        }
    } else if require_sig {
        return Err("the binary package is not signed".into());
    }
    let find = |prefix: &str| -> Result<String, String> {
        names
            .iter()
            .find(|n| n.starts_with(&format!("{prefix}.tar")) && !n.ends_with(".sig"))
            .cloned()
            .ok_or_else(|| format!("no {prefix} archive in the package"))
    };
    let m = find("metadata")?;
    unpack_tar(decompressor(&m, std::fs::File::open(parts.join(&m)).map_err(|e| e.to_string())?)?, "metadata", meta)?;
    let i = find("image")?;
    unpack_tar(decompressor(&i, std::fs::File::open(parts.join(&i)).map_err(|e| e.to_string())?)?, "image", image)?;
    let _ = std::fs::remove_dir_all(&parts);
    Ok(())
}

/// Minimal SHA-1 (only used to check the binhost index's SHA1 field).
mod sha1_smol_like {
    pub fn sha1_hex(data: &[u8]) -> String {
        let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
        let ml = (data.len() as u64).wrapping_mul(8);
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&ml.to_be_bytes());
        for chunk in msg.chunks(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
            for (i, wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A827999),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                    _ => (b ^ c ^ d, 0xCA62C1D6),
                };
                let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = t;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }
        h.iter().map(|x| format!("{x:08x}")).collect()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn known() {
            assert_eq!(super::sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
            assert_eq!(super::sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn date() {
        assert_eq!(super::http_date(784111777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }
}
