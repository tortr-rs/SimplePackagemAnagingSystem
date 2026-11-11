//! hoard: the command line of SimplePackagemAnagingSystem (SPAS), a native
//! package manager for Gentoo ebuild repositories.

mod atom;
mod autounmask;
mod binhost;
mod binpkg;
mod build;
mod config;
mod depspec;
mod elf;
mod ebuild_env;
mod envupdate;
mod fetch;
mod merge;
mod preserve;
mod repo;
mod resolve;
mod shellvars;
mod sync;
mod util;
mod vdb;
mod version;

use atom::Atom;
use config::Config;
use resolve::{Action, Opts, Plan};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use util::{blue, bold, error, green, info, red, warn, yellow};
use vdb::Vdb;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
hoard - SimplePackagemAnagingSystem (SPAS), a package manager for Gentoo

usage: hoard [--root DIR] <command> [options] [arguments]

commands:
  install, i  <pkg...>    build and install packages (and their dependencies)
      -p, --pretend         only show what would be done
      -y, --yes             don't ask for confirmation
      -1, --oneshot         don't add the packages to the world file
      -u, --update          only update when a newer version is available
      -D, --deep            consider the whole dependency tree
      -N, --newuse          rebuild packages whose USE flags changed
      -n, --noreplace       skip packages that are already installed
      -f, --fetchonly       download the sources (or binary packages) only
      -g, --getbinpkg       use prebuilt packages from the Gentoo binhost
                            (default when FEATURES contains getbinpkg)
      -k, --usepkg          use packages built earlier (PKGDIR), no downloads
      -b, --buildpkg        also save each package built from source in PKGDIR
                            (default when FEATURES contains buildpkg)
      -S, --source          build everything from source
      --nodeps              ignore dependencies
      --autounmask=n        don't work out package.use / keyword / license changes
      --autounmask-write    write those changes without asking (needed with -y)
      --with-bdeps=y|n      also update/install build deps of installed packages
                            (default: taken from EMERGE_DEFAULT_OPTS, else n)
      -q, --quiet           hide build output (it is still logged)
      -jN, --jobs=N         build up to N packages at once (default: from
                            EMERGE_DEFAULT_OPTS, else 1)
  upgrade, up             update the whole system (install -uDN @world)
  remove, rm  <pkg...>    uninstall packages            (-p, -y, --force)
  autoremove              uninstall packages nothing needs any more (-p, -y)
  sync [repo...]          update the repositories (verifies the gentoo tree)
  search, s  <regex>      search package names (-d: descriptions too)
  info <pkg>              versions, keywords, masks and USE flags of a package
  list [regex]            installed packages
  files <pkg>             files installed by a package
  owns <path...>          which package installed a file
  env-update              regenerate /etc/profile.env and ld.so.conf
  verify [repo]           check a repository against its signed Manifests

Configuration is the same as Portage's: /etc/portage/make.conf, package.use,
package.accept_keywords, package.mask, repos.conf, the selected profile.
Sets: @world, @system, @selected, @preserved-rebuild.
";

struct Args {
    flags: HashSet<String>,
    pos: Vec<String>,
}

fn parse_args(raw: &[String]) -> Args {
    let mut flags = HashSet::new();
    let mut pos = Vec::new();
    let long = |c: char| -> Option<&'static str> {
        Some(match c {
            'p' => "pretend",
            'y' => "yes",
            '1' => "oneshot",
            'u' => "update",
            'D' => "deep",
            'N' => "newuse",
            'n' => "noreplace",
            'f' => "fetchonly",
            'g' => "getbinpkg",
            'b' => "buildpkg",
            'k' => "usepkg",
            'S' => "source",
            'q' => "quiet",
            'v' => "verbose",
            'd' => "description",
            _ => return None,
        })
    };
    let mut only_pos = false;
    for a in raw {
        if only_pos {
            pos.push(a.clone());
        } else if a == "--" {
            only_pos = true;
        } else if a.len() > 2 && a.starts_with("-j") && a[2..].chars().all(|c| c.is_ascii_digit()) {
            flags.insert(format!("jobs={}", &a[2..]));
        } else if let Some(l) = a.strip_prefix("--") {
            flags.insert(l.to_string());
        } else if a.starts_with('-') && a.len() > 1 && !a[1..].starts_with(|c: char| c.is_ascii_digit() && a.len() > 2) {
            for c in a[1..].chars() {
                match long(c) {
                    Some(l) => {
                        flags.insert(l.to_string());
                    }
                    None => {
                        flags.insert(c.to_string());
                    }
                }
            }
        } else {
            pos.push(a.clone());
        }
    }
    Args { flags, pos }
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    let mut root = PathBuf::from("/");
    if let Some(i) = raw.iter().position(|a| a == "--root") {
        if i + 1 < raw.len() {
            root = PathBuf::from(&raw[i + 1]);
            raw.drain(i..i + 2);
        }
    }
    if raw.is_empty() || raw[0] == "help" || raw[0] == "--help" || raw[0] == "-h" {
        print!("{HELP}");
        return;
    }
    if raw[0] == "--version" || raw[0] == "-V" {
        println!("hoard (SimplePackagemAnagingSystem) {VERSION}");
        return;
    }
    let cmd = raw.remove(0);
    let code = if cmd == "__internal" {
        internal(&raw, &root)
    } else {
        match run(&cmd, &parse_args(&raw), &root) {
            Ok(c) => c,
            Err(e) => {
                error(&e);
                1
            }
        }
    };
    ebuild_env::cleanup_lib();
    std::process::exit(code);
}

