//! Dependency specification strings: DEPEND, SRC_URI, LICENSE, REQUIRED_USE, RESTRICT...

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Leaf(String),
    All(Vec<Node>),
    /// `|| ( ... )`
    Any(Vec<Node>),
    /// `^^ ( ... )`
    ExactlyOne(Vec<Node>),
    /// `?? ( ... )`
    AtMostOne(Vec<Node>),
    /// `flag? ( ... )` / `!flag? ( ... )`
    Cond { flag: String, neg: bool, children: Vec<Node> },
}

pub fn parse(s: &str) -> Result<Vec<Node>, String> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let mut pos = 0;
    let nodes = parse_seq(&toks, &mut pos, false)?;
    if pos != toks.len() {
        return Err(format!("unbalanced ')' in '{s}'"));
    }
    Ok(nodes)
}

fn parse_seq(toks: &[&str], pos: &mut usize, nested: bool) -> Result<Vec<Node>, String> {
    let mut out = Vec::new();
    while *pos < toks.len() {
        let t = toks[*pos];
        if t == ")" {
            if !nested {
                return Err("unexpected ')'".into());
            }
            return Ok(out);
        }
        *pos += 1;
        let group = |pos: &mut usize| -> Result<Vec<Node>, String> {
            if toks.get(*pos) != Some(&"(") {
                return Err(format!("expected '(' after '{t}'"));
            }
            *pos += 1;
            let inner = parse_seq(toks, pos, true)?;
            if toks.get(*pos) != Some(&")") {
                return Err("missing ')'".into());
            }
            *pos += 1;
            Ok(inner)
        };
        let node = match t {
            "(" => {
                *pos -= 1;
                Node::All(group(pos)?)
            }
            "||" => Node::Any(group(pos)?),
            "^^" => Node::ExactlyOne(group(pos)?),
            "??" => Node::AtMostOne(group(pos)?),
            _ if t.ends_with('?') && t.len() > 1 => {
                let body = &t[..t.len() - 1];
                let (flag, neg) = match body.strip_prefix('!') {
                    Some(f) => (f.to_string(), true),
                    None => (body.to_string(), false),
                };
                Node::Cond { flag, neg, children: group(pos)? }
            }
            _ => Node::Leaf(t.to_string()),
        };
        out.push(node);
    }
    if nested {
        return Err("missing ')'".into());
    }
    Ok(out)
}

/// Drop USE conditionals that don't apply and flatten plain groups.
pub fn reduce(nodes: &[Node], use_: &HashSet<String>) -> Vec<Node> {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            Node::Leaf(_) => out.push(n.clone()),
            Node::All(c) => out.extend(reduce(c, use_)),
            Node::Cond { flag, neg, children } => {
                if use_.contains(flag) != *neg {
                    out.extend(reduce(children, use_));
                }
            }
            Node::Any(c) => {
                let r = reduce_group(c, use_);
                if !r.is_empty() {
                    out.push(Node::Any(r));
                }
            }
            Node::ExactlyOne(c) => out.push(Node::ExactlyOne(reduce_group(c, use_))),
            Node::AtMostOne(c) => out.push(Node::AtMostOne(reduce_group(c, use_))),
        }
    }
    out
}

/// Members of a ||/^^/?? group stay separate alternatives.
fn reduce_group(c: &[Node], use_: &HashSet<String>) -> Vec<Node> {
    let mut out = Vec::new();
    for n in c {
        match n {
            Node::Cond { flag, neg, children } => {
                if use_.contains(flag) != *neg {
                    let r = reduce(children, use_);
                    match r.len() {
                        0 => {}
                        1 => out.push(r.into_iter().next().unwrap()),
                        _ => out.push(Node::All(r)),
                    }
                }
            }
            Node::All(ch) => {
                let r = reduce(ch, use_);
                match r.len() {
                    0 => {}
                    1 => out.push(r.into_iter().next().unwrap()),
                    _ => out.push(Node::All(r)),
                }
            }
            _ => out.extend(reduce(std::slice::from_ref(n), use_)),
        }
    }
    out
}

