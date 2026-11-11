//! Dependency resolution: from target atoms to an ordered merge list.

use crate::autounmask::{Change, Kind};
use crate::binhost::{BinPkg, Binhost};
use crate::atom::{use_reqs_satisfied, Atom, Blocker, SlotOp};
use crate::config::{Config, UseInfo};
use crate::depspec::{self, Node};
use crate::repo::Ebuild;
use crate::vdb::{Installed, Vdb};
use crate::version::Version;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    B,
    D,
    R,
    I,
    P,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    New,
    NewSlot,
    Upgrade(Version),
    Downgrade(Version),
    Reinstall,
}

#[derive(Clone)]
pub struct PkgNode {
    pub e: Rc<Ebuild>,
    pub use_: Rc<UseInfo>,
    pub action: Action,
    pub deps: Vec<(usize, DepKind)>,
    /// installed packages to remove after this one merges (weak blockers)
    pub uninstall_after: Vec<String>,
    pub reason: Option<String>,
    /// the package that pulled this one in
    pub parent: Option<String>,
    /// install this prebuilt package instead of building the ebuild
    pub binpkg: Option<Rc<BinPkg>>,
}

#[derive(Clone, Default)]
pub struct Opts {
    pub update: bool,
    pub deep: bool,
    pub newuse: bool,
    pub noreplace: bool,
    pub nodeps: bool,
    pub with_bdeps: bool,
    /// work out /etc/portage changes when nothing satisfies a dependency
    pub autounmask: bool,
}

#[derive(Clone)]
struct State {
    nodes: Vec<PkgNode>,
    index: HashMap<(String, String), usize>,
    blockers: Vec<(usize, Atom)>,
    deep_seen: HashSet<String>,
}

pub struct Resolver<'a> {
    cfg: &'a Config,
    vdb: &'a Vdb,
    opts: Opts,
    st: State,
    path: Vec<String>,
    /// while walking build deps of installed packages: skip anything not installed
    installed_only: bool,
    binhost: Option<&'a Binhost>,
    /// changes that would let the failed dependency resolve
    proposal: Vec<Change>,
}

pub struct Plan {
    pub nodes: Vec<PkgNode>,
    pub order: Vec<usize>,
    pub warnings: Vec<String>,
}

fn kind_key(k: DepKind) -> &'static str {
    match k {
        DepKind::B => "BDEPEND",
        DepKind::D => "DEPEND",
        DepKind::R => "RDEPEND",
        DepKind::I => "IDEPEND",
        DepKind::P => "PDEPEND",
    }
}