/// One parallel job: build (or unpack) a package and leave it ready for merging.
fn build_one(args: &[String], root: &Path) -> Result<(), String> {
    let (cpv, repo, kind, path, buildpkg) = (&args[0], &args[1], &args[2], &args[3], args[4] == "1");
    let cfg = Config::load(root)?;
    let at = Atom::parse(&format!("={cpv}")).map_err(|_| format!("bad cpv {cpv}"))?;
    let ver = at.version.clone().ok_or_else(|| format!("bad cpv {cpv}"))?;
    let e = cfg
        .repos
        .ebuilds(&at.cat, &at.pn)
        .iter()
        .find(|e| e.ver == ver && e.repo_name == *repo)
        .cloned()
        .ok_or_else(|| format!("{cpv}::{repo} is gone from the repository"))?;
    let u = cfg.pkg_use(&e);
    let vdb = Vdb::load(&cfg.vdb_path());
    let replacing: Vec<String> = vdb.pkgs.iter().filter(|p| p.cp() == e.cp() && p.slot == e.slot()).map(|p| p.ver.to_string()).collect();
    let bopts = build::BuildOpts { replacing, quiet: true };
    let b = match kind.as_str() {
        "src" => {
            let b = build::build(&cfg, &e, &u, &bopts)?;
            if buildpkg {
                if let Err(err) = binpkg::create(&cfg, &e, &u, &b) {
                    warn(&format!("could not create a binary package: {err}"));
                }
            }
            b
        }
        "local" | "remote" => {
            let index = if kind == "local" { binhost::Binhost::load_local(&cfg) } else { binhost::Binhost::load(&cfg, false)? };
            let bin = index.by_path(cpv, path).ok_or_else(|| format!("binary package {path} is no longer listed"))?;
            build::from_binary(&cfg, &e, &u, &bin, &bopts)?
        }
        k => return Err(format!("unknown job kind {k}")),
    };
    b.save_state()
}

/// Helpers called from inside ebuilds (has_version, best_version, ver_test).
fn internal(args: &[String], root: &Path) -> i32 {
    match args.first().map(|s| s.as_str()) {
        Some("vercmp") if args.len() == 3 => {
            match (version::Version::parse(&args[1]), version::Version::parse(&args[2])) {
                (Some(a), Some(b)) => {
                    println!("{}", a.cmp(&b) as i32);
                    0
                }
                _ => 2,
            }
        }
        Some(w @ ("has_version" | "best_version")) if args.len() == 3 => {
            let root = if args[1] == "-r" {
                std::env::var("SPAS_ROOT").map(PathBuf::from).unwrap_or_else(|_| root.to_path_buf())
            } else {
                PathBuf::from("/")
            };
            let Ok(a) = Atom::parse(&args[2]) else { return 2 };
            let vdb = Vdb::load(&root.join("var/db/pkg"));
            let parent: HashSet<String> = std::env::var("USE").unwrap_or_default().split_whitespace().map(|s| s.to_string()).collect();
            let m = vdb.matching(&a, Some(&parent));
            if w == "best_version" {
                if let Some(b) = m.iter().max_by(|x, y| x.ver.cmp(&y.ver)) {
                    println!("{}", b.cpv());
                    return 0;
                }
                println!();
                return 1;
            }
            if m.is_empty() { 1 } else { 0 }
        }
        Some("build-one") if args.len() == 6 => match build_one(&args[1..], root) {
            Ok(()) => 0,
            Err(e) => {
                error(&e);
                1
            }
        },
        Some("elf-analyze") if args.len() == 2 => {
            let a = elf::analyze(Path::new(&args[1]));
            print!("--- NEEDED.ELF.2\n{}--- PROVIDES\n{}--- REQUIRES\n{}", a.needed_elf2, a.provides, a.requires);
            0
        }
        _ => 2,
    }
}

fn need_root() -> Result<(), String> {
    if util::is_root() { Ok(()) } else { Err("this needs root: run it with sudo".into()) }
}