/// Every leaf, conditionals included (for search/info output).
pub fn all_leaves(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        match n {
            Node::Leaf(s) => out.push(s.clone()),
            Node::All(c) | Node::Any(c) | Node::ExactlyOne(c) | Node::AtMostOne(c) => all_leaves(c, out),
            Node::Cond { children, .. } => all_leaves(children, out),
        }
    }
}

/// Leaves of a reduced tree, taking all alternatives of || groups.
pub fn flat_leaves(nodes: &[Node]) -> Vec<String> {
    let mut v = Vec::new();
    all_leaves(nodes, &mut v);
    v
}

pub fn render(nodes: &[Node]) -> String {
    let mut parts = Vec::new();
    for n in nodes {
        parts.push(match n {
            Node::Leaf(s) => s.clone(),
            Node::All(c) => format!("( {} )", render(c)),
            Node::Any(c) => format!("|| ( {} )", render(c)),
            Node::ExactlyOne(c) => format!("^^ ( {} )", render(c)),
            Node::AtMostOne(c) => format!("?? ( {} )", render(c)),
            Node::Cond { flag, neg, children } => {
                format!("{}{}? ( {} )", if *neg { "!" } else { "" }, flag, render(children))
            }
        });
    }
    parts.join(" ")
}

/// Check REQUIRED_USE. Returns the failing (rendered) constraints.
pub fn check_required_use(nodes: &[Node], use_: &HashSet<String>) -> Vec<String> {
    let mut bad = Vec::new();
    for n in nodes {
        if !req_ok(n, use_) {
            bad.push(render(std::slice::from_ref(n)));
        }
    }
    bad
}

fn req_ok(n: &Node, use_: &HashSet<String>) -> bool {
    let count = |c: &Vec<Node>| c.iter().filter(|x| req_ok(x, use_)).count();
    match n {
        Node::Leaf(f) => match f.strip_prefix('!') {
            Some(f) => !use_.contains(f),
            None => use_.contains(f.as_str()),
        },
        Node::All(c) => c.iter().all(|x| req_ok(x, use_)),
        Node::Any(c) => c.is_empty() || count(c) >= 1,
        Node::ExactlyOne(c) => c.is_empty() || count(c) == 1,
        Node::AtMostOne(c) => count(c) <= 1,
        Node::Cond { flag, neg, children } => {
            if use_.contains(flag) != *neg {
                children.iter().all(|x| req_ok(x, use_))
            } else {
                true
            }
        }
    }
}

/// SRC_URI after reduction: (uri, filename) pairs; `uri -> name` renames.
pub fn src_uri_files(nodes: &[Node]) -> Vec<(String, String)> {
    let leaves = flat_leaves(nodes);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < leaves.len() {
        let uri = &leaves[i];
        if i + 2 < leaves.len() && leaves[i + 1] == "->" {
            out.push((uri.clone(), leaves[i + 2].clone()));
            i += 3;
        } else {
            let name = uri.rsplit('/').next().unwrap_or(uri).to_string();
            out.push((uri.clone(), name));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reduce_and_render() {
        let n = parse("a/b ssl? ( dev-libs/openssl ) !ssl? ( || ( x/y ( z/w q/r ) ) )").unwrap();
        let mut u = HashSet::new();
        assert_eq!(render(&reduce(&n, &u)), "a/b || ( x/y ( z/w q/r ) )");
        u.insert("ssl".to_string());
        assert_eq!(render(&reduce(&n, &u)), "a/b dev-libs/openssl");
    }
    #[test]
    fn required_use() {
        let n = parse("^^ ( a b ) c? ( d )").unwrap();
        let u: HashSet<String> = ["a", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(check_required_use(&n, &u), vec!["c? ( d )"]);
    }
    #[test]
    fn srcuri() {
        let n = parse("https://x/a.tar.gz -> b.tar.gz https://y/c.zip").unwrap();
        assert_eq!(
            src_uri_files(&n),
            vec![("https://x/a.tar.gz".into(), "b.tar.gz".into()), ("https://y/c.zip".into(), "c.zip".into())]
        );
    }
}
