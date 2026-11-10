//! Running ebuild code: metadata generation and build/install phases, through
//! the bash library in ebuild.sh.

use crate::config::Config;
use crate::repo::{Meta, Repo, Repos};
use crate::util::md5_file;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const LIB: &str = include_str!("ebuild.sh");

pub const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Write the bash library somewhere every user can read it, once per process.
pub fn lib_path() -> PathBuf {
    use std::sync::OnceLock;
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("spas-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("ebuild.sh");
        let _ = std::fs::write(&p, LIB);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644));
        p
    })
    .clone()
}

pub fn cleanup_lib() {
    let dir = std::env::temp_dir().join(format!("spas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(dir);
}

pub fn self_exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "hoard".into())
}

/// EAPI as declared in the ebuild text (PMS: the first EAPI= assignment).
pub fn parse_eapi(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(v) = l.strip_prefix("EAPI=") {
            let v = v.split('#').next().unwrap_or("").trim();
            return v.trim_matches(|c| c == '"' || c == '\'').to_string();
        }
        break;
    }
    "0".into()
}

fn bash_compat(eapi: &str) -> &'static str {
    match eapi {
        "7" => "4.2",
        "8" => "5.0",
        _ => "5.3",
    }
}

pub fn pkg_vars(cat: &str, pn: &str, ver: &crate::version::Version) -> Vec<(String, String)> {
    let pv = ver.pv().to_string();
    let pr = ver.pr();
    let pvr = if ver.rev.is_empty() { pv.clone() } else { format!("{pv}-{pr}") };
    vec![
        ("CATEGORY".into(), cat.into()),
        ("PN".into(), pn.into()),
        ("PV".into(), pv.clone()),
        ("PR".into(), pr),
        ("PVR".into(), pvr.clone()),
        ("P".into(), format!("{pn}-{pv}")),
        ("PF".into(), format!("{pn}-{pvr}")),
    ]
}

pub fn generate_metadata(repos: &Repos, repo: &Repo, cat: &str, pf: &str, ebuild: &Path) -> Result<Meta, String> {
    let (pn, ver) = crate::atom::split_pv(pf).ok_or_else(|| format!("bad ebuild name {pf}"))?;
    let eapi = parse_eapi(ebuild);
    if !crate::repo::SUPPORTED_EAPIS.contains(&eapi.as_str()) {
        let mut map = HashMap::new();
        map.insert("EAPI".to_string(), eapi);
        return Ok(Meta { map });
    }
    let tmp = std::env::temp_dir().join(format!("spas-meta-{}-{pf}", std::process::id()));
    let mut cmd = Command::new("bash");
    cmd.env_clear()
        .arg("--norc")
        .arg("--noprofile")
        .arg(lib_path())
        .env("PATH", PATH)
        .env("SPAS_MODE", "depend")
        .env("SPAS_BIN", self_exe())
        .env("SPAS_META_OUT", &tmp)
        .env("SPAS_ECLASSDIRS", repo.eclass_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":"))
        .env("EBUILD", ebuild)
        .env("EBUILD_PHASE", "depend")
        .env("EAPI", &eapi)
        .env("BASH_COMPAT", bash_compat(&eapi))
        .env("PORTAGE_REPO_NAME", &repo.name)
        .env("FILESDIR", ebuild.parent().unwrap().join("files"))
        .env("WORKDIR", "/nonexistent")
        .env("ROOT", "")
        .env("EPREFIX", "")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in pkg_vars(cat, pn, &ver) {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| format!("cannot run bash: {e}"))?;
    let text = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    if !out.status.success() || text.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("sourcing the ebuild failed:\n{}", err.trim_end()));
    }
    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map.insert("_md5_".into(), md5_file(ebuild).unwrap_or_default());
    let mut ecl = Vec::new();
    for e in map.get("INHERITED").cloned().unwrap_or_default().split_whitespace() {
        if let Some(p) = repos.find_eclass(repo, e) {
            ecl.push(format!("{e}\t{}", md5_file(&p).unwrap_or_default()));
        }
    }
    map.insert("_eclasses_".into(), ecl.join("\t"));
    Ok(Meta { map })
}