fn run(cmd: &str, a: &Args, root: &Path) -> Result<i32, String> {
    match cmd {
        "install" | "i" => {
            cmd_install(Config::load(root)?, a, &a.pos)
        }
        "upgrade" | "up" => {
            let cfg = Config::load(root)?;
            let mut a2 = Args { flags: a.flags.clone(), pos: vec![] };
            for f in ["update", "deep", "newuse", "oneshot"] {
                a2.flags.insert(f.into());
            }
            cmd_install(cfg, &a2, &["@world".to_string()])
        }
        "remove" | "rm" | "uninstall" => cmd_remove(&Config::load(root)?, a),
        "autoremove" | "depclean" => cmd_autoremove(&Config::load(root)?, a),
        "sync" => {
            need_root()?;
            let cfg = Config::load(root)?;
            let _l = util::lock(Path::new("/var/lib/spas/lock"))?;
            let mut failed = false;
            for r in &cfg.repos.list {
                if !a.pos.is_empty() && !a.pos.contains(&r.name) {
                    continue;
                }
                if let Err(e) = sync::sync_repo(r, &cfg.repos.main) {
                    error(&e);
                    failed = true;
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        "verify" => {
            let cfg = Config::load(root)?;
            let name = a.pos.first().cloned().unwrap_or_else(|| cfg.repos.main.clone());
            let r = cfg.repos.by_name(&name).ok_or_else(|| format!("no repository called {name}"))?;
            if r.sync_type == "git" {
                info(&format!("Verifying {name} (OpenPGP signature of the checked-out commit)"));
                sync::verify_git_head(r)?;
                info(&format!("{name}: commit signature is good"));
                return Ok(0);
            }
            sync::verify_tree(r)?;
            Ok(0)
        }
        "search" | "s" => cmd_search(&Config::load(root)?, a),
        "info" => cmd_info(&Config::load(root)?, a),
        "list" | "ls" => {
            let vdb = Vdb::load(&root.join("var/db/pkg"));
            let re = match a.pos.first() {
                Some(p) => Some(regex::Regex::new(&format!("(?i){p}")).map_err(|e| e.to_string())?),
                None => None,
            };
            for p in &vdb.pkgs {
                let n = format!("{}:{}", p.cpv(), p.slot);
                if re.as_ref().map(|r| r.is_match(&n)).unwrap_or(true) {
                    println!("{}{}", p.cpv(), if p.slot != "0" { format!(":{}", p.slot) } else { String::new() });
                }
            }
            Ok(0)
        }
        "files" => {
            let cfg = Config::load(root)?;
            let vdb = Vdb::load(&cfg.vdb_path());
            for t in &a.pos {
                let atom = resolve::parse_user_atom(&cfg, &vdb, t)?;
                let m = vdb.matching_nouse(&atom);
                if m.is_empty() {
                    return Err(format!("{t} is not installed"));
                }
                for p in m {
                    for e in p.contents() {
                        match e {
                            vdb::Entry::Sym { path, target, .. } => println!("{path} -> {target}"),
                            vdb::Entry::Dir(_) => {}
                            other => println!("{}", other.path()),
                        }
                    }
                }
            }
            Ok(0)
        }
        "owns" => {
            let vdb = Vdb::load(&root.join("var/db/pkg"));
            let mut canon = merge::Canon::new();
            let mut idx = std::collections::HashMap::new();
            for p in &vdb.pkgs {
                for e in p.contents() {
                    idx.entry(canon.get(&root.join(e.path().trim_start_matches('/')))).or_insert_with(Vec::new).push(p.cpv());
                }
            }
            let mut code = 0;
            for f in &a.pos {
                let p = PathBuf::from(f);
                let abs = if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(p) };
                match idx.get(&canon.get(&abs)) {
                    Some(owners) => {
                        let mut o = owners.clone();
                        o.dedup();
                        println!("{} {}", o.join(" "), abs.display());
                    }
                    None => {
                        eprintln!("{} is not owned by any package", abs.display());
                        code = 1;
                    }
                }
            }
            Ok(code)
        }
        "env-update" => {
            need_root()?;
            envupdate::env_update(root, true)?;
            Ok(0)
        }
        _ => Err(format!("unknown command '{cmd}' (see hoard help)")),
    }
}

// ------------------------------------------------------------------ install

fn use_display(cfg: &Config, n: &resolve::PkgNode, old: Option<&vdb::Installed>) -> String {
    let u = &n.use_;
    let hidden: Vec<String> = cfg.var("USE_EXPAND_HIDDEN").split_whitespace().map(|s| s.to_lowercase()).collect();
    let mut groups: Vec<(String, Vec<String>)> = vec![("USE".into(), vec![])];
    let mut iuse = u.iuse.clone();
    iuse.sort();
    iuse.dedup();
    for f in &iuse {
        let on = u.enabled.contains(f);
        let fixed = u.forced.contains(f) || u.masked.contains(f);
        let mut prefix_var = None;
        for ue in cfg.use_expand_vars() {
            let p = format!("{}_", ue.to_lowercase());
            if f.starts_with(&p) {
                prefix_var = Some((ue.clone(), f[p.len()..].to_string()));
                break;
            }
        }
        if let Some((v, _)) = &prefix_var {
            if hidden.contains(&v.to_lowercase()) {
                continue;
            }
        }
        let name = prefix_var.as_ref().map(|(_, s)| s.clone()).unwrap_or_else(|| f.clone());
        let mut s = if on { red(&name) } else { blue(&format!("-{name}")) };
        if let Some(o) = old {
            if !o.iuse.contains(f) {
                s.push_str(&yellow("%"));
            } else if o.use_.contains(f) != on {
                s.push_str(&green("*"));
            }
        }
        if fixed {
            s = format!("({s})");
        }
        let gname = prefix_var.map(|(v, _)| v).unwrap_or_else(|| "USE".into());
        match groups.iter_mut().find(|(g, _)| *g == gname) {
            Some(g) => g.1.push(s),
            None => groups.push((gname, vec![s])),
        }
    }
    groups.iter().filter(|(_, v)| !v.is_empty()).map(|(g, v)| format!("{g}=\"{}\"", v.join(" "))).collect::<Vec<_>>().join(" ")
}

fn print_plan(cfg: &Config, vdb: &Vdb, plan: &Plan, verbose: bool) -> u64 {
    let mut total = 0u64;
    let dd = fetch::distdir(cfg);
    for &i in &plan.order {
        let n = &plan.nodes[i];
        let old = vdb.by_cp_slot(&n.e.cp(), n.e.slot());
        let tag = match &n.action {
            Action::New => green("N    "),
            Action::NewSlot => green("NS   "),
            Action::Upgrade(_) => blue("U    "),
            Action::Downgrade(_) => yellow("UD   "),
            Action::Reinstall => yellow("R    "),
        };
        let from = match &n.action {
            Action::Upgrade(v) | Action::Downgrade(v) => format!(" {}", blue(&format!("[{v}]"))),
            _ => String::new(),
        };
        let mut size = 0u64;
        if let Some(b) = &n.binpkg {
            if !binhost::local_path(cfg, b).exists() {
                size = b.size;
            }
        } else if let Ok(files) = fetch::distfiles(cfg, &n.e, &n.use_) {
            for f in files {
                if !dd.join(&f.name).exists() {
                    size += f.size.unwrap_or(0);
                }
            }
        }
        total += size;
        let slot = if n.e.slot() != "0" { format!(":{}", n.e.slot()) } else { String::new() };
        let size_s = if size > 0 { format!(" {}", util::human_size(size)) } else { String::new() };
        println!(
            "[{} {}] {}{}::{}{} {}{}{}",
            if n.binpkg.is_some() { yellow("binary") } else { green("ebuild") },
            tag,
            green(&n.e.cpv()),
            slot,
            n.e.repo_name,
            from,
            use_display(cfg, n, old),
            size_s,
            n.reason.as_ref().map(|r| format!("  ({r})")).unwrap_or_default()
        );
        if verbose {
            if let Some(p) = &n.parent {
                println!("    {}", blue(&format!("pulled in by {p}")));
            }
        }
        for u in &n.uninstall_after {
            println!("[{}     ] {}  (blocked by {})", red("uninstall"), u, n.e.cpv());
        }
    }
    total
}

/// Resolve the targets; on failure also return the /etc/portage changes that would help.
fn resolve_targets(
    cfg: &Config,
    vdb: &Vdb,
    opts: &Opts,
    binhost: Option<&binhost::Binhost>,
    targets: &[String],
) -> Result<(Plan, Vec<String>), (String, Vec<autounmask::Change>)> {
    let mut r = resolve::Resolver::new(cfg, vdb, opts.clone());
    if let Some(b) = binhost {
        r.use_binhost(b);
    }
    let mut world_add: Vec<String> = Vec::new();
    for t in targets {
        let atoms = resolve::expand_target(cfg, vdb, t).map_err(|e| (e, vec![]))?;
        for atom in atoms {
            if let Err(e) = r.add_target(&atom) {
                let p = r.take_proposal();
                return Err((e, p));
            }
            if !t.starts_with('@') {
                let mut w = atom.cp();
                if let Some(s) = atom.slot.as_ref().and_then(|s| s.slot.clone()) {
                    w.push_str(&format!(":{s}"));
                }
                world_add.push(w);
            }
        }
    }
    let plan = r.finish().map_err(|e| (e, vec![]))?;
    Ok((plan, world_add))
}

fn cmd_install(mut cfg: Config, a: &Args, targets: &[String]) -> Result<i32, String> {
    if targets.is_empty() {
        return Err("nothing to install (see hoard help)".into());
    }
    let pretend = a.flags.contains("pretend");
    if !pretend {
        need_root()?;
    }
    let _lock = if pretend { None } else { Some(util::lock(Path::new("/var/lib/spas/lock"))?) };
    let vdb = Vdb::load(&cfg.vdb_path());
    let opts = Opts {
        update: a.flags.contains("update"),
        deep: a.flags.contains("deep"),
        newuse: a.flags.contains("newuse"),
        noreplace: a.flags.contains("noreplace"),
        nodeps: a.flags.contains("nodeps"),
        with_bdeps: a.flags.contains("with-bdeps=y")
            || (!a.flags.contains("with-bdeps=n") && cfg.var("EMERGE_DEFAULT_OPTS").split_whitespace().any(|o| o == "--with-bdeps=y")),
        autounmask: !a.flags.contains("autounmask=n"),
    };
    let source_only = a.flags.contains("source");
    let want_remote = !source_only && (a.flags.contains("getbinpkg") || cfg.feature("getbinpkg"));
    let buildpkg = a.flags.contains("buildpkg") || cfg.feature("buildpkg");
    // our own earlier builds are used whenever binaries are wanted at all
    let want_local = !source_only && (want_remote || buildpkg || a.flags.contains("usepkg"));
    let mut bins = binhost::Binhost::empty();
    if want_local {
        bins.absorb(binhost::Binhost::load_local(&cfg));
    }
    if want_remote {
        match binhost::Binhost::load(&cfg, false) {
            Ok(b) => bins.absorb(b),
            Err(e) => warn(&format!("{e}; building from source")),
        }
    }
    let binhost = if bins.is_empty() { None } else { Some(bins) };
    info("Calculating dependencies...");
    // retry with each proposed /etc/portage change applied in memory
    let mut changes: Vec<autounmask::Change> = Vec::new();
    let (plan, world_add) = loop {
        match resolve_targets(&cfg, &vdb, &opts, binhost.as_ref(), targets) {
            Ok(x) => break x,
            Err((msg, proposal)) => {
                let new: Vec<autounmask::Change> = proposal.into_iter().filter(|c| !changes.contains(c)).collect();
                if new.is_empty() || changes.len() > 64 {
                    if !changes.is_empty() {
                        warn("even with these changes the dependencies can't be satisfied:");
                        autounmask::show(&cfg.config_root, &changes);
                    }
                    return Err(msg);
                }
                for c in &new {
                    cfg.apply_change(c)?;
                }
                changes.extend(new);
            }
        }
    };
    let cfg = &cfg;
    for w in &plan.warnings {
        warn(w);
    }
    if plan.order.is_empty() {
        info("Nothing to merge; everything is up to date.");
        if !pretend && !a.flags.contains("oneshot") {
            add_world(cfg, &world_add)?;
        }
        return Ok(0);
    }
    println!();
    let total = print_plan(cfg, &vdb, &plan, a.flags.contains("verbose"));
    println!(
        "\nTotal: {} package{}{}",
        plan.order.len(),
        if plan.order.len() == 1 { "" } else { "s" },
        if total > 0 { format!(", size of downloads: {}", util::human_size(total)) } else { String::new() }
    );
    if !changes.is_empty() {
        warn("this plan needs the following changes to /etc/portage:");
        autounmask::show(&cfg.config_root, &changes);
        if pretend {
            info("run without -p and hoard will offer to write them");
            return Ok(0);
        }
        // -y alone never edits /etc/portage: that takes the prompt or --autounmask-write
        let write = if a.flags.contains("autounmask-write") {
            true
        } else if a.flags.contains("yes") {
            info("not writing them because of --yes; add --autounmask-write to allow that");
            false
        } else {
            util::ask("Write these changes to /etc/portage?")
        };
        if !write {
            info("Nothing was changed or merged.");
            return Ok(1);
        }
        autounmask::write(&cfg.config_root, &changes)?;
    }
    if pretend {
        return Ok(0);
    }
    if !a.flags.contains("yes") && !util::ask("Would you like to merge these packages?") {
        info("Quitting.");
        return Ok(0);
    }
    let fetchonly = a.flags.contains("fetchonly");
    let quiet = a.flags.contains("quiet");
    let jobs = job_count(a, cfg);
    let mut elogs: Vec<(String, PathBuf)> = Vec::new();
    let ok = if fetchonly {
        let count = plan.order.len();
        for (k, &i) in plan.order.iter().enumerate() {
            let n = &plan.nodes[i];
            info(&format!("({}/{count}) Fetching {}", k + 1, bold(&n.e.cpv())));
            if let Some(b) = &n.binpkg {
                binhost::fetch(cfg, b)?;
            } else {
                let files = fetch::distfiles(cfg, &n.e, &n.use_)?;
                fetch::fetch(cfg, &n.e, &n.use_, &files)?;
            }
        }
        true
    } else if jobs > 1 {
        run_parallel(cfg, a, &plan, &world_add, jobs, buildpkg, root_of(cfg), &mut elogs)?
    } else {
        run_sequential(cfg, a, &plan, &world_add, quiet, buildpkg, &mut elogs)?
    };
    if !fetchonly {
        if let Err(e) = preserve::prune(cfg) {
            warn(&format!("preserved libraries: {e}"));
        }
        preserve::report(cfg);
    }
    if !ok {
        print_elogs(&elogs);
        return Ok(1);
    }
    print_elogs(&elogs);
    for (_, p) in &elogs {
        let _ = std::fs::remove_file(p);
    }
    let cfg_files = pending_cfg_files(cfg);
    if cfg_files > 0 {
        warn(&format!("{cfg_files} config file(s) in /etc need updating (look for ._cfg* files)"));
    }
    Ok(0)
}

fn root_of(cfg: &Config) -> &Path {
    &cfg.root
}

/// --jobs=N / -jN, or the same in EMERGE_DEFAULT_OPTS; default 1.
fn job_count(a: &Args, cfg: &Config) -> usize {
    let from = |t: &str| t.strip_prefix("jobs=").or_else(|| t.strip_prefix("--jobs=")).or_else(|| t.strip_prefix("-j")).and_then(|n| n.parse::<usize>().ok());
    if let Some(n) = a.flags.iter().find_map(|f| from(f)) {
        return n.max(1);
    }
    cfg.var("EMERGE_DEFAULT_OPTS").split_whitespace().find_map(from).unwrap_or(1).max(1)
}

fn build_log(cfg: &Config, n: &resolve::PkgNode) -> PathBuf {
    build::builddir_for(cfg, &n.e).join("temp/build.log")
}

/// Everything after a package is merged: env-update, blockers, world, elog, cleanup.
fn after_merge(
    cfg: &Config,
    a: &Args,
    n: &resolve::PkgNode,
    b: &build::Build,
    world_add: &[String],
    elogs: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    if let Err(e) = envupdate::env_update(&cfg.root, true) {
        warn(&format!("env-update: {e}"));
    }
    for u in &n.uninstall_after {
        let v = Vdb::load(&cfg.vdb_path());
        if let Some(inst) = v.pkgs.iter().find(|p| p.cpv() == *u) {
            if let Err(e) = merge::uninstall(cfg, inst, false) {
                warn(&e);
            }
        }
    }
    if !a.flags.contains("oneshot") {
        let mine: Vec<String> = world_add.iter().filter(|w| w.split(':').next() == Some(n.e.cp().as_str())).cloned().collect();
        add_world(cfg, &mine)?;
    }
    let elog = b.temp.join("spas-elog");
    if elog.exists() {
        let keep = b.builddir.parent().unwrap().join(format!(".{}.elog", n.e.pf()));
        let _ = std::fs::copy(&elog, &keep);
        elogs.push((n.e.cpv(), keep));
    }
    if !cfg.feature("keepwork") && !cfg.feature("noclean") {
        let _ = std::fs::remove_dir_all(&b.builddir);
    }
    info(&format!("{} installed", bold(&n.e.cpv())));
    Ok(())
}

fn run_sequential(
    cfg: &Config,
    a: &Args,
    plan: &Plan,
    world_add: &[String],
    quiet: bool,
    buildpkg: bool,
    elogs: &mut Vec<(String, PathBuf)>,
) -> Result<bool, String> {
    let count = plan.order.len();
    for (k, &i) in plan.order.iter().enumerate() {
        let n = &plan.nodes[i];
        let how = if n.binpkg.is_some() { "Installing (binary)" } else { "Emerging" };
        info(&format!("({}/{count}) {how} {}", k + 1, bold(&n.e.cpv())));
        let vdb_now = Vdb::load(&cfg.vdb_path());
        let replacing: Vec<String> = vdb_now.pkgs.iter().filter(|p| p.cp() == n.e.cp() && p.slot == n.e.slot()).map(|p| p.ver.to_string()).collect();
        let bopts = build::BuildOpts { replacing, quiet };
        let prepared = match &n.binpkg {
            Some(b) => build::from_binary(cfg, &n.e, &n.use_, b, &bopts),
            None => build::build(cfg, &n.e, &n.use_, &bopts),
        };
        let res = prepared.and_then(|b| {
            // package it before merging, like Portage (a failure here isn't fatal)
            if buildpkg && n.binpkg.is_none() {
                if let Err(e) = binpkg::create(cfg, &n.e, &n.use_, &b) {
                    warn(&format!("could not create a binary package: {e}"));
                }
            }
            merge::install(cfg, &n.e, &n.use_, &b, quiet).map(|_| b)
        });
        match res {
            Ok(b) => after_merge(cfg, a, n, &b, world_add, elogs)?,
            Err(e) => {
                error(&e);
                let log = build_log(cfg, n);
                if log.exists() {
                    error(&format!("build log: {}", log.display()));
                }
                let rest: Vec<String> = plan.order[k + 1..].iter().map(|&j| plan.nodes[j].e.cpv()).collect();
                if !rest.is_empty() {
                    warn(&format!("not merged because of this failure: {}", rest.join(" ")));
                }
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Build up to `jobs` packages at once in child hoard processes; merge each one
/// here, one at a time, as soon as it is built.
#[allow(clippy::too_many_arguments)]
fn run_parallel(
    cfg: &Config,
    a: &Args,
    plan: &Plan,
    world_add: &[String],
    jobs: usize,
    buildpkg: bool,
    root: &Path,
    elogs: &mut Vec<(String, PathBuf)>,
) -> Result<bool, String> {
    #[derive(PartialEq, Clone, Copy)]
    enum St {
        Pending,
        Running,
        Merged,
        Failed,
    }
    let count = plan.order.len();
    let pos: std::collections::HashMap<usize, usize> = plan.order.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let mut st: std::collections::HashMap<usize, St> = plan.order.iter().map(|&i| (i, St::Pending)).collect();
    let mut running: std::collections::HashMap<i32, (usize, std::process::Child)> = std::collections::HashMap::new();
    let mut merged = 0usize;
    let mut stop = false;
    let job_log = |n: &resolve::PkgNode| build::builddir_for(cfg, &n.e).with_extension("hoard-job.log");
    info(&format!("Building with up to {jobs} parallel jobs (build output goes to each package's build.log)"));
    loop {
        // start whatever is ready: everything it depends on (earlier in the plan) is merged
        if !stop {
            for &i in &plan.order {
                if running.len() >= jobs {
                    break;
                }
                if st[&i] != St::Pending {
                    continue;
                }
                let n = &plan.nodes[i];
                let ready = n.deps.iter().all(|&(c, k)| {
                    k == resolve::DepKind::P || pos.get(&c).map(|&pc| pc >= pos[&i] || st[&c] == St::Merged).unwrap_or(true)
                });
                if !ready {
                    continue;
                }
                let (kind, path) = match &n.binpkg {
                    Some(b) if b.local => ("local", b.path.clone()),
                    Some(b) => ("remote", b.path.clone()),
                    None => ("src", "-".to_string()),
                };
                let log = job_log(n);
                if let Some(d) = log.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let out = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
                let child = std::process::Command::new(ebuild_env::self_exe())
                    .arg("--root")
                    .arg(root)
                    .args(["__internal", "build-one", &n.e.cpv(), &n.e.repo_name, kind, &path, if buildpkg { "1" } else { "0" }])
                    .stdin(std::process::Stdio::null())
                    .stdout(out.try_clone().map_err(|e| e.to_string())?)
                    .stderr(out)
                    .spawn()
                    .map_err(|e| format!("starting a build job: {e}"))?;
                let how = if n.binpkg.is_some() { "Unpacking" } else { "Building" };
                info(&format!("({}/{count}) {how} {}", pos[&i] + 1, bold(&n.e.cpv())));
                running.insert(child.id() as i32, (i, child));
                st.insert(i, St::Running);
            }
        }
        if running.is_empty() {
            break;
        }
        let mut status = 0;
        // SAFETY: plain waitpid on our own children
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            return Err("waiting for build jobs failed".into());
        }
        let Some((i, _child)) = running.remove(&pid) else { continue };
        let n = &plan.nodes[i];
        let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        let result = if success {
            build::Build::load_state(cfg, &n.e, &n.use_, n.binpkg.is_some()).and_then(|b| {
                merge::install(cfg, &n.e, &n.use_, &b, true).map(|_| b)
            })
        } else {
            Err(String::new())
        };
        match result {
            Ok(b) => {
                after_merge(cfg, a, n, &b, world_add, elogs)?;
                let _ = std::fs::remove_file(job_log(n));
                st.insert(i, St::Merged);
                merged += 1;
                info(&format!("Jobs: {merged} of {count} complete, {} running", running.len()));
            }
            Err(e) => {
                st.insert(i, St::Failed);
                stop = true;
                error(&format!("{} failed{}", n.e.cpv(), if e.is_empty() { String::new() } else { format!(": {e}") }));
                let jl = std::fs::read_to_string(job_log(n)).unwrap_or_default();
                for l in jl.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev() {
                    eprintln!("    {l}");
                }
                let bl = build_log(cfg, n);
                if bl.exists() {
                    error(&format!("build log: {}", bl.display()));
                }
                if !running.is_empty() {
                    info(&format!("waiting for {} running job(s) to finish...", running.len()));
                }
            }
        }
    }
    let left: Vec<String> = plan.order.iter().filter(|i| st[*i] == St::Pending).map(|&i| plan.nodes[i].e.cpv()).collect();
    if !left.is_empty() {
        warn(&format!("not merged because of the failure: {}", left.join(" ")));
    }
    Ok(!st.values().any(|s| *s == St::Failed))
}

fn print_elogs(elogs: &[(String, PathBuf)]) {
    let mut printed = false;
    for (cpv, p) in elogs {
        let t = std::fs::read_to_string(p).unwrap_or_default();
        let lines: Vec<&str> = t.lines().filter(|l| !l.split(' ').nth(1).map(|lv| lv == "QA").unwrap_or(false)).collect();
        if lines.is_empty() {
            continue;
        }
        if !printed {
            eprintln!("\n{}", bold("Messages from the packages:"));
            printed = true;
        }
        eprintln!("\n{} {}:", green(">>>"), bold(cpv));
        for l in lines {
            let mut it = l.splitn(3, ' ');
            let (_phase, level, msg) = (it.next(), it.next().unwrap_or(""), it.next().unwrap_or(""));
            let star = match level {
                "WARN" => yellow(" *"),
                "ERROR" => red(" *"),
                _ => green(" *"),
            };
            eprintln!("{star} {msg}");
        }
    }
}

fn pending_cfg_files(cfg: &Config) -> usize {
    fn walk(d: &Path, n: &mut usize, depth: usize) {
        if depth > 6 {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let p = e.path();
                if name.starts_with("._cfg") {
                    *n += 1;
                } else if p.is_dir() && !p.is_symlink() {
                    walk(&p, n, depth + 1);
                }
            }
        }
    }
    let mut n = 0;
    walk(&cfg.root.join("etc"), &mut n, 0);
    n
}

fn add_world(cfg: &Config, add: &[String]) -> Result<(), String> {
    if add.is_empty() {
        return Ok(());
    }
    let p = cfg.world_file();
    let mut w = vdb::read_world(&p);
    let mut changed = false;
    for x in add {
        if !w.contains(x) {
            w.push(x.clone());
            changed = true;
            info(&format!("Recording {} in the world file", bold(x)));
        }
    }
    if changed {
        vdb::write_world(&p, &w).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

// ------------------------------------------------------------------ remove

fn confirm(a: &Args, q: &str) -> bool {
    a.flags.contains("yes") || util::ask(q)
}

fn cmd_remove(cfg: &Config, a: &Args) -> Result<i32, String> {
    if a.pos.is_empty() {
        return Err("nothing to remove".into());
    }
    let pretend = a.flags.contains("pretend");
    if !pretend {
        need_root()?;
    }
    let _lock = if pretend { None } else { Some(util::lock(Path::new("/var/lib/spas/lock"))?) };
    let vdb = Vdb::load(&cfg.vdb_path());
    let mut victims: Vec<&vdb::Installed> = Vec::new();
    for t in &a.pos {
        let atom = resolve::parse_user_atom(cfg, &vdb, t)?;
        let m = vdb.matching_nouse(&atom);
        if m.is_empty() {
            return Err(format!("{t} is not installed"));
        }
        for p in m {
            let in_system = cfg.system.iter().any(|s| s.matches_nouse(&p.target()));
            if in_system && !a.flags.contains("force") {
                return Err(format!("{} is part of @system; removing it could break the system (use --force if you are sure)", p.cpv()));
            }
            victims.push(p);
        }
    }
    println!();
    for v in &victims {
        println!("[{}] {}", red("uninstall"), green(&v.cpv()));
    }
    if pretend || !confirm(a, "Would you like to unmerge these packages?") {
        return Ok(0);
    }
    for v in &victims {
        merge::uninstall(cfg, v, a.flags.contains("verbose"))?;
    }
    let p = cfg.world_file();
    let gone: HashSet<String> = victims.iter().map(|v| v.cp()).collect();
    let still: HashSet<String> = Vdb::load(&cfg.vdb_path()).pkgs.iter().map(|p| p.cp()).collect();
    let w: Vec<String> = vdb::read_world(&p)
        .into_iter()
        .filter(|e| {
            let cp = e.split(':').next().unwrap_or(e);
            !(gone.contains(cp) && !still.contains(cp))
        })
        .collect();
    vdb::write_world(&p, &w).map_err(|e| e.to_string())?;
    if let Err(e) = preserve::prune(cfg) {
        warn(&format!("preserved libraries: {e}"));
    }
    envupdate::env_update(&cfg.root, true)?;
    preserve::report(cfg);
    Ok(0)
}

fn cmd_autoremove(cfg: &Config, a: &Args) -> Result<i32, String> {
    let pretend = a.flags.contains("pretend");
    if !pretend {
        need_root()?;
    }
    let _lock = if pretend { None } else { Some(util::lock(Path::new("/var/lib/spas/lock"))?) };
    let vdb = Vdb::load(&cfg.vdb_path());
    if vdb::read_world(&cfg.world_file()).is_empty() {
        return Err("the world file is empty; refusing to remove everything".into());
    }
    let list = resolve::unneeded(cfg, &vdb);
    if list.is_empty() {
        info("Nothing to remove.");
        return Ok(0);
    }
    println!();
    for c in &list {
        println!("[{}] {}", red("uninstall"), green(c));
    }
    println!("\n{} package(s) are not needed by @world or @system.", list.len());
    if pretend || !confirm(a, "Would you like to unmerge these packages?") {
        return Ok(0);
    }
    for c in &list {
        let v = Vdb::load(&cfg.vdb_path());
        if let Some(inst) = v.pkgs.iter().find(|p| p.cpv() == *c) {
            if let Err(e) = merge::uninstall(cfg, inst, false) {
                error(&e);
            }
        }
    }
    envupdate::env_update(&cfg.root, true)?;
    Ok(0)
}

// ------------------------------------------------------------------ queries

fn cmd_search(cfg: &Config, a: &Args) -> Result<i32, String> {
    let pat = a.pos.first().ok_or("search for what?")?;
    let re = regex::Regex::new(&format!("(?i){pat}")).map_err(|e| e.to_string())?;
    let vdb = Vdb::load(&cfg.vdb_path());
    let desc = a.flags.contains("description");
    let mut n = 0;
    for (cat, pn) in cfg.repos.all_packages() {
        let cp = format!("{cat}/{pn}");
        let name_hit = re.is_match(&cp);
        if !name_hit && !desc {
            continue;
        }
        let ebuilds = cfg.repos.ebuilds(&cat, &pn);
        let Some(latest) = ebuilds.iter().find(|e| cfg.visibility(e).is_none()).or(ebuilds.first()) else { continue };
        if !name_hit && !re.is_match(latest.meta.get("DESCRIPTION")) {
            continue;
        }
        n += 1;
        let inst: Vec<String> = vdb.pkgs.iter().filter(|p| p.cp() == cp).map(|p| p.ver.to_string()).collect();
        let vis = if cfg.visibility(latest).is_none() { latest.ver.to_string() } else { format!("{} (masked)", latest.ver) };
        println!("{}  {}", green("*"), bold(&cp));
        println!(
            "      latest: {vis}   installed: {}",
            if inst.is_empty() { "no".to_string() } else { green(&inst.join(" ")) }
        );
        println!("      {}", latest.meta.get("DESCRIPTION"));
    }
    if n == 0 {
        info("no packages found");
    }
    Ok(0)
}

fn cmd_info(cfg: &Config, a: &Args) -> Result<i32, String> {
    let vdb = Vdb::load(&cfg.vdb_path());
    for t in &a.pos {
        let atom = resolve::parse_user_atom(cfg, &vdb, t)?;
        let ebuilds = cfg.repos.matching(&atom);
        println!("{}", bold(&atom.cp()));
        for e in ebuilds.iter() {
            let installed = vdb.pkgs.iter().any(|p| p.cpv() == e.cpv());
            let vis = match cfg.visibility(e) {
                None => green("ok"),
                Some(w) => red(&w),
            };
            println!(
                "  {:<24} :{:<8} ::{:<8} {}{}",
                e.ver.to_string(),
                e.meta.get("SLOT"),
                e.repo_name,
                vis,
                if installed { format!("  {}", green("[installed]")) } else { String::new() }
            );
        }
        let best = ebuilds.iter().find(|e| cfg.visibility(e).is_none()).or(ebuilds.first());
        if let Some(e) = best {
            let u = cfg.pkg_use(e);
            let mut flags: Vec<String> = u
                .iuse
                .iter()
                .map(|f| {
                    let s = if u.enabled.contains(f) { red(f) } else { blue(&format!("-{f}")) };
                    if u.forced.contains(f) || u.masked.contains(f) { format!("({s})") } else { s }
                })
                .collect();
            flags.sort();
            flags.dedup();
            println!("  Description: {}", e.meta.get("DESCRIPTION"));
            println!("  Homepage:    {}", e.meta.get("HOMEPAGE"));
            println!("  License:     {}", e.meta.get("LICENSE"));
            println!("  Keywords:    {}", e.meta.get("KEYWORDS"));
            println!("  USE ({}):  {}", e.ver, flags.join(" "));
        }
        for p in vdb.matching_nouse(&atom) {
            let mut u: Vec<&String> = p.use_.iter().filter(|f| !f.contains('_') || p.iuse.contains(*f)).collect();
            u.sort();
            println!("  Installed:   {}  USE=\"{}\"", p.cpv(), u.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
        }
        println!();
    }
    Ok(0)
}
