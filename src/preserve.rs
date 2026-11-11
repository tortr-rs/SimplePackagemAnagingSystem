//! preserve-libs: when an upgrade would delete a shared library that other
//! installed programs still link against, keep it until they are rebuilt.
//! Uses Portage's registry (/var/lib/portage/preserved_libs_registry), so
//! Portage and hoard see the same preserved libraries.

use crate::config::Config;
use crate::util::{info, warn};
use crate::vdb::{Entry, Installed, Vdb};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One NEEDED.ELF.2 line.
struct Needed {
    path: String,
    soname: String,
    needed: Vec<String>,
    multilib: String,
}

fn parse_needed(text: &str) -> Vec<Needed> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(';').collect();
            (f.len() >= 6).then(|| Needed {
                path: f[1].to_string(),
                soname: f[2].to_string(),
                needed: f[4].split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect(),
                multilib: f[5].to_string(),
            })
        })
        .collect()
}

fn root_path(root: &Path, rel: &str) -> PathBuf {
    root.join(rel.trim_start_matches('/'))
}

/// (multilib, soname) -> packages whose files link against it.
fn consumers(vdb: &Vdb, skip: &[String]) -> HashMap<(String, String), Vec<String>> {
    let mut m: HashMap<(String, String), Vec<String>> = HashMap::new();
    for p in &vdb.pkgs {
        let cpv = p.cpv();
        if skip.contains(&cpv) {
            continue;
        }
        for n in parse_needed(&p.read("NEEDED.ELF.2")) {
            for s in n.needed {
                let v = m.entry((n.multilib.clone(), s)).or_default();
                if !v.contains(&cpv) {
                    v.push(cpv.clone());
                }
            }
        }
    }
    m
}

/// (multilib, soname) provided by installed packages other than `skip`.
fn providers(vdb: &Vdb, skip: &[String]) -> HashSet<(String, String)> {
    let mut s = HashSet::new();
    for p in &vdb.pkgs {
        if skip.contains(&p.cpv()) {
            continue;
        }
        let mut cat = String::new();
        for tok in p.read("PROVIDES").split_whitespace() {
            if let Some(c) = tok.strip_suffix(':') {
                cat = c.to_string();
            } else {
                s.insert((cat.clone(), tok.to_string()));
            }
        }
    }
    s
}

/// Which of `old`'s files must be kept because they are libraries other
/// packages still need. `removed` are the paths the upgrade is about to delete;
/// `new_needed_elf2` / `new_provides` describe the incoming version.
pub fn select(cfg: &Config, vdb: &Vdb, old: &Installed, new_cpv: &str, removed: &HashSet<String>, new_needed_elf2: &str, new_provides: &str) -> Vec<Entry> {
    if !cfg.feature("preserve-libs") {
        return vec![];
    }
    let old_needed = parse_needed(&old.read("NEEDED.ELF.2"));
    let skip = vec![old.cpv(), new_cpv.to_string()];
    let mut provided: HashSet<(String, String)> = providers(vdb, &skip);
    let mut cat = String::new();
    for tok in new_provides.split_whitespace() {
        if let Some(c) = tok.strip_suffix(':') {
            cat = c.to_string();
        } else {
            provided.insert((cat.clone(), tok.to_string()));
        }
    }
    let users = consumers(vdb, &skip);
    // the new version's own files count as consumers too
    let new_users: HashSet<(String, String)> =
        parse_needed(new_needed_elf2).into_iter().flat_map(|n| n.needed.into_iter().map(move |s| (n.multilib.clone(), s))).collect();
    let contents = old.contents();
    let mut keep: Vec<Entry> = Vec::new();
    for lib in old_needed.iter().filter(|n| !n.soname.is_empty() && !n.multilib.is_empty()) {
        let key = (lib.multilib.clone(), lib.soname.clone());
        if provided.contains(&key) {
            continue;
        }
        let needed_by = users.get(&key).map(|v| v.len()).unwrap_or(0) > 0 || new_users.contains(&key);
        if !needed_by {
            continue;
        }
        // the library file itself, and the soname symlink pointing at it
        let dir = lib.path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let link = format!("{dir}/{}", lib.soname);
        for e in &contents {
            let p = e.path();
            let wanted = match e {
                Entry::Obj { .. } => p == lib.path,
                Entry::Sym { .. } => p == link && p != lib.path,
                _ => false,
            };
            if wanted && removed.contains(p) && !keep.iter().any(|k| k.path() == p) {
                keep.push(e.clone());
            }
        }
    }
    keep
}

