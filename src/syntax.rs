//! Tree-sitter highlighting for the grammars compiled into twain. Files no grammar handles
//! fall back to GtkSourceView's own highlighting.
//!
//! Highlights are plain text tags named `syntax:<capture>`, colored from twain's own palette:
//! the Adwaita style schemes only have colors for a dozen kinds of token.

use gtk::prelude::*;
use std::cell::RefCell;
use std::sync::OnceLock;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Larger files are left to GtkSourceView, which highlights lazily.
const MAX_LEN: usize = 2 << 20;

/// A color for light and one for dark mode, from the GNOME palette.
type Color = (&'static str, &'static str);

const KEYWORD: Color = ("#813d9c", "#dc8add");
const STRING: Color = ("#26773a", "#8ff0a4");
const SPECIAL: Color = ("#a51d2d", "#f66151");
const CONSTANT: Color = ("#c64600", "#ffa348");
const TYPE: Color = ("#0b6e7f", "#5ec8d8");
const FUNCTION: Color = ("#1a5fb4", "#99c1f1");
const MACRO: Color = ("#8a6100", "#f8e45c");
const PROPERTY: Color = ("#a3305b", "#f5a3c5");
const MODULE: Color = ("#865e3c", "#cdab8f");
const COMMENT: Color = ("#77767b", "#9a9996");
const DOC: Color = ("#5e5c64", "#b0afac");

#[derive(Clone, Copy)]
struct Look {
    color: Option<Color>,
    bold: bool,
    italic: bool,
}

const fn fg(color: Color) -> Look {
    Look {
        color: Some(color),
        bold: false,
        italic: false,
    }
}

impl Look {
    const fn bold(self) -> Look {
        Look { bold: true, ..self }
    }

    const fn italic(self) -> Look {
        Look {
            italic: true,
            ..self
        }
    }
}

const ITALIC: Look = Look {
    color: None,
    bold: false,
    italic: true,
};

/// Capture names we color. A capture uses the listed name sharing the most dot-separated
/// parts with it, so `function.method` uses `function` and `keyword.return` `keyword`.
const HIGHLIGHTS: &[(&str, Look)] = &[
    ("attribute", fg(MACRO)),
    ("boolean", fg(CONSTANT)),
    ("character", fg(STRING)),
    ("charset", fg(KEYWORD).bold()),
    ("comment", fg(COMMENT).italic()),
    ("comment.documentation", fg(DOC).italic()),
    ("conditional", fg(KEYWORD).bold()),
    ("constant", fg(CONSTANT)),
    ("constructor", fg(TYPE)),
    ("escape", fg(SPECIAL)),
    ("field", fg(PROPERTY)),
    ("function", fg(FUNCTION)),
    ("function.macro", fg(MACRO)),
    ("import", fg(KEYWORD).bold()),
    ("keyframes", fg(KEYWORD).bold()),
    ("keyword", fg(KEYWORD).bold()),
    ("label", fg(MODULE).italic()),
    ("media", fg(KEYWORD).bold()),
    ("method", fg(FUNCTION)),
    ("module", fg(MODULE)),
    ("namespace", fg(MODULE)),
    ("number", fg(CONSTANT)),
    ("parameter", ITALIC),
    ("preproc", fg(MACRO)),
    ("property", fg(PROPERTY)),
    ("punctuation.special", fg(SPECIAL)),
    ("repeat", fg(KEYWORD).bold()),
    ("string", fg(STRING)),
    ("string.special", fg(SPECIAL)),
    ("string.special.key", fg(PROPERTY)),
    ("string.special.symbol", fg(CONSTANT)),
    ("supports", fg(KEYWORD).bold()),
    ("tag", fg(TYPE)),
    ("type", fg(TYPE)),
    ("variable.builtin", fg(KEYWORD).italic()),
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
        config(l::LANGUAGE, "rust", l::HIGHLIGHTS_QUERY, l::INJECTIONS_QUERY, "")
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

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
}

/// Highlights `buffer`, which holds `text`, with tree-sitter. Returns false, adding nothing,
/// if no grammar handles `path`. Call it before adding other tags, so they take priority.
pub fn highlight(buffer: &sourceview5::Buffer, text: &str, path: &str) -> bool {
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
    recolor(buffer);

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

/// Sets the colors of `buffer`'s syntax tags, if it has any, for light or dark mode.
pub fn recolor(buffer: &sourceview5::Buffer) {
    let dark = adw::StyleManager::default().is_dark();
    let table = buffer.tag_table();
    for (name, look) in HIGHLIGHTS {
        let Some(tag) = table.lookup(&format!("syntax:{name}")) else {
            return;
        };
        if let Some((light, dark_color)) = look.color {
            tag.set_foreground(Some(if dark { dark_color } else { light }));
        }
        if look.bold {
            tag.set_weight(700);
        }
        if look.italic {
            tag.set_style(gtk::pango::Style::Italic);
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
