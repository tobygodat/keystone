//! Syntax highlighting for code shown line by line, such as the file editor and diffs.
//! [`highlight`] turns a file's text into spans per line with tree-sitter grammars; the
//! theme colors each [`Syntax`] through `Colors::syntax`.

use std::ops::Range;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// What a span of code is, as far as its color goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Syntax {
    Comment,
    Keyword,
    String,
    /// An escape or interpolation inside a string.
    Escape,
    Number,
    /// Booleans, `null`, and named constants.
    Constant,
    Type,
    Function,
    Property,
    /// Attributes, decorators, and macros.
    Attribute,
    Tag,
    Operator,
    Punctuation,
}

/// One highlighted byte range of a line, not counting its line ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub syntax: Syntax,
}

/// A language [`highlight`] knows, found from a file's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language(usize);

impl Language {
    pub fn for_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        GRAMMARS
            .iter()
            .position(|grammar| {
                grammar.names.contains(&name)
                    || extension
                        .as_deref()
                        .is_some_and(|extension| grammar.extensions.contains(&extension))
            })
            .map(Self)
    }

    pub fn name(self) -> &'static str {
        GRAMMARS[self.0].name
    }
}

struct Grammar {
    name: &'static str,
    extensions: &'static [&'static str],
    /// Whole file names, for files with no telling extension.
    names: &'static [&'static str],
    /// The grammar and its highlight query. TypeScript's query only covers what it adds
    /// to JavaScript, so JavaScript's is appended to it.
    load: fn() -> (tree_sitter::Language, String),
}

