//! Small shared helpers: output, hashing, config-file reading.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

pub fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn green(s: &str) -> String {
    paint("32;01", s)
}
pub fn yellow(s: &str) -> String {
    paint("33;01", s)
}
pub fn red(s: &str) -> String {
    paint("31;01", s)
}
pub fn blue(s: &str) -> String {
    paint("34;01", s)
}
pub fn bold(s: &str) -> String {
    paint("01", s)
}

pub fn info(msg: &str) {
    eprintln!("{} {msg}", green(">>>"));
}
pub fn warn(msg: &str) {
    eprintln!("{} {msg}", yellow(" * "));
}
pub fn error(msg: &str) {
    eprintln!("{} {msg}", red("!!!"));
}

/// Ask a yes/no question on the terminal. Defaults to yes on empty input.
pub fn ask(q: &str) -> bool {
    eprint!("\n{} [{}/{}] ", bold(q), green("Yes"), red("No"));
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    if std::io::stdin().read_line(&mut s).is_err() {
        return false;
    }
    let s = s.trim().to_lowercase();
    s.is_empty() || s == "y" || s == "yes"
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn hash_reader<D: blake2::digest::Digest>(mut r: impl Read) -> std::io::Result<String> {
    let mut d = D::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        d.update(&buf[..n]);
    }
    Ok(hex(&d.finalize()))
}

pub fn md5_file(p: &Path) -> std::io::Result<String> {
    hash_reader::<md5::Md5>(std::fs::File::open(p)?)
}

/// (size, BLAKE2B, SHA512) of a file in one pass.
pub fn manifest_hashes(p: &Path) -> std::io::Result<(u64, String, String)> {
    use blake2::Digest;
    let mut f = std::fs::File::open(p)?;
    let mut b = blake2::Blake2b512::new();
    let mut s = sha2::Sha512::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        b.update(&buf[..n]);
        s.update(&buf[..n]);
    }
    Ok((size, hex(&b.finalize()), hex(&s.finalize())))
}

pub fn blake2b_str(s: &str) -> String {
    use blake2::Digest;
    hex(&blake2::Blake2b512::digest(s.as_bytes()))
}

/// Files of a config directory, recursively, sorted, skipping hidden and backup files.
pub fn sorted_config_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut ents: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            !n.starts_with('.') && !n.ends_with('~') && !n.ends_with(".bak")
        })
        .collect();
    ents.sort();
    for p in ents {
        if p.is_dir() {
            out.extend(sorted_config_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Non-comment, non-blank lines of a file, or of every file under a directory.
pub fn read_lines_dir(p: &Path) -> Vec<String> {
    let files = if p.is_dir() { sorted_config_files(p) } else if p.exists() { vec![p.to_path_buf()] } else { vec![] };
    let mut out = Vec::new();
    for f in files {
        if let Ok(t) = std::fs::read_to_string(&f) {
            for line in t.lines() {
                let l = match line.find('#') {
                    Some(i) => &line[..i],
                    None => line,
                };
                let l = l.trim();
                if !l.is_empty() {
                    out.push(l.to_string());
                }
            }
        }
    }
    out
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

pub fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let t = std::fs::read_to_string("/etc/passwd").ok()?;
    for l in t.lines() {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() > 3 && f[0] == name {
            return Some((f[2].parse().ok()?, f[3].parse().ok()?));
        }
    }
    None
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Exclusive lock held for the life of the returned file.
pub fn lock(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::io::AsRawFd;
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        info(&format!("waiting for lock on {}...", path.display()));
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("could not lock {}", path.display()));
        }
    }
    Ok(f)
}

pub fn human_size(n: u64) -> String {
    let mut f = n as f64;
    for u in ["B", "KiB", "MiB", "GiB"] {
        if f < 1024.0 || u == "GiB" {
            return if u == "B" { format!("{n} B") } else { format!("{f:.1} {u}") };
        }
        f /= 1024.0;
    }
    unreachable!()
}

/// Simple glob for COLLISION_IGNORE / UNINSTALL_IGNORE style patterns (`*` and `?`).
pub fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}
