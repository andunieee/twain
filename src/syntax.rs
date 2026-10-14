//! Tree-sitter highlighting for the grammars compiled into twain. Files no grammar handles
//! fall back to GtkSourceView's own highlighting.
//!
//! Highlights are plain text tags named `syntax:<capture>`, colored from the GtkSourceView
//! style scheme so they follow the light/dark theme like everything else.

use gtk::prelude::*;
use std::cell::RefCell;
use std::sync::OnceLock;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Larger files are left to GtkSourceView, which highlights lazily.
const MAX_LEN: usize = 2 << 20;

/// Capture names we color, each with the scheme styles to try in order: schemes only define
/// some `def:` styles and rely on GtkSourceView's language files to map the others.
/// Captures match their longest listed prefix, so `function.method` uses `function`.
const HIGHLIGHTS: &[(&str, &[&str])] = &[
    ("attribute", &["def:preprocessor"]),
    ("boolean", &["def:boolean", "def:constant"]),
    ("character", &["def:character", "def:constant"]),
    ("comment", &["def:comment"]),
    ("constant", &["def:constant"]),
    (
        "constant.builtin",
        &["def:special-constant", "def:constant"],
    ),
    ("constructor", &["def:type"]),
    ("escape", &["def:special-char"]),
    ("function", &["def:function"]),
    ("function.builtin", &["def:builtin", "def:function"]),
    ("function.macro", &["def:preprocessor"]),
    ("keyword", &["def:keyword", "def:statement"]),
    ("label", &["def:preprocessor"]),
    ("number", &["def:number"]),
    ("operator", &["def:operator"]),
    ("punctuation.special", &["def:special-char"]),
    ("string", &["def:string"]),
    ("string.special", &["def:special-char", "def:string"]),
    ("tag", &["def:keyword", "def:statement"]),
    ("type", &["def:type"]),
    ("variable.builtin", &["def:builtin", "def:statement"]),
];

struct Lang {
    name: &'static str,
    /// Lowercase file extensions, or whole file names.
    files: &'static [&'static str],
    build: fn() -> Result<HighlightConfiguration, tree_sitter::QueryError>,
    config: OnceLock<Option<HighlightConfiguration>>,
}

