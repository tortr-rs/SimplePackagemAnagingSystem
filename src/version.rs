//! Package versions, following the comparison rules of the Package Manager Specification.

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SuffixKind {
    Alpha,
    Beta,
    Pre,
    Rc,
    P,
}

impl SuffixKind {
    fn name(self) -> &'static str {
        match self {
            SuffixKind::Alpha => "alpha",
            SuffixKind::Beta => "beta",
            SuffixKind::Pre => "pre",
            SuffixKind::Rc => "rc",
            SuffixKind::P => "p",
        }
    }
    /// Ordering rank relative to "no suffix" (which sits between rc and p).
    fn rank(self) -> i32 {
        match self {
            SuffixKind::Alpha => 0,
            SuffixKind::Beta => 1,
            SuffixKind::Pre => 2,
            SuffixKind::Rc => 3,
            SuffixKind::P => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suffix {
    pub kind: SuffixKind,
    /// digits as written, may be empty
    pub num: String,
}

#[derive(Clone, Debug)]
pub struct Version {
    pub nums: Vec<String>,
    pub letter: Option<char>,
    pub suffixes: Vec<Suffix>,
    /// revision digits as written ("" when there is no -r)
    pub rev: String,
    raw: String,
}

/// Compare two digit strings as arbitrarily large integers.
fn cmp_int(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let raw = s.to_string();
        let (main, rev) = match s.rfind("-r") {
            Some(i) if s[i + 2..].chars().all(|c| c.is_ascii_digit()) && i + 2 < s.len() => {
                (&s[..i], s[i + 2..].to_string())
            }
            _ => (s, String::new()),
        };
        let b = main.as_bytes();
        let mut i = 0;
        let mut nums = Vec::new();
        loop {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                return None;
            }
            nums.push(main[start..i].to_string());
            if i < b.len() && b[i] == b'.' {
                i += 1;
                continue;
            }
            break;
        }
        let mut letter = None;
        if i < b.len() && b[i].is_ascii_lowercase() {
            letter = Some(b[i] as char);
            i += 1;
        }
        let mut suffixes = Vec::new();
        while i < b.len() {
            if b[i] != b'_' {
                return None;
            }
            i += 1;
            let rest = &main[i..];
            let (kind, len) = if rest.starts_with("alpha") {
                (SuffixKind::Alpha, 5)
            } else if rest.starts_with("beta") {
                (SuffixKind::Beta, 4)
            } else if rest.starts_with("pre") {
                (SuffixKind::Pre, 3)
            } else if rest.starts_with("rc") {
                (SuffixKind::Rc, 2)
            } else if rest.starts_with('p') {
                (SuffixKind::P, 1)
            } else {
                return None;
            };
            i += len;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            suffixes.push(Suffix { kind, num: main[start..i].to_string() });
        }
        Some(Version { nums, letter, suffixes, rev, raw })
    }

    /// The version without its revision (PV).
    pub fn pv(&self) -> &str {
        if self.rev.is_empty() {
            &self.raw
        } else {
            &self.raw[..self.raw.len() - self.rev.len() - 2]
        }
    }

    /// PR, which is "r0" when there is no revision.
    pub fn pr(&self) -> String {
        if self.rev.is_empty() {
            "r0".into()
        } else {
            format!("r{}", self.rev)
        }
    }

    /// Comparison ignoring the revision.
    pub fn cmp_base(&self, other: &Version) -> Ordering {
        // first component: integer
        let o = cmp_int(&self.nums[0], &other.nums[0]);
        if o != Ordering::Equal {
            return o;
        }
        let n = self.nums.len().min(other.nums.len());
        for k in 1..n {
            let (a, b) = (&self.nums[k], &other.nums[k]);
            let o = if a.starts_with('0') || b.starts_with('0') {
                a.trim_end_matches('0').cmp(b.trim_end_matches('0'))
            } else {
                cmp_int(a, b)
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        let o = self.nums.len().cmp(&other.nums.len());
        if o != Ordering::Equal {
            return o;
        }
        let o = self.letter.cmp(&other.letter);
        if o != Ordering::Equal {
            return o;
        }
        let n = self.suffixes.len().min(other.suffixes.len());
        for k in 0..n {
            let (a, b) = (&self.suffixes[k], &other.suffixes[k]);
            let o = a.kind.rank().cmp(&b.kind.rank());
            if o != Ordering::Equal {
                return o;
            }
            let o = cmp_int(&a.num, &b.num);
            if o != Ordering::Equal {
                return o;
            }
        }
        if self.suffixes.len() > n {
            return if self.suffixes[n].kind == SuffixKind::P { Ordering::Greater } else { Ordering::Less };
        }
        if other.suffixes.len() > n {
            return if other.suffixes[n].kind == SuffixKind::P { Ordering::Less } else { Ordering::Greater };
        }
        Ordering::Equal
    }

    /// The `=pkg-1.2*` style prefix match: every component that is spelled out in
    /// `pattern` has to be equal in `self`.
    pub fn matches_prefix(&self, pattern: &Version) -> bool {
        if pattern.nums.len() > self.nums.len() {
            return false;
        }
        for (k, (a, b)) in pattern.nums.iter().zip(&self.nums).enumerate() {
            let eq = if k == 0 || !(a.starts_with('0') || b.starts_with('0')) {
                cmp_int(a, b) == Ordering::Equal
            } else {
                a.trim_end_matches('0') == b.trim_end_matches('0')
            };
            if !eq {
                return false;
            }
        }
        if pattern.letter.is_some() {
            if pattern.nums.len() != self.nums.len() || pattern.letter != self.letter {
                return false;
            }
        }
        if !pattern.suffixes.is_empty() {
            if pattern.nums.len() != self.nums.len() || pattern.letter != self.letter {
                return false;
            }
            if pattern.suffixes.len() > self.suffixes.len() {
                return false;
            }
            let last = pattern.suffixes.len() - 1;
            for (k, (a, b)) in pattern.suffixes.iter().zip(&self.suffixes).enumerate() {
                if a.kind != b.kind {
                    return false;
                }
                // the final suffix may omit its number and then matches any number
                let any_num = k == last && a.num.is_empty();
                if !any_num && cmp_int(&a.num, &b.num) != Ordering::Equal {
                    return false;
                }
            }
        }
        if !pattern.rev.is_empty() {
            if pattern.suffixes.len() != self.suffixes.len() || cmp_int(&pattern.rev, &self.rev) != Ordering::Equal {
                return false;
            }
        }
        true
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_base(other).then_with(|| cmp_int(&self.rev, &other.rev))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[allow(dead_code)]
pub fn suffix_name(k: SuffixKind) -> &'static str {
    k.name()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    #[test]
    fn ordering() {
        let ordered = [
            "1.0_alpha", "1.0_alpha1", "1.0_beta", "1.0_pre1", "1.0_rc1", "1.0", "1.0-r1", "1.0_p1", "1.0a",
            "1.0.1", "1.01", "1.1", "1.2", "1.10", "2",
        ];
        for w in ordered.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("1.0"), v("1.0-r0"));
        assert_eq!(v("1.0"), v("1.00"));
        assert!(v("12.0.0_rc3") < v("12.0.0"));
    }
    #[test]
    fn parsing() {
        assert!(Version::parse("1.0_foo").is_none());
        assert!(Version::parse("a1").is_none());
        assert_eq!(v("2.3_p20240101-r2").pv(), "2.3_p20240101");
        assert_eq!(v("2.3-r2").pr(), "r2");
    }
    #[test]
    fn prefix() {
        assert!(v("1.2.3").matches_prefix(&v("1.2")));
        assert!(!v("1.20").matches_prefix(&v("1.2")));
        assert!(v("1.2_rc4").matches_prefix(&v("1.2_rc")));
    }
}
