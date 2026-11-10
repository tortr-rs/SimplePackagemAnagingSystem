//! Reader for the shell-variable files Gentoo uses for configuration
//! (make.conf, make.defaults, env.d, package.env files). It understands
//! KEY=value with '...' / "..." quoting, line continuations, $VAR / ${VAR}
//! expansion, comments, `export`, and `source`/`.` includes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type Vars = Vec<(String, String)>;

/// Parse `path`. Expansions see variables from this file first, then `outer`.
pub fn parse_file(path: &Path, outer: &HashMap<String, String>) -> Result<Vars, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut local: HashMap<String, String> = HashMap::new();
    let mut out = Vars::new();
    parse_into(&text, path, outer, &mut local, &mut out, 0)?;
    Ok(out)
}

pub fn parse_str(text: &str, outer: &HashMap<String, String>) -> Result<Vars, String> {
    let mut local = HashMap::new();
    let mut out = Vars::new();
    parse_into(text, Path::new("<string>"), outer, &mut local, &mut out, 0)?;
    Ok(out)
}

fn parse_into(
    text: &str,
    path: &Path,
    outer: &HashMap<String, String>,
    local: &mut HashMap<String, String>,
    out: &mut Vars,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("{}: too many nested source statements", path.display()));
    }
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    let err = |msg: &str, i: usize| -> String {
        let line = c[..i.min(c.len())].iter().filter(|&&x| x == '\n').count() + 1;
        format!("{}:{line}: {msg}", path.display())
    };
    while i < c.len() {
        // skip blanks, newlines, ';'
        while i < c.len() && (c[i].is_whitespace() || c[i] == ';') {
            i += 1;
        }
        if i >= c.len() {
            break;
        }
        if c[i] == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // read a word
        let start = i;
        while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_' || c[i] == '.' || c[i] == '/' || c[i] == '-') {
            i += 1;
        }
        let word: String = c[start..i].iter().collect();
        if word == "export" && i < c.len() && c[i].is_whitespace() {
            continue;
        }
        if (word == "source" || word == ".") && i < c.len() && c[i].is_whitespace() {
            while i < c.len() && (c[i] == ' ' || c[i] == '\t') {
                i += 1;
            }
            let (val, ni) = read_value(&c, i, outer, local).map_err(|m| err(&m, i))?;
            i = ni;
            let mut p = PathBuf::from(&val);
            if p.is_relative() {
                p = path.parent().unwrap_or(Path::new("/")).join(p);
            }
            let t = std::fs::read_to_string(&p).map_err(|e| err(&format!("source {}: {e}", p.display()), i))?;
            parse_into(&t, &p, outer, local, out, depth + 1)?;
            continue;
        }
        if word.is_empty() || i >= c.len() || c[i] != '=' {
            return Err(err(&format!("expected VAR=value, found '{}'", if word.is_empty() { c[i].to_string() } else { word }), i));
        }
        if !word.chars().next().map(|x| x.is_ascii_alphabetic() || x == '_').unwrap_or(false)
            || !word.chars().all(|x| x.is_ascii_alphanumeric() || x == '_')
        {
            return Err(err(&format!("invalid variable name '{word}'"), i));
        }
        i += 1; // '='
        let (val, ni) = read_value(&c, i, outer, local).map_err(|m| err(&m, i))?;
        i = ni;
        local.insert(word.clone(), val.clone());
        out.push((word, val));
    }
    Ok(())
}

fn lookup(name: &str, outer: &HashMap<String, String>, local: &HashMap<String, String>) -> String {
    local.get(name).or_else(|| outer.get(name)).cloned().unwrap_or_default()
}

/// Read one shell word starting at `i`, performing quote removal and expansion.
fn read_value(
    c: &[char],
    mut i: usize,
    outer: &HashMap<String, String>,
    local: &HashMap<String, String>,
) -> Result<(String, usize), String> {
    let mut v = String::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() || ch == ';' {
            break;
        }
        match ch {
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    v.push(c[i]);
                    i += 1;
                }
                if i >= c.len() {
                    return Err("unterminated '".into());
                }
                i += 1;
            }
            '"' => {
                i += 1;
                loop {
                    if i >= c.len() {
                        return Err("unterminated \"".into());
                    }
                    match c[i] {
                        '"' => {
                            i += 1;
                            break;
                        }
                        '\\' if i + 1 < c.len() => {
                            let n = c[i + 1];
                            match n {
                                '\n' => {}
                                '"' | '\\' | '$' | '`' => v.push(n),
                                _ => {
                                    v.push('\\');
                                    v.push(n);
                                }
                            }
                            i += 2;
                        }
                        '$' => {
                            let (s, ni) = expand(c, i, outer, local)?;
                            v.push_str(&s);
                            i = ni;
                        }
                        x => {
                            v.push(x);
                            i += 1;
                        }
                    }
                }
            }
            '\\' if i + 1 < c.len() => {
                if c[i + 1] != '\n' {
                    v.push(c[i + 1]);
                }
                i += 2;
            }
            '$' => {
                let (s, ni) = expand(c, i, outer, local)?;
                v.push_str(&s);
                i = ni;
            }
            '#' if v.is_empty() => break,
            x => {
                v.push(x);
                i += 1;
            }
        }
    }
    Ok((v, i))
}

fn expand(
    c: &[char],
    i: usize,
    outer: &HashMap<String, String>,
    local: &HashMap<String, String>,
) -> Result<(String, usize), String> {
    // c[i] == '$'
    let mut j = i + 1;
    if j < c.len() && c[j] == '{' {
        j += 1;
        let s = j;
        while j < c.len() && c[j] != '}' {
            j += 1;
        }
        if j >= c.len() {
            return Err("unterminated ${".into());
        }
        let inner: String = c[s..j].iter().collect();
        let val = if let Some((name, def)) = inner.split_once(":-") {
            let v = lookup(name, outer, local);
            if v.is_empty() { def.to_string() } else { v }
        } else if let Some((name, def)) = inner.split_once('-') {
            match local.get(name).or_else(|| outer.get(name)) {
                Some(v) => v.clone(),
                None => def.to_string(),
            }
        } else {
            lookup(&inner, outer, local)
        };
        return Ok((val, j + 1));
    }
    let s = j;
    while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
        j += 1;
    }
    if s == j {
        return Ok(("$".into(), i + 1));
    }
    let name: String = c[s..j].iter().collect();
    Ok((lookup(&name, outer, local), j))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basics() {
        let mut outer = HashMap::new();
        outer.insert("USE".to_string(), "a b".to_string());
        let v = parse_str(
            "# comment\nCOMMON=\"-O2 -pipe\"\nCFLAGS=\"${COMMON} -march=native\"\nexport X='lit $Y'\nUSE=\"${USE} c\"\nMULTI=\"one\n  two\"\nZ=a\\ b # trailing\n",
            &outer,
        )
        .unwrap();
        let m: HashMap<_, _> = v.into_iter().collect();
        assert_eq!(m["CFLAGS"], "-O2 -pipe -march=native");
        assert_eq!(m["X"], "lit $Y");
        assert_eq!(m["USE"], "a b c");
        assert_eq!(m["MULTI"], "one\n  two");
        assert_eq!(m["Z"], "a b");
    }
}
