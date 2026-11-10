//! Building a package from its ebuild: build directory, phases, image post-processing.

use crate::config::{Config, UseInfo};
use crate::ebuild_env::{self, PhaseOpts, Runner};
use crate::fetch;
use crate::repo::Ebuild;
use crate::util::{info, warn};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Build {
    pub builddir: PathBuf,
    pub image: PathBuf,
    pub temp: PathBuf,
    /// latest saved environment
    pub env_file: PathBuf,
    pub base_env: Vec<(String, String)>,
    pub a: Vec<String>,
    /// metadata directory of the binary package this came from
    pub bin_meta: Option<PathBuf>,
}

fn is_ident(k: &str) -> bool {
    !k.is_empty()
        && k.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The environment every phase starts with.
pub fn phase_env(
    cfg: &Config,
    e: &Ebuild,
    use_: &UseInfo,
    builddir: &Path,
    a: &[String],
) -> Vec<(String, String)> {
    let mut skip: Vec<String> = cfg.var("PROFILE_ONLY_VARIABLES").split_whitespace().map(|s| s.to_string()).collect();
    skip.extend(cfg.var("ENV_UNSET").split_whitespace().map(|s| s.to_string()));
    let mut env: Vec<(String, String)> = Vec::new();
    let set = |env: &mut Vec<(String, String)>, k: &str, v: String| {
        if let Some(x) = env.iter_mut().find(|(n, _)| n == k) {
            x.1 = v;
        } else {
            env.push((k.to_string(), v));
        }
    };
    let mut keys: Vec<&String> = cfg.vars.keys().collect();
    keys.sort();
    for k in keys {
        if is_ident(k) && !skip.contains(k) {
            set(&mut env, k, cfg.vars[k].clone());
        }
    }
    for (k, v) in cfg.pkg_env_vars(e) {
        set(&mut env, &k, v);
    }
    // USE and the USE_EXPAND variables, filtered to what this package has
    let mut on: Vec<&String> = use_.enabled.iter().collect();
    on.sort();
    set(&mut env, "USE", on.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
    for ue in cfg.use_expand_vars() {
        let p = format!("{}_", ue.to_lowercase());
        let vals: Vec<&str> = on.iter().filter_map(|f| f.strip_prefix(&p)).collect();
        set(&mut env, ue, vals.join(" "));
    }
    let mut eff: Vec<&String> = use_.effective.iter().collect();
    eff.sort();
    set(&mut env, "SPAS_IUSE_EFFECTIVE", eff.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
    for (k, v) in ebuild_env::pkg_vars(&e.cat, &e.pn, &e.ver) {
        set(&mut env, &k, v);
    }
    let eapi = e.meta.get("EAPI").to_string();
    let repo = &cfg.repos.list[e.repo];
    let b = |p: &str| builddir.join(p).display().to_string();
    let root = if cfg.root == Path::new("/") { String::new() } else { cfg.root.display().to_string().trim_end_matches('/').to_string() };
    let pairs: Vec<(&str, String)> = vec![
        ("EAPI", eapi.clone()),
        ("BASH_COMPAT", match eapi.as_str() { "7" => "4.2", "8" => "5.0", _ => "5.3" }.to_string()),
        ("EBUILD", e.path.display().to_string()),
        ("FILESDIR", e.path.parent().unwrap().join("files").display().to_string()),
        ("DISTDIR", b("distdir")),
        ("PORTAGE_ACTUAL_DISTDIR", fetch::distdir(cfg).display().to_string()),
        ("WORKDIR", b("work")),
        ("T", b("temp")),
        ("D", b("image")),
        ("ED", b("image")),
        ("HOME", b("homedir")),
        ("TMPDIR", b("temp")),
        ("PORTAGE_BUILDDIR", builddir.display().to_string()),
        ("PORTAGE_TMPDIR", cfg.var("PORTAGE_TMPDIR").to_string()),
        ("PORTAGE_CONFIGROOT", "/".to_string()),
        ("PORTAGE_REPO_NAME", repo.name.clone()),
        ("ROOT", root.clone()),
        ("EROOT", root.clone()),
        ("SYSROOT", String::new()),
        ("ESYSROOT", String::new()),
        ("BROOT", String::new()),
        ("EPREFIX", String::new()),
        ("MERGE_TYPE", "source".to_string()),
        ("A", a.join(" ")),
        ("PATH", ebuild_env::PATH.to_string()),
        ("TERM", std::env::var("TERM").unwrap_or_else(|_| "dumb".into())),
        ("SPAS_BIN", ebuild_env::self_exe()),
        ("SPAS_ECLASSDIRS", repo.eclass_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")),
        ("SPAS_BASHRCS", cfg.bashrcs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")),
        ("SPAS_ROOT", cfg.root.display().to_string()),
    ];
    for (k, v) in pairs {
        set(&mut env, k, v);
    }
    if !crate::util::color() {
        set(&mut env, "NOCOLOR", "true".into());
    }
    env
}

fn chown_r(p: &Path, uid: u32, gid: u32) {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    unsafe {
        libc::lchown(c.as_ptr(), uid, gid);
    }
    if p.is_dir() && !p.is_symlink() {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                chown_r(&e.path(), uid, gid);
            }
        }
    }
}

pub struct BuildOpts {
    pub replacing: Vec<String>,
    pub quiet: bool,
}

pub fn runner<'a>(cfg: &'a Config, env: Vec<(String, String)>, log: Option<PathBuf>, quiet: bool) -> Runner<'a> {
    let ids = if crate::util::is_root() && cfg.feature("userpriv") { crate::util::lookup_user("portage") } else { None };
    Runner { cfg, env, log, portage_ids: ids, quiet }
}

pub fn build(cfg: &Config, e: &Ebuild, use_: &UseInfo, opts: &BuildOpts) -> Result<Build, String> {
    let files = fetch::distfiles(cfg, e, use_)?;
    fetch::fetch(cfg, e, use_, &files)?;
    let a: Vec<String> = files.iter().map(|d| d.name.clone()).collect();

    let tmp = cfg.var("PORTAGE_TMPDIR");
    let builddir = PathBuf::from(if tmp.is_empty() { "/var/tmp" } else { tmp }).join("spas").join(&e.cat).join(e.pf());
    if builddir.exists() {
        std::fs::remove_dir_all(&builddir).map_err(|err| format!("{}: {err}", builddir.display()))?;
    }
    for d in ["work", "temp", "homedir", "distdir"] {
        std::fs::create_dir_all(builddir.join(d)).map_err(|err| format!("{}: {err}", builddir.display()))?;
    }
    let distdir = fetch::distdir(cfg);
    for f in &a {
        let _ = std::os::unix::fs::symlink(distdir.join(f), builddir.join("distdir").join(f));
    }
    let restrict_userpriv = fetch::restrict_has(e, use_, "userpriv");
    let mut r = runner(cfg, phase_env(cfg, e, use_, &builddir, &a), Some(builddir.join("temp/build.log")), opts.quiet);
    if restrict_userpriv {
        r.portage_ids = None;
    }
    if let Some((uid, gid)) = r.portage_ids {
        chown_r(&builddir, uid, gid);
        // the build user must be able to reach the build directory
        for p in builddir.ancestors().skip(1) {
            if p == Path::new(tmp) || p.parent().is_none() {
                break;
            }
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
        }
    }
    let replacing = opts.replacing.join(" ");
    let extra = vec![("REPLACING_VERSIONS".to_string(), replacing.clone())];
    let live = fetch::properties_has(e, use_, "live");
    let sandbox = cfg.feature("sandbox");
    let usersandbox = cfg.feature("usersandbox");
    let netsandbox = cfg.feature("network-sandbox");
    let temp = builddir.join("temp");
    let phases_defined: Vec<String> = e.meta.get("DEFINED_PHASES").split_whitespace().map(|s| s.to_string()).collect();
    let test = cfg.feature("test") && !fetch::restrict_has(e, use_, "test");

    let mut env_in: Option<PathBuf> = None;
    let mut step = 0;
    let mut run = |phase: &str, o: PhaseOpts, env_in: &mut Option<PathBuf>| -> Result<(), String> {
        step += 1;
        let out = temp.join(format!("environment.{step}.{phase}"));
        r.phase(phase, o, env_in.as_deref(), Some(&out), &extra)?;
        if let Some((uid, gid)) = r.portage_ids {
            chown_r(&out, uid, gid);
        }
        *env_in = Some(out);
        Ok(())
    };
    let root_opts = PhaseOpts { drop_privs: false, sandbox: false, no_network: false };
    let user = |net: bool| PhaseOpts { drop_privs: true, sandbox: sandbox && usersandbox, no_network: netsandbox && !net };

    if phases_defined.iter().any(|p| p == "pretend") {
        run("pretend", root_opts, &mut env_in)?;
    }
    info(&format!("Building {}", crate::util::bold(&e.cpv())));
    run("setup", PhaseOpts { sandbox: false, ..root_opts }, &mut env_in)?;
    run("unpack", user(live), &mut env_in)?;
    run("prepare", user(false), &mut env_in)?;
    run("configure", user(false), &mut env_in)?;
    run("compile", user(false), &mut env_in)?;
    if test {
        run("test", user(false), &mut env_in)?;
    }
    std::fs::create_dir_all(builddir.join("image")).map_err(|err| err.to_string())?;
    run("install", PhaseOpts { drop_privs: false, sandbox, no_network: netsandbox }, &mut env_in)?;

    let image = builddir.join("image");
    postprocess(cfg, e, use_, &image, &temp)?;
    let env_file = env_in.unwrap();
    Ok(Build { builddir, image, temp, env_file, base_env: r.env.clone(), a, bin_meta: None })
}

fn fresh_builddir(cfg: &Config, e: &Ebuild) -> Result<PathBuf, String> {
    let tmp = cfg.var("PORTAGE_TMPDIR");
    let builddir = PathBuf::from(if tmp.is_empty() { "/var/tmp" } else { tmp }).join("spas").join(&e.cat).join(e.pf());
    if builddir.exists() {
        std::fs::remove_dir_all(&builddir).map_err(|err| format!("{}: {err}", builddir.display()))?;
    }
    for d in ["temp", "homedir", "image"] {
        std::fs::create_dir_all(builddir.join(d)).map_err(|err| format!("{}: {err}", builddir.display()))?;
    }
    Ok(builddir)
}

/// Prepare a prebuilt package for merging: download, verify, unpack, run pkg_setup.
pub fn from_binary(cfg: &Config, e: &Ebuild, use_: &UseInfo, bin: &crate::binhost::BinPkg, opts: &BuildOpts) -> Result<Build, String> {
    let gpkg = crate::binhost::fetch(cfg, bin)?;
    let builddir = fresh_builddir(cfg, e)?;
    let temp = builddir.join("temp");
    let image = builddir.join("image");
    let meta = builddir.join("binmeta");
    info(&format!("Unpacking binary package {}", crate::util::bold(&bin.file_name())));
    crate::binhost::unpack(&gpkg, &builddir, &meta, &image, cfg.feature("binpkg-request-signature"))?;

    // the environment saved when the package was built
    let env_file = temp.join("environment.binary");
    let f = std::fs::File::open(meta.join("environment.bz2")).map_err(|err| format!("binary package has no environment: {err}"))?;
    let mut s = Vec::new();
    std::io::Read::read_to_end(&mut bzip2::read::BzDecoder::new(f), &mut s).map_err(|err| err.to_string())?;
    std::fs::write(&env_file, ebuild_env::strip_locale(&s)).map_err(|err| err.to_string())?;

    let mut env = phase_env(cfg, e, use_, &builddir, &[]);
    for (k, v) in env.iter_mut() {
        if k == "MERGE_TYPE" {
            *v = "binary".into();
        }
    }
    let mut r = runner(cfg, env, Some(temp.join("build.log")), opts.quiet);
    r.portage_ids = None;
    let extra = vec![("REPLACING_VERSIONS".to_string(), opts.replacing.join(" "))];
    let root_opts = PhaseOpts { drop_privs: false, sandbox: false, no_network: false };
    let defined: Vec<&str> = e.meta.get("DEFINED_PHASES").split_whitespace().collect();
    let mut cur = env_file.clone();
    if defined.contains(&"pretend") {
        let out = temp.join("environment.pretend");
        r.phase("pretend", root_opts, Some(&cur), Some(&out), &extra)?;
        cur = out;
    }
    let out = temp.join("environment.setup");
    r.phase("setup", root_opts, Some(&cur), Some(&out), &extra)?;
    Ok(Build { builddir, image, temp, env_file: out, base_env: r.env.clone(), a: vec![], bin_meta: Some(meta) })
}

// ---------------------------------------------------------------- image post-processing

fn list_file(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(|s| s.to_string()).collect()
}

fn under(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        let p = p.trim_end_matches('/');
        p.is_empty() || path == p || path.starts_with(&format!("{p}/")) || crate::util::glob_match(p, path)
    })
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