impl<'a> Resolver<'a> {
    pub fn new(cfg: &'a Config, vdb: &'a Vdb, opts: Opts) -> Resolver<'a> {
        Resolver {
            cfg,
            vdb,
            opts,
            st: State { nodes: vec![], index: HashMap::new(), blockers: vec![], deep_seen: HashSet::new() },
            path: vec![],
            installed_only: false,
            binhost: None,
            proposal: vec![],
        }
    }

    /// The /etc/portage changes worked out for the last failure, if any.
    pub fn take_proposal(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.proposal)
    }

    /// The cheapest set of changes that makes some version satisfy `a`
    /// (USE changes before licenses before keywords, newest version on ties).
    fn propose(&self, a: &Atom, parent_use: Option<&HashSet<String>>) -> Vec<Change> {
        let reqs = a.use_reqs(parent_use);
        let required_by = {
            let mut p: Vec<String> = self.path.iter().rev().cloned().collect();
            p.insert(0, a.to_string());
            p.join(" <- ")
        };
        let mut best: Option<(u32, Vec<Change>)> = None;
        for e in self.cfg.repos.matching(a) {
            if !crate::repo::SUPPORTED_EAPIS.contains(&e.meta.get("EAPI")) || self.cfg.mask_reason(&e).is_some() {
                continue;
            }
            let atom = format!("={}", e.cpv());
            let mut out = Vec::new();
            if !self.cfg.keywords_ok(&e) {
                let testing = format!("~{}", self.cfg.arch);
                if !e.meta.get("KEYWORDS").split_whitespace().any(|k| k == testing) {
                    continue; // not even in testing for this arch: leave it alone
                }
                out.push(Change { kind: Kind::Keywords, atom: atom.clone(), tokens: vec![testing], required_by: required_by.clone() });
            }
            let lic = self.cfg.unaccepted_licenses(&e);
            if !lic.is_empty() {
                out.push(Change { kind: Kind::License, atom: atom.clone(), tokens: lic, required_by: required_by.clone() });
            }
            let u = self.cfg.pkg_use(&e);
            let mut flags = Vec::new();
            let mut impossible = false;
            for r in &reqs {
                let have = if u.effective.contains(&r.flag) { u.enabled.contains(&r.flag) } else { r.default.unwrap_or(!r.enabled) };
                if have == r.enabled {
                    continue;
                }
                // flags outside IUSE, or forced/masked by the profile, can't be changed in package.use
                if !u.effective.contains(&r.flag) || u.forced.contains(&r.flag) || u.masked.contains(&r.flag) {
                    impossible = true;
                    break;
                }
                flags.push(if r.enabled { r.flag.clone() } else { format!("-{}", r.flag) });
            }
            if impossible {
                continue;
            }
            if !flags.is_empty() {
                // >= like Portage, so the flags stay set across upgrades
                out.push(Change { kind: Kind::Use, atom: format!(">={}", e.cpv()), tokens: flags, required_by: required_by.clone() });
            }
            if out.is_empty() {
                continue;
            }
            let cost: u32 = out.iter().map(|c| match c.kind { Kind::Use => 1, Kind::License => 2, Kind::Keywords => 4 }).sum();
            if best.as_ref().map(|(b, _)| cost < *b).unwrap_or(true) {
                best = Some((cost, out));
            }
        }
        best.map(|(_, c)| c).unwrap_or_default()
    }

    /// Use prebuilt packages from this binhost where they match exactly.
    pub fn use_binhost(&mut self, b: &'a Binhost) {
        self.binhost = Some(b);
    }

    fn installed_best(&self, a: &Atom, parent_use: Option<&HashSet<String>>) -> Option<&'a Installed> {
        self.vdb.matching(a, parent_use).into_iter().max_by(|x, y| x.ver.cmp(&y.ver))
    }

    fn graph_match(&self, a: &Atom, parent_use: Option<&HashSet<String>>) -> Option<usize> {
        let reqs = a.use_reqs(parent_use);
        self.st.nodes.iter().position(|n| {
            a.matches_nouse(&n.e.target(None, None))
                && use_reqs_satisfied(&reqs, Some(&n.use_.effective), Some(&n.use_.enabled))
        })
    }

    fn ctx(&self) -> String {
        if self.path.is_empty() {
            String::new()
        } else {
            format!("\n  required by: {}", self.path.iter().rev().cloned().collect::<Vec<_>>().join(" <- "))
        }
    }

    /// Visible ebuilds for an atom, best first.
    fn candidates(&self, a: &Atom) -> Vec<Rc<Ebuild>> {
        self.cfg.repos.matching(a).into_iter().filter(|e| self.cfg.visibility(e).is_none()).collect()
    }

    fn explain_none(&self, a: &Atom, parent_use: Option<&HashSet<String>>) -> String {
        let all = self.cfg.repos.matching(a);
        if all.is_empty() {
            let any = self.cfg.repos.ebuilds(&a.cat, &a.pn);
            if any.is_empty() {
                return format!("there are no ebuilds for {}{}", a.cp(), self.ctx());
            }
            return format!("no version of {} matches {a}{}", a.cp(), self.ctx());
        }
        let mut msg = format!("all ebuilds that could satisfy \"{a}\" are unusable:");
        let reqs = a.use_reqs(parent_use);
        for e in all.iter().take(8) {
            match self.cfg.visibility(e) {
                Some(why) => msg.push_str(&format!("\n    {}::{}  ({why})", e.cpv(), e.repo_name)),
                None => {
                    let u = self.cfg.pkg_use(e);
                    let need: Vec<String> = reqs
                        .iter()
                        .filter(|r| {
                            let have = if u.effective.contains(&r.flag) { u.enabled.contains(&r.flag) } else { r.default.unwrap_or(!r.enabled) };
                            have != r.enabled
                        })
                        .map(|r| format!("{}{}", if r.enabled { "" } else { "-" }, r.flag))
                        .collect();
                    msg.push_str(&format!(
                        "\n    {}::{}  (needs USE=\"{}\"; add \"{} {}\" to /etc/portage/package.use)",
                        e.cpv(),
                        e.repo_name,
                        need.join(" "),
                        e.cp(),
                        need.join(" ")
                    ));
                }
            }
        }
        msg.push_str(&self.ctx());
        msg
    }

    pub fn add_target(&mut self, a: &Atom) -> Result<(), String> {
        self.resolve_atom(a, None, true).map(|_| ())
    }

    fn resolve_atom(&mut self, a: &Atom, parent_use: Option<&HashSet<String>>, target: bool) -> Result<Option<usize>, String> {
        if a.blocker.is_some() {
            return Ok(None);
        }
        if self.cfg.is_provided(a) {
            return Ok(None);
        }
        if let Some(i) = self.graph_match(a, parent_use) {
            return Ok(Some(i));
        }
        let inst = self.installed_best(a, parent_use);
        let check_update = self.opts.update && (target || self.opts.deep);
        if let Some(inst) = inst {
            let mut keep = true;
            if target && !self.opts.update && !self.opts.noreplace {
                keep = false;
            } else if check_update || (self.opts.newuse && (target || self.opts.deep)) {
                keep = !self.wants_replacing(a, inst, parent_use);
            }
            if keep {
                if self.opts.deep && !self.opts.nodeps {
                    self.deep_installed(inst)?;
                }
                return Ok(None);
            }
        }
        let reqs = a.use_reqs(parent_use);
        let mut chosen: Option<(Rc<Ebuild>, UseInfo)> = None;
        let cands = self.candidates(a);
        for e in &cands {
            // stay in the installed package's slot when updating
            if let (Some(i), false) = (inst, target) {
                if e.slot() != i.slot && cands.iter().any(|c| c.slot() == i.slot) {
                    continue;
                }
            }
            let u = self.cfg.pkg_use(&e);
            if use_reqs_satisfied(&reqs, Some(&u.effective), Some(&u.enabled)) {
                chosen = Some((e.clone(), u));
                break;
            }
        }
        let Some((e, u)) = chosen else {
            if self.opts.autounmask {
                self.proposal = self.propose(a, parent_use);
            }
            return Err(self.explain_none(a, parent_use));
        };
        let key = (e.cp(), e.slot().to_string());
        if let Some(&other) = self.st.index.get(&key) {
            let o = &self.st.nodes[other];
            return Err(format!(
                "slot conflict in {}:{}: {} is already selected, but {a} needs {}{}",
                e.cp(),
                e.slot(),
                o.e.cpv(),
                e.cpv(),
                self.ctx()
            ));
        }
        let req = depspec::parse(e.meta.get("REQUIRED_USE")).unwrap_or_default();
        let bad = depspec::check_required_use(&req, &u.enabled);
        if !bad.is_empty() {
            return Err(format!(
                "{} has unsatisfied REQUIRED_USE: {}\n  (adjust USE for {} in /etc/portage/package.use){}",
                e.cpv(),
                bad.join("  "),
                e.cp(),
                self.ctx()
            ));
        }
        let installed_same_slot = self.vdb.by_cp_slot(&e.cp(), e.slot());
        let action = match installed_same_slot {
            Some(i) if i.ver == e.ver => Action::Reinstall,
            Some(i) if i.ver < e.ver => Action::Upgrade(i.ver.clone()),
            Some(i) => Action::Downgrade(i.ver.clone()),
            None if self.vdb.pkgs.iter().any(|p| p.cp() == e.cp()) => Action::NewSlot,
            None => Action::New,
        };
        let idx = self.st.nodes.len();
        let binpkg = self.binhost.and_then(|b| b.find(&e, &u));
        let use_ = Rc::new(u);
        let parent = self.path.last().cloned();
        self.st.nodes.push(PkgNode {
            e: e.clone(),
            use_: use_.clone(),
            action,
            deps: vec![],
            uninstall_after: vec![],
            reason: None,
            parent,
            binpkg,
        });
        self.st.index.insert(key, idx);
        if !self.opts.nodeps {
            self.path.push(e.cpv());
            let r = self.resolve_deps(idx, &e, &use_);
            self.path.pop();
            r?;
        }
        Ok(Some(idx))
    }

    /// Should an installed package that satisfies `a` be replaced (update / newuse)?
    fn wants_replacing(&self, a: &Atom, inst: &Installed, parent_use: Option<&HashSet<String>>) -> bool {
        let reqs = a.use_reqs(parent_use);
        let same_slot: Vec<Rc<Ebuild>> = self.candidates(a).into_iter().filter(|e| e.slot() == inst.slot).collect();
        if self.opts.update {
            if let Some(best) = same_slot.iter().find(|e| {
                let u = self.cfg.pkg_use(e);
                use_reqs_satisfied(&reqs, Some(&u.effective), Some(&u.enabled))
            }) {
                if best.ver > inst.ver {
                    return true;
                }
            }
        }
        if self.opts.newuse {
            if let Some(same) = self.cfg.repos.ebuilds(&inst.cat, &inst.pn).iter().find(|e| e.ver == inst.ver) {
                let u = self.cfg.pkg_use(same);
                // forced and masked flags never trigger rebuilds
                let iuse: HashSet<&String> =
                    u.iuse.iter().filter(|f| !u.forced.contains(*f) && !u.masked.contains(*f)).collect();
                let want: HashSet<&String> = u.enabled.iter().filter(|f| iuse.contains(f)).collect();
                let have: HashSet<&String> = inst.use_.iter().filter(|f| iuse.contains(f)).collect();
                if want != have {
                    return true;
                }
                // flags added to or removed from IUSE
                let all_new: HashSet<&String> = u.iuse.iter().collect();
                let old_iuse: HashSet<&String> = inst
                    .iuse_declared
                    .iter()
                    .filter(|f| !all_new.contains(f) || iuse.contains(f))
                    .collect();
                if !inst.iuse_declared.is_empty() && iuse != old_iuse {
                    return true;
                }
            }
        }
        false
    }

    fn deep_installed(&mut self, inst: &Installed) -> Result<(), String> {
        if !self.st.deep_seen.insert(inst.cpv()) {
            return Ok(());
        }
        self.path.push(inst.cpv());
        // dynamic deps: if the installed version's ebuild is still in its repo,
        // use its current dependencies (Portage does the same)
        let current = self
            .cfg
            .repos
            .ebuilds(&inst.cat, &inst.pn)
            .iter()
            .find(|e| e.ver == inst.ver && (inst.repo.is_empty() || e.repo_name == inst.repo))
            .cloned();
        // runtime deps fully; build deps only to update what is already installed
        // unless --with-bdeps=y
        for k in ["BDEPEND", "DEPEND", "RDEPEND", "IDEPEND", "PDEPEND"] {
            let nodes = match &current {
                Some(e) => depspec::reduce(&depspec::parse(e.meta.get(k)).unwrap_or_default(), &inst.use_),
                None => depspec::parse(&inst.read(k)).unwrap_or_default(),
            };
            let prev = self.installed_only;
            self.installed_only = matches!(k, "BDEPEND" | "DEPEND") && !self.opts.with_bdeps;
            let r = self.resolve_tree(&nodes, None, Some(&inst.use_.clone()), DepKind::R);
            self.installed_only = prev;
            if let Err(e) = r {
                self.path.pop();
                return Err(e);
            }
        }
        self.path.pop();
        Ok(())
    }

    fn resolve_deps(&mut self, idx: usize, e: &Ebuild, u: &UseInfo) -> Result<(), String> {
        let binary = self.st.nodes[idx].binpkg.is_some();
        for kind in [DepKind::B, DepKind::D, DepKind::R, DepKind::I, DepKind::P] {
            let raw = depspec::parse(e.meta.get(kind_key(kind))).map_err(|err| format!("{}: {err}", e.cpv()))?;
            let reduced = depspec::reduce(&raw, &u.enabled);
            // a prebuilt package needs no build deps, like an installed one
            let prev = self.installed_only;
            if binary && matches!(kind, DepKind::B | DepKind::D) && !self.opts.with_bdeps {
                self.installed_only = true;
            }
            let r = self.resolve_tree(&reduced, Some(idx), Some(&u.enabled), kind);
            self.installed_only = prev;
            r?;
        }
        Ok(())
    }

    fn resolve_tree(&mut self, nodes: &[Node], parent: Option<usize>, puse: Option<&HashSet<String>>, kind: DepKind) -> Result<(), String> {
        for n in nodes {
            match n {
                Node::Leaf(s) => {
                    let a = Atom::parse(s)?;
                    if a.blocker.is_some() {
                        if let Some(p) = parent {
                            self.st.blockers.push((p, a));
                        }
                        continue;
                    }
                    if self.installed_only && !self.vdb.pkgs.iter().any(|p| p.cat == a.cat && p.pn == a.pn) {
                        continue;
                    }
                    if let Some(c) = self.resolve_atom(&a, puse, false)? {
                        if let Some(p) = parent {
                            if c != p {
                                self.st.nodes[p].deps.push((c, kind));
                            }
                        }
                    }
                }
                Node::All(c) => self.resolve_tree(c, parent, puse, kind)?,
                Node::Any(alts) => {
                    if self.installed_only && !alts.iter().any(|x| self.cp_installed(x)) {
                        continue;
                    }
                    self.resolve_any(alts, parent, puse, kind)?
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn satisfied_now(&self, n: &Node, puse: Option<&HashSet<String>>) -> bool {
        match n {
            Node::Leaf(s) => match Atom::parse(s) {
                Ok(a) if a.blocker.is_some() => true,
                Ok(a) => self.graph_match(&a, puse).is_some() || self.installed_best(&a, puse).is_some() || self.cfg.is_provided(&a),
                Err(_) => false,
            },
            Node::All(c) => c.iter().all(|x| self.satisfied_now(x, puse)),
            Node::Any(c) => c.iter().any(|x| self.satisfied_now(x, puse)),
            _ => true,
        }
    }

    fn cp_installed(&self, n: &Node) -> bool {
        let leaves = depspec::flat_leaves(std::slice::from_ref(n));
        !leaves.is_empty()
            && leaves.iter().all(|l| {
                Atom::parse(l).ok().map_or(false, |a| a.blocker.is_some() || self.vdb.pkgs.iter().any(|p| p.cat == a.cat && p.pn == a.pn))
            })
    }

    fn resolve_any(&mut self, alts: &[Node], parent: Option<usize>, puse: Option<&HashSet<String>>, kind: DepKind) -> Result<(), String> {
        if alts.is_empty() {
            return Ok(());
        }
        if let Some(alt) = alts.iter().find(|x| self.satisfied_now(x, puse)) {
            let alt = alt.clone();
            return self.resolve_tree(std::slice::from_ref(&alt), parent, puse, kind);
        }
        // prefer alternatives whose packages are already installed (in any version)
        let mut ordered: Vec<&Node> = alts.iter().filter(|x| self.cp_installed(x)).collect();
        ordered.extend(alts.iter().filter(|x| !self.cp_installed(x)));
        let mut first_err = None;
        for alt in ordered {
            let snapshot = self.st.clone();
            let path = self.path.clone();
            match self.resolve_tree(std::slice::from_ref(alt), parent, puse, kind) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.st = snapshot;
                    self.path = path;
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        Err(first_err.unwrap())
    }

    /// Installed packages whose `:=` dependency is broken by a sub-slot change in the plan.
    fn subslot_rebuilds(&mut self) -> Result<(), String> {
        loop {
            let mut added = false;
            let changes: Vec<(String, String, String, usize)> = self
                .st
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| {
                    let old = self.vdb.by_cp_slot(&n.e.cp(), n.e.slot())?;
                    (old.subslot != n.e.subslot()).then(|| (n.e.cp(), n.e.slot().to_string(), old.subslot.clone(), i))
                })
                .collect();
            if changes.is_empty() {
                return Ok(());
            }
            let in_graph: HashSet<String> = self.st.nodes.iter().map(|n| n.e.cp()).collect();
            let mut todo: Vec<(&Installed, usize, String)> = Vec::new();
            for p in &self.vdb.pkgs {
                if in_graph.contains(&p.cp()) {
                    continue;
                }
                let text = format!("{} {}", p.read("RDEPEND"), p.read("DEPEND"));
                for (cp, slot, oldsub, idx) in &changes {
                    if !text.contains(cp.as_str()) {
                        continue;
                    }
                    let hit = depspec::flat_leaves(&depspec::parse(&text).unwrap_or_default()).iter().any(|l| {
                        Atom::parse(l).ok().map_or(false, |a| {
                            a.blocker.is_none()
                                && a.cp() == *cp
                                && a.slot.as_ref().map_or(false, |s| {
                                    s.op == Some(SlotOp::Eq)
                                        && s.slot.as_deref().map_or(true, |x| x == slot)
                                        && s.subslot.as_deref() == Some(oldsub.as_str())
                                })
                        })
                    });
                    if hit {
                        todo.push((p, *idx, cp.clone()));
                        break;
                    }
                }
            }
            for (p, dep_idx, cp) in todo {
                if self.st.index.contains_key(&(p.cp(), p.slot.clone())) {
                    continue;
                }
                let exact = Atom::parse(&format!("={}", p.cpv())).unwrap();
                let pick = self.candidates(&exact).into_iter().next().or_else(|| {
                    let slot_atom = Atom::parse(&format!("{}:{}", p.cp(), p.slot)).ok()?;
                    self.candidates(&slot_atom).into_iter().next()
                });
                let Some(e) = pick else {
                    self.warnings_push(format!("{} needs a rebuild for {cp} but no ebuild for it is available", p.cpv()));
                    continue;
                };
                let u = self.cfg.pkg_use(&e);
                let action = if e.ver == p.ver { Action::Reinstall } else if e.ver > p.ver { Action::Upgrade(p.ver.clone()) } else { Action::Downgrade(p.ver.clone()) };
                let idx = self.st.nodes.len();
                // a binary only helps if it was built against the new sub-slot
                let new_slot = self.st.nodes[dep_idx].e.meta.get("SLOT").to_string();
                let binpkg = self.binhost.and_then(|b| b.find(&e, &u)).filter(|b| {
                    let built_against = format!("{cp}:{new_slot}=");
                    b.get("RDEPEND").contains(&built_against) || b.get("DEPEND").contains(&built_against)
                });
                let use_ = Rc::new(u);
                self.st.nodes.push(PkgNode {
                    e: e.clone(),
                    use_: use_.clone(),
                    action,
                    deps: vec![(dep_idx, DepKind::D)],
                    uninstall_after: vec![],
                    reason: Some(format!("rebuild for {cp} sub-slot change")),
                    parent: None,
                    binpkg,
                });
                self.st.index.insert((e.cp(), e.slot().to_string()), idx);
                self.path.push(e.cpv());
                let r = self.resolve_deps(idx, &e, &use_);
                self.path.pop();
                r?;
                added = true;
            }
            if !added {
                return Ok(());
            }
        }
    }

    fn warnings_push(&mut self, w: String) {
        // stored as a reason on nothing; surfaced through finish()
        self.path.clear();
        WARN.with(|v| v.borrow_mut().push(w));
    }

    fn check_blockers(&mut self) -> Result<(), String> {
        let replaced: HashMap<(String, String), Version> =
            self.st.nodes.iter().map(|n| ((n.e.cp(), n.e.slot().to_string()), n.e.ver.clone())).collect();
        let blockers = self.st.blockers.clone();
        for (b, a) in blockers {
            let bn = self.st.nodes[b].e.clone();
            let plain = a.unblocked();
            for (i, n) in self.st.nodes.iter().enumerate() {
                if i == b || n.e.cp() == bn.cp() {
                    continue;
                }
                if plain.matches(&n.e.target(Some(&n.use_.effective), Some(&n.use_.enabled)), Some(&self.st.nodes[b].use_.enabled)) {
                    return Err(format!("{} and {} block each other ({a}); they cannot both be installed", bn.cpv(), n.e.cpv()));
                }
            }
            for inst in self.vdb.matching(&plain, Some(&self.st.nodes[b].use_.enabled)) {
                if inst.cp() == bn.cp() {
                    continue;
                }
                // being replaced by a version outside the blocker's range?
                if let Some(v) = replaced.get(&(inst.cp(), inst.slot.clone())) {
                    let t = crate::atom::MatchTarget {
                        cat: &inst.cat,
                        pn: &inst.pn,
                        version: v,
                        slot: &inst.slot,
                        subslot: &inst.subslot,
                        repo: &inst.repo,
                        iuse: None,
                        use_: None,
                    };
                    if !plain.matches_nouse(&t) {
                        continue;
                    }
                }
                match a.blocker {
                    Some(Blocker::Weak) => {
                        let list = &mut self.st.nodes[b].uninstall_after;
                        if !list.contains(&inst.cpv()) {
                            list.push(inst.cpv());
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{} is blocked by the installed {} ({a}); remove it first with: hoard remove ={}",
                            bn.cpv(),
                            inst.cpv(),
                            inst.cpv()
                        ))
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Plan, String> {
        if !self.opts.nodeps {
            self.subslot_rebuilds()?;
            self.check_blockers()?;
        }
        let n = self.st.nodes.len();
        let mut state = vec![0u8; n]; // 0 new, 1 visiting, 2 done
        let mut order = Vec::new();
        let mut warnings: Vec<String> = WARN.with(|v| std::mem::take(&mut *v.borrow_mut()));
        fn visit(i: usize, nodes: &[PkgNode], state: &mut [u8], order: &mut Vec<usize>, warnings: &mut Vec<String>) {
            if state[i] != 0 {
                return;
            }
            state[i] = 1;
            for &(c, k) in &nodes[i].deps {
                if k == DepKind::P {
                    continue;
                }
                if state[c] == 1 {
                    if matches!(k, DepKind::B | DepKind::D) {
                        warnings.push(format!("circular build dependency: {} <-> {}", nodes[i].e.cpv(), nodes[c].e.cpv()));
                    }
                    continue;
                }
                visit(c, nodes, state, order, warnings);
            }
            state[i] = 2;
            order.push(i);
            for &(c, k) in &nodes[i].deps {
                if k == DepKind::P {
                    visit(c, nodes, state, order, warnings);
                }
            }
        }
        for i in 0..n {
            visit(i, &self.st.nodes, &mut state, &mut order, &mut warnings);
        }
        Ok(Plan { nodes: self.st.nodes, order, warnings })
    }
}

thread_local! {
    static WARN: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Expand @world / @system / @selected or a package name into atoms.
pub fn expand_target(cfg: &Config, vdb: &Vdb, s: &str) -> Result<Vec<Atom>, String> {
    match s {
        "@world" => {
            let mut v = expand_target(cfg, vdb, "@selected")?;
            v.extend(cfg.system.iter().cloned());
            Ok(v)
        }
        "@system" => Ok(cfg.system.clone()),
        "@preserved-rebuild" => Ok(crate::preserve::rebuild_targets(cfg, vdb).iter().filter_map(|a| Atom::parse(a).ok()).collect()),
        "@selected" => Ok(crate::vdb::read_world(&cfg.world_file())
            .iter()
            .filter(|l| !l.starts_with('@'))
            .filter_map(|l| Atom::parse(l).ok())
            .collect()),
        _ if s.starts_with('@') => Err(format!("unknown set {s}")),
        _ => Ok(vec![parse_user_atom(cfg, vdb, s)?]),
    }
}

/// Accept "pkg", "cat/pkg", "=cat/pkg-1.0" etc. and fill in a missing category.
pub fn parse_user_atom(cfg: &Config, vdb: &Vdb, s: &str) -> Result<Atom, String> {
    if let Ok(a) = Atom::parse(s) {
        if a.blocker.is_none() {
            return Ok(a);
        }
    }
    // no category given: find it
    let ops = ["<=", ">=", "=", "~", "<", ">"];
    let (op, rest) = ops.iter().find_map(|o| s.strip_prefix(o).map(|r| (*o, r))).unwrap_or(("", s));
    let name_end = rest.find([':', '[']).unwrap_or(rest.len());
    let name_ver = &rest[..name_end];
    let pn = if op.is_empty() { name_ver.to_string() } else { crate::atom::split_pv(name_ver.trim_end_matches('*')).map(|(p, _)| p.to_string()).unwrap_or_default() };
    if pn.is_empty() {
        return Err(format!("'{s}' is not a valid package atom"));
    }
    let mut cats = cfg.repos.categories_for(&pn);
    for p in &vdb.pkgs {
        if p.pn == pn && !cats.contains(&p.cat) {
            cats.push(p.cat.clone());
        }
    }
    match cats.len() {
        0 => Err(format!("there is no package called '{pn}'")),
        1 => Atom::parse(&format!("{op}{}/{rest}", cats[0])),
        _ => Err(format!(
            "'{pn}' is ambiguous, say which one you mean:\n    {}",
            cats.iter().map(|c| format!("{c}/{pn}")).collect::<Vec<_>>().join("\n    ")
        )),
    }
}

/// Packages that nothing in @world/@system needs any more.
/// For `||` groups only one installed alternative is kept, preferring one that
/// something else already needs (like Portage's depclean).
pub fn unneeded<'v>(cfg: &Config, vdb: &'v Vdb) -> Vec<String> {
    let loose = |a: &Atom| -> Atom {
        let mut a = a.clone();
        a.usedeps.clear();
        if let Some(s) = &mut a.slot {
            s.subslot = None;
            s.op = None;
            if s.slot.is_none() {
                a.slot = None;
            }
        }
        a
    };
    let mut keep: HashSet<String> = HashSet::new();
    let mut queue: Vec<&'v Installed> = Vec::new();
    let mut deferred: Vec<Vec<Node>> = Vec::new();
    let mark = |a: &Atom, keep: &mut HashSet<String>, queue: &mut Vec<&'v Installed>| {
        for i in vdb.matching_nouse(&loose(a)) {
            if keep.insert(i.cpv()) {
                queue.push(i);
            }
        }
    };
    for a in &expand_target(cfg, vdb, "@world").unwrap_or_default() {
        mark(a, &mut keep, &mut queue);
    }
    fn collect(nodes: &[Node], plain: &mut Vec<Atom>, groups: &mut Vec<Vec<Node>>) {
        for n in nodes {
            match n {
                Node::Leaf(l) => {
                    if let Ok(a) = Atom::parse(l) {
                        if a.blocker.is_none() {
                            plain.push(a);
                        }
                    }
                }
                Node::All(c) => collect(c, plain, groups),
                Node::Any(c) => groups.push(c.clone()),
                _ => {}
            }
        }
    }
    let alt_installed = |n: &Node| -> bool {
        depspec::flat_leaves(std::slice::from_ref(n))
            .iter()
            .filter_map(|l| Atom::parse(l).ok())
            .filter(|a| a.blocker.is_none())
            .all(|a| !vdb.matching_nouse(&loose(&a)).is_empty())
    };
    loop {
        while let Some(p) = queue.pop() {
            for k in ["RDEPEND", "PDEPEND", "DEPEND", "BDEPEND", "IDEPEND"] {
                let mut plain = Vec::new();
                collect(&depspec::parse(&p.read(k)).unwrap_or_default(), &mut plain, &mut deferred);
                for a in plain {
                    mark(&a, &mut keep, &mut queue);
                }
            }
        }
        // resolve || groups: skip if an alternative is already kept, else keep the first installed one
        let groups = std::mem::take(&mut deferred);
        let mut progressed = false;
        for g in groups {
            let kept = g.iter().any(|alt| {
                let leaves: Vec<Atom> = depspec::flat_leaves(std::slice::from_ref(alt)).iter().filter_map(|l| Atom::parse(l).ok()).filter(|a| a.blocker.is_none()).collect();
                !leaves.is_empty() && leaves.iter().all(|a| vdb.matching_nouse(&loose(a)).iter().any(|i| keep.contains(&i.cpv())))
            });
            if kept {
                continue;
            }
            if let Some(alt) = g.iter().find(|alt| alt_installed(alt)) {
                let mut plain = Vec::new();
                let mut sub = Vec::new();
                collect(std::slice::from_ref(alt), &mut plain, &mut sub);
                for a in plain {
                    mark(&a, &mut keep, &mut queue);
                }
                deferred.extend(sub);
                progressed = true;
            }
        }
        if !progressed && queue.is_empty() {
            break;
        }
    }
    vdb.pkgs.iter().filter(|p| !keep.contains(&p.cpv())).map(|p| p.cpv()).collect()
}
