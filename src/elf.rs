//! Minimal ELF reading: what a file provides (SONAME) and needs (DT_NEEDED),
//! written to the VDB as NEEDED.ELF.2 / PROVIDES / REQUIRES in Portage's format.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfInfo {
    /// scanelf-style machine name: X86_64, 386, AARCH64, ...
    pub arch: String,
    /// Portage multilib category: x86_64, x86_32, x86_x32, arm_64, ...
    pub multilib: String,
    pub soname: String,
    pub runpath: String,
    pub needed: Vec<String>,
    /// DT_FLAGS_1 has DF_1_PIE: an executable even without PT_INTERP (static-pie)
    pub pie: bool,
}

fn machine(m: u16, class64: bool) -> (&'static str, &'static str) {
    match m {
        0 => ("NONE", ""),
        3 => ("386", "x86_32"),
        62 => ("X86_64", if class64 { "x86_64" } else { "x86_x32" }),
        183 => ("AARCH64", "arm_64"),
        40 => ("ARM", "arm_32"),
        20 => ("PPC", "ppc_32"),
        21 => ("PPC64", "ppc_64"),
        243 => ("RISCV", if class64 { "riscv_lp64d" } else { "riscv_ilp32" }),
        _ => ("UNKNOWN_TYPE", ""),
    }
}

struct Reader<'a> {
    d: &'a [u8],
    le: bool,
    c64: bool,
}

impl Reader<'_> {
    fn u16(&self, o: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(o..o + 2)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }
    fn u32(&self, o: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(o..o + 4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn u64(&self, o: usize) -> Option<u64> {
        let b: [u8; 8] = self.d.get(o..o + 8)?.try_into().ok()?;
        Some(if self.le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) })
    }
    /// address-sized field
    fn addr(&self, o: usize) -> Option<u64> {
        if self.c64 { self.u64(o) } else { self.u32(o).map(|v| v as u64) }
    }
    fn cstr(&self, o: usize) -> Option<String> {
        let rest = self.d.get(o..)?;
        let end = rest.iter().position(|&c| c == 0)?;
        Some(String::from_utf8_lossy(&rest[..end]).into_owned())
    }
}

/// Parse ELF bytes. None if this isn't ELF.
pub fn parse(d: &[u8]) -> Option<ElfInfo> {
    if d.len() < 52 || &d[..4] != b"\x7fELF" {
        return None;
    }
    let c64 = d[4] == 2;
    let r = Reader { d, le: d[5] != 2, c64 };
    let (arch, multilib) = machine(r.u16(18)?, c64);
    let mut info = ElfInfo { arch: arch.into(), multilib: multilib.into(), soname: String::new(), runpath: String::new(), needed: vec![], pie: false };
    // section headers: find SHT_DYNAMIC and its string table
    let (shoff, shentsize, shnum) = if c64 {
        (r.u64(0x28)? as usize, r.u16(0x3a)? as usize, r.u16(0x3c)? as usize)
    } else {
        (r.u32(0x20)? as usize, r.u16(0x2e)? as usize, r.u16(0x30)? as usize)
    };
    if shoff == 0 || shentsize == 0 {
        return Some(info);
    }
    let sh = |i: usize| -> Option<(u32, usize, usize, u32)> {
        let b = shoff.checked_add(i.checked_mul(shentsize)?)?;
        // (type, offset, size, link)
        if c64 {
            Some((r.u32(b + 4)?, r.u64(b + 0x18)? as usize, r.u64(b + 0x20)? as usize, r.u32(b + 0x28)?))
        } else {
            Some((r.u32(b + 4)?, r.u32(b + 0x10)? as usize, r.u32(b + 0x14)? as usize, r.u32(b + 0x18)?))
        }
    };
    for i in 0..shnum.min(4096) {
        let Some((ty, off, size, link)) = sh(i) else { break };
        if ty != 6 {
            continue; // SHT_DYNAMIC
        }
        let Some((_, stroff, _, _)) = sh(link as usize) else { break };
        let ent = if c64 { 16 } else { 8 };
        let mut rpath = String::new();
        for k in 0..size / ent {
            let o = off + k * ent;
            let (Some(tag), Some(val)) = (r.addr(o), r.addr(o + ent / 2)) else { break };
            let s = || r.cstr(stroff + val as usize).unwrap_or_default();
            match tag {
                0 => break,
                1 => info.needed.push(s()),
                14 => info.soname = s(),
                15 => rpath = s(),
                29 => info.runpath = s(),
                0x6fff_fffb => info.pie = val & 0x0800_0000 != 0,
                _ => {}
            }
        }
        if info.runpath.is_empty() {
            info.runpath = rpath;
        }
        break;
    }
    Some(info)
}

/// ET_DYN without a PT_INTERP: a shared object rather than a PIE executable.
fn is_shared_object(d: &[u8]) -> bool {
    let c64 = d[4] == 2;
    let r = Reader { d, le: d[5] != 2, c64 };
    if r.u16(16) != Some(3) {
        return false;
    }
    let (phoff, phentsize, phnum) = if c64 {
        (r.u64(0x20).unwrap_or(0) as usize, r.u16(0x36).unwrap_or(0) as usize, r.u16(0x38).unwrap_or(0) as usize)
    } else {
        (r.u32(0x1c).unwrap_or(0) as usize, r.u16(0x2a).unwrap_or(0) as usize, r.u16(0x2c).unwrap_or(0) as usize)
    };
    !(0..phnum.min(4096)).any(|i| r.u32(phoff + i * phentsize) == Some(3)) // PT_INTERP
}

