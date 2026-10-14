//! Tree-sitter highlighting. Grammars are WebAssembly modules from
//! `@cursorless/tree-sitter-wasms`, downloaded the first time a file needs one, together with
//! the highlight queries of the sources they were built from, and kept in
//! `$XDG_CACHE_HOME/twain/grammars`. Until its grammar is ready, and for files no grammar
//! handles, a file gets GtkSourceView's own highlighting.
//!
//! Highlights are plain text tags named `syntax:<capture>`, colored from twain's own palette:
//! the Adwaita style schemes only have colors for a dozen kinds of token.

use gtk::prelude::*;
use gtk::{gio, glib};
use sourceview5::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use tree_sitter::{WasmStore, wasmtime};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// The version of `@cursorless/tree-sitter-wasms` to use. Grammars cached from an older one
/// are downloaded again.
const VERSION: &str = "0.10.0";

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

const HIGHLIGHTS_QUERY: &str = "queries/highlights.scm";
const INJECTIONS_QUERY: &str = "queries/injections.scm";
const LOCALS_QUERY: &str = "queries/locals.scm";
const QUERY_KINDS: [&str; 3] = ["highlights", "injections", "locals"];

struct Lang {
    /// The grammar's name in the package, and in injection queries.
    name: &'static str,
    /// Lowercase file extensions, or whole file names.
    files: &'static [&'static str],
    /// Query files, as a package from the `devDependencies` of the one with the grammars, and
    /// a path in it. A file is a highlights, injections or locals query after the start of its
    /// name; several of a kind are joined in order.
    queries: &'static [(&'static str, &'static str)],
}

const fn lang(
    name: &'static str,
    files: &'static [&'static str],
    queries: &'static [(&'static str, &'static str)],
) -> Lang {
    Lang {
        name,
        files,
        queries,
    }
}