// ---------------------------------------------------------------- registry

pub fn registry_path(cfg: &Config) -> PathBuf {
    cfg.root.join("var/lib/portage/preserved_libs_registry")
}

/// "cat/pn:slot" -> (cpv, counter, paths)
type Registry = BTreeMap<String, (String, String, Vec<String>)>;

fn load_registry(cfg: &Config) -> Registry {
    let text = std::fs::read_to_string(registry_path(cfg)).unwrap_or_default();
    let mut r = Registry::new();
    let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&text) else { return r };
    for (k, v) in m {
        let Some(a) = v.as_array() else { continue };
        if a.len() < 3 {
            continue;
        }
        let s = |x: &serde_json::Value| x.as_str().map(|s| s.to_string()).unwrap_or_else(|| x.to_string());
        let paths: Vec<String> = a[2].as_array().map(|p| p.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
        r.insert(k, (s(&a[0]), s(&a[1]), paths));
    }
    r
}

fn store_registry(cfg: &Config, r: &Registry) -> Result<(), String> {
    let path = registry_path(cfg);
    let mut m = serde_json::Map::new();
    for (k, (cpv, counter, paths)) in r {
        if paths.is_empty() {
            continue;
        }
        m.insert(k.clone(), serde_json::json!([cpv, counter, paths]));
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(m)).map_err(|e| e.to_string())?;
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = path.with_extension("hoard-new");
    std::fs::write(&tmp, text.replace("  ", "\t")).map_err(|e| format!("{}: {e}", tmp.display()))?;
    // Portage keeps it root:portage 0660
    if let Some((_, gid)) = crate::util::lookup_user("portage") {
        let c = std::ffi::CString::new(tmp.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: plain chown on a path we just wrote
        unsafe {
            libc::chown(c.as_ptr(), 0, gid);
        }
    }
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o660));
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Record libraries kept for the package that now owns them.
pub fn register(cfg: &Config, cp_slot: &str, cpv: &str, counter: &str, paths: &[String]) -> Result<(), String> {
    let mut r = load_registry(cfg);
    let e = r.entry(cp_slot.to_string()).or_insert_with(|| (cpv.to_string(), counter.to_string(), vec![]));
    e.0 = cpv.to_string();
    e.1 = counter.to_string();
    for p in paths {
        if !e.2.contains(p) {
            e.2.push(p.clone());
        }
    }
    store_registry(cfg, &r)
}

/// soname of a preserved file: its DT_SONAME, or for a symlink its name.
fn soname_of(cfg: &Config, p: &str) -> Option<(String, String)> {
    let full = root_path(&cfg.root, p);
    let md = std::fs::symlink_metadata(&full).ok()?;
    if md.file_type().is_symlink() {
        return None; // the real file carries the soname
    }
    let i = crate::elf::scan_file(&full)?;
    (!i.soname.is_empty() && !i.multilib.is_empty()).then(|| (i.multilib, i.soname))
}

