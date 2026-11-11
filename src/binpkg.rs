//! Creating GLEP 78 binary packages from our own builds (FEATURES=buildpkg)
//! and keeping PKGDIR's Packages index in Portage's format.

use crate::build::Build;
use crate::config::{Config, UseInfo};
use crate::repo::Ebuild;
use crate::util::info;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn read(meta: &Path, k: &str) -> String {
    std::fs::read_to_string(meta.join(k)).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn walk_size(p: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(p) else { return 0 };
    if md.is_dir() {
        std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| walk_size(&e.path())).sum()).unwrap_or(0)
    } else if md.is_file() {
        md.len()
    } else {
        0
    }
}

/// Lowest BUILD_ID above every package of this PF already in PKGDIR.
fn next_build_id(dir: &Path, pf: &str) -> u64 {
    let mut max = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(id) = n.strip_prefix(&format!("{pf}-")).and_then(|r| r.strip_suffix(".gpkg.tar")).and_then(|r| r.parse::<u64>().ok()) {
                max = max.max(id);
            }
        }
    }
    max + 1
}

fn tar_dir(src: &Path, prefix: &str, out: &Path) -> Result<(), String> {
    let f = std::fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let enc = zstd::stream::write::Encoder::new(f, 9).map_err(|e| e.to_string())?.auto_finish();
    let mut b = tar::Builder::new(enc);
    b.follow_symlinks(false);
    b.append_dir_all(prefix, src).map_err(|e| format!("packing {prefix}: {e}"))?;
    b.into_inner().map_err(|e| e.to_string())?;
    Ok(())
}

/// Package a finished build into PKGDIR; returns the .gpkg.tar path.
pub fn create(cfg: &Config, e: &Ebuild, use_: &UseInfo, build: &Build) -> Result<PathBuf, String> {
    let pkgdir = crate::binhost::pkgdir(cfg);
    let dir = pkgdir.join(&e.cat).join(&e.pn);
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let _lock = crate::util::lock(&pkgdir.join(".hoard.lock"))?;
    let build_id = next_build_id(&dir, &e.pf());
    let base = format!("{}-{build_id}", e.pf());

    let work = build.builddir.join("binpkg");
    let _ = std::fs::remove_dir_all(&work);
    let meta = work.join("metadata");
    std::fs::create_dir_all(&meta).map_err(|err| err.to_string())?;
    crate::merge::write_metadata(cfg, e, use_, build, &meta)?;
    std::fs::write(meta.join("BUILD_ID"), format!("{build_id}\n")).map_err(|err| err.to_string())?;
    std::fs::write(meta.join("SIZE"), format!("{}\n", walk_size(&build.image))).map_err(|err| err.to_string())?;

    info(&format!("Packaging {} as {base}.gpkg.tar", crate::util::bold(&e.cpv())));
    std::fs::write(work.join("gpkg-1"), "").map_err(|err| err.to_string())?;
    tar_dir(&meta, "metadata", &work.join("metadata.tar.zst"))?;
    tar_dir(&build.image, "image", &work.join("image.tar.zst"))?;
    let members = ["gpkg-1", "metadata.tar.zst", "image.tar.zst"];
    let mut manifest = String::new();
    for m in members {
        let (size, b2, sha) = crate::util::manifest_hashes(&work.join(m)).map_err(|err| format!("{m}: {err}"))?;
        manifest.push_str(&format!("DATA {m} {size} BLAKE2B {b2} SHA512 {sha}\n"));
    }
    std::fs::write(work.join("Manifest"), manifest).map_err(|err| err.to_string())?;

    let dest = dir.join(format!("{base}.gpkg.tar"));
    let tmp = dir.join(format!(".{base}.gpkg.tar.partial"));
    {
        let f = std::fs::File::create(&tmp).map_err(|err| format!("{}: {err}", tmp.display()))?;
        let mut b = tar::Builder::new(f);
        for m in members.iter().chain(std::iter::once(&"Manifest")) {
            b.append_path_with_name(work.join(m), format!("{base}/{m}")).map_err(|err| err.to_string())?;
        }
        b.into_inner().map_err(|err| err.to_string())?;
    }
    std::fs::rename(&tmp, &dest).map_err(|err| err.to_string())?;
    let rel = format!("{}/{}/{base}.gpkg.tar", e.cat, e.pn);
    update_index(&pkgdir, &meta, &e.cpv(), &rel, build_id, &dest)?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(dest)
}

/// Add (or replace) this package's entry in PKGDIR/Packages.
fn update_index(pkgdir: &Path, meta: &Path, cpv: &str, rel: &str, build_id: u64, file: &Path) -> Result<(), String> {
    let index = pkgdir.join("Packages");
    let text = std::fs::read_to_string(&index).unwrap_or_default();
    let mut header: BTreeMap<String, String> = BTreeMap::new();
    let mut blocks: Vec<String> = Vec::new();
    for (i, block) in text.split("\n\n").enumerate() {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        if i == 0 && !block.lines().any(|l| l.starts_with("CPV:")) {
            for l in block.lines() {
                if let Some((k, v)) = l.split_once(':') {
                    header.insert(k.trim().into(), v.trim().into());
                }
            }
            continue;
        }
        if block.lines().any(|l| l == format!("PATH: {rel}")) {
            continue;
        }
        blocks.push(block.to_string());
    }
    let data = std::fs::read(file).map_err(|e| e.to_string())?;
    let md5 = {
        use md5::Digest;
        crate::util::hex(&md5::Md5::digest(&data))
    };
    let mut f: BTreeMap<&str, String> = BTreeMap::new();
    f.insert("BUILD_ID", build_id.to_string());
    f.insert("CPV", cpv.to_string());
    f.insert("MD5", md5);
    f.insert("SHA1", crate::binhost::sha1_hex(&data));
    f.insert("SIZE", data.len().to_string());
    f.insert("PATH", rel.to_string());
    f.insert("MTIME", crate::util::now().to_string());
    f.insert("REPO", read(meta, "repository"));
    for k in [
        "BUILD_TIME", "DEFINED_PHASES", "EAPI", "IUSE", "KEYWORDS", "LICENSE", "RESTRICT", "PROPERTIES", "PROVIDES", "REQUIRES",
        "USE", "DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND",
    ] {
        f.insert(k, read(meta, k));
    }
    let slot = read(meta, "SLOT");
    if slot != "0" {
        f.insert("SLOT", slot);
    }
    let entry: String = f.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
    blocks.push(entry);
    header.entry("VERSION".into()).or_insert_with(|| "0".into());
    header.insert("PACKAGES".into(), blocks.len().to_string());
    header.insert("TIMESTAMP".into(), crate::util::now().to_string());
    let mut out: String = header.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
    for b in &blocks {
        out.push('\n');
        out.push_str(b);
        out.push('\n');
    }
    let tmp = pkgdir.join(".Packages.hoard");
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &index).map_err(|e| e.to_string())?;
    Ok(())
}
