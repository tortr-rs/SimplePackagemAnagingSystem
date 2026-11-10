//! Ebuild repositories: repos.conf, layout.conf, the md5-dict metadata cache.

use crate::atom::{Atom, MatchTarget};
use crate::util::{md5_file, read_lines_dir};
use crate::version::Version;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const SUPPORTED_EAPIS: &[&str] = &["7", "8", "9"];

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub map: HashMap<String, String>,
}

impl Meta {
    pub fn get(&self, k: &str) -> &str {
        self.map.get(k).map(|s| s.as_str()).unwrap_or("")
    }
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
    pub location: PathBuf,
    pub masters: Vec<String>,
    pub priority: i32,
    pub sync_type: String,
    pub sync_uri: String,
    pub settings: HashMap<String, String>,
    /// eclass search path, lowest priority first (masters, then this repo)
    pub eclass_dirs: Vec<PathBuf>,
}

pub struct Ebuild {
    pub cat: String,
    pub pn: String,
    pub ver: Version,
    pub repo: usize,
    pub repo_name: String,
    pub path: PathBuf,
    pub meta: Meta,
}

impl Ebuild {
    pub fn pf(&self) -> String {
        format!("{}-{}", self.pn, self.ver)
    }
    pub fn cpv(&self) -> String {
        format!("{}/{}-{}", self.cat, self.pn, self.ver)
    }
    pub fn cp(&self) -> String {
        format!("{}/{}", self.cat, self.pn)
    }
    pub fn slot(&self) -> &str {
        let s = self.meta.get("SLOT");
        let s = s.split('/').next().unwrap_or("0");
        if s.is_empty() { "0" } else { s }
    }
    pub fn subslot(&self) -> &str {
        let s = self.meta.get("SLOT");
        match s.split_once('/') {
            Some((_, b)) => b,
            None => self.slot(),
        }
    }
    pub fn target<'a>(
        &'a self,
        iuse: Option<&'a std::collections::HashSet<String>>,
        use_: Option<&'a std::collections::HashSet<String>>,
    ) -> MatchTarget<'a> {
        MatchTarget {
            cat: &self.cat,
            pn: &self.pn,
            version: &self.ver,
            slot: self.slot(),
            subslot: self.subslot(),
            repo: &self.repo_name,
            iuse,
            use_,
        }
    }
}

pub struct Repos {
    pub list: Vec<Repo>,
    pub main: String,
    eclass_md5: RefCell<HashMap<PathBuf, Option<String>>>,
    cache: RefCell<HashMap<(String, String), Rc<Vec<Rc<Ebuild>>>>>,
    categories: RefCell<Option<Vec<String>>>,
    /// set when metadata had to be generated, so we can say so once
    pub regen_note: RefCell<bool>,
}