impl Lang {
    const fn new(
        name: &'static str,
        files: &'static [&'static str],
        build: fn() -> Result<HighlightConfiguration, tree_sitter::QueryError>,
    ) -> Lang {
        Lang {
            name,
            files,
            build,
            config: OnceLock::new(),
        }
    }

    fn config(&'static self) -> Option<&'static HighlightConfiguration> {
        self.config
            .get_or_init(|| match (self.build)() {
                Ok(mut config) => {
                    config.configure(&HIGHLIGHTS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
                    Some(config)
                }
                Err(e) => {
                    eprintln!("twain: bad {} highlight query: {e}", self.name);
                    None
                }
            })
            .as_ref()
    }
}

fn config(
    lang: impl Into<tree_sitter::Language>,
    name: &str,
    highlights: &str,
    injections: &str,
    locals: &str,
) -> Result<HighlightConfiguration, tree_sitter::QueryError> {
    HighlightConfiguration::new(lang.into(), name, highlights, injections, locals)
}

static LANGS: [Lang; 17] = [
    Lang::new(
        "bash",
        &["sh", "bash", "zsh", ".bashrc", ".bash_profile", "PKGBUILD"],
        || {
            config(
                tree_sitter_bash::LANGUAGE,
                "bash",
                tree_sitter_bash::HIGHLIGHT_QUERY,
                "",
                "",
            )
        },
    ),
    Lang::new("c", &["c", "h"], || {
        config(
            tree_sitter_c::LANGUAGE,
            "c",
            tree_sitter_c::HIGHLIGHT_QUERY,
            "",
            "",
        )
    }),
    Lang::new(
        "cpp",
        &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
        || {
            // C++'s query only adds to C's.
            let highlights = [
                tree_sitter_cpp::HIGHLIGHT_QUERY,
                tree_sitter_c::HIGHLIGHT_QUERY,
            ];
            config(
                tree_sitter_cpp::LANGUAGE,
                "cpp",
                &highlights.concat(),
                "",
                "",
            )
        },
    ),
    Lang::new("css", &["css"], || {
        config(
            tree_sitter_css::LANGUAGE,
            "css",
            tree_sitter_css::HIGHLIGHTS_QUERY,
            "",
            "",
        )
    }),
    Lang::new("go", &["go"], || {
        config(
            tree_sitter_go::LANGUAGE,
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
            "",
        )
    }),
    Lang::new("html", &["html", "htm"], || {
        use tree_sitter_html as l;
        config(
            l::LANGUAGE,
            "html",
            l::HIGHLIGHTS_QUERY,
            l::INJECTIONS_QUERY,
            "",
        )
    }),
    Lang::new("java", &["java"], || {
        config(
            tree_sitter_java::LANGUAGE,
            "java",
            tree_sitter_java::HIGHLIGHTS_QUERY,
            "",
            "",
        )
    }),
    Lang::new("javascript", &["js", "mjs", "cjs", "jsx"], || {
        use tree_sitter_javascript as l;
        let highlights = [l::JSX_HIGHLIGHT_QUERY, l::HIGHLIGHT_QUERY].concat();
        config(
            l::LANGUAGE,
            "javascript",
            &highlights,
            l::INJECTIONS_QUERY,
            l::LOCALS_QUERY,
        )
    }),
    Lang::new("json", &["json"], || {
        config(
            tree_sitter_json::LANGUAGE,
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            "",
        )
    }),
    Lang::new("lua", &["lua"], || {
        use tree_sitter_lua as l;
        config(
            l::LANGUAGE,
            "lua",
            l::HIGHLIGHTS_QUERY,
            l::INJECTIONS_QUERY,
            l::LOCALS_QUERY,
        )
    }),
    Lang::new("python", &["py", "pyi", "pyw"], || {
        use tree_sitter_python as l;
        config(l::LANGUAGE, "python", l::HIGHLIGHTS_QUERY, "", "")
    }),
    Lang::new("ruby", &["rb", "gemspec", "Gemfile", "Rakefile"], || {
        use tree_sitter_ruby as l;
        config(
            l::LANGUAGE,
            "ruby",
            l::HIGHLIGHTS_QUERY,
            "",
            l::LOCALS_QUERY,
        )
    }),
    Lang::new("rust", &["rs"], || {
        use tree_sitter_rust as l;
        config(
            l::LANGUAGE,
            "rust",
            l::HIGHLIGHTS_QUERY,
            l::INJECTIONS_QUERY,
            "",
        )
    }),
    Lang::new("toml", &["toml", "Cargo.lock"], || {
        use tree_sitter_toml_ng as l;
        config(l::LANGUAGE, "toml", l::HIGHLIGHTS_QUERY, "", "")
    }),
    Lang::new("typescript", &["ts", "mts", "cts"], || {
        // TypeScript's queries only add to JavaScript's.
        use {tree_sitter_javascript as js, tree_sitter_typescript as l};
        let highlights = [l::HIGHLIGHTS_QUERY, js::HIGHLIGHT_QUERY].concat();
        let locals = [l::LOCALS_QUERY, js::LOCALS_QUERY].concat();
        config(
            l::LANGUAGE_TYPESCRIPT,
            "typescript",
            &highlights,
            js::INJECTIONS_QUERY,
            &locals,
        )
    }),
    Lang::new("tsx", &["tsx"], || {
        use {tree_sitter_javascript as js, tree_sitter_typescript as l};
        let highlights = [
            l::HIGHLIGHTS_QUERY,
            js::JSX_HIGHLIGHT_QUERY,
            js::HIGHLIGHT_QUERY,
        ]
        .concat();
        let locals = [l::LOCALS_QUERY, js::LOCALS_QUERY].concat();
        config(
            l::LANGUAGE_TSX,
            "tsx",
            &highlights,
            js::INJECTIONS_QUERY,
            &locals,
        )
    }),
    Lang::new("yaml", &["yaml", "yml"], || {
        config(
            tree_sitter_yaml::LANGUAGE,
            "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            "",
            "",
        )
    }),
];

fn for_path(path: &str) -> Option<&'static HighlightConfiguration> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    LANGS
        .iter()
        .find(|l| l.files.contains(&name))
        .or_else(|| {
            LANGS
                .iter()
                .find(|l| ext.as_deref().is_some_and(|e| l.files.contains(&e)))
        })
        .and_then(Lang::config)
}

