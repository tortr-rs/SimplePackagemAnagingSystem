//! Merging an image into the live filesystem, unmerging, and recording
//! packages in /var/db/pkg.

use crate::atom::{Atom, SlotOp};
use crate::build::{runner, Build};
use crate::config::{Config, UseInfo};
use crate::depspec::{self, Node};
use crate::ebuild_env::PhaseOpts;
use crate::repo::Ebuild;
use crate::util::{info, md5_file, warn};
use crate::vdb::{Entry, Installed, Vdb};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Canonical form of a path whose final component may not exist,
/// so /bin/foo and /usr/bin/foo compare equal on merged-usr systems.
pub struct Canon {
    cache: HashMap<PathBuf, PathBuf>,
}

impl Canon {
    pub fn new() -> Canon {
        Canon { cache: HashMap::new() }
    }
    pub fn get(&mut self, p: &Path) -> PathBuf {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return p.to_path_buf() };
        let cp = if let Some(c) = self.cache.get(parent) {
            c.clone()
        } else {
            let c = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            self.cache.insert(parent.to_path_buf(), c.clone());
            c
        };
        cp.join(name)
    }
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_encoded_bytes()).unwrap()
}

fn lchown(p: &Path, uid: u32, gid: u32) {
    unsafe {
        libc::lchown(cstr(p).as_ptr(), uid, gid);
    }
}

fn set_mtime(p: &Path, sec: i64, nsec: i64) {
    let ts = [
        libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as _ },
        libc::timespec { tv_sec: sec as libc::time_t, tv_nsec: nsec as _ },
    ];
    unsafe {
        libc::utimensat(libc::AT_FDCWD, cstr(p).as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW);
    }
}

pub struct Protect {
    protect: Vec<String>,
    mask: Vec<String>,
}

impl Protect {
    pub fn new(cfg: &Config) -> Protect {
        let norm = |s: &str| -> Vec<String> {
            s.split_whitespace().map(|x| x.trim_end_matches('/').to_string()).filter(|x| !x.is_empty()).collect()
        };
        Protect { protect: norm(cfg.var("CONFIG_PROTECT")), mask: norm(cfg.var("CONFIG_PROTECT_MASK")) }
    }
    fn best(list: &[String], path: &str) -> usize {
        list.iter()
            .filter(|p| path == p.as_str() || path.starts_with(&format!("{p}/")))
            .map(|p| p.len())
            .max()
            .unwrap_or(0)
    }
    pub fn protected(&self, path: &str) -> bool {
        let p = Self::best(&self.protect, path);
        p > 0 && p > Self::best(&self.mask, path)
    }
}

fn root_path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel.trim_start_matches('/'))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for p in v {
            out.push(p.clone());
            if p.is_dir() && !p.is_symlink() {
                walk(&p, out);
            }
        }
    }
}

/// ._cfgNNNN_name for a protected file; None when an identical one is already waiting.
fn cfg_name(dest: &Path, md5: &str) -> Option<PathBuf> {
    let dir = dest.parent().unwrap();
    let name = dest.file_name().unwrap().to_string_lossy().to_string();
    for n in 0..10000 {
        let p = dir.join(format!("._cfg{n:04}_{name}"));
        if !p.exists() {
            return Some(p);
        }
        if md5_file(&p).ok().as_deref() == Some(md5) {
            return None;
        }
    }
    Some(dir.join(format!("._cfg9999_{name}")))
}

pub struct MergeResult {
    pub contents: Vec<Entry>,
    pub protected_updates: Vec<String>,
}