fn parse_ini(text: &str) -> BTreeMap<String, HashMap<String, String>> {
    let mut out: BTreeMap<String, HashMap<String, String>> = BTreeMap::new();
    let mut sect = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            sect = l[1..l.len() - 1].trim().to_string();
            out.entry(sect.clone()).or_default();
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            out.entry(sect.clone()).or_default().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn layout_conf(loc: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(t) = std::fs::read_to_string(loc.join("metadata/layout.conf")) {
        for line in t.lines() {
            let l = line.trim();
            if l.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                m.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    m
}

impl Repos {
    pub fn load(config_root: &Path) -> Result<Repos, String> {
        let mut sections: BTreeMap<String, HashMap<String, String>> = BTreeMap::new();
        // built-in default for the gentoo repository, same as Portage ships
        sections.insert(
            "DEFAULT".into(),
            [("main-repo".to_string(), "gentoo".to_string())].into_iter().collect(),
        );
        sections.insert(
            "gentoo".into(),
            [
                ("location", "/var/db/repos/gentoo"),
                ("sync-type", "rsync"),
                ("sync-uri", "rsync://rsync.gentoo.org/gentoo-portage"),
                ("sync-openpgp-key-path", "/usr/share/openpgp-keys/gentoo-release.asc"),
            ]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        );
        let rc = config_root.join("etc/portage/repos.conf");
        let mut texts = Vec::new();
        if rc.is_dir() {
            for f in crate::util::sorted_config_files(&rc) {
                texts.push(std::fs::read_to_string(&f).unwrap_or_default());
            }
        } else if rc.is_file() {
            texts.push(std::fs::read_to_string(&rc).unwrap_or_default());
        }
        for t in texts {
            for (s, kv) in parse_ini(&t) {
                sections.entry(s).or_default().extend(kv);
            }
        }
        let main = sections.get("DEFAULT").and_then(|d| d.get("main-repo")).cloned().unwrap_or("gentoo".into());
        let mut list = Vec::new();
        for (sect, kv) in &sections {
            if sect == "DEFAULT" {
                continue;
            }
            let Some(loc) = kv.get("location") else { continue };
            let location = PathBuf::from(loc);
            if !location.join("profiles").is_dir() {
                continue;
            }
            let name = std::fs::read_to_string(location.join("profiles/repo_name"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| sect.clone());
            let layout = layout_conf(&location);
            let masters = match layout.get("masters") {
                Some(m) => m.split_whitespace().map(|s| s.to_string()).collect(),
                None if name != main => vec![main.clone()],
                None => vec![],
            };
            let priority = kv
                .get("priority")
                .and_then(|p| p.parse().ok())
                .unwrap_or(if name == main { -1000 } else { 0 });
            list.push(Repo {
                name,
                location,
                masters,
                priority,
                sync_type: kv.get("sync-type").cloned().unwrap_or_default(),
                sync_uri: kv.get("sync-uri").cloned().unwrap_or_default(),
                settings: kv.clone(),
                eclass_dirs: vec![],
            });
        }
        if list.is_empty() {
            return Err("no ebuild repositories found (is /var/db/repos/gentoo synced?)".into());
        }
        list.sort_by_key(|r| r.priority);
        // eclass search paths
        let snapshot: Vec<(String, PathBuf, Vec<String>)> =
            list.iter().map(|r| (r.name.clone(), r.location.clone(), r.masters.clone())).collect();
        for r in list.iter_mut() {
            let mut dirs = Vec::new();
            for m in &r.masters {
                if let Some((_, loc, _)) = snapshot.iter().find(|(n, _, _)| n == m) {
                    dirs.push(loc.join("eclass"));
                }
            }
            dirs.push(r.location.join("eclass"));
            r.eclass_dirs = dirs;
        }
        Ok(Repos {
            list,
            main,
            eclass_md5: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            categories: RefCell::new(None),
            regen_note: RefCell::new(false),
        })
    }

    pub fn by_name(&self, n: &str) -> Option<&Repo> {
        self.list.iter().find(|r| r.name == n)
    }

    pub fn categories(&self) -> Vec<String> {
        if let Some(c) = &*self.categories.borrow() {
            return c.clone();
        }
        let mut cats: Vec<String> = Vec::new();
        for r in &self.list {
            for l in read_lines_dir(&r.location.join("profiles/categories")) {
                if !cats.contains(&l) {
                    cats.push(l);
                }
            }
        }
        cats.sort();
        *self.categories.borrow_mut() = Some(cats.clone());
        cats
    }

    /// All package names (cat/pn) across repositories.
    pub fn all_packages(&self) -> Vec<(String, String)> {
        let mut out = std::collections::BTreeSet::new();
        for cat in self.categories() {
            for r in &self.list {
                if let Ok(rd) = std::fs::read_dir(r.location.join(&cat)) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().to_string();
                        if e.path().is_dir() && !n.starts_with('.') {
                            out.insert((cat.clone(), n));
                        }
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// Find categories that contain a package called `pn`.
    pub fn categories_for(&self, pn: &str) -> Vec<String> {
        let mut out = Vec::new();
        for cat in self.categories() {
            if self.list.iter().any(|r| r.location.join(&cat).join(pn).is_dir()) {
                out.push(cat);
            }
        }
        out
    }

    fn eclass_md5(&self, p: &Path) -> Option<String> {
        if let Some(v) = self.eclass_md5.borrow().get(p) {
            return v.clone();
        }
        let v = md5_file(p).ok();
        self.eclass_md5.borrow_mut().insert(p.to_path_buf(), v.clone());
        v
    }

    pub fn find_eclass(&self, repo: &Repo, name: &str) -> Option<PathBuf> {
        for d in repo.eclass_dirs.iter().rev() {
            let p = d.join(format!("{name}.eclass"));
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    /// Every ebuild for cat/pn in every repo, newest first (ties: higher priority repo first).
    pub fn ebuilds(&self, cat: &str, pn: &str) -> Rc<Vec<Rc<Ebuild>>> {
        let key = (cat.to_string(), pn.to_string());
        if let Some(v) = self.cache.borrow().get(&key) {
            return v.clone();
        }
        let mut out: Vec<Rc<Ebuild>> = Vec::new();
        for (ri, r) in self.list.iter().enumerate() {
            let dir = r.location.join(cat).join(pn);
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            let mut files: Vec<String> =
                rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".ebuild")).collect();
            files.sort();
            for f in files {
                let pf = &f[..f.len() - 7];
                let Some((fpn, ver)) = crate::atom::split_pv(pf) else { continue };
                if fpn != pn {
                    continue;
                }
                let path = dir.join(&f);
                match self.metadata(r, cat, pf, &path) {
                    Ok(meta) => out.push(Rc::new(Ebuild {
                        cat: cat.to_string(),
                        pn: pn.to_string(),
                        ver,
                        repo: ri,
                        repo_name: r.name.clone(),
                        path,
                        meta,
                    })),
                    Err(e) => crate::util::warn(&format!("{cat}/{pf}::{}: {e}", r.name)),
                }
            }
        }
        out.sort_by(|a, b| b.ver.cmp(&a.ver).then_with(|| self.list[b.repo].priority.cmp(&self.list[a.repo].priority)));
        let rc = Rc::new(out);
        self.cache.borrow_mut().insert(key, rc.clone());
        rc
    }

    pub fn matching(&self, atom: &Atom) -> Vec<Rc<Ebuild>> {
        self.ebuilds(&atom.cat, &atom.pn).iter().filter(|e| atom.matches_nouse(&e.target(None, None))).cloned().collect()
    }

    fn read_cache_file(p: &Path) -> Option<Meta> {
        let t = std::fs::read_to_string(p).ok()?;
        let mut map = HashMap::new();
        for line in t.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.to_string(), v.to_string());
            }
        }
        Some(Meta { map })
    }

    fn cache_valid(&self, repo: &Repo, meta: &Meta, ebuild: &Path) -> bool {
        let Ok(m) = md5_file(ebuild) else { return false };
        if meta.get("_md5_") != m {
            return false;
        }
        let ecl = meta.get("_eclasses_");
        let parts: Vec<&str> = ecl.split('\t').filter(|s| !s.is_empty()).collect();
        for pair in parts.chunks(2) {
            if pair.len() != 2 {
                return false;
            }
            let Some(p) = self.find_eclass(repo, pair[0]) else { return false };
            if self.eclass_md5(&p).as_deref() != Some(pair[1]) {
                return false;
            }
        }
        true
    }

    fn metadata(&self, repo: &Repo, cat: &str, pf: &str, ebuild: &Path) -> Result<Meta, String> {
        let repo_cache = repo.location.join("metadata/md5-cache").join(cat).join(pf);
        let own_cache = PathBuf::from("/var/cache/hoard/md5-cache").join(&repo.name).join(cat).join(pf);
        for c in [&repo_cache, &own_cache] {
            if let Some(mut m) = Self::read_cache_file(c) {
                if self.cache_valid(repo, &m, ebuild) {
                    finish_meta(&mut m);
                    return Ok(m);
                }
            }
        }
        // stale or missing: source the ebuild ourselves
        if !*self.regen_note.borrow() {
            *self.regen_note.borrow_mut() = true;
            crate::util::info("generating metadata for ebuilds without a valid cache entry...");
        }
        let mut m = crate::ebuild_env::generate_metadata(self, repo, cat, pf, ebuild)?;
        // store it in our own cache when we can
        let mut text = String::new();
        let mut keys: Vec<&String> = m.map.keys().collect();
        keys.sort();
        for k in keys {
            text.push_str(&format!("{k}={}\n", m.map[k]));
        }
        if let Some(parent) = own_cache.parent() {
            if std::fs::create_dir_all(parent).is_ok() {
                let _ = std::fs::write(&own_cache, text);
            }
        }
        finish_meta(&mut m);
        Ok(m)
    }

    /// thirdpartymirrors: name -> list of base URLs
    pub fn third_party_mirrors(&self) -> HashMap<String, Vec<String>> {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for r in &self.list {
            for l in read_lines_dir(&r.location.join("profiles/thirdpartymirrors")) {
                let mut it = l.split_whitespace();
                if let Some(name) = it.next() {
                    m.insert(name.to_string(), it.map(|s| s.to_string()).collect());
                }
            }
        }
        m
    }
}

/// Fill INHERITED (eclass list) from the cache's _eclasses_ field.
fn finish_meta(m: &mut Meta) {
    if !m.map.contains_key("INHERITED") {
        let ecl = m.get("_eclasses_").to_string();
        let names: Vec<&str> = ecl.split('\t').step_by(2).filter(|s| !s.is_empty()).collect();
        m.map.insert("INHERITED".into(), names.join(" "));
    }
    if m.get("EAPI").is_empty() {
        m.map.insert("EAPI".into(), "0".into());
    }
}
