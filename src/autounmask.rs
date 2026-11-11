//! Proposing and writing the smallest /etc/portage change that makes a
//! dependency installable: a package.use line, a ~arch keyword or a license.
//! package.mask is never overridden automatically.

use crate::util::info;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Use,
    Keywords,
    License,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: Kind,
    /// "=cat/pn-ver"
    pub atom: String,
    pub tokens: Vec<String>,
    /// the dependency chain that needed it
    pub required_by: String,
}

impl Change {
    pub fn line(&self) -> String {
        format!("{} {}", self.atom, self.tokens.join(" "))
    }
    fn file_name(&self) -> &'static str {
        match self.kind {
            Kind::Use => "package.use",
            Kind::Keywords => "package.accept_keywords",
            Kind::License => "package.license",
        }
    }
}

/// Where a change of this kind goes: a file inside the package.* directory,
/// or the package.* file itself when the user keeps a single file.
pub fn target_file(config_root: &Path, c: &Change) -> PathBuf {
    let p = config_root.join("etc/portage").join(c.file_name());
    if p.is_file() { p } else { p.join("zz-hoard-autounmask") }
}

pub fn show(config_root: &Path, changes: &[Change]) {
    let mut files: Vec<PathBuf> = changes.iter().map(|c| target_file(config_root, c)).collect();
    files.dedup();
    for f in files {
        println!("\n{}:", crate::util::bold(&f.display().to_string()));
        for c in changes.iter().filter(|c| target_file(config_root, c) == f) {
            println!("  # required by {}", c.required_by);
            println!("  {}", crate::util::green(&c.line()));
        }
    }
    println!();
}

pub fn write(config_root: &Path, changes: &[Change]) -> Result<(), String> {
    for c in changes {
        let f = target_file(config_root, c);
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut text = std::fs::read_to_string(&f).unwrap_or_default();
        if text.lines().any(|l| l.trim() == c.line()) {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("# required by {} (added by hoard)\n{}\n", c.required_by, c.line()));
        std::fs::write(&f, text).map_err(|e| format!("{}: {e}", f.display()))?;
        info(&format!("Added \"{}\" to {}", c.line(), f.display()));
    }
    Ok(())
}