/// Check for files owned by other packages before touching anything.
fn collisions(cfg: &Config, image: &Path, vdb: &Vdb, replacing: &[&Installed], canon: &mut Canon) -> Vec<(String, String)> {
    let skip: HashSet<String> = replacing.iter().map(|i| i.cpv()).collect();
    let mut owners: HashMap<PathBuf, String> = HashMap::new();
    for p in &vdb.pkgs {
        if skip.contains(&p.cpv()) {
            continue;
        }
        for e in p.contents() {
            if !matches!(e, Entry::Dir(_)) {
                owners.insert(canon.get(&root_path(&cfg.root, e.path())), p.cpv());
            }
        }
    }
    let ignore: Vec<String> = cfg.var("COLLISION_IGNORE").split_whitespace().map(|s| s.to_string()).collect();
    let mut all = Vec::new();
    walk(image, &mut all);
    let mut out = Vec::new();
    for p in all {
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.is_dir() {
            continue;
        }
        let rel = format!("/{}", p.strip_prefix(image).unwrap().display());
        if ignore.iter().any(|g| crate::util::glob_match(g, &rel) || rel.starts_with(&format!("{}/", g.trim_end_matches("/*")))) {
            continue;
        }
        let dest = root_path(&cfg.root, &rel);
        if std::fs::symlink_metadata(&dest).is_err() {
            continue;
        }
        if let Some(o) = owners.get(&canon.get(&dest)) {
            out.push((rel, o.clone()));
        } else if cfg.feature("collision-protect") && !Protect::new(cfg).protected(&rel) && !dest.is_dir() {
            out.push((rel, "(not owned by any package)".into()));
        }
    }
    out
}

pub fn merge_image(cfg: &Config, image: &Path, old_contents: &HashMap<String, String>) -> Result<MergeResult, String> {
    let protect = Protect::new(cfg);
    let if_modified = cfg.feature("config-protect-if-modified");
    let mut all = Vec::new();
    walk(image, &mut all);
    let mut contents = Vec::new();
    let mut protected_updates = Vec::new();
    for p in all {
        let md = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let rel = format!("/{}", p.strip_prefix(image).unwrap().display());
        let dest = root_path(&cfg.root, &rel);
        let ft = md.file_type();
        let err = |e: std::io::Error| format!("{}: {e}", dest.display());
        if ft.is_dir() {
            match std::fs::symlink_metadata(&dest) {
                Ok(d) if d.is_dir() || (d.file_type().is_symlink() && dest.is_dir()) => {}
                Ok(_) => return Err(format!("cannot merge directory {rel}: a file is in the way")),
                Err(_) => {
                    std::fs::create_dir(&dest).map_err(err)?;
                    lchown(&dest, md.uid(), md.gid());
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(md.mode() & 0o7777)).map_err(err)?;
                }
            }
            contents.push(Entry::Dir(rel));
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&p).map_err(|e| e.to_string())?;
            if let Ok(d) = std::fs::symlink_metadata(&dest) {
                if d.is_dir() {
                    warn(&format!("not replacing directory {rel} with a symlink"));
                    continue;
                }
            }
            let tmp = dest.parent().unwrap().join(format!(".spas.{}.tmp", dest.file_name().unwrap().to_string_lossy()));
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(&target, &tmp).map_err(err)?;
            lchown(&tmp, md.uid(), md.gid());
            set_mtime(&tmp, md.mtime(), md.mtime_nsec());
            std::fs::rename(&tmp, &dest).map_err(err)?;
            contents.push(Entry::Sym { path: rel, target: target.display().to_string(), mtime: md.mtime().to_string() });
        } else if ft.is_file() {
            let md5 = md5_file(&p).map_err(|e| e.to_string())?;
            let mut write_to = Some(dest.clone());
            if protect.protected(&rel) {
                if let Ok(dm) = std::fs::metadata(&dest) {
                    if dm.is_file() {
                        let cur = md5_file(&dest).unwrap_or_default();
                        let unmodified = if_modified && old_contents.get(&rel).map(|m| *m == cur).unwrap_or(false);
                        if cur != md5 && !unmodified {
                            write_to = cfg_name(&dest, &md5);
                            protected_updates.push(rel.clone());
                        }
                    }
                }
            }
            if let Some(w) = write_to {
                if std::fs::symlink_metadata(&w).map(|m| m.is_dir()).unwrap_or(false) {
                    return Err(format!("cannot merge file {rel}: a directory is in the way"));
                }
                let tmp = w.parent().unwrap().join(format!(".spas.{}.tmp", w.file_name().unwrap().to_string_lossy()));
                std::fs::copy(&p, &tmp).map_err(err)?;
                lchown(&tmp, md.uid(), md.gid());
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(md.mode() & 0o7777)).map_err(err)?;
                set_mtime(&tmp, md.mtime(), md.mtime_nsec());
                std::fs::rename(&tmp, &w).map_err(err)?;
            }
            contents.push(Entry::Obj { path: rel, md5, mtime: md.mtime().to_string() });
        } else if ft.is_fifo() {
            if std::fs::symlink_metadata(&dest).is_err() {
                unsafe {
                    libc::mkfifo(cstr(&dest).as_ptr(), (md.mode() & 0o7777) as libc::mode_t);
                }
                lchown(&dest, md.uid(), md.gid());
            }
            contents.push(Entry::Fif(rel));
        } else if ft.is_char_device() || ft.is_block_device() {
            if std::fs::symlink_metadata(&dest).is_err() {
                unsafe {
                    libc::mknod(cstr(&dest).as_ptr(), md.mode() as libc::mode_t, md.rdev() as libc::dev_t);
                }
                lchown(&dest, md.uid(), md.gid());
            }
            contents.push(Entry::Dev(rel));
        }
    }
    Ok(MergeResult { contents, protected_updates })
}