/// Grammar for a language named in an injection query, e.g. `regex` or `js`.
fn for_injection(name: &str) -> Option<&'static HighlightConfiguration> {
    LANGS
        .iter()
        .find(|l| l.name == name || l.files.contains(&name))
        .and_then(Lang::config)
}

/// Everything `Style::apply` may set, cleared before applying another style.
const RESET: &[&str] = &[
    "foreground-set",
    "background-set",
    "paragraph-background-set",
    "weight-set",
    "style-set",
    "underline-set",
    "underline-rgba-set",
    "strikethrough-set",
    "scale-set",
];

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

/// Highlights `buffer`, which holds `text`, with tree-sitter. Returns false, adding nothing,
/// if no grammar handles `path`. Call it before adding other tags, so they take priority.
pub fn highlight(
    buffer: &sourceview5::Buffer,
    text: &str,
    path: &str,
    scheme: Option<&sourceview5::StyleScheme>,
) -> bool {
    if text.len() > MAX_LEN {
        return false;
    }
    let Some(config) = for_path(path) else {
        return false;
    };
    let table = buffer.tag_table();
    let tags: Vec<_> = HIGHLIGHTS
        .iter()
        .map(|(name, _)| {
            let tag = gtk::TextTag::new(Some(&format!("syntax:{name}")));
            table.add(&tag);
            tag
        })
        .collect();
    set_style_scheme(buffer, scheme);

    HIGHLIGHTER.with_borrow_mut(|highlighter| {
        let Ok(events) = highlighter.highlight(config, text.as_bytes(), None, None, |name| {
            for_injection(name)
        }) else {
            return;
        };
        // Events come in text order, so byte offsets convert to char offsets incrementally.
        let (mut byte, mut chars) = (0, 0);
        let mut iter_at = |b: usize| {
            chars += text.get(byte..b)?.chars().count();
            byte = b;
            Some(buffer.iter_at_offset(chars as i32))
        };
        let mut stack = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(h)) => stack.push(h.0),
                Ok(HighlightEvent::HighlightEnd) => _ = stack.pop(),
                Ok(HighlightEvent::Source { start, end }) => {
                    let Some(&h) = stack.last() else { continue };
                    let (Some(start), Some(end)) = (iter_at(start), iter_at(end)) else {
                        return;
                    };
                    buffer.apply_tag(&tags[h], &start, &end);
                }
                Err(_) => return,
            }
        }
    });
    true
}

/// Recolors the syntax tags of `buffer`, if it has any.
pub fn set_style_scheme(buffer: &sourceview5::Buffer, scheme: Option<&sourceview5::StyleScheme>) {
    let table = buffer.tag_table();
    for (name, styles) in HIGHLIGHTS {
        let Some(tag) = table.lookup(&format!("syntax:{name}")) else {
            return;
        };
        for prop in RESET {
            tag.set_property(prop, false);
        }
        if let Some(style) = scheme.and_then(|s| styles.iter().find_map(|id| s.style(id))) {
            style.apply(&tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_compile() {
        for lang in &LANGS {
            if let Err(e) = (lang.build)() {
                panic!("{}: {e}", lang.name);
            }
        }
    }
}
