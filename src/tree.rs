//! The directory tree shown in the sidebar.

use std::collections::BTreeMap;
use std::rc::Rc;

pub struct Node {
    pub name: String,
    /// Index into the file list for files, `None` for directories.
    pub file: Option<usize>,
    pub status: char,
    pub children: Vec<Rc<Node>>,
}

#[derive(Default)]
struct Dir<'a> {
    dirs: BTreeMap<&'a str, Dir<'a>>,
    files: Vec<(&'a str, usize, char)>,
}

/// Builds a tree from `(path, status)` pairs. Directories come before files and chains of
/// single-child directories are merged ("src/ui" instead of "src" → "ui").
pub fn build(paths: &[(&str, char)]) -> Vec<Rc<Node>> {
    let mut root = Dir::default();
    for (i, &(path, status)) in paths.iter().enumerate() {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = parts.pop().unwrap();
        let dir = parts
            .into_iter()
            .fold(&mut root, |d, p| d.dirs.entry(p).or_default());
        dir.files.push((name, i, status));
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
            file: None,
            status: ' ',
            children: convert(sub),
        }));
    }
    let mut files = dir.files;
    files.sort_by(|a, b| a.0.cmp(b.0));
    for (name, i, status) in files {
        out.push(Rc::new(Node {
            name: name.into(),
            file: Some(i),
            status,
            children: Vec::new(),
        }));
    }
    out
}

/// File indices in depth-first (display) order.
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