/// The grammars with highlight queries.
static LANGS: &[Lang] = &[
    lang("agda", &["agda"], &[("tree-sitter-agda", HIGHLIGHTS_QUERY)]),
    lang(
        "bash",
        &["sh", "bash", "zsh", ".bashrc", ".bash_profile", "PKGBUILD"],
        &[("tree-sitter-bash", HIGHLIGHTS_QUERY)],
    ),
    lang("c", &["c", "h"], &[("tree-sitter-c", HIGHLIGHTS_QUERY)]),
    lang(
        "clojure",
        &["clj", "cljs", "cljc", "edn"],
        &[("tree-sitter-clojure", HIGHLIGHTS_QUERY)],
    ),
    lang(
        "cpp",
        &["cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++"],
        // C++'s highlights only add to C's.
        &[
            ("tree-sitter-cpp", HIGHLIGHTS_QUERY),
            ("tree-sitter-c", HIGHLIGHTS_QUERY),
            ("tree-sitter-cpp", INJECTIONS_QUERY),
        ],
    ),
    lang("css", &["css"], &[("tree-sitter-css", HIGHLIGHTS_QUERY)]),
    lang("dart", &["dart"], &[("tree-sitter-dart", HIGHLIGHTS_QUERY)]),
    lang(
        "dtd",
        &["dtd"],
        &[("tree-sitter-xml", "queries/dtd/highlights.scm")],
    ),
    lang(
        "elixir",
        &["ex", "exs"],
        &[
            ("tree-sitter-elixir", HIGHLIGHTS_QUERY),
            ("tree-sitter-elixir", INJECTIONS_QUERY),
        ],
    ),
    lang(
        "elm",
        &["elm"],
        &[
            ("@elm-tooling/tree-sitter-elm", HIGHLIGHTS_QUERY),
            ("@elm-tooling/tree-sitter-elm", INJECTIONS_QUERY),
            ("@elm-tooling/tree-sitter-elm", LOCALS_QUERY),
        ],
    ),
    lang(
        "gleam",
        &["gleam"],
        &[
            ("tree-sitter-gleam", HIGHLIGHTS_QUERY),
            ("tree-sitter-gleam", LOCALS_QUERY),
        ],
    ),
    lang("go", &["go"], &[("tree-sitter-go", HIGHLIGHTS_QUERY)]),
    lang(
        "haskell",
        &["hs"],
        &[
            ("tree-sitter-haskell", HIGHLIGHTS_QUERY),
            ("tree-sitter-haskell", INJECTIONS_QUERY),
            ("tree-sitter-haskell", LOCALS_QUERY),
        ],
    ),
    lang(
        "html",
        &["html", "htm"],
        &[
            ("tree-sitter-html", HIGHLIGHTS_QUERY),
            ("tree-sitter-html", INJECTIONS_QUERY),
        ],
    ),
    lang("java", &["java"], &[("tree-sitter-java", HIGHLIGHTS_QUERY)]),
    lang(
        "javascript",
        &["js", "mjs", "cjs", "jsx"],
        &[
            ("tree-sitter-javascript", "queries/highlights-jsx.scm"),
            ("tree-sitter-javascript", HIGHLIGHTS_QUERY),
            ("tree-sitter-javascript", INJECTIONS_QUERY),
            ("tree-sitter-javascript", LOCALS_QUERY),
        ],
    ),
    lang("json", &["json"], &[("tree-sitter-json", HIGHLIGHTS_QUERY)]),
    lang(
        "kotlin",
        &["kt", "kts"],
        &[("tree-sitter-kotlin", HIGHLIGHTS_QUERY)],
    ),
    lang(
        "lua",
        &["lua"],
        &[
            ("tree-sitter-lua", HIGHLIGHTS_QUERY),
            ("tree-sitter-lua", INJECTIONS_QUERY),
            ("tree-sitter-lua", LOCALS_QUERY),
        ],
    ),
    lang(
        "markdown",
        &["md", "markdown"],
        &[
            (
                "tree-sitter-markdown",
                "tree-sitter-markdown/queries/highlights.scm",
            ),
            (
                "tree-sitter-markdown",
                "tree-sitter-markdown/queries/injections.scm",
            ),
        ],
    ),
    // Only injected, into Markdown.
    lang(
        "markdown_inline",
        &[],
        &[
            (
                "tree-sitter-markdown",
                "tree-sitter-markdown-inline/queries/highlights.scm",
            ),
            (
                "tree-sitter-markdown",
                "tree-sitter-markdown-inline/queries/injections.scm",
            ),
        ],
    ),
    lang(
        "nix",
        &["nix"],
        &[
            ("tree-sitter-nix", HIGHLIGHTS_QUERY),
            ("tree-sitter-nix", INJECTIONS_QUERY),
            ("tree-sitter-nix", LOCALS_QUERY),
        ],
    ),
    lang(
        "php",
        &["php"],
        &[
            ("tree-sitter-php", HIGHLIGHTS_QUERY),
            ("tree-sitter-php", INJECTIONS_QUERY),
        ],
    ),
    lang(
        "properties",
        &["properties"],
        &[("tree-sitter-properties", HIGHLIGHTS_QUERY)],
    ),
    lang(
        "python",
        &["py", "pyi", "pyw"],
        &[("tree-sitter-python", HIGHLIGHTS_QUERY)],
    ),
    lang(
        "r",
        &["r"],
        &[
            ("tree-sitter-r", HIGHLIGHTS_QUERY),
            ("tree-sitter-r", LOCALS_QUERY),
        ],
    ),
    lang(
        "ruby",
        &["rb", "gemspec", "Gemfile", "Rakefile"],
        &[
            ("tree-sitter-ruby", HIGHLIGHTS_QUERY),
            ("tree-sitter-ruby", LOCALS_QUERY),
        ],
    ),
    lang(
        "rust",
        &["rs"],
        &[
            ("tree-sitter-rust", HIGHLIGHTS_QUERY),
            ("tree-sitter-rust", INJECTIONS_QUERY),
        ],
    ),
    lang(
        "scala",
        &["scala", "sc", "sbt"],
        &[
            ("tree-sitter-scala", HIGHLIGHTS_QUERY),
            ("tree-sitter-scala", LOCALS_QUERY),
        ],
    ),
    // Its own highlights are out of date with its grammar, which builds on CSS's.
    lang("scss", &["scss"], &[("tree-sitter-css", HIGHLIGHTS_QUERY)]),
    lang(
        "swift",
        &["swift"],
        &[
            ("tree-sitter-swift", HIGHLIGHTS_QUERY),
            ("tree-sitter-swift", INJECTIONS_QUERY),
            ("tree-sitter-swift", LOCALS_QUERY),
        ],
    ),
    lang(
        "typescript",
        &["ts", "mts", "cts"],
        // TypeScript's queries only add to JavaScript's.
        &[
            ("tree-sitter-typescript", HIGHLIGHTS_QUERY),
            ("tree-sitter-javascript", HIGHLIGHTS_QUERY),
            ("tree-sitter-javascript", INJECTIONS_QUERY),
            ("tree-sitter-typescript", LOCALS_QUERY),
            ("tree-sitter-javascript", LOCALS_QUERY),
        ],
    ),
    lang(
        "tsx",
        &["tsx"],
        &[
            ("tree-sitter-typescript", HIGHLIGHTS_QUERY),
            ("tree-sitter-javascript", "queries/highlights-jsx.scm"),
            ("tree-sitter-javascript", HIGHLIGHTS_QUERY),
            ("tree-sitter-javascript", INJECTIONS_QUERY),
            ("tree-sitter-typescript", LOCALS_QUERY),
            ("tree-sitter-javascript", LOCALS_QUERY),
        ],
    ),
    lang(
        "xml",
        &["xml", "svg", "xsd", "xsl", "xslt"],
        &[("tree-sitter-xml", "queries/xml/highlights.scm")],
    ),
    lang(
        "yaml",
        &["yaml", "yml"],
        &[("tree-sitter-yaml", HIGHLIGHTS_QUERY)],
    ),
    lang(
        "zig",
        &["zig"],
        &[
            ("tree-sitter-zig", HIGHLIGHTS_QUERY),
            ("tree-sitter-zig", INJECTIONS_QUERY),
            ("tree-sitter-zig", LOCALS_QUERY),
        ],
    ),
];

