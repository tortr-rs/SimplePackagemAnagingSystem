//! Package atoms: `>=cat/pkg-1.0:slot/sub=::repo[use,-flag,opt?]`, blockers and cpvs.

use crate::version::Version;
use std::collections::HashSet;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Eq,
    EqGlob,
    Tilde,
    Ge,
    Gt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    Weak,
    Strong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotOp {
    /// `:=` rebuild when the provider's sub-slot changes
    Eq,
    /// `:*` any slot, no rebuilds
    Star,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotDep {
    pub slot: Option<String>,
    pub subslot: Option<String>,
    pub op: Option<SlotOp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UseDepKind {
    /// flag
    Enabled,
    /// -flag
    Disabled,
    /// flag=
    Same,
    /// !flag=
    Opposite,
    /// flag?
    IfEnabled,
    /// !flag?
    IfDisabledThenDisabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseDep {
    pub flag: String,
    pub kind: UseDepKind,
    /// (+) / (-) default for when the flag is missing from the target's IUSE
    pub default: Option<bool>,
}

/// A plain requirement after conditional USE dependencies have been evaluated
/// against the parent package: `flag` must be `enabled`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UseReq {
    pub flag: String,
    pub enabled: bool,
    pub default: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    pub blocker: Option<Blocker>,
    pub op: Option<Op>,
    pub cat: String,
    pub pn: String,
    pub version: Option<Version>,
    pub slot: Option<SlotDep>,
    pub repo: Option<String>,
    pub usedeps: Vec<UseDep>,
}

/// Everything about a package an atom can test.
pub struct MatchTarget<'a> {
    pub cat: &'a str,
    pub pn: &'a str,
    pub version: &'a Version,
    pub slot: &'a str,
    pub subslot: &'a str,
    pub repo: &'a str,
    pub iuse: Option<&'a HashSet<String>>,
    pub use_: Option<&'a HashSet<String>>,
}

fn valid_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-' | '.')
}

/// Split "pn-1.2-r3" into ("pn", Version). The version is the leftmost suffix that parses.
pub fn split_pv(s: &str) -> Option<(&str, Version)> {
    let bytes = s.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'-' && i > 0 && i + 1 < s.len() && bytes[i + 1].is_ascii_digit() {
            if let Some(v) = Version::parse(&s[i + 1..]) {
                return Some((&s[..i], v));
            }
        }
    }
    None
}

impl Atom {
    pub fn parse(s: &str) -> Result<Atom, String> {
        let orig = s;
        let mut s = s;
        let mut blocker = None;
        if let Some(r) = s.strip_prefix("!!") {
            blocker = Some(Blocker::Strong);
            s = r;
        } else if let Some(r) = s.strip_prefix('!') {
            blocker = Some(Blocker::Weak);
            s = r;
        }
        let mut usedeps = Vec::new();
        if s.ends_with(']') {
            let open = s.find('[').ok_or_else(|| format!("bad atom '{orig}'"))?;
            for part in s[open + 1..s.len() - 1].split(',') {
                let part = part.trim();
                if part.is_empty() {
                    return Err(format!("empty USE dependency in '{orig}'"));
                }
                usedeps.push(parse_usedep(part).ok_or_else(|| format!("bad USE dependency '{part}' in '{orig}'"))?);
            }
            s = &s[..open];
        }
        let mut repo = None;
        if let Some(i) = s.find("::") {
            repo = Some(s[i + 2..].to_string());
            s = &s[..i];
        }
        let mut slot = None;
        if let Some(i) = s.find(':') {
            let sp = &s[i + 1..];
            s = &s[..i];
            slot = Some(if sp == "*" {
                SlotDep { slot: None, subslot: None, op: Some(SlotOp::Star) }
            } else if sp == "=" {
                SlotDep { slot: None, subslot: None, op: Some(SlotOp::Eq) }
            } else {
                let (body, op) = match sp.strip_suffix('=') {
                    Some(b) => (b, Some(SlotOp::Eq)),
                    None => (sp, None),
                };
                let (sl, sub) = match body.split_once('/') {
                    Some((a, b)) => (a.to_string(), Some(b.to_string())),
                    None => (body.to_string(), None),
                };
                if sl.is_empty() {
                    return Err(format!("bad slot in '{orig}'"));
                }
                SlotDep { slot: Some(sl), subslot: sub, op }
            });
        }
        let op = if let Some(r) = s.strip_prefix(">=") {
            s = r;
            Some(Op::Ge)
        } else if let Some(r) = s.strip_prefix("<=") {
            s = r;
            Some(Op::Le)
        } else if let Some(r) = s.strip_prefix('>') {
            s = r;
            Some(Op::Gt)
        } else if let Some(r) = s.strip_prefix('<') {
            s = r;
            Some(Op::Lt)
        } else if let Some(r) = s.strip_prefix('=') {
            s = r;
            Some(Op::Eq)
        } else if let Some(r) = s.strip_prefix('~') {
            s = r;
            Some(Op::Tilde)
        } else {
            None
        };
        let (cat, rest) = s.split_once('/').ok_or_else(|| format!("atom '{orig}' has no category"))?;
        if cat.is_empty() || !cat.chars().all(valid_name_char) {
            return Err(format!("bad category in '{orig}'"));
        }
        let mut op = op;
        let (pn, version) = if op.is_some() {
            let mut rest = rest;
            if let Some(r) = rest.strip_suffix('*') {
                if op != Some(Op::Eq) {
                    return Err(format!("'*' is only valid with '=' in '{orig}'"));
                }
                op = Some(Op::EqGlob);
                rest = r;
            }
            let (pn, v) = split_pv(rest).ok_or_else(|| format!("atom '{orig}' needs a version"))?;
            (pn.to_string(), Some(v))
        } else {
            (rest.to_string(), None)
        };
        if pn.is_empty() || !pn.chars().all(valid_name_char) {
            return Err(format!("bad package name in '{orig}'"));
        }
        Ok(Atom { blocker, op, cat: cat.to_string(), pn, version, slot, repo, usedeps })
    }