/// How a phase runs.
#[derive(Clone, Copy)]
pub struct PhaseOpts {
    /// run as the portage user (userpriv)
    pub drop_privs: bool,
    /// run inside sys-apps/sandbox
    pub sandbox: bool,
    /// private network namespace with only loopback
    pub no_network: bool,
}

pub struct Runner<'a> {
    #[allow(dead_code)]
    pub cfg: &'a Config,
    pub env: Vec<(String, String)>,
    pub log: Option<PathBuf>,
    pub portage_ids: Option<(u32, u32)>,
    pub quiet: bool,
}

fn bring_up_loopback() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if s < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo\0".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut ifr) == 0 {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(s, libc::SIOCSIFFLAGS, &ifr);
        }
        libc::close(s);
    }
}

impl<'a> Runner<'a> {
    /// Run one phase. `env_in`: saved environment to start from (None = source the ebuild).
    pub fn phase(
        &self,
        phase: &str,
        opts: PhaseOpts,
        env_in: Option<&Path>,
        env_out: Option<&Path>,
        extra: &[(String, String)],
    ) -> Result<(), String> {
        let lib = lib_path();
        let sandbox_bin = Path::new("/usr/bin/sandbox");
        let use_sandbox = opts.sandbox && sandbox_bin.exists();
        let mut cmd = if use_sandbox {
            let mut c = Command::new(sandbox_bin);
            c.arg("bash");
            c
        } else {
            Command::new("bash")
        };
        cmd.env_clear().arg("--norc").arg("--noprofile").arg(&lib);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.env("SPAS_MODE", "phase")
            .env("EBUILD_PHASE", phase)
            .env("SPAS_ENV_IN", env_in.map(|p| p.display().to_string()).unwrap_or_default())
            .env("SPAS_ENV_OUT", env_out.map(|p| p.display().to_string()).unwrap_or_default())
            .stdin(Stdio::null());
        if use_sandbox {
            let builddir = self.env.iter().find(|(k, _)| k == "PORTAGE_BUILDDIR").map(|(_, v)| v.clone()).unwrap_or_default();
            let home = self.env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.clone()).unwrap_or_default();
            cmd.env(
                "SANDBOX_WRITE",
                format!(
                    "{builddir}:/dev/null:/dev/zero:/dev/full:/dev/tty:/dev/ptmx:/dev/pts/:/dev/shm/:/dev/fd/:/proc/self/fd/:/dev/stdout:/dev/stderr:/tmp/:/var/tmp/"
                ),
            )
            .env("SANDBOX_PREDICT", format!("{home}:/dev/crypto:/proc/self/coredump_filter:/dev/random:/proc/self/oom_score_adj:/var/cache/fontconfig"))
            .env("SANDBOX_READ", "/")
            .env("SANDBOX_VERBOSE", "1");
        }
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        cmd.stdout(writer.try_clone().map_err(|e| e.to_string())?).stderr(writer);
        let ids = if opts.drop_privs { self.portage_ids } else { None };
        let no_net = opts.no_network && crate::util::is_root();
        unsafe {
            cmd.pre_exec(move || {
                if no_net && libc::unshare(libc::CLONE_NEWNET) == 0 {
                    bring_up_loopback();
                }
                if let Some((uid, gid)) = ids {
                    if libc::setgroups(1, &gid) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start bash: {e}"))?;
        drop(cmd);
        let mut log = self.log.as_ref().and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        let stdout = std::io::stdout();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if !self.quiet {
                let mut o = stdout.lock();
                let _ = o.write_all(&buf[..n]);
                let _ = o.flush();
            }
            if let Some(l) = log.as_mut() {
                let _ = l.write_all(&buf[..n]);
            }
        }
        let st = child.wait().map_err(|e| e.to_string())?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("phase {phase} failed"))
        }
    }
}

/// Drop LANG/LC_* from a saved environment: the machine that built the package
/// may have had locales this one doesn't (Portage filters them the same way).
pub fn strip_locale(env: &[u8]) -> String {
    String::from_utf8_lossy(env)
        .split_inclusive('\n')
        .filter(|l| {
            let v = l.strip_prefix("declare -x ").unwrap_or(l);
            let name = v.split('=').next().unwrap_or("");
            let is_lc = name.strip_prefix("LC_").map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_uppercase())).unwrap_or(false);
            !(v.contains('=') && (name == "LANG" || name == "LANGUAGE" || is_lc))
        })
        .collect()
}