fn rel(image: &Path, p: &Path) -> String {
    format!("/{}", p.strip_prefix(image).unwrap().display())
}

fn postprocess(cfg: &Config, e: &Ebuild, use_: &UseInfo, image: &Path, temp: &Path) -> Result<(), String> {
    for f in ["usr/share/info/dir", "usr/share/info/dir.gz", "usr/share/info/dir.bz2", "usr/share/info/dir.xz"] {
        let _ = std::fs::remove_file(image.join(f));
    }
    let mut all = Vec::new();
    walk(image, &mut all);

    // ---- strip
    let restrict_strip = fetch::restrict_has(e, use_, "strip");
    let strip_incl = list_file(&temp.join(".spas_dostrip_include"));
    let strip_excl = list_file(&temp.join(".spas_dostrip_exclude"));
    if !cfg.feature("nostrip") {
        let chost = cfg.var("CHOST");
        let strip_cmd = if !cfg.var("STRIP").is_empty() {
            cfg.var("STRIP").to_string()
        } else if !chost.is_empty() && Path::new(&format!("/usr/bin/{chost}-strip")).exists() {
            format!("{chost}-strip")
        } else {
            "strip".to_string()
        };
        let flags = {
            let f = cfg.var("PORTAGE_STRIP_FLAGS");
            if f.is_empty() {
                "--strip-unneeded -N __gentoo_check_ldflags__ -R .comment -R .GCC.command.line -R .note.gnu.gold-version".to_string()
            } else {
                f.to_string()
            }
        };
        let mut stripped = 0;
        for p in &all {
            let Ok(md) = std::fs::symlink_metadata(p) else { continue };
            if !md.is_file() {
                continue;
            }
            let r = rel(image, p);
            if r.starts_with("/usr/lib/debug/") || under(&r, &strip_excl) {
                continue;
            }
            if restrict_strip && !under(&r, &strip_incl) {
                continue;
            }
            let mut head = [0u8; 18];
            let n = std::fs::File::open(p).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
            let args: Option<Vec<String>> = if n >= 18 && &head[..4] == b"\x7fELF" {
                let et = if head[5] == 2 { u16::from_be_bytes([head[16], head[17]]) } else { u16::from_le_bytes([head[16], head[17]]) };
                match et {
                    1 => Some(vec!["-g".into()]),
                    2 | 3 => Some(flags.split_whitespace().map(|s| s.to_string()).collect()),
                    _ => None,
                }
            } else if n >= 8 && &head[..8] == b"!<arch>\n" && r.ends_with(".a") {
                Some(vec!["-g".into()])
            } else {
                None
            };
            let Some(args) = args else { continue };
            let mode = md.mode();
            // make sure we can write it, then restore the mode (setuid etc.)
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o200));
            let st = std::process::Command::new(&strip_cmd)
                .args(&args)
                .arg(p)
                .env("PATH", ebuild_env::PATH)
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode & 0o7777));
            match st {
                Ok(s) if s.success() => stripped += 1,
                _ => warn(&format!("strip failed on {r}")),
            }
        }
        if stripped > 0 {
            info(&format!("Stripped {stripped} files"));
        }
    }

    // ---- docompress
    let comp = cfg.var("PORTAGE_COMPRESS").to_string();
    if !comp.is_empty() {
        let suffix = match Path::new(&comp).file_name().and_then(|s| s.to_str()).unwrap_or("") {
            "gzip" | "pigz" => ".gz",
            "bzip2" | "pbzip2" | "lbzip2" => ".bz2",
            "xz" | "pixz" => ".xz",
            "zstd" | "pzstd" => ".zst",
            "lzip" | "plzip" => ".lz",
            "lz4" => ".lz4",
            "lzma" => ".lzma",
            _ => "",
        };
        if suffix.is_empty() {
            warn(&format!("unknown PORTAGE_COMPRESS '{comp}', not compressing docs"));
            return Ok(());
        }
        let mut incl = vec!["/usr/share/doc".to_string(), "/usr/share/info".to_string(), "/usr/share/man".to_string()];
        incl.extend(list_file(&temp.join(".spas_docompress_include")));
        let mut excl = vec![format!("/usr/share/doc/{}/html", e.pf())];
        excl.extend(list_file(&temp.join(".spas_docompress_exclude")));
        let ex_suffixes = format!(
            r"\.({}|gz|bz2|xz|zst|lz|lz4|lzma|Z|z|tgz|tbz2|zip)$",
            cfg.var("PORTAGE_COMPRESS_EXCLUDE_SUFFIXES").split_whitespace().collect::<Vec<_>>().join("|")
        );
        let re = regex::Regex::new(&ex_suffixes).ok();
        let mut targets = Vec::new();
        for p in &all {
            let Ok(md) = std::fs::symlink_metadata(p) else { continue };
            if !md.is_file() {
                continue;
            }
            let r = rel(image, p);
            if !under(&r, &incl) || under(&r, &excl) {
                continue;
            }
            if re.as_ref().map(|re| re.is_match(&r)).unwrap_or(false) {
                continue;
            }
            if md.len() == 0 {
                continue;
            }
            targets.push(p.clone());
        }
        let flags: Vec<String> = cfg.var("PORTAGE_COMPRESS_FLAGS").split_whitespace().map(|s| s.to_string()).collect();
        for chunk in targets.chunks(200) {
            let st = std::process::Command::new(&comp)
                .args(&flags)
                .arg("-f")
                .args(chunk)
                .env("PATH", ebuild_env::PATH)
                .status()
                .map_err(|err| format!("{comp}: {err}"))?;
            if !st.success() {
                warn(&format!("{comp} reported errors while compressing documentation"));
            }
        }
        // symlinks pointing at files that are now compressed
        for p in &all {
            let Ok(md) = std::fs::symlink_metadata(p) else { continue };
            if !md.file_type().is_symlink() {
                continue;
            }
            let r = rel(image, p);
            if !under(&r, &incl) || under(&r, &excl) {
                continue;
            }
            let Ok(target) = std::fs::read_link(p) else { continue };
            let resolved = if target.is_absolute() { image.join(target.strip_prefix("/").unwrap()) } else { p.parent().unwrap().join(&target) };
            let comp_target = PathBuf::from(format!("{}{suffix}", resolved.display()));
            if !resolved.exists() && comp_target.exists() {
                let new_link = PathBuf::from(format!("{}{suffix}", p.display()));
                let _ = std::fs::remove_file(p);
                let _ = std::os::unix::fs::symlink(format!("{}{suffix}", target.display()), &new_link);
            }
        }
    }
    Ok(())
}