pub fn scan_file(p: &Path) -> Option<ElfInfo> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut head = [0u8; 4];
    f.read_exact(&mut head).ok()?;
    if &head != b"\x7fELF" {
        return None;
    }
    let d = std::fs::read(p).ok()?;
    let mut i = parse(&d)?;
    // like Portage: a shared object without DT_SONAME is known by its file name
    if i.soname.is_empty() && !i.pie && is_shared_object(&d) {
        i.soname = p.file_name()?.to_string_lossy().into_owned();
    }
    Some(i)
}

/// NEEDED.ELF.2 / NEEDED / PROVIDES / REQUIRES for an image directory.
#[derive(Default, Debug)]
pub struct Analysis {
    pub needed_elf2: String,
    pub needed: String,
    pub provides: String,
    pub requires: String,
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for p in v {
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() => walk(&p, out),
                Ok(m) if m.is_file() => out.push(p),
                _ => {}
            }
        }
    }
}

fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

pub fn analyze(image: &Path) -> Analysis {
    let mut files = Vec::new();
    walk(image, &mut files);
    let mut entries: Vec<(String, ElfInfo)> = Vec::new();
    for f in files {
        if let Some(i) = scan_file(&f) {
            let rel = format!("/{}", f.strip_prefix(image).unwrap().display());
            entries.push((rel, i));
        }
    }
    let mut a = Analysis::default();
    for (path, i) in &entries {
        a.needed_elf2.push_str(&format!("{};{};{};{};{};{}\n", i.arch, path, i.soname, i.runpath, i.needed.join(","), i.multilib));
        if !i.needed.is_empty() {
            a.needed.push_str(&format!("{path} {}\n", i.needed.join(",")));
        }
    }
    // PROVIDES / REQUIRES the way Portage's SonameDepsProcessor derives them
    let mut provides: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut requires: BTreeMap<&str, BTreeMap<&str, Vec<Vec<String>>>> = BTreeMap::new();
    for (path, i) in &entries {
        if i.multilib.is_empty() {
            continue;
        }
        if !i.soname.is_empty() {
            provides.entry(&i.multilib).or_default().insert(&i.soname);
        }
        let dir = path.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }).unwrap_or("/");
        let runpaths: Vec<String> = i
            .runpath
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|r| normalize(&r.replace("${ORIGIN}", dir).replace("$ORIGIN", dir)))
            .collect();
        for n in &i.needed {
            requires.entry(&i.multilib).or_default().entry(n).or_default().push(runpaths.clone());
        }
    }
    let mut req_out: Vec<String> = Vec::new();
    for (cat, needs) in &requires {
        let mut left: Vec<&str> = Vec::new();
        for (soname, consumers) in needs {
            if provides.get(cat).map(|p| p.contains(soname)).unwrap_or(false) {
                continue;
            }
            // satisfied by a file of this package found through every consumer's runpath
            let local_dirs: Vec<&str> = entries
                .iter()
                .filter(|(p, i)| &i.multilib == cat && p.rsplit('/').next() == Some(soname))
                .map(|(p, _)| p.rsplit_once('/').map(|(d, _)| d).unwrap_or(""))
                .collect();
            if !local_dirs.is_empty() && consumers.iter().all(|rp| rp.iter().any(|r| local_dirs.contains(&r.as_str()))) {
                continue;
            }
            left.push(soname);
        }
        if !left.is_empty() {
            req_out.push(format!("{cat}:"));
            req_out.extend(left.iter().map(|s| s.to_string()));
        }
    }
    let mut prov_out: Vec<String> = Vec::new();
    for (cat, s) in &provides {
        prov_out.push(format!("{cat}:"));
        prov_out.extend(s.iter().map(|x| x.to_string()));
    }
    if !prov_out.is_empty() {
        a.provides = prov_out.join(" ") + "\n";
    }
    if !req_out.is_empty() {
        a.requires = req_out.join(" ") + "\n";
    }
    a
}

/// Write the analysis next to a VDB entry / binpkg metadata.
pub fn write(a: &Analysis, dir: &Path) -> Result<(), String> {
    for (name, text) in [("NEEDED.ELF.2", &a.needed_elf2), ("NEEDED", &a.needed), ("PROVIDES", &a.provides), ("REQUIRES", &a.requires)] {
        if !text.is_empty() {
            std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn real_binaries() {
        // the running test binary is a dynamically linked ELF that needs libc
        let me = std::env::current_exe().unwrap();
        let i = super::scan_file(&me).unwrap();
        assert!(i.needed.iter().any(|n| n.starts_with("libc.so")), "{i:?}");
        assert_eq!(super::normalize("/usr/lib/../lib64/./x"), "/usr/lib64/x");
    }
}