    pub fn cp(&self) -> String {
        format!("{}/{}", self.cat, self.pn)
    }

    /// Does the package match, ignoring USE dependencies?
    pub fn matches_nouse(&self, t: &MatchTarget) -> bool {
        if t.cat != self.cat || t.pn != self.pn {
            return false;
        }
        if let (Some(op), Some(v)) = (self.op, &self.version) {
            use std::cmp::Ordering::*;
            let ok = match op {
                Op::Lt => t.version < v,
                Op::Le => t.version <= v,
                Op::Eq => t.version == v,
                Op::Ge => t.version >= v,
                Op::Gt => t.version > v,
                Op::Tilde => t.version.cmp_base(v) == Equal,
                Op::EqGlob => t.version.matches_prefix(v),
            };
            if !ok {
                return false;
            }
        }
        if let Some(sd) = &self.slot {
            if let Some(s) = &sd.slot {
                if s != t.slot {
                    return false;
                }
            }
            if let Some(ss) = &sd.subslot {
                if ss != t.subslot {
                    return false;
                }
            }
        }
        if let Some(r) = &self.repo {
            if r != t.repo {
                return false;
            }
        }
        true
    }

    /// USE requirements once conditional forms are resolved against the parent's USE.
    pub fn use_reqs(&self, parent_use: Option<&HashSet<String>>) -> Vec<UseReq> {
        let mut out = Vec::new();
        for u in &self.usedeps {
            let pe = parent_use.map(|p| p.contains(&u.flag)).unwrap_or(false);
            let req = match u.kind {
                UseDepKind::Enabled => Some(true),
                UseDepKind::Disabled => Some(false),
                UseDepKind::Same => Some(pe),
                UseDepKind::Opposite => Some(!pe),
                UseDepKind::IfEnabled => if pe { Some(true) } else { None },
                UseDepKind::IfDisabledThenDisabled => if !pe { Some(false) } else { None },
            };
            if let Some(enabled) = req {
                out.push(UseReq { flag: u.flag.clone(), enabled, default: u.default });
            }
        }
        out
    }

    pub fn matches(&self, t: &MatchTarget, parent_use: Option<&HashSet<String>>) -> bool {
        if !self.matches_nouse(t) {
            return false;
        }
        let reqs = self.use_reqs(parent_use);
        use_reqs_satisfied(&reqs, t.iuse, t.use_)
    }

    /// The atom without the blocker prefix.
    pub fn unblocked(&self) -> Atom {
        let mut a = self.clone();
        a.blocker = None;
        a
    }
}

pub fn use_reqs_satisfied(reqs: &[UseReq], iuse: Option<&HashSet<String>>, use_: Option<&HashSet<String>>) -> bool {
    if reqs.is_empty() {
        return true;
    }
    let (Some(iuse), Some(use_)) = (iuse, use_) else { return true };
    for r in reqs {
        let have = if iuse.contains(&r.flag) {
            use_.contains(&r.flag)
        } else {
            match r.default {
                Some(d) => d,
                None => return false,
            }
        };
        if have != r.enabled {
            return false;
        }
    }
    true
}