/// Remove a package's files, keeping anything in `keep` (canonical paths) or owned by others.
pub fn unmerge_files(cfg: &Config, entries: &[Entry], keep: &HashSet<PathBuf>, others: &HashSet<PathBuf>, canon: &mut Canon, verbose: bool) {
    let protect = Protect::new(cfg);
    let if_modified = cfg.feature("config-protect-if-modified");
    let ignore: Vec<String> = cfg.var("UNINSTALL_IGNORE").split_whitespace().map(|s| s.to_string()).collect();
    let mut dirs = Vec::new();
    let show = |tag: &str, what: &str, p: &str| {
        if verbose {
            eprintln!("{tag} {what} {p}");
        }
    };
    for e in entries {
        let rel = e.path();
        if ignore.iter().any(|g| crate::util::glob_match(g, rel)) {
            continue;
        }
        let dest = root_path(&cfg.root, rel);
        let c = canon.get(&dest);
        if let Entry::Dir(_) = e {
            dirs.push(dest);
            continue;
        }
        if keep.contains(&c) || others.contains(&c) {
            show("---", "replaced", rel);
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(&dest) else { continue };
        match e {
            Entry::Obj { md5, mtime, .. } => {
                if !md.is_file() {
                    show("---", "!obj", rel);
                    continue;
                }
                let cur = md5_file(&dest).unwrap_or_default();
                if protect.protected(rel) && (cur != *md5 || !if_modified) {
                    show("---", "cfgpro", rel);
                    continue;
                }
                if cur != *md5 && md.mtime().to_string() != *mtime {
                    show("---", "!md5", rel);
                    continue;
                }
                let _ = std::fs::remove_file(&dest);
                show("<<<", "obj", rel);
            }
            Entry::Sym { .. } => {
                if !md.file_type().is_symlink() {
                    continue;
                }
                // never take away a symlink that leads to a non-empty directory
                if dest.is_dir() && std::fs::read_dir(&dest).map(|mut r| r.next().is_some()).unwrap_or(false) {
                    show("---", "!empty", rel);
                    continue;
                }
                let _ = std::fs::remove_file(&dest);
                show("<<<", "sym", rel);
            }
            Entry::Fif(_) | Entry::Dev(_) => {
                let _ = std::fs::remove_file(&dest);
                show("<<<", "dev", rel);
            }
            Entry::Dir(_) => {}
        }
    }
    dirs.sort_by(|a, b| b.as_os_str().len().cmp(&a.as_os_str().len()).then(b.cmp(a)));
    for d in dirs {
        let c = canon.get(&d);
        if keep.contains(&c) {
            continue;
        }
        if std::fs::symlink_metadata(&d).map(|m| m.file_type().is_symlink()).unwrap_or(true) {
            continue;
        }
        if std::fs::remove_dir(&d).is_ok() {
            let rel = d.strip_prefix(&cfg.root).map(|r| format!("/{}", r.display())).unwrap_or_else(|_| d.display().to_string());
            show("<<<", "dir", &rel);
        }
    }
}

/// Fill in `:=` slot operators with the slot/sub-slot actually installed.
fn resolve_slot_ops(nodes: &[Node], vdb: &Vdb) -> Vec<Node> {
    nodes
        .iter()
        .map(|n| match n {
            Node::Leaf(s) => {
                let Ok(mut a) = Atom::parse(s) else { return n.clone() };
                if a.blocker.is_none() {
                    if let Some(sd) = &a.slot {
                        if sd.op == Some(SlotOp::Eq) {
                            let best = vdb.matching_nouse(&a).into_iter().max_by(|x, y| x.ver.cmp(&y.ver));
                            if let Some(b) = best {
                                a.slot = Some(crate::atom::SlotDep {
                                    slot: Some(b.slot.clone()),
                                    subslot: Some(b.subslot.clone()),
                                    op: Some(SlotOp::Eq),
                                });
                                return Node::Leaf(a.to_string());
                            }
                        }
                    }
                }
                n.clone()
            }
            Node::All(c) => Node::All(resolve_slot_ops(c, vdb)),
            Node::Any(c) => Node::Any(resolve_slot_ops(c, vdb)),
            other => other.clone(),
        })
        .collect()
}

fn write_vdb(cfg: &Config, e: &Ebuild, use_: &UseInfo, build: &Build, contents: &[Entry], dir: &Path) -> Result<(), String> {
    write_metadata(cfg, e, use_, build, dir)?;
    let w = |k: &str, v: &str| -> Result<(), String> {
        std::fs::write(dir.join(k), format!("{v}\n")).map_err(|err| format!("{k}: {err}"))
    };
    w("COUNTER", &crate::vdb::next_counter(&cfg.root).to_string())?;
    let size: u64 = contents
        .iter()
        .filter_map(|c| match c {
            Entry::Obj { path, .. } => std::fs::metadata(root_path(&cfg.root, path)).ok().map(|m| m.len()),
            _ => None,
        })
        .sum();
    w("SIZE", &size.to_string())?;
    let text: String = contents.iter().map(|c| format!("{}\n", c.line())).collect();
    std::fs::write(dir.join("CONTENTS"), text).map_err(|err| err.to_string())?;
    Ok(())
}

/// Everything recorded about a built package except what depends on where it
/// was merged (CONTENTS, COUNTER, SIZE): shared by the VDB and binary packages.
pub fn write_metadata(cfg: &Config, e: &Ebuild, use_: &UseInfo, build: &Build, dir: &Path) -> Result<(), String> {
    let vdb = Vdb::load(&cfg.vdb_path());
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let w = |k: &str, v: &str| -> Result<(), String> {
        std::fs::write(dir.join(k), format!("{v}\n")).map_err(|err| format!("{k}: {err}"))
    };
    let reduce = |k: &str| -> String {
        let n = depspec::parse(e.meta.get(k)).unwrap_or_default();
        depspec::render(&depspec::reduce(&n, &use_.enabled))
    };
    for k in ["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"] {
        let n = depspec::parse(e.meta.get(k)).unwrap_or_default();
        let r = depspec::render(&resolve_slot_ops(&depspec::reduce(&n, &use_.enabled), &vdb));
        if !r.is_empty() {
            w(k, &r)?;
        }
    }
    for k in ["LICENSE", "RESTRICT", "PROPERTIES"] {
        let r = reduce(k);
        if !r.is_empty() {
            w(k, &r)?;
        }
    }
    let mut on: Vec<&String> = use_.enabled.iter().collect();
    on.sort();
    let mut eff: Vec<&String> = use_.effective.iter().collect();
    eff.sort();
    let join = |v: &[&String]| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
    w("CATEGORY", &e.cat)?;
    w("PF", &e.pf())?;
    w("SLOT", if e.meta.get("SLOT").is_empty() { "0" } else { e.meta.get("SLOT") })?;
    w("EAPI", e.meta.get("EAPI"))?;
    for k in ["KEYWORDS", "IUSE", "DESCRIPTION", "HOMEPAGE", "INHERITED", "DEFINED_PHASES", "REQUIRED_USE"] {
        if !e.meta.get(k).is_empty() {
            w(k, e.meta.get(k))?;
        }
    }
    w("IUSE_EFFECTIVE", &join(&eff))?;
    w("USE", &join(&on))?;
    w("repository", &e.repo_name)?;
    w("BUILD_TIME", &crate::util::now().to_string())?;
    for k in ["CBUILD", "CHOST", "CFLAGS", "CXXFLAGS", "LDFLAGS"] {
        // a binary package records the flags it was really built with
        let from_bin = build.bin_meta.as_ref().and_then(|m| std::fs::read_to_string(m.join(k)).ok());
        let v = match &from_bin {
            Some(s) => s.trim(),
            None => build.base_env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or(""),
        };
        if !v.is_empty() {
            w(k, v)?;
        }
    }
    // ELF data: from the binary package, or from scanning the image after the build
    let elf_src = build.bin_meta.clone().unwrap_or_else(|| build.builddir.join("elfmeta"));
    for k in ["NEEDED", "NEEDED.ELF.2", "REQUIRES", "PROVIDES", "BUILD_ID"] {
        if let Ok(s) = std::fs::read(elf_src.join(k)) {
            std::fs::write(dir.join(k), s).map_err(|err| format!("{k}: {err}"))?;
        }
    }
    let mut feats: Vec<&String> = cfg.features.iter().collect();
    feats.sort();
    w("FEATURES", &join(&feats))?;
    std::fs::copy(&e.path, dir.join(format!("{}.ebuild", e.pf()))).map_err(|err| err.to_string())?;
    // environment.bz2
    let env = std::fs::read(&build.env_file).unwrap_or_default();
    let f = std::fs::File::create(dir.join("environment.bz2")).map_err(|err| err.to_string())?;
    let mut enc = bzip2::write::BzEncoder::new(f, bzip2::Compression::best());
    std::io::Write::write_all(&mut enc, &env).map_err(|err| err.to_string())?;
    enc.finish().map_err(|err| err.to_string())?;
    Ok(())
}

/// Extract a VDB entry's environment.bz2 so its pkg_prerm/pkg_postrm can run.
fn vdb_env(inst: &Installed, dest: &Path) -> Option<PathBuf> {
    let f = std::fs::File::open(inst.dir.join("environment.bz2")).ok()?;
    let mut dec = bzip2::read::BzDecoder::new(f);
    let mut s = Vec::new();
    std::io::Read::read_to_end(&mut dec, &mut s).ok()?;
    std::fs::write(dest, crate::ebuild_env::strip_locale(&s)).ok()?;
    Some(dest.to_path_buf())
}

/// Environment for running phases of an installed package (prerm/postrm).
fn installed_phase_env(cfg: &Config, inst: &Installed, workdir: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for (k, v) in crate::ebuild_env::pkg_vars(&inst.cat, &inst.pn, &inst.ver) {
        env.push((k, v));
    }
    let eapi = inst.read("EAPI");
    let root = if cfg.root == Path::new("/") { String::new() } else { cfg.root.display().to_string() };
    let t = workdir.display().to_string();
    for (k, v) in [
        ("EAPI", eapi.clone()),
        ("BASH_COMPAT", match eapi.as_str() { "7" => "4.2", "8" => "5.0", _ => "5.3" }.to_string()),
        ("USE", inst.read("USE")),
        ("SPAS_IUSE_EFFECTIVE", inst.read("IUSE_EFFECTIVE")),
        ("T", t.clone()),
        ("HOME", t.clone()),
        ("TMPDIR", t.clone()),
        ("WORKDIR", t.clone()),
        ("PORTAGE_BUILDDIR", t.clone()),
        ("ROOT", root.clone()),
        ("EROOT", root.clone()),
        ("SYSROOT", String::new()),
        ("ESYSROOT", String::new()),
        ("BROOT", String::new()),
        ("EPREFIX", String::new()),
        ("D", String::new()),
        ("ED", String::new()),
        ("PATH", crate::ebuild_env::PATH.to_string()),
        ("TERM", std::env::var("TERM").unwrap_or_else(|_| "dumb".into())),
        ("PORTAGE_REPO_NAME", inst.read("repository")),
        ("EBUILD", inst.dir.join(format!("{}.ebuild", inst.pf())).display().to_string()),
        ("SPAS_BIN", crate::ebuild_env::self_exe()),
        ("SPAS_BASHRCS", cfg.bashrcs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")),
        ("SPAS_ROOT", cfg.root.display().to_string()),
        ("PORTAGE_CONFIGROOT", "/".to_string()),
        ("MERGE_TYPE", "source".to_string()),
    ] {
        env.push((k.to_string(), v));
    }
    if !crate::util::color() {
        env.push(("NOCOLOR".into(), "true".into()));
    }
    env
}

fn run_installed_phases(cfg: &Config, inst: &Installed, phase: &str, replaced_by: Option<&str>, workdir: &Path) {
    let defined = inst.read("DEFINED_PHASES");
    let has_hooks = !cfg.bashrcs.is_empty();
    if !defined.split_whitespace().any(|p| p == phase) && !has_hooks {
        return;
    }
    let _ = std::fs::create_dir_all(workdir);
    let Some(envf) = vdb_env(inst, &workdir.join(format!("environment.{}", inst.pf()))) else { return };
    let r = runner(cfg, installed_phase_env(cfg, inst, workdir), None, false);
    let extra = vec![("REPLACED_BY_VERSION".to_string(), replaced_by.unwrap_or("").to_string())];
    let opts = PhaseOpts { drop_privs: false, sandbox: false, no_network: false };
    if let Err(e) = r.phase(phase, opts, Some(&envf), None, &extra) {
        warn(&format!("{} {phase}: {e} (continuing)", inst.cpv()));
    }
}

/// Merge a freshly built package, replacing `old` (same slot) if given.
pub fn install(cfg: &Config, e: &Ebuild, use_: &UseInfo, build: &Build, quiet: bool) -> Result<(), String> {
    let vdb = Vdb::load(&cfg.vdb_path());
    let old: Vec<&Installed> = vdb.pkgs.iter().filter(|p| p.cp() == e.cp() && p.slot == e.slot()).collect();
    let replacing: Vec<String> = old.iter().map(|o| o.ver.to_string()).collect();
    let r = runner(cfg, build.base_env.clone(), Some(build.temp.join("build.log")), quiet);
    let extra = vec![("REPLACING_VERSIONS".to_string(), replacing.join(" "))];
    let root_opts = PhaseOpts { drop_privs: false, sandbox: false, no_network: false };

    let preinst_env = build.temp.join("environment.preinst");
    r.phase("preinst", root_opts, Some(&build.env_file), Some(&preinst_env), &extra)?;

    let mut canon = Canon::new();
    let col = collisions(cfg, &build.image, &vdb, &old, &mut canon);
    if !col.is_empty() {
        let mut msg = format!("{} would overwrite files owned by other packages:\n", e.cpv());
        for (p, o) in col.iter().take(30) {
            msg.push_str(&format!("    {p}  ({o})\n"));
        }
        if col.len() > 30 {
            msg.push_str(&format!("    ... and {} more\n", col.len() - 30));
        }
        msg.push_str("Nothing was merged. (FEATURES=-protect-owned turns this check off.)");
        return Err(msg);
    }

    info(&format!("Merging {} to {}", crate::util::bold(&e.cpv()), cfg.root.display()));
    let old_md5: HashMap<String, String> = old
        .iter()
        .flat_map(|o| o.contents())
        .filter_map(|c| match c {
            Entry::Obj { path, md5, .. } => Some((path, md5)),
            _ => None,
        })
        .collect();
    let res = merge_image(cfg, &build.image, &old_md5)?;

    // register the new package
    let final_dir = cfg.vdb_path().join(&e.cat).join(e.pf());
    let merging = cfg.vdb_path().join(&e.cat).join(format!("-MERGING-{}", e.pf()));
    let _ = std::fs::remove_dir_all(&merging);
    let mut tmp_build = Build {
        builddir: build.builddir.clone(),
        image: build.image.clone(),
        temp: build.temp.clone(),
        env_file: preinst_env.clone(),
        base_env: build.base_env.clone(),
        a: build.a.clone(),
        bin_meta: build.bin_meta.clone(),
    };
    if !tmp_build.env_file.exists() {
        tmp_build.env_file = build.env_file.clone();
    }
    write_vdb(cfg, e, use_, &tmp_build, &res.contents, &merging)?;

    // take the old version(s) out
    let keep: HashSet<PathBuf> = res.contents.iter().map(|c| canon.get(&root_path(&cfg.root, c.path()))).collect();
    let new_paths: HashSet<&str> = res.contents.iter().map(|c| c.path()).collect();
    let new_needed = std::fs::read_to_string(merging.join("NEEDED.ELF.2")).unwrap_or_default();
    let new_provides = std::fs::read_to_string(merging.join("PROVIDES")).unwrap_or_default();
    for o in &old {
        info(&format!("Unmerging old {}", o.cpv()));
        let work = build.temp.join(format!("old-{}", o.pf()));
        run_installed_phases(cfg, o, "prerm", Some(&e.ver.to_string()), &work);
        let others = other_owned(cfg, &vdb, &[o.cpv(), e.cpv()], &mut canon);
        // preserve-libs: old libraries that installed programs still link against stay
        let removed: HashSet<String> = o
            .contents()
            .iter()
            .filter(|c| !matches!(c, Entry::Dir(_)) && !new_paths.contains(c.path()))
            .map(|c| c.path().to_string())
            .collect();
        let preserved = crate::preserve::select(cfg, &vdb, o, &e.cpv(), &removed, &new_needed, &new_provides);
        let mut keep = keep.clone();
        for p in &preserved {
            keep.insert(canon.get(&root_path(&cfg.root, p.path())));
        }
        unmerge_files(cfg, &o.contents(), &keep, &others, &mut canon, false);
        if !preserved.is_empty() {
            let mut text = std::fs::read_to_string(merging.join("CONTENTS")).unwrap_or_default();
            for p in &preserved {
                text.push_str(&format!("{}\n", p.line()));
                info(&format!("Preserving {} (still used by installed programs)", p.path()));
            }
            std::fs::write(merging.join("CONTENTS"), text).map_err(|err| err.to_string())?;
            let counter = std::fs::read_to_string(merging.join("COUNTER")).unwrap_or_default();
            let paths: Vec<String> = preserved.iter().map(|p| p.path().to_string()).collect();
            crate::preserve::register(cfg, &format!("{}:{}", e.cp(), e.slot()), &e.cpv(), counter.trim(), &paths)?;
        }
        run_installed_phases(cfg, o, "postrm", Some(&e.ver.to_string()), &work);
        if o.dir != final_dir {
            let _ = std::fs::remove_dir_all(&o.dir);
        }
    }
    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir).map_err(|err| err.to_string())?;
    }
    std::fs::rename(&merging, &final_dir).map_err(|err| format!("registering {}: {err}", e.cpv()))?;

    r.phase("postinst", root_opts, Some(&preinst_env), None, &extra)?;
    for p in &res.protected_updates {
        warn(&format!("config file {p} needs updating (._cfg file written next to it)"));
    }
    Ok(())
}

