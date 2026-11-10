//! Configuration: the profile stack, make.conf, /etc/portage/package.*, USE
//! calculation and package visibility (EAPI, masks, keywords, licenses).
//!
//! hoard reads the same configuration files Portage does, so both can manage
//! the same system.

use crate::atom::Atom;
use crate::depspec;
use crate::repo::{Ebuild, Repos, SUPPORTED_EAPIS};
use crate::shellvars;
use crate::util::{read_lines_dir, sorted_config_files, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Built-in defaults (what Portage keeps in make.globals).
const GLOBALS: &str = r#"
CFLAGS=""
CXXFLAGS=""
LDFLAGS=""
FFLAGS=""
FCFLAGS=""
GENTOO_MIRRORS="http://distfiles.gentoo.org"
ACCEPT_PROPERTIES="*"
ACCEPT_RESTRICT="*"
DISTDIR="/var/cache/distfiles"
PKGDIR="/var/cache/binpkgs"
PORTAGE_TMPDIR="/var/tmp"
FEATURES="config-protect-if-modified network-sandbox protect-owned sandbox strict userpriv usersandbox"
COLLISION_IGNORE="/lib/modules/* *.py[co] *$py.class"
UNINSTALL_IGNORE="/lib/modules/*"
PORTAGE_COMPRESS="bzip2"
PORTAGE_COMPRESS_FLAGS="-9"
PORTAGE_COMPRESS_EXCLUDE_SUFFIXES="css gif htm[l]? jp[e]?g js pdf png"
CONFIG_PROTECT="/etc"
CONFIG_PROTECT_MASK="/etc/env.d"
"#;

const BASE_INCREMENTALS: &[&str] = &[
    "USE", "USE_EXPAND", "USE_EXPAND_HIDDEN", "USE_EXPAND_IMPLICIT", "USE_EXPAND_UNPREFIXED", "IUSE_IMPLICIT",
    "FEATURES", "ACCEPT_KEYWORDS", "ACCEPT_LICENSE", "ACCEPT_PROPERTIES", "ACCEPT_RESTRICT", "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK", "PROFILE_ONLY_VARIABLES", "ENV_UNSET", "BOOTSTRAP_USE",
];

/// USE state from the global layers: flags switched on/off, and whether `-*` was seen.
#[derive(Default, Clone)]
pub struct UseState {
    pub reset: bool,
    pub flags: Vec<(String, bool)>,
}

impl UseState {
    fn apply(&mut self, tok: &str) {
        if tok == "-*" {
            self.reset = true;
            self.flags.clear();
        } else if let Some(f) = tok.strip_prefix('-') {
            self.set(f, false);
        } else if !tok.is_empty() {
            self.set(tok, true);
        }
    }
    fn set(&mut self, f: &str, on: bool) {
        if let Some(x) = self.flags.iter_mut().find(|(n, _)| n == f) {
            x.1 = on;
        } else {
            self.flags.push((f.to_string(), on));
        }
    }
}

pub struct UseInfo {
    /// IUSE names as declared (without +/-)
    pub iuse: Vec<String>,
    pub effective: HashSet<String>,
    pub enabled: HashSet<String>,
    pub forced: HashSet<String>,
    pub masked: HashSet<String>,
}

type AtomList = Vec<(Atom, Vec<String>)>;

pub struct Config {
    pub root: PathBuf,
    pub config_root: PathBuf,
    pub repos: Repos,
    /// final value of every variable (incrementals joined with spaces)
    pub vars: HashMap<String, String>,
    #[allow(dead_code)]
    pub profile_dirs: Vec<PathBuf>,
    pub system: Vec<Atom>,
    pub arch: String,
    pub features: HashSet<String>,
    pub bashrcs: Vec<PathBuf>,
    global_use: UseState,
    env_use: Vec<String>,
    implicit: HashSet<String>,
    use_expand: Vec<String>,
    masks: Vec<(Atom, String)>,
    unmasks: Vec<Atom>,
    pkg_use_profile: AtomList,
    pkg_use_user: AtomList,
    use_force: Vec<String>,
    use_mask: Vec<String>,
    use_stable_force: Vec<String>,
    use_stable_mask: Vec<String>,
    pkg_use_force: AtomList,
    pkg_use_mask: AtomList,
    pkg_use_stable_force: AtomList,
    pkg_use_stable_mask: AtomList,
    accept_keywords: Vec<String>,
    pkg_keywords: AtomList,
    accept_license: Vec<String>,
    pkg_license: AtomList,
    license_groups: HashMap<String, Vec<String>>,
    pkg_env: AtomList,
    pub provided: Vec<Atom>,
}

fn incr_apply(list: &mut Vec<String>, value: &str) {
    for t in value.split_whitespace() {
        if t == "-*" {
            list.clear();
        } else if let Some(x) = t.strip_prefix('-') {
            list.retain(|y| y != x);
        } else if !list.iter().any(|y| y == t) {
            list.push(t.to_string());
        }
    }
}

/// Incremental token list where "-x" cancels an earlier "x" (use.mask, package.mask...).
fn incr_tokens(list: &mut Vec<String>, toks: impl IntoIterator<Item = String>) {
    for t in toks {
        if let Some(x) = t.strip_prefix('-') {
            if x == "*" {
                list.clear();
            } else {
                list.retain(|y| y != x);
            }
        } else if !list.contains(&t) {
            list.push(t);
        }
    }
}

/// "atom tok tok VAR: val val" lines, with USE_EXPAND prefixes expanded.
fn atom_lines(path: &Path, expand_use: bool) -> AtomList {
    let mut out = Vec::new();
    for l in read_lines_dir(path) {
        let mut it = l.split_whitespace();
        let Some(a) = it.next() else { continue };
        let atom = match Atom::parse(a) {
            Ok(x) => x,
            Err(e) => {
                warn(&format!("{}: {e}", path.display()));
                continue;
            }
        };
        let mut toks = Vec::new();
        let mut prefix: Option<String> = None;
        for t in it {
            if expand_use {
                if let Some(v) = t.strip_suffix(':') {
                    prefix = Some(v.to_lowercase());
                    continue;
                }
            }
            match &prefix {
                Some(p) => match t.strip_prefix('-') {
                    Some(x) => toks.push(format!("-{p}_{x}")),
                    None => toks.push(format!("{p}_{t}")),
                },
                None => toks.push(t.to_string()),
            }
        }
        out.push((atom, toks));
    }
    out
}

fn atoms_incremental(path: &Path, list: &mut Vec<(Atom, String)>) {
    for l in read_lines_dir(path) {
        if let Some(x) = l.strip_prefix('-') {
            list.retain(|(a, _)| a.to_string() != x);
            continue;
        }
        match Atom::parse(&l) {
            Ok(a) => list.push((a, path.display().to_string())),
            Err(e) => warn(&format!("{}: {e}", path.display())),
        }
    }
}

impl Config {
    pub fn load(root: &Path) -> Result<Config, String> {
        let config_root = PathBuf::from("/");
        let repos = Repos::load(&config_root)?;
        let etc = config_root.join("etc/portage");

        // ---- profile stack
        let mut profile_dirs = Vec::new();
        let mp = etc.join("make.profile");
        let mp = std::fs::canonicalize(&mp)
            .map_err(|_| "no profile selected: /etc/portage/make.profile is missing".to_string())?;
        walk_profile(&mp, &repos, &mut profile_dirs, 0)?;
        let user_profile = etc.join("profile");
        if user_profile.is_dir() {
            profile_dirs.push(user_profile);
        }

        // ---- variable layers
        let mut layers: Vec<shellvars::Vars> = Vec::new();
        let empty = HashMap::new();
        layers.push(shellvars::parse_str(GLOBALS, &empty)?);
        // gather layers while keeping a running view for ${VAR} references
        let mut running: HashMap<String, String> = HashMap::new();
        let push_layer = |layers: &mut Vec<shellvars::Vars>, v: shellvars::Vars, running: &mut HashMap<String, String>| {
            for (k, val) in &v {
                if BASE_INCREMENTALS.contains(&k.as_str()) {
                    let e = running.entry(k.clone()).or_default();
                    *e = format!("{e} {val}");
                } else {
                    running.insert(k.clone(), val.clone());
                }
            }
            layers.push(v);
        };
        let first = layers.pop().unwrap();
        push_layer(&mut layers, first, &mut running);
        for d in &profile_dirs {
            let f = d.join("make.defaults");
            if f.is_file() {
                let v = shellvars::parse_file(&f, &running)?;
                push_layer(&mut layers, v, &mut running);
            }
        }
        // CONFIG_PROTECT(_MASK) contributions from env.d
        let mut envd = Vec::new();
        for f in sorted_config_files(&root.join("etc/env.d")) {
            if let Ok(v) = shellvars::parse_file(&f, &empty) {
                for (k, val) in v {
                    if k == "CONFIG_PROTECT" || k == "CONFIG_PROTECT_MASK" {
                        envd.push((k, val));
                    }
                }
            }
        }
        push_layer(&mut layers, envd, &mut running);
        let user_layers_start = layers.len();
        let mut user_reset: HashSet<String> = HashSet::new();
        let mc = etc.join("make.conf");
        let mc_files = if mc.is_dir() { sorted_config_files(&mc) } else if mc.is_file() { vec![mc] } else { vec![] };
        for f in mc_files {
            let v = shellvars::parse_file(&f, &running)?;
            push_layer(&mut layers, v, &mut running);
        }
        let mut env_layer = Vec::new();
        let mut env_use = Vec::new();
        for k in ["USE", "FEATURES", "ACCEPT_KEYWORDS", "MAKEOPTS", "ACCEPT_LICENSE"] {
            if let Ok(v) = std::env::var(k) {
                if k == "USE" {
                    env_use = v.split_whitespace().map(|s| s.to_string()).collect();
                }
                env_layer.push((k.to_string(), v));
            }
        }
        push_layer(&mut layers, env_layer, &mut running);

        // which variables are USE_EXPAND (needed to know what else is incremental)
        let mut use_expand = Vec::new();
        for l in &layers {
            for (k, v) in l {
                if k == "USE_EXPAND" {
                    incr_apply(&mut use_expand, v);
                }
            }
        }
        let mut incrementals: HashSet<String> = BASE_INCREMENTALS.iter().map(|s| s.to_string()).collect();
        incrementals.extend(use_expand.iter().cloned());

        let mut vars: HashMap<String, String> = HashMap::new();
        let mut incr: HashMap<String, Vec<String>> = HashMap::new();
        let mut global_use = UseState::default();
        for (li, l) in layers.iter().enumerate() {
            for (k, v) in l {
                // a USE_EXPAND variable set by the user replaces the profile's values
                if li >= user_layers_start && use_expand.contains(k) && !user_reset.contains(k) {
                    user_reset.insert(k.clone());
                    incr.remove(k);
                }
                if k == "USE" {
                    for t in v.split_whitespace() {
                        global_use.apply(t);
                    }
                }
                if incrementals.contains(k) {
                    incr_apply(incr.entry(k.clone()).or_default(), v);
                } else {
                    vars.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, v) in &incr {
            vars.insert(k.clone(), v.join(" "));
        }
        let arch = vars.get("ARCH").cloned().unwrap_or_default();
        // USE_EXPAND values become flags
        for ue in &use_expand {
            if let Some(vals) = incr.get(ue) {
                let p = ue.to_lowercase();
                for v in vals {
                    global_use.set(&format!("{p}_{v}"), true);
                }
            }
        }
        let unprefixed: Vec<String> = incr.get("USE_EXPAND_UNPREFIXED").cloned().unwrap_or_default();
        for ue in &unprefixed {
            if let Some(v) = vars.get(ue) {
                for t in v.split_whitespace() {
                    global_use.set(t, true);
                }
            }
        }
        // implicit IUSE
        let mut implicit: HashSet<String> = incr.get("IUSE_IMPLICIT").cloned().unwrap_or_default().into_iter().collect();
        for v in incr.get("USE_EXPAND_IMPLICIT").cloned().unwrap_or_default() {
            let values = vars.get(&format!("USE_EXPAND_VALUES_{v}")).cloned().unwrap_or_default();
            if unprefixed.contains(&v) {
                implicit.extend(values.split_whitespace().map(|s| s.to_string()));
            } else if use_expand.contains(&v) {
                let p = v.to_lowercase();
                implicit.extend(values.split_whitespace().map(|s| format!("{p}_{s}")));
            }
        }
        let features: HashSet<String> = incr.get("FEATURES").cloned().unwrap_or_default().into_iter().collect();

        // ---- profile files
        let mut system: Vec<String> = Vec::new();
        let mut masks: Vec<(Atom, String)> = Vec::new();
        let mut unmasks: Vec<Atom> = Vec::new();
        let mut use_force = Vec::new();
        let mut use_mask = Vec::new();
        let mut use_stable_force = Vec::new();
        let mut use_stable_mask = Vec::new();
        let mut pkg_use_profile = Vec::new();
        let mut pkg_use_force = Vec::new();
        let mut pkg_use_mask = Vec::new();
        let mut pkg_use_stable_force = Vec::new();
        let mut pkg_use_stable_mask = Vec::new();
        let mut pkg_keywords = Vec::new();
        let mut provided = Vec::new();
        let mut bashrcs = Vec::new();
        for r in &repos.list {
            atoms_incremental(&r.location.join("profiles/package.mask"), &mut masks);
        }
        for d in &profile_dirs {
            for l in read_lines_dir(&d.join("packages")) {
                if let Some(a) = l.strip_prefix("-*") {
                    system.retain(|x| x != a);
                } else if let Some(a) = l.strip_prefix('*') {
                    if !system.iter().any(|x| x == a) {
                        system.push(a.to_string());
                    }
                }
            }
            atoms_incremental(&d.join("package.mask"), &mut masks);
            for l in read_lines_dir(&d.join("package.unmask")) {
                if let Ok(a) = Atom::parse(&l) {
                    unmasks.push(a);
                }
            }
            let toks = |f: &str| read_lines_dir(&d.join(f)).into_iter().flat_map(|l| {
                l.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>()
            }).collect::<Vec<String>>();
            incr_tokens(&mut use_force, toks("use.force"));
            incr_tokens(&mut use_mask, toks("use.mask"));
            incr_tokens(&mut use_stable_force, toks("use.stable.force"));
            incr_tokens(&mut use_stable_mask, toks("use.stable.mask"));
            pkg_use_profile.extend(atom_lines(&d.join("package.use"), true));
            pkg_use_force.extend(atom_lines(&d.join("package.use.force"), false));
            pkg_use_mask.extend(atom_lines(&d.join("package.use.mask"), false));
            pkg_use_stable_force.extend(atom_lines(&d.join("package.use.stable.force"), false));
            pkg_use_stable_mask.extend(atom_lines(&d.join("package.use.stable.mask"), false));
            pkg_keywords.extend(atom_lines(&d.join("package.accept_keywords"), false));
            for l in read_lines_dir(&d.join("package.provided")) {
                if let Ok(a) = Atom::parse(&format!("={l}")) {
                    provided.push(a);
                }
            }
            if d.join("profile.bashrc").is_file() {
                bashrcs.push(d.join("profile.bashrc"));
            }
            if d.join("bashrc").is_file() {
                bashrcs.push(d.join("bashrc"));
            }
        }
        let system: Vec<Atom> = system.iter().filter_map(|s| Atom::parse(s).ok()).collect();

        // ---- user configuration
        atoms_incremental(&etc.join("package.mask"), &mut masks);
        for l in read_lines_dir(&etc.join("package.unmask")) {
            match Atom::parse(&l) {
                Ok(a) => unmasks.push(a),
                Err(e) => warn(&format!("package.unmask: {e}")),
            }
        }
        let pkg_use_user = atom_lines(&etc.join("package.use"), true);
        pkg_keywords.extend(atom_lines(&etc.join("package.keywords"), false));
        pkg_keywords.extend(atom_lines(&etc.join("package.accept_keywords"), false));
        let pkg_license = atom_lines(&etc.join("package.license"), false);
        let pkg_env = atom_lines(&etc.join("package.env"), false);
        if etc.join("bashrc").is_file() {
            bashrcs.push(etc.join("bashrc"));
        }

        let mut license_groups: HashMap<String, Vec<String>> = HashMap::new();
        for r in &repos.list {
            for l in read_lines_dir(&r.location.join("profiles/license_groups")) {
                let mut it = l.split_whitespace();
                if let Some(g) = it.next() {
                    license_groups.entry(g.to_string()).or_default().extend(it.map(|s| s.to_string()));
                }
            }
        }

        Ok(Config {
            root: root.to_path_buf(),
            config_root,
            repos,
            accept_keywords: incr.get("ACCEPT_KEYWORDS").cloned().unwrap_or_default(),
            accept_license: incr.get("ACCEPT_LICENSE").cloned().unwrap_or_default(),
            vars,
            profile_dirs,
            system,
            arch,
            features,
            bashrcs,
            global_use,
            env_use,
            implicit,
            use_expand,
            masks,
            unmasks,
            pkg_use_profile,
            pkg_use_user,
            use_force,
            use_mask,
            use_stable_force,
            use_stable_mask,
            pkg_use_force,
            pkg_use_mask,
            pkg_use_stable_force,
            pkg_use_stable_mask,
            pkg_keywords,
            pkg_license,
            license_groups,
            pkg_env,
            provided,
        })
    }

    pub fn var(&self, k: &str) -> &str {
        self.vars.get(k).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn feature(&self, f: &str) -> bool {
        self.features.contains(f)
    }

    pub fn use_expand_vars(&self) -> &[String] {
        &self.use_expand
    }

    fn matches(a: &Atom, e: &Ebuild) -> bool {
        a.matches_nouse(&e.target(None, None))
    }

    /// Stable keyword for our arch present -> the use.stable.* files apply.
    fn is_stable(&self, e: &Ebuild) -> bool {
        e.meta.get("KEYWORDS").split_whitespace().any(|k| k == self.arch)
    }

    pub fn pkg_use(&self, e: &Ebuild) -> UseInfo {
        let mut iuse = Vec::new();
        let mut state: HashMap<String, bool> = HashMap::new();
        for t in e.meta.get("IUSE").split_whitespace() {
            let (name, def) = if let Some(n) = t.strip_prefix('+') {
                (n, Some(true))
            } else if let Some(n) = t.strip_prefix('-') {
                (n, Some(false))
            } else {
                (t, None)
            };
            iuse.push(name.to_string());
            if def == Some(true) && !self.global_use.reset {
                state.insert(name.to_string(), true);
            }
        }
        for (f, on) in &self.global_use.flags {
            state.insert(f.clone(), *on);
        }
        let apply = |state: &mut HashMap<String, bool>, toks: &[String]| {
            for t in toks {
                if t == "-*" {
                    state.clear();
                } else if let Some(f) = t.strip_prefix('-') {
                    state.insert(f.to_string(), false);
                } else {
                    state.insert(t.clone(), true);
                }
            }
        };
        for (a, toks) in self.pkg_use_profile.iter().chain(self.pkg_use_user.iter()) {
            if Self::matches(a, e) {
                apply(&mut state, toks);
            }
        }
        apply(&mut state, &self.env_use);

        let mut effective: HashSet<String> = iuse.iter().cloned().collect();
        effective.extend(self.implicit.iter().cloned());
        if !self.arch.is_empty() {
            effective.insert(self.arch.clone());
        }

        let stable = self.is_stable(e);
        let mut force: Vec<String> = self.use_force.clone();
        let mut mask: Vec<String> = self.use_mask.clone();
        if stable {
            incr_tokens(&mut force, self.use_stable_force.clone());
            incr_tokens(&mut mask, self.use_stable_mask.clone());
        }
        let per = |list: &AtomList, target: &mut Vec<String>| {
            for (a, toks) in list {
                if Self::matches(a, e) {
                    incr_tokens(target, toks.clone());
                }
            }
        };
        per(&self.pkg_use_force, &mut force);
        per(&self.pkg_use_mask, &mut mask);
        if stable {
            per(&self.pkg_use_stable_force, &mut force);
            per(&self.pkg_use_stable_mask, &mut mask);
        }
        let forced: HashSet<String> = force.into_iter().filter(|f| effective.contains(f)).collect();
        let mut masked: HashSet<String> = mask.into_iter().filter(|f| effective.contains(f)).collect();

        // the test flag follows FEATURES=test
        if effective.contains("test") {
            let restrict_test = e.meta.get("RESTRICT").split_whitespace().any(|r| r == "test");
            if self.feature("test") && !restrict_test {
                state.insert("test".into(), true);
            } else {
                masked.insert("test".into());
            }
        }

        let mut enabled: HashSet<String> =
            state.into_iter().filter(|(f, on)| *on && effective.contains(f)).map(|(f, _)| f).collect();
        enabled.extend(forced.iter().cloned());
        enabled.retain(|f| !masked.contains(f));
        UseInfo { iuse, effective, enabled, forced, masked }
    }

    fn license_accepted(&self, lic: &str, toks: &[String]) -> bool {
        let mut ok = false;
        for t in toks {
            let (neg, t) = match t.strip_prefix('-') {
                Some(x) => (true, x),
                None => (false, t.as_str()),
            };
            let hit = if t == "*" {
                true
            } else if let Some(g) = t.strip_prefix('@') {
                self.group_has(g, lic, 0)
            } else {
                t == lic
            };
            if hit {
                ok = !neg;
            }
        }
        ok
    }

    fn group_has(&self, g: &str, lic: &str, depth: usize) -> bool {
        if depth > 10 {
            return false;
        }
        self.license_groups.get(g).map_or(false, |m| {
            m.iter().any(|x| match x.strip_prefix('@') {
                Some(sub) => self.group_has(sub, lic, depth + 1),
                None => x == lic,
            })
        })
    }

    fn accepted_keywords(&self, e: &Ebuild) -> Vec<String> {
        let mut acc = self.accept_keywords.clone();
        for (a, toks) in &self.pkg_keywords {
            if Self::matches(a, e) {
                if toks.is_empty() {
                    acc.push(format!("~{}", self.arch));
                } else {
                    incr_tokens(&mut acc, toks.clone());
                }
            }
        }
        acc
    }

    pub fn keywords_ok(&self, e: &Ebuild) -> bool {
        let kws: Vec<&str> = e.meta.get("KEYWORDS").split_whitespace().collect();
        for a in self.accepted_keywords(e) {
            let ok = match a.as_str() {
                "**" => true,
                "*" => kws.iter().any(|k| !k.starts_with('~') && !k.starts_with('-')),
                "~*" => kws.iter().any(|k| k.starts_with('~')),
                x => kws.contains(&x),
            };
            if ok {
                return true;
            }
        }
        false
    }

    pub fn mask_reason(&self, e: &Ebuild) -> Option<String> {
        for (a, src) in &self.masks {
            if Self::matches(a, e) && !self.unmasks.iter().any(|u| Self::matches(u, e)) {
                return Some(format!("package.mask ({src})"));
            }
        }
        None
    }

    /// Why a package can't be installed, or None if it can.
    pub fn visibility(&self, e: &Ebuild) -> Option<String> {
        let eapi = e.meta.get("EAPI");
        if !SUPPORTED_EAPIS.contains(&eapi) {
            return Some(format!("EAPI {eapi} is not supported"));
        }
        if let Some(r) = self.mask_reason(e) {
            return Some(r);
        }
        if !self.keywords_ok(e) {
            let kws = e.meta.get("KEYWORDS");
            let mine: Vec<&str> = kws.split_whitespace().filter(|k| k.trim_start_matches(['~', '-']) == self.arch).collect();
            return Some(if mine.is_empty() {
                format!("missing keyword for {} (KEYWORDS=\"{kws}\")", self.arch)
            } else {
                format!("keyword {} is not accepted", mine.join(" "))
            });
        }
        let u = self.pkg_use(e);
        let lic = depspec::parse(e.meta.get("LICENSE")).unwrap_or_default();
        let lic = depspec::reduce(&lic, &u.enabled);
        let mut toks = self.accept_license.clone();
        for (a, t) in &self.pkg_license {
            if Self::matches(a, e) {
                toks.extend(t.iter().cloned());
            }
        }
        if let Some(bad) = self.unaccepted_license(&lic, &toks) {
            return Some(format!("license {bad} is not accepted (ACCEPT_LICENSE)"));
        }
        None
    }

    fn unaccepted_license(&self, nodes: &[depspec::Node], toks: &[String]) -> Option<String> {
        for n in nodes {
            match n {
                depspec::Node::Leaf(l) => {
                    if !self.license_accepted(l, toks) {
                        return Some(l.clone());
                    }
                }
                depspec::Node::Any(c) => {
                    if !c.iter().any(|x| self.unaccepted_license(std::slice::from_ref(x), toks).is_none()) {
                        return Some(depspec::render(c));
                    }
                }
                depspec::Node::All(c) => {
                    if let Some(b) = self.unaccepted_license(c, toks) {
                        return Some(b);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Variables from /etc/portage/env files selected by package.env.
    pub fn pkg_env_vars(&self, e: &Ebuild) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (a, files) in &self.pkg_env {
            if Self::matches(a, e) {
                for f in files {
                    let p = self.config_root.join("etc/portage/env").join(f);
                    match shellvars::parse_file(&p, &self.vars) {
                        Ok(v) => out.extend(v),
                        Err(err) => warn(&err),
                    }
                }
            }
        }
        out
    }

    pub fn is_provided(&self, a: &Atom) -> bool {
        self.provided.iter().any(|p| p.cat == a.cat && p.pn == a.pn)
    }

    pub fn vdb_path(&self) -> PathBuf {
        self.root.join("var/db/pkg")
    }

    pub fn world_file(&self) -> PathBuf {
        self.root.join("var/lib/portage/world")
    }
}

fn walk_profile(dir: &Path, repos: &Repos, out: &mut Vec<PathBuf>, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err("profile parent chain is too deep".into());
    }
    for l in read_lines_dir(&dir.join("parent")) {
        let p = if let Some((repo, rel)) = l.split_once(':') {
            let r = repos.by_name(repo).ok_or_else(|| format!("profile parent '{l}': unknown repository"))?;
            r.location.join("profiles").join(rel)
        } else {
            dir.join(&l)
        };
        let p = std::fs::canonicalize(&p).map_err(|_| format!("profile parent {} does not exist", p.display()))?;
        walk_profile(&p, repos, out, depth + 1)?;
    }
    if !out.contains(&dir.to_path_buf()) {
        out.push(dir.to_path_buf());
    }
    Ok(())
}