const GRAMMARS: &[Grammar] = &[
    Grammar {
        name: "Bash",
        extensions: &["sh", "bash", "zsh"],
        names: &[".bashrc", ".bash_profile", ".zshrc", ".profile"],
        load: || {
            (
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "C",
        extensions: &["c", "h"],
        names: &[],
        load: || {
            (
                tree_sitter_c::LANGUAGE.into(),
                tree_sitter_c::HIGHLIGHT_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "CSS",
        extensions: &["css"],
        names: &[],
        load: || {
            (
                tree_sitter_css::LANGUAGE.into(),
                tree_sitter_css::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "Go",
        extensions: &["go"],
        names: &[],
        load: || {
            (
                tree_sitter_go::LANGUAGE.into(),
                tree_sitter_go::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "HTML",
        extensions: &["html", "htm"],
        names: &[],
        load: || {
            (
                tree_sitter_html::LANGUAGE.into(),
                tree_sitter_html::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "JavaScript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        names: &[],
        load: || {
            (
                tree_sitter_javascript::LANGUAGE.into(),
                format!(
                    "{}\n{}",
                    tree_sitter_javascript::HIGHLIGHT_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
                ),
            )
        },
    },
    Grammar {
        name: "JSON",
        extensions: &["json"],
        names: &[],
        load: || {
            (
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "Python",
        extensions: &["py", "pyi"],
        names: &[],
        load: || {
            (
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "Rust",
        extensions: &["rs"],
        names: &[],
        load: || {
            (
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "TOML",
        extensions: &["toml"],
        names: &["Cargo.lock"],
        load: || {
            (
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
    Grammar {
        name: "TypeScript",
        extensions: &["ts", "mts", "cts"],
        names: &[],
        load: || {
            (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                format!(
                    "{}\n{}",
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    tree_sitter_javascript::HIGHLIGHT_QUERY
                ),
            )
        },
    },
    Grammar {
        name: "TSX",
        extensions: &["tsx"],
        names: &[],
        load: || {
            (
                tree_sitter_typescript::LANGUAGE_TSX.into(),
                format!(
                    "{}\n{}\n{}",
                    tree_sitter_typescript::HIGHLIGHTS_QUERY,
                    tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                    tree_sitter_javascript::HIGHLIGHT_QUERY
                ),
            )
        },
    },
    Grammar {
        name: "YAML",
        extensions: &["yaml", "yml"],
        names: &[],
        load: || {
            (
                tree_sitter_yaml::LANGUAGE.into(),
                tree_sitter_yaml::HIGHLIGHTS_QUERY.into(),
            )
        },
    },
];

/// The capture names the grammars' queries use, and what each is drawn as. A capture
/// takes the entry that matches most of its dotted parts, so `string.escape` is an
/// escape and `function.method` a function. Captures not listed, such as plain
/// variables, stay uncolored.
const CAPTURES: &[(&str, Syntax)] = &[
    ("comment", Syntax::Comment),
    ("keyword", Syntax::Keyword),
    ("variable.builtin", Syntax::Keyword),
    ("string", Syntax::String),
    ("string.escape", Syntax::Escape),
    ("escape", Syntax::Escape),
    ("embedded", Syntax::Escape),
    ("number", Syntax::Number),
    ("constant", Syntax::Constant),
    ("boolean", Syntax::Constant),
    ("type", Syntax::Type),
    ("constructor", Syntax::Type),
    ("function", Syntax::Function),
    ("function.macro", Syntax::Attribute),
    ("property", Syntax::Property),
    ("attribute", Syntax::Attribute),
    ("tag", Syntax::Tag),
    ("operator", Syntax::Operator),
    ("punctuation", Syntax::Punctuation),
];

/// Each grammar's compiled queries, built the first time a file of it is highlighted.
static CONFIGS: [OnceLock<Option<HighlightConfiguration>>; GRAMMARS.len()] =
    [const { OnceLock::new() }; GRAMMARS.len()];

fn config(language: Language) -> Option<&'static HighlightConfiguration> {
    CONFIGS[language.0]
        .get_or_init(|| {
            let grammar = &GRAMMARS[language.0];
            let (ts_language, query) = (grammar.load)();
            let mut config =
                HighlightConfiguration::new(ts_language, grammar.name, &query, "", "").ok()?;
            config.configure(&CAPTURES.iter().map(|(name, _)| *name).collect::<Vec<_>>());
            Some(config)
        })
        .as_ref()
}

/// The spans of each line of `text`, one entry per `'\n'`-separated line, in order and
/// not overlapping. A line ending in `'\r'` keeps it out of every span. Text that does
/// not parse still gets the spans of the parts that do.
pub fn highlight(language: Language, text: &str) -> Vec<Vec<Span>> {
    highlight_until(language, text, &AtomicUsize::new(0)).unwrap_or_default()
}

/// [`highlight`], stopping early with `None` once `cancelled` is set to anything but
/// zero, so a pass made stale by a newer edit doesn't run on to the end of the file.
pub fn highlight_until(
    language: Language,
    text: &str,
    cancelled: &AtomicUsize,
) -> Option<Vec<Vec<Span>>> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    let mut lines = vec![Vec::new(); starts.len()];
    let Some(config) = config(language) else {
        return Some(lines);
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) =
        highlighter.highlight(config, text.as_bytes(), None, Some(cancelled), |_| None)
    else {
        return (cancelled.load(Ordering::Relaxed) == 0).then_some(lines);
    };
    let mut stack = Vec::new();
    for event in events {
        let Ok(event) = event else {
            break;
        };
        match event {
            HighlightEvent::HighlightStart(capture) => stack.push(CAPTURES[capture.0].1),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { mut start, end } => {
                let Some(&syntax) = stack.last() else {
                    continue;
                };
                while start < end {
                    let line = starts.partition_point(|&s| s <= start) - 1;
                    let line_start = starts[line];
                    let line_end = starts.get(line + 1).map_or(text.len(), |&next| next - 1);
                    let content_end = if text[line_start..line_end].ends_with('\r') {
                        line_end - 1
                    } else {
                        line_end
                    };
                    let span_end = end.min(content_end);
                    if span_end > start {
                        push(
                            &mut lines[line],
                            start - line_start..span_end - line_start,
                            syntax,
                        );
                    }
                    start = end.min(line_end + 1).max(start + 1);
                }
            }
        }
    }
    (cancelled.load(Ordering::Relaxed) == 0).then_some(lines)
}

/// Appends a span, joining it to the last one when they touch and match.
fn push(line: &mut Vec<Span>, range: Range<usize>, syntax: Syntax) {
    if let Some(last) = line.last_mut()
        && last.syntax == syntax
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    line.push(Span { range, syntax });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(path: &str, text: &str) -> Vec<Vec<(String, Syntax)>> {
        let language = Language::for_path(Path::new(path)).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        highlight(language, text)
            .into_iter()
            .zip(lines)
            .map(|(spans, line)| {
                spans
                    .into_iter()
                    .map(|span| (line[span.range].to_owned(), span.syntax))
                    .collect()
            })
            .collect()
    }

    fn has(lines: &[Vec<(String, Syntax)>], line: usize, text: &str, syntax: Syntax) -> bool {
        lines[line].contains(&(text.to_owned(), syntax))
    }

    #[test]
    fn languages_are_found_by_extension_and_name() {
        let name = |path: &str| Language::for_path(Path::new(path)).map(Language::name);
        assert_eq!(name("src/main.rs"), Some("Rust"));
        assert_eq!(name("App.TSX"), Some("TSX"));
        assert_eq!(name("a/b.d.ts"), Some("TypeScript"));
        assert_eq!(name("Cargo.lock"), Some("TOML"));
        assert_eq!(name(".zshrc"), Some("Bash"));
        assert_eq!(name("notes.txt"), None);
        assert_eq!(name("Makefile"), None);
    }

    #[test]
    fn every_grammar_compiles_its_query() {
        for index in 0..GRAMMARS.len() {
            assert!(
                config(Language(index)).is_some(),
                "{}",
                GRAMMARS[index].name
            );
        }
    }

    #[test]
    fn rust_spans_are_per_line_and_name_what_they_cover() {
        let text = "/* one\ntwo */\nfn main() {\n    let s = \"hi\\n\"; // done\n}";
        let lines = spans("a.rs", text);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], [("/* one".to_owned(), Syntax::Comment)]);
        assert_eq!(lines[1], [("two */".to_owned(), Syntax::Comment)]);
        assert!(has(&lines, 2, "fn", Syntax::Keyword));
        assert!(has(&lines, 2, "main", Syntax::Function));
        assert!(has(&lines, 3, "\\n", Syntax::Escape));
        assert!(has(&lines, 3, "// done", Syntax::Comment));
    }

    #[test]
    fn typescript_layers_its_query_over_javascripts() {
        let lines = spans("a.ts", "const m: Map<string, number> = new Map(); // c");
        assert!(has(&lines, 0, "const", Syntax::Keyword));
        assert!(has(&lines, 0, "Map", Syntax::Type));
        assert!(has(&lines, 0, "// c", Syntax::Comment));
    }

    #[test]
    fn crlf_line_endings_stay_out_of_spans() {
        let text = "# a\r\nx = 1\r\n";
        let lines = spans("a.py", text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], [("# a".to_owned(), Syntax::Comment)]);
        let raw = highlight(Language::for_path(Path::new("a.py")).unwrap(), text);
        assert!(raw[0].iter().all(|span| span.range.end <= 3));
    }

    #[test]
    fn a_cancelled_pass_returns_nothing() {
        let language = Language::for_path(Path::new("a.rs")).unwrap();
        let text = "fn main() {}\n".repeat(20_000);
        assert_eq!(highlight_until(language, &text, &AtomicUsize::new(1)), None);
        assert!(highlight_until(language, &text, &AtomicUsize::new(0)).is_some());
    }

    #[test]
    fn broken_and_empty_text_still_highlight() {
        assert_eq!(spans("a.rs", ""), [Vec::new()]);
        let lines = spans("a.rs", "fn (((\n\"open");
        assert_eq!(lines.len(), 2);
        assert!(has(&lines, 0, "fn", Syntax::Keyword));
    }
}