/// Canonical paths of files owned by packages other than `skip`.
fn other_owned(cfg: &Config, vdb: &Vdb, skip: &[String], canon: &mut Canon) -> HashSet<PathBuf> {
    let mut s = HashSet::new();
    for p in &vdb.pkgs {
        if skip.contains(&p.cpv()) {
            continue;
        }
        for e in p.contents() {
            if !matches!(e, Entry::Dir(_)) {
                s.insert(canon.get(&root_path(&cfg.root, e.path())));
            }
        }
    }
    s
}

/// Uninstall a package completely.
pub fn uninstall(cfg: &Config, inst: &Installed, verbose: bool) -> Result<(), String> {
    let vdb = Vdb::load(&cfg.vdb_path());
    let tmp = cfg.var("PORTAGE_TMPDIR");
    let work = PathBuf::from(if tmp.is_empty() { "/var/tmp" } else { tmp }).join("spas").join(&inst.cat).join(format!("{}-unmerge", inst.pf()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    info(&format!("Unmerging {}", crate::util::bold(&inst.cpv())));
    run_installed_phases(cfg, inst, "prerm", None, &work);
    let mut canon = Canon::new();
    let others = other_owned(cfg, &vdb, &[inst.cpv()], &mut canon);
    unmerge_files(cfg, &inst.contents(), &HashSet::new(), &others, &mut canon, verbose);
    run_installed_phases(cfg, inst, "postrm", None, &work);
    std::fs::remove_dir_all(&inst.dir).map_err(|e| format!("{}: {e}", inst.dir.display()))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}
