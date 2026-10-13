//! The directory tree shown in the sidebar.

use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Node {
    pub name: String,
    pub dir: bool,
    /// Index into the file list for changed files, `None` for unchanged files and directories.
    pub file: Option<usize>,
    /// Whether this is a changed file or a directory containing one.
    pub changed: bool,
    pub status: char,
    pub children: Vec<Rc<Node>>,
}

#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<&'a str, Dir<'a>>,
    files: Vec<(&'a str, Option<usize>, char)>,
    changed: bool,
}

/// Builds a tree from `(path, status)` pairs of changed files plus the paths of `unchanged`
/// ones. Directories come before files and chains of single-child directories are merged
/// ("src/ui" instead of "src" → "ui").
pub fn build(paths: &[(&str, char)], unchanged: &[&str]) -> Vec<Rc<Node>> {
    let mut root = Dir::default();
    let changed = paths.iter().enumerate().map(|(i, &(p, s))| (p, Some(i), s));
    let unchanged = unchanged.iter().map(|&p| (p, None, ' '));
    for (path, file, status) in changed.chain(unchanged) {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap();
        let dir = parts.into_iter().fold(&mut root, |d, p| {
            let sub = d.dirs.entry(p).or_default();
            sub.changed |= file.is_some();
            sub
        });
        dir.files.push((name, file, status));
    }
    convert(root)
}

fn convert(dir: Dir) -> Vec<Rc<Node>> {
    let mut out = Vec::new();
    for (name, mut sub) in dir.dirs {
        let mut name = name.to_string();
        while sub.files.is_empty() && sub.dirs.len() == 1 {
            let (child_name, child) = sub.dirs.pop_first().unwrap();
            name = format!("{name}/{child_name}");
            sub = child;
        }
        out.push(Rc::new(Node {
            name,
            dir: true,
            file: None,
            changed: sub.changed,
            status: ' ',
            children: convert(sub),
        }));
    }
    let mut files = dir.files;
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (name, file, status) in files {
        out.push(Rc::new(Node {
            name: name.into(),
            dir: false,
            file,
            changed: file.is_some(),
            status,
            children: Vec::new(),
        }));
    }
    out
}

/// Changed file indices in depth-first (display) order.
pub fn file_order(nodes: &[Rc<Node>]) -> Vec<usize> {
    let mut out = Vec::new();
    for n in nodes {
        match n.file {
            Some(i) => out.push(i),
            None => out.extend(file_order(&n.children)),
        }
    }
    out
}