fn parse_usedep(p: &str) -> Option<UseDep> {
    let (body, default) = if let Some(b) = p.strip_suffix("(+)") {
        (b, Some(true))
    } else if let Some(b) = p.strip_suffix("(-)") {
        (b, Some(false))
    } else {
        (p, None)
    };
    // the default can also sit before the ?/= marker: foo(+)?  foo(-)=
    let fix = |flag: &str| -> (String, Option<bool>) {
        if let Some(f) = flag.strip_suffix("(+)") {
            (f.to_string(), Some(true))
        } else if let Some(f) = flag.strip_suffix("(-)") {
            (f.to_string(), Some(false))
        } else {
            (flag.to_string(), default)
        }
    };
    let (flag, kind) = if let Some(f) = body.strip_prefix('!').and_then(|b| b.strip_suffix('=')) {
        (f, UseDepKind::Opposite)
    } else if let Some(f) = body.strip_prefix('!').and_then(|b| b.strip_suffix('?')) {
        (f, UseDepKind::IfDisabledThenDisabled)
    } else if let Some(f) = body.strip_suffix('=') {
        (f, UseDepKind::Same)
    } else if let Some(f) = body.strip_suffix('?') {
        (f, UseDepKind::IfEnabled)
    } else if let Some(f) = body.strip_prefix('-') {
        (f, UseDepKind::Disabled)
    } else {
        (body, UseDepKind::Enabled)
    };
    let (flag, default) = fix(flag);
    if flag.is_empty() {
        return None;
    }
    Some(UseDep { flag, kind, default })
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.blocker {
            Some(Blocker::Strong) => f.write_str("!!")?,
            Some(Blocker::Weak) => f.write_str("!")?,
            None => {}
        }
        let opstr = match self.op {
            Some(Op::Lt) => "<",
            Some(Op::Le) => "<=",
            Some(Op::Eq) | Some(Op::EqGlob) => "=",
            Some(Op::Tilde) => "~",
            Some(Op::Ge) => ">=",
            Some(Op::Gt) => ">",
            None => "",
        };
        write!(f, "{opstr}{}/{}", self.cat, self.pn)?;
        if let Some(v) = &self.version {
            write!(f, "-{v}")?;
        }
        if self.op == Some(Op::EqGlob) {
            f.write_str("*")?;
        }
        if let Some(sd) = &self.slot {
            f.write_str(":")?;
            if let Some(s) = &sd.slot {
                f.write_str(s)?;
                if let Some(ss) = &sd.subslot {
                    write!(f, "/{ss}")?;
                }
            }
            match sd.op {
                Some(SlotOp::Eq) => f.write_str("=")?,
                Some(SlotOp::Star) => f.write_str("*")?,
                None => {}
            }
        }
        if let Some(r) = &self.repo {
            write!(f, "::{r}")?;
        }
        if !self.usedeps.is_empty() {
            let parts: Vec<String> = self
                .usedeps
                .iter()
                .map(|u| {
                    let d = match u.default {
                        Some(true) => "(+)",
                        Some(false) => "(-)",
                        None => "",
                    };
                    match u.kind {
                        UseDepKind::Enabled => format!("{}{d}", u.flag),
                        UseDepKind::Disabled => format!("-{}{d}", u.flag),
                        UseDepKind::Same => format!("{}{d}=", u.flag),
                        UseDepKind::Opposite => format!("!{}{d}=", u.flag),
                        UseDepKind::IfEnabled => format!("{}{d}?", u.flag),
                        UseDepKind::IfDisabledThenDisabled => format!("!{}{d}?", u.flag),
                    }
                })
                .collect();
            write!(f, "[{}]", parts.join(","))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        for s in [
            ">=dev-libs/openssl-3.0:0/3=[static-libs?,-bindist(-)]",
            "!!<sys-apps/foo-1.2-r3",
            "=dev-lang/python-3.12*:3.12",
            "dev-qt/qtbase:6=",
            "app-misc/foo-bar::guru",
            "~x11-libs/gtk+-3.24.0",
        ] {
            assert_eq!(Atom::parse(s).unwrap().to_string(), s);
        }
        let a = Atom::parse("x11-libs/gtk+:3").unwrap();
        assert_eq!(a.pn, "gtk+");
        let a = Atom::parse("=dev-libs/foo-2-bar-1.0-r1").unwrap();
        assert_eq!(a.pn, "foo-2-bar");
        assert_eq!(a.version.unwrap().to_string(), "1.0-r1");
    }
    #[test]
    fn matching() {
        let v = Version::parse("3.12.4").unwrap();
        let t = MatchTarget { cat: "dev-lang", pn: "python", version: &v, slot: "3.12", subslot: "3.12", repo: "gentoo", iuse: None, use_: None };
        assert!(Atom::parse("=dev-lang/python-3.12*").unwrap().matches(&t, None));
        assert!(Atom::parse(">=dev-lang/python-3.11:3.12").unwrap().matches(&t, None));
        assert!(!Atom::parse("<dev-lang/python-3.12").unwrap().matches(&t, None));
    }
}