/// Packages that still link against preserved libraries: @preserved-rebuild.
pub fn rebuild_targets(cfg: &Config, vdb: &Vdb) -> Vec<String> {
    let r = load_registry(cfg);
    let users = consumers(vdb, &[]);
    let mut out: Vec<String> = Vec::new();
    for (owner_key, (_, _, paths)) in &r {
        for p in paths {
            let Some(key) = soname_of(cfg, p) else { continue };
            for cpv in users.get(&key).into_iter().flatten() {
                let Some(inst) = vdb.pkgs.iter().find(|x| x.cpv() == *cpv) else { continue };
                let atom = format!("{}:{}", inst.cp(), inst.slot);
                // the package that owns the preserved copy isn't a consumer of it
                if atom == *owner_key || out.contains(&atom) {
                    continue;
                }
                out.push(atom);
            }
        }
    }
    out
}

/// Drop preserved libraries nothing links against any more (and registry
/// entries whose files are gone).
pub fn prune(cfg: &Config) -> Result<(), String> {
    let mut r = load_registry(cfg);
    if r.is_empty() {
        return Ok(());
    }
    let vdb = Vdb::load(&cfg.vdb_path());
    let mut changed = false;
    let keys: Vec<String> = r.keys().cloned().collect();
    for k in keys {
        let (cpv, _, paths) = r.get(&k).cloned().unwrap();
        let owner = vdb.pkgs.iter().find(|p| p.cpv() == cpv);
        // libraries still needed by someone other than their owner
        let skip: Vec<String> = owner.map(|o| vec![o.cpv()]).unwrap_or_default();
        let users = consumers(&vdb, &skip);
        let mut drop_paths: Vec<String> = Vec::new();
        let mut still_needed_dirs_names: HashSet<String> = HashSet::new();
        for p in &paths {
            if std::fs::symlink_metadata(root_path(&cfg.root, p)).is_err() {
                drop_paths.push(p.clone());
                continue;
            }
            if let Some(key) = soname_of(cfg, p) {
                if users.get(&key).map(|v| !v.is_empty()).unwrap_or(false) {
                    still_needed_dirs_names.insert(key.1.clone());
                    continue;
                }
                drop_paths.push(p.clone());
            }
        }
        // a soname symlink goes with its library
        for p in &paths {
            let is_link = std::fs::symlink_metadata(root_path(&cfg.root, p)).map(|m| m.file_type().is_symlink()).unwrap_or(false);
            if is_link {
                let name = p.rsplit('/').next().unwrap_or("");
                if !still_needed_dirs_names.contains(name) && !drop_paths.contains(p) {
                    drop_paths.push(p.clone());
                }
            }
        }
        if drop_paths.is_empty() {
            continue;
        }
        for p in &drop_paths {
            let full = root_path(&cfg.root, p);
            if std::fs::symlink_metadata(&full).is_ok() {
                info(&format!("Removing preserved library {p} (nothing needs it now)"));
                if let Err(e) = std::fs::remove_file(&full) {
                    warn(&format!("{p}: {e}"));
                    continue;
                }
            }
        }
        // and take them out of the owner's CONTENTS
        if let Some(o) = owner {
            let keep: Vec<String> = o.contents().into_iter().filter(|e| !drop_paths.iter().any(|d| d == e.path())).map(|e| e.line()).collect();
            let text: String = keep.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(o.dir.join("CONTENTS"), text).map_err(|e| e.to_string())?;
        }
        let e = r.get_mut(&k).unwrap();
        e.2.retain(|p| !drop_paths.contains(p));
        changed = true;
    }
    if changed {
        store_registry(cfg, &r)?;
        let _ = std::process::Command::new("ldconfig").arg("-r").arg(&cfg.root).env("PATH", crate::ebuild_env::PATH).status();
    }
    Ok(())
}

/// Tell the user what is waiting for a rebuild.
pub fn report(cfg: &Config) {
    let vdb = Vdb::load(&cfg.vdb_path());
    let t = rebuild_targets(cfg, &vdb);
    if !t.is_empty() {
        warn(&format!(
            "{} package(s) still use preserved old libraries: {}\n    rebuild them with: hoard install @preserved-rebuild",
            t.len(),
            t.join(" ")
        ));
    }
}