fn for_path(path: &str) -> Option<&'static Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    LANGS.iter().find(|l| l.files.contains(&name)).or_else(|| {
        LANGS
            .iter()
            .find(|l| ext.as_deref().is_some_and(|e| l.files.contains(&e)))
    })
}

/// Language named in an injection query, e.g. `regex` or `js`.
fn for_injection(name: &str) -> Option<&'static Lang> {
    LANGS
        .iter()
        .find(|l| l.name == name || l.files.contains(&name))
}

enum State {
    /// Being fetched. These buffers, each with its file's language, are highlighted again
    /// once it's ready.
    Loading(Vec<(glib::WeakRef<sourceview5::Buffer>, &'static Lang)>),
    /// None if it couldn't be had.
    Ready(Option<&'static HighlightConfiguration>),
}

thread_local! {
    static STATES: RefCell<HashMap<&'static str, State>> = RefCell::default();
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new({
        let mut highlighter = Highlighter::new();
        match WasmStore::new(engine()) {
            Ok(store) => _ = highlighter.parser.set_wasm_store(store),
            Err(e) => eprintln!("twain: no WebAssembly store: {e}"),
        }
        highlighter
    });
}

fn engine() -> &'static wasmtime::Engine {
    static ENGINE: OnceLock<wasmtime::Engine> = OnceLock::new();
    ENGINE.get_or_init(wasmtime::Engine::default)
}

/// Grammars are loaded into this store, from any thread. Parsers have stores of their own.
fn loader() -> Result<&'static Mutex<WasmStore>, Error> {
    static LOADER: OnceLock<Result<Mutex<WasmStore>, String>> = OnceLock::new();
    LOADER
        .get_or_init(|| {
            WasmStore::new(engine())
                .map(Mutex::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| e.as_str().into())
}

/// The grammar for `lang`. If it isn't ready, starts fetching it and returns None; `buffer`,
/// a file in `file_lang`, is then highlighted again once it's ready.
fn grammar(
    lang: &'static Lang,
    buffer: &sourceview5::Buffer,
    file_lang: &'static Lang,
) -> Option<&'static HighlightConfiguration> {
    STATES.with_borrow_mut(|states| {
        let state = states.entry(lang.name).or_insert_with(|| {
            fetch(lang);
            State::Loading(Vec::new())
        });
        let waiting = match state {
            State::Ready(config) => return *config,
            State::Loading(waiting) => waiting,
        };
        if !waiting
            .iter()
            .any(|(b, _)| b.upgrade().as_ref() == Some(buffer))
        {
            waiting.push((buffer.downgrade(), file_lang));
        }
        None
    })
}

fn fetch(lang: &'static Lang) {
    glib::spawn_future_local(async move {
        let config = match gio::spawn_blocking(move || load(lang)).await {
            Ok(Ok(config)) => Some(&*Box::leak(Box::new(config))),
            Ok(Err(e)) => {
                eprintln!("twain: no {} grammar: {e}", lang.name);
                None
            }
            Err(_) => None,
        };
        let state = STATES.with_borrow_mut(|states| states.insert(lang.name, State::Ready(config)));
        if let Some(State::Loading(waiting)) = state
            && config.is_some()
        {
            for (buffer, file_lang) in waiting {
                if let Some(buffer) = buffer.upgrade() {
                    let (start, end) = buffer.bounds();
                    let text = buffer.text(&start, &end, true);
                    // Take over from GtkSourceView.
                    if apply(&buffer, &text, file_lang) {
                        buffer.set_language(None);
                    }
                }
            }
        }
    });
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Builds `lang`'s grammar from the cache, first downloading it if it isn't there or comes
/// from an older version of the package.
fn load(lang: &Lang) -> Result<HighlightConfiguration, Error> {
    let dir = glib::user_cache_dir()
        .join("twain/grammars")
        .join(lang.name);
    let cached = fs::read_to_string(dir.join("version")).ok();
    if cached.as_deref().is_none_or(|v| older(v.trim(), VERSION))
        && let Err(e) = download(lang, &dir)
    {
        // An older grammar beats none.
        if cached.is_none() {
            return Err(e);
        }
        eprintln!("twain: could not update the {} grammar: {e}", lang.name);
    }
    let wasm = fs::read(dir.join("grammar.wasm"))?;
    let language = loader()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .load_language(lang.name, &wasm)?;
    let query = |kind| fs::read_to_string(dir.join(format!("{kind}.scm"))).unwrap_or_default();
    let mut config = HighlightConfiguration::new(
        language,
        lang.name,
        &query("highlights"),
        &query("injections"),
        &query("locals"),
    )?;
    config.configure(&HIGHLIGHTS.iter().map(|(n, _)| *n).collect::<Vec<_>>());
    Ok(config)
}

/// Downloads `lang`'s grammar and queries into `dir`.
fn download(lang: &Lang, dir: &Path) -> Result<(), Error> {
    let base = format!("https://unpkg.com/@cursorless/tree-sitter-wasms@{VERSION}");
    let package: serde_json::Value =
        serde_json::from_slice(&get(&format!("{base}/package.json"))?)?;
    let wasm = get(&format!("{base}/out/tree-sitter-{}.wasm", lang.name))?;
    // Queries come from the sources the grammars were built from.
    let mut queries: [String; 3] = Default::default();
    for (package_name, path) in lang.queries {
        let source = package["devDependencies"][package_name]
            .as_str()
            .ok_or_else(|| format!("no {package_name} in {base}/package.json"))?;
        let url = match source.strip_prefix("github:") {
            Some(repo) => {
                let (repo, rev) = repo.split_once('#').unwrap_or((repo, "HEAD"));
                format!("https://raw.githubusercontent.com/{repo}/{rev}/{path}")
            }
            None => format!("https://unpkg.com/{package_name}@{source}/{path}"),
        };
        let file = path.rsplit('/').next().unwrap_or(path);
        let kind = QUERY_KINDS
            .iter()
            .position(|k| file.starts_with(k))
            .ok_or_else(|| format!("unknown kind of query: {path}"))?;
        queries[kind] += &String::from_utf8(get(&url)?)?;
        queries[kind].push('\n');
    }

    fs::create_dir_all(dir)?;
    // Written last, so an interrupted download is done again.
    _ = fs::remove_file(dir.join("version"));
    fs::write(dir.join("grammar.wasm"), wasm)?;
    for (kind, query) in QUERY_KINDS.iter().zip(queries) {
        fs::write(dir.join(format!("{kind}.scm")), query)?;
    }
    fs::write(dir.join("version"), VERSION)?;
    Ok(())
}

fn get(url: &str) -> Result<Vec<u8>, Error> {
    Ok(ureq::get(url).call()?.body_mut().read_to_vec()?)
}

/// Whether version `a` comes before `b`.
fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| {
        v.split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect::<Vec<u32>>()
    };
    parts(a) < parts(b)
}

/// Highlights `buffer`, which holds `text`, with tree-sitter. Returns false, adding nothing,
/// if no grammar handles `path`, or if its grammar isn't ready yet: the buffer is then
/// highlighted once it is, and its GtkSourceView language unset.
pub fn highlight(buffer: &sourceview5::Buffer, text: &str, path: &str) -> bool {
    for_path(path).is_some_and(|lang| apply(buffer, text, lang))
}

fn apply(buffer: &sourceview5::Buffer, text: &str, lang: &'static Lang) -> bool {
    if text.len() > MAX_LEN {
        return false;
    }
    let Some(config) = grammar(lang, buffer, lang) else {
        return false;
    };
    let table = buffer.tag_table();
    let (start, end) = buffer.bounds();
    let tags: Vec<_> = HIGHLIGHTS
        .iter()
        .map(|(name, _)| {
            let name = format!("syntax:{name}");
            let tag = table.lookup(&name).unwrap_or_else(|| {
                let tag = gtk::TextTag::new(Some(&name));
                table.add(&tag);
                // Below the diff's own tags, even those added earlier.
                tag.set_priority(0);
                tag
            });
            // From an earlier pass, before an injected language was ready.
            buffer.remove_tag(&tag, &start, &end);
            tag
        })
        .collect();
    recolor(buffer);

    HIGHLIGHTER.with_borrow_mut(|highlighter| {
        let Ok(events) = highlighter.highlight(config, text.as_bytes(), None, None, |name| {
            for_injection(name).and_then(|l| grammar(l, buffer, lang))
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
    fn versions() {
        assert!(older("0.9.1", "0.10.0"));
        assert!(!older("0.10.0", "0.10.0"));
        assert!(!older("0.10.1", "0.10.0"));
    }

    #[test]
    #[ignore = "downloads every grammar into the cache"]
    fn grammars_load() {
        let mut errors = Vec::new();
        for lang in LANGS {
            let config = match load(lang) {
                Ok(config) => Box::leak(Box::new(config)),
                Err(e) => {
                    errors.push(format!("{}: {e}", lang.name));
                    continue;
                }
            };
            // Parsing uses another store than the one the grammar was loaded into.
            HIGHLIGHTER.with_borrow_mut(|highlighter| {
                let events = highlighter
                    .highlight(config, b"x = 1\n", None, None, |_| None)
                    .unwrap();
                for event in events {
                    event.unwrap();
                }
            });
        }
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}
