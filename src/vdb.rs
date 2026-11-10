//! The installed package database (/var/db/pkg), shared with Portage.

use crate::atom::{split_pv, Atom, MatchTarget};
use crate::version::Version;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Installed {
    pub cat: String,
    pub pn: String,
    pub ver: Version,
    pub dir: PathBuf,
    pub slot: String,
    pub subslot: String,
    pub repo: String,
    pub use_: HashSet<String>,
    pub iuse: HashSet<String>,
    /// IUSE as declared by the ebuild (without implicit flags)
    pub iuse_declared: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Dir(String),
    Obj { path: String, md5: String, mtime: String },
    Sym { path: String, target: String, mtime: String },
    Fif(String),
    Dev(String),
}

impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Entry::Dir(p) | Entry::Fif(p) | Entry::Dev(p) => p,
            Entry::Obj { path, .. } | Entry::Sym { path, .. } => path,
        }
    }
    pub fn line(&self) -> String {
        match self {
            Entry::Dir(p) => format!("dir {p}"),
            Entry::Obj { path, md5, mtime } => format!("obj {path} {md5} {mtime}"),
            Entry::Sym { path, target, mtime } => format!("sym {path} -> {target} {mtime}"),
            Entry::Fif(p) => format!("fif {p}"),
            Entry::Dev(p) => format!("dev {p}"),
        }
    }
}

pub fn parse_contents(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for l in text.lines() {
        let Some((kind, rest)) = l.split_once(' ') else { continue };
        match kind {
            "dir" => out.push(Entry::Dir(rest.to_string())),
            "fif" => out.push(Entry::Fif(rest.to_string())),
            "dev" => out.push(Entry::Dev(rest.to_string())),
            "obj" => {
                let mut it = rest.rsplitn(3, ' ');
                let (Some(mtime), Some(md5), Some(path)) = (it.next(), it.next(), it.next()) else { continue };
                out.push(Entry::Obj { path: path.into(), md5: md5.into(), mtime: mtime.into() });
            }
            "sym" => {
                let Some((path, rest)) = rest.split_once(" -> ") else { continue };
                let Some((target, mtime)) = rest.rsplit_once(' ') else { continue };
                out.push(Entry::Sym { path: path.into(), target: target.into(), mtime: mtime.into() });
            }
            _ => {}
        }
    }
    out
}

impl Installed {
    pub fn pf(&self) -> String {
        format!("{}-{}", self.pn, self.ver)
    }
    pub fn cpv(&self) -> String {
        format!("{}/{}-{}", self.cat, self.pn, self.ver)
    }
    pub fn cp(&self) -> String {
        format!("{}/{}", self.cat, self.pn)
    }
    pub fn read(&self, key: &str) -> String {
        std::fs::read_to_string(self.dir.join(key)).map(|s| s.trim_end().to_string()).unwrap_or_default()
    }
    pub fn target(&self) -> MatchTarget<'_> {
        MatchTarget {
            cat: &self.cat,
            pn: &self.pn,
            version: &self.ver,
            slot: &self.slot,
            subslot: &self.subslot,
            repo: &self.repo,
            iuse: Some(&self.iuse),
            use_: Some(&self.use_),
        }
    }
    pub fn contents(&self) -> Vec<Entry> {
        parse_contents(&std::fs::read_to_string(self.dir.join("CONTENTS")).unwrap_or_default())
    }
}

pub struct Vdb {
    pub pkgs: Vec<Installed>,
}

impl Vdb {
    pub fn load(path: &Path) -> Vdb {
        let mut pkgs = Vec::new();
        if let Ok(cats) = std::fs::read_dir(path) {
            for c in cats.flatten() {
                let cat = c.file_name().to_string_lossy().to_string();
                if cat.starts_with('.') || !c.path().is_dir() {
                    continue;
                }
                let Ok(ps) = std::fs::read_dir(c.path()) else { continue };
                for p in ps.flatten() {
                    let pf = p.file_name().to_string_lossy().to_string();
                    if pf.starts_with('-') || pf.starts_with('.') {
                        continue;
                    }
                    if let Some(i) = load_one(&cat, &pf, &p.path()) {
                        pkgs.push(i);
                    }
                }
            }
        }
        pkgs.sort_by(|a, b| a.cp().cmp(&b.cp()).then(a.ver.cmp(&b.ver)));
        Vdb { pkgs }
    }

    pub fn matching(&self, a: &Atom, parent_use: Option<&HashSet<String>>) -> Vec<&Installed> {
        self.pkgs.iter().filter(|p| a.matches(&p.target(), parent_use)).collect()
    }

    pub fn matching_nouse(&self, a: &Atom) -> Vec<&Installed> {
        self.pkgs.iter().filter(|p| a.matches_nouse(&p.target())).collect()
    }

    pub fn by_cp_slot(&self, cp: &str, slot: &str) -> Option<&Installed> {
        self.pkgs.iter().find(|p| p.cp() == cp && p.slot == slot)
    }

}

fn load_one(cat: &str, pf: &str, dir: &Path) -> Option<Installed> {
    let (pn, ver) = split_pv(pf)?;
    let rd = |k: &str| std::fs::read_to_string(dir.join(k)).map(|s| s.trim().to_string()).unwrap_or_default();
    let slotfull = rd("SLOT");
    let (slot, subslot) = match slotfull.split_once('/') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => {
            let s = if slotfull.is_empty() { "0".to_string() } else { slotfull };
            (s.clone(), s)
        }
    };
    let iuse_raw = {
        let e = rd("IUSE_EFFECTIVE");
        if e.is_empty() { rd("IUSE") } else { e }
    };
    let iuse = iuse_raw.split_whitespace().map(|s| s.trim_start_matches(['+', '-']).to_string()).collect();
    Some(Installed {
        cat: cat.to_string(),
        pn: pn.to_string(),
        ver,
        dir: dir.to_path_buf(),
        slot,
        subslot,
        repo: rd("repository"),
        use_: rd("USE").split_whitespace().map(|s| s.to_string()).collect(),
        iuse,
        iuse_declared: rd("IUSE").split_whitespace().map(|s| s.trim_start_matches(['+', '-']).to_string()).collect(),
    })
}

/// Next value of Portage's global merge counter.
pub fn next_counter(root: &Path) -> u64 {
    let p = root.join("var/cache/edb/counter");
    let cur: u64 = std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let n = cur + 1;
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(&p, n.to_string());
    n
}

pub fn read_world(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

pub fn write_world(path: &Path, entries: &[String]) -> std::io::Result<()> {
    let mut v: Vec<String> = entries.to_vec();
    v.sort();
    v.dedup();
    let tmp = path.with_extension("hoard-new");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&tmp, v.iter().map(|s| format!("{s}\n")).collect::<String>())?;
    std::fs::rename(tmp, path)
}
