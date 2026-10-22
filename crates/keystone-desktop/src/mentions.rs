//! The composer's `@` picker. Typing `@` at the start of a word lists the files in the
//! session's folder that fuzzy-match what follows, from an index the Files view's
//! walker builds on a worker (hidden and ignored files left out, at most
//! [`MAX_FILES`]). Choosing one replaces the word with `@relative/path` as one token,
//! which Backspace deletes whole; Files rows insert the same token. On Send the text
//! goes as typed: Claude Code reads mentioned files itself and Codex reads them with
//! its own tools (docs/research/file-mentions-probe.md). Keystone never reads them.

use crate::slash_menu::composer_overlay;
use crate::{Keystone, files};
use gpui::{
    AnyElement, App, Context, Entity, Focusable, FontWeight, HighlightStyle, IntoElement, Pixels,
    StyledText, Subscription, Window, div, prelude::*, px,
};
use keystone_ui::{fuzzy, prelude::*, text_input};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most files one folder's index holds.
pub(crate) const MAX_FILES: usize = 200_000;
/// The most rows the picker lists.
const LIMIT: usize = 50;
/// How old an index may be before an open picker lists the folder again.
const FRESH: Duration = Duration::from_secs(15);

/// The picker's own state, kept while the app runs.
pub(crate) struct Mentions {
    pub(crate) list: Entity<VirtualListState>,
    /// The session and draft this window's composer last edited. The picker opens
    /// only for that draft, so not where it was loaded, shared from another window or
    /// closed with Escape.
    pub(crate) edited: Option<(u64, String)>,
    /// The session, draft and `@word` an Enter or Tab came in for before that query's
    /// rows were ranked; the best row replaces that word when they are.
    pending: Option<(u64, String, Range<usize>)>,
    /// The listing of the folder last searched; others are dropped.
    indexes: HashMap<PathBuf, Index>,
    search: Option<Search>,
    _edited: Subscription,
}

/// One folder's files, relative and `/`-separated, shallowest first.
#[derive(Default)]
struct Index {
    files: Arc<Vec<String>>,
    truncated: bool,
    error: Option<String>,
    /// When the last listing finished, or `None` before the first.
    built: Option<Instant>,
    /// Counts finished listings, so a search knows which one it ranked.
    generation: u64,
    loading: bool,
}

/// The rows for one query of one index, or a ranking of them still running.
struct Search {
    key: (PathBuf, u64, String),
    found: Arc<Vec<Found>>,
    running: Option<gpui::Task<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub path: String,
    /// The bytes of `path` the query matched.
    positions: Vec<usize>,
}

/// Where the picker would insert: the `@word` around the caret and what it searches.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    session: u64,
    root: PathBuf,
    range: Range<usize>,
    query: String,
}

impl Mentions {
    /// Each edit of `composer` lets the picker open for the edited draft and
    /// highlights its first row.
    pub(crate) fn new(composer: &Entity<TextInput>, cx: &mut Context<Keystone>) -> Self {
        Self {
            list: cx.new(|cx| VirtualListState::new(0, cx)),
            edited: None,
            pending: None,
            indexes: HashMap::new(),
            search: None,
            _edited: cx.subscribe(composer, |this, input, _: &text_input::Changed, cx| {
                let draft = input.read(cx).value().to_owned();
                this.mentions.edited = this.selected(cx).map(|session| (session, draft));
                this.mentions.pending = None;
                this.mentions.list.update(cx, |list, _| list.move_to(0));
            }),
        }
    }
}

/// The `@word` the caret is in or at the end of, and the query after its `@` (and any
/// opening quote), unless that word is already a token.
fn word_at<'a>(
    text: &'a str,
    caret: usize,
    tokens: &[Range<usize>],
) -> Option<(Range<usize>, &'a str)> {
    let start = text[..caret].rfind(char::is_whitespace).map_or(0, |i| {
        i + text[i..].chars().next().map_or(1, char::len_utf8)
    });
    let end = caret
        + text[caret..]
            .find(char::is_whitespace)
            .unwrap_or(text.len() - caret);
    let typed = text[start..caret].strip_prefix('@')?;
    if tokens.iter().any(|token| token.start == start) {
        return None;
    }
    Some((start..end, typed.strip_prefix('"').unwrap_or(typed)))
}

/// How `path` is written in a message: `@path`, quoted when it has spaces, the forms
/// Claude Code reads. A path with a quote or a control character such as a newline
/// has no form that reads back, so it has none.
pub(crate) fn token(path: &str) -> Option<String> {
    if path.contains(|c: char| c == '"' || c.is_control()) {
        None
    } else if path.contains(char::is_whitespace) {
        Some(format!("@\"{path}\""))
    } else {
        Some(format!("@{path}"))
    }
}

/// The ranges of `text` that are mentions of files under `root`, written as the
/// picker writes them, for a draft loaded from storage.
pub(crate) fn tokens_in(text: &str, root: &Path) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut rest = 0;
    while let Some(at) = text[rest..].find('@').map(|i| rest + i) {
        rest = at + 1;
        if text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
        {
            continue;
        }
        let (path, end) = match text[at + 1..].strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(close) => (&quoted[..close], at + 2 + close + 1),
                None => continue,
            },
            None => {
                let end = text[at..]
                    .find(char::is_whitespace)
                    .map_or(text.len(), |i| at + i);
                (&text[at + 1..end], end)
            }
        };
        let after = text[end..].chars().next();
        if after.is_none_or(char::is_whitespace)
            && token(path).as_deref() == Some(&text[at..end])
            && Path::new(path)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            && root.join(path).is_file()
        {
            found.push(at..end);
            rest = end;
        }
    }
    found
}

/// The files under `root` the Files view would show, relative and shallowest first,
/// and whether there were more than `limit`.
pub(crate) fn index_files(root: &Path, limit: usize) -> std::io::Result<(Vec<String>, bool)> {
    let mut files = Vec::new();
    let mut failure = None;
    let mut truncated = false;
    for result in files::walker(root).build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                failure.get_or_insert(error);
                continue;
            }
        };
        let is_file = entry.file_type().is_some_and(|t| t.is_file())
            || (entry.path_is_symlink() && entry.path().is_file());
        let Some(relative) = entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .filter(|path| is_file && token(path).is_some())
        else {
            continue;
        };
        if files.len() == limit {
            truncated = true;
            break;
        }
        files.push(relative.to_owned());
    }
    if let (true, Some(error)) = (files.is_empty(), failure) {
        return Err(std::io::Error::other(error));
    }
    files.sort_by_cached_key(|path| (path.matches('/').count(), path.to_lowercase()));
    Ok((files, truncated))
}

/// The best [`LIMIT`] files for `query`: by score, then the shorter path, then the
/// index's order. An empty query lists the first files.
pub(crate) fn rank(files: &[String], query: &str) -> Vec<Found> {
    if query.is_empty() {
        return files
            .iter()
            .take(LIMIT)
            .map(|path| Found {
                path: path.clone(),
                positions: Vec::new(),
            })
            .collect();
    }
    let mut scored: Vec<(i32, usize, Vec<usize>)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, path)| {
            fuzzy::score(query, path).map(|found| (found.score, i, found.positions))
        })
        .collect();
    let key =
        |(score, i, _): &(i32, usize, Vec<usize>)| (std::cmp::Reverse(*score), files[*i].len(), *i);
    if scored.len() > LIMIT {
        scored.select_nth_unstable_by_key(LIMIT - 1, key);
        scored.truncate(LIMIT);
    }
    scored.sort_unstable_by_key(key);
    scored
        .into_iter()
        .map(|(_, i, positions)| Found {
            path: files[i].clone(),
            positions,
        })
        .collect()
}

impl Keystone {
    /// The selected session's recorded folder, which its mentions are relative to. A
    /// session without one has no mentions: the project folder Files falls back to
    /// may not be the folder it will run in.
    pub(crate) fn mention_root(&self, cx: &App) -> Option<PathBuf> {
        self.session_root(self.selected(cx)?, cx)
    }

    /// `session`'s recorded folder; see [`Self::mention_root`].
    pub(crate) fn session_root(&self, session: u64, cx: &App) -> Option<PathBuf> {
        match self.conversation_folder(session, cx)? {
            crate::presentation::ConversationFolder::Recorded(directory) => {
                Some(directory.path().to_owned())
            }
            _ => None,
        }
    }

    /// Where the picker would insert, while it is open: this window's composer last
    /// edited the selected session's draft into an `@word` at the caret, Escape has
    /// not closed it, and no palette or project list is open over it.
    fn mention_target(&self, cx: &App) -> Option<Target> {
        let session = self.selected(cx)?;
        let input = self.composer.read(cx);
        let draft = input.value();
        let (range, query) = word_at(draft, input.cursor(), input.tokens())?;
        let edited_here = self
            .mentions
            .edited
            .as_ref()
            .is_some_and(|(edited, text)| *edited == session && text == draft);
        if !edited_here || self.palette.is_some() || self.projects.is_some() {
            return None;
        }
        Some(Target {
            session,
            root: self.mention_root(cx)?,
            range,
            query: query.to_owned(),
        })
    }

    /// The picker's rows for `target`, ranked for an earlier query while this one's
    /// ranking runs, and whether they are this query's.
    fn mention_rows(&self, target: &Target) -> (Arc<Vec<Found>>, bool) {
        let generation = self
            .mentions
            .indexes
            .get(&target.root)
            .map(|i| i.generation);
        match &self.mentions.search {
            Some(search) if search.key.0 == target.root => (
                search.found.clone(),
                Some(search.key.1) == generation
                    && search.key.2 == target.query
                    && search.running.is_none(),
            ),
            _ => (Arc::default(), false),
        }
    }

    /// Whether the picker is open with rows, so Up, Down, Enter and Tab drive it.
    pub(crate) fn mention_open(&self, cx: &App) -> bool {
        self.mention_target(cx)
            .is_some_and(|target| !self.mention_rows(&target).0.is_empty())
    }

    /// Lists `root` on a worker unless a listing runs or a fresh one is in hand.
    fn index_mentions(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.mentions.indexes.retain(|folder, _| *folder == root);
        let index = self.mentions.indexes.entry(root.clone()).or_default();
        if index.loading || index.built.is_some_and(|built| built.elapsed() < FRESH) {
            return;
        }
        index.loading = true;
        let listing = cx.background_spawn({
            let root = root.clone();
            async move { index_files(&root, MAX_FILES) }
        });
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            let _ = this.update(cx, |this, cx| {
                let index = this.mentions.indexes.entry(root).or_default();
                index.loading = false;
                index.built = Some(Instant::now());
                index.generation += 1;
                match listing {
                    Ok((files, truncated)) => {
                        index.files = Arc::new(files);
                        index.truncated = truncated;
                        index.error = None;
                    }
                    Err(error) => index.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Ranks `target`'s query on a worker unless that ranking is in hand or running.
    fn search_mentions(&mut self, target: &Target, cx: &mut Context<Self>) {
        let Some(index) = self.mentions.indexes.get(&target.root) else {
            return;
        };
        let key = (target.root.clone(), index.generation, target.query.clone());
        if self.mentions.search.as_ref().is_some_and(|s| s.key == key) {
            return;
        }
        let (files, query) = (index.files.clone(), target.query.clone());
        let ranking = cx.background_spawn(async move { rank(&files, &query) });
        let running = cx.spawn(async move |this, cx| {
            let found = ranking.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(search) = &mut this.mentions.search {
                    search.found = Arc::new(found);
                    search.running = None;
                }
                let pending = this.mentions.pending.take();
                if let Some(target) = this.mention_target(cx)
                    && pending
                        == Some((
                            target.session,
                            this.composer.read(cx).value().to_owned(),
                            target.range.clone(),
                        ))
                {
                    let best = this.mention_rows(&target).0.first().map(|f| f.path.clone());
                    this.choose_mention(best, cx);
                }
                cx.notify();
            });
        });
        let found = self
            .mentions
            .search
            .take()
            .filter(|s| s.key.0 == target.root)
            .map(|s| s.found)
            .unwrap_or_default();
        self.mentions.search = Some(Search {
            key,
            found,
            running: Some(running),
        });
    }

    /// Escape: closes an open picker, and reports whether there was one.
    pub(crate) fn dismiss_mention(&mut self, cx: &mut Context<Self>) -> bool {
        if self.mention_target(cx).is_none() {
            return false;
        }
        self.mentions.edited = None;
        self.mentions.pending = None;
        cx.notify();
        true
    }

    /// Enter or Tab: chooses the highlighted row. While the rows shown were ranked for
    /// an earlier query, it waits for this query's and then chooses its best match.
    pub(crate) fn choose_highlighted_mention(
        &mut self,
        _: Option<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.mention_target(cx) else {
            return;
        };
        let (found, current) = self.mention_rows(&target);
        if current {
            let row = self.mentions.list.read(cx).highlighted();
            let path = row.and_then(|row| found.get(row)).map(|f| f.path.clone());
            return self.choose_mention(path, cx);
        }
        let draft = self.composer.read(cx).value().to_owned();
        self.mentions.pending = Some((target.session, draft, target.range.clone()));
        self.search_mentions(&target, cx);
    }

    /// Replaces the `@word` with a token for `path`, a file of the picker's rows.
    fn choose_mention(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(target) = self.mention_target(cx) else {
            return;
        };
        let Some(token) = path.and_then(|path| token(&path)) else {
            return;
        };
        self.mentions.pending = None;
        self.composer
            .update(cx, |input, cx| input.insert_token(target.range, &token, cx));
        self.save_draft(cx);
        self.refresh(cx);
    }

    /// Inserts a mention of `path`, a file under the session's recorded folder, at the
    /// caret.
    pub(crate) fn mention_file(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected(cx).is_none() {
            return;
        }
        let Some(relative) = self
            .mention_root(cx)
            .and_then(|root| Some(path.strip_prefix(root).ok()?.to_str()?.to_owned()))
        else {
            return;
        };
        if let Some(token) = token(&relative) {
            self.insert_mention(&token, window, cx);
        }
    }

    /// Inserts `token` at the composer's caret as one token, and saves the draft.
    pub(crate) fn insert_mention(
        &mut self,
        token: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.update(cx, |input, cx| {
            let caret = input.cursor();
            input.insert_token(caret..caret, token, cx)
        });
        self.save_draft(cx);
        self.composer.focus_handle(cx).focus(window, cx);
        self.refresh(cx);
    }

    /// Marks the mentions of existing files in the composer's draft as tokens, after
    /// a draft is loaded as plain text.
    pub(crate) fn restore_mention_tokens(&self, cx: &mut Context<Self>) {
        let Some(root) = self.mention_root(cx) else {
            return;
        };
        self.composer.update(cx, |input, cx| {
            let tokens = tokens_in(input.value(), &root);
            input.set_tokens(tokens, cx)
        });
    }

    /// The picker above the composer, while it is open.
    pub(crate) fn mention_menu(
        &self,
        window_width: Pixels,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let target = self.mention_target(cx)?;
        let index = self.mentions.indexes.get(&target.root);
        let stale =
            index.is_none_or(|i| !i.loading && i.built.is_none_or(|b| b.elapsed() >= FRESH));
        let (found, current) = self.mention_rows(&target);
        let ranked = index.is_some_and(|i| i.built.is_some());
        if stale || (ranked && !current) {
            let view = cx.weak_entity();
            let root = target.root.clone();
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    this.index_mentions(root, cx);
                    if let Some(target) = this.mention_target(cx) {
                        this.search_mentions(&target, cx);
                    }
                });
            });
        }
        let status = match index {
            Some(Index {
                error: Some(error), ..
            }) => Some(format!("Couldn't list files. {error}")),
            Some(Index { built: None, .. }) | None => Some("Listing files…".to_owned()),
            Some(Index {
                truncated: true, ..
            }) => Some("Searching the first 200,000 files in this folder.".to_owned()),
            _ => None,
        };
        let row_height = cx.theme().density.control(px(32.));
        let count = found.len();
        let shown = found.clone();
        let rows = move |row: usize, highlighted: bool, _: &mut Window, cx: &mut App| {
            found.get(row).map_or_else(
                || div().h(row_height).into_any_element(),
                |found| file_row(found, highlighted, row_height, cx),
            )
        };
        let choose = self.scoped_listener(cx, move |this, row: &usize, window, cx| {
            this.choose_mention(shown.get(*row).map(|f| f.path.clone()), cx);
            this.composer.focus_handle(cx).focus(window, cx);
        });
        let list = self.mentions.list.clone();
        let card = self.menu_card(
            "mention-menu",
            &list,
            count,
            rows,
            choose,
            status,
            None::<fn(&gpui::ClickEvent, &mut Window, &mut App)>,
            (ranked && current).then_some("No matching files"),
            cx,
        );
        Some(composer_overlay(card, window_width))
    }
}

fn file_row(found: &Found, highlighted: bool, height: Pixels, cx: &mut App) -> AnyElement {
    let marked = HighlightStyle {
        color: Some(Color::Accent.to_hsla(cx)),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let path = StyledText::new(found.path.clone()).with_highlights(
        fuzzy::ranges(&found.path, &found.positions)
            .into_iter()
            .map(|range| (range, marked)),
    );
    h_flex()
        .h(height)
        .px(spacing::SM)
        .gap(spacing::SM)
        .rounded(radius::MENU_ROW)
        .when(highlighted, |d| d.bg(cx.theme().colors.selection_fill))
        .child(Icon::new(IconName::File).size(px(14.)).color(Color::Muted))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_size(TextSize::Ui.px())
                .whitespace_nowrap()
                .text_ellipsis()
                .child(path),
        )
        .into_any_element()
}

#[cfg(all(test, feature = "ui-tests"))]
#[path = "mentions_tests.rs"]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_word_at_the_caret_opens_the_picker_unless_it_is_a_token() {
        assert_eq!(word_at("@", 1, &[]), Some((0..1, "")));
        assert_eq!(word_at("see @mod-0", 10, &[]), Some((4..10, "mod-0")));
        assert_eq!(word_at("see @mod tail", 6, &[]), Some((4..8, "m")));
        assert_eq!(word_at("a\n@\"my", 6, &[]), Some((2..6, "my")));
        assert_eq!(word_at("mail me@host", 12, &[]), None);
        assert_eq!(word_at("@a.rs ", 6, &[]), None);
        assert_eq!(word_at("@a.rs", 5, &[0..5]), None);
        assert_eq!(word_at("é @x", 5, &[]), Some((3..5, "x")));
    }

    #[test]
    fn paths_with_spaces_are_quoted_and_restored() {
        assert_eq!(token("src/main.rs").unwrap(), "@src/main.rs");
        assert_eq!(token("my notes/a b.txt").unwrap(), "@\"my notes/a b.txt\"");
        assert_eq!(token("src/a\"b.rs"), None);
        assert_eq!(token("src/a\nb.rs"), None);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("my notes")).unwrap();
        fs::write(dir.path().join("my notes/a b.txt"), "").unwrap();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/b.rs"), "").unwrap();
        let text = "@a.rs and @\"my notes/a b.txt\" not @a.rsx, me@a.rs, @gone.rs @/etc/hosts \
                    @src/../a.rs @./a.rs @\"a.rs\" @";
        let found: Vec<&str> = tokens_in(text, dir.path())
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["@a.rs", "@\"my notes/a b.txt\""]);
    }

    #[test]
    fn the_index_skips_ignored_and_hidden_files_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        for file in [
            "build/out.o",
            "src/deep/b.rs",
            "src/a.rs",
            "README.md",
            ".env",
            "quo\"te.rs",
        ] {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::write(root.join(file), "").unwrap();
        }
        let (files, truncated) = index_files(root, 10).unwrap();
        assert_eq!(files, ["README.md", "src/a.rs", "src/deep/b.rs"]);
        assert!(!truncated);
        let (files, truncated) = index_files(root, 2).unwrap();
        assert_eq!(files.len(), 2);
        assert!(truncated);
        assert!(index_files(&root.join("missing"), 10).is_err());
    }

    #[test]
    fn ranking_prefers_the_best_match_then_the_shorter_path() {
        let files: Vec<String> = [
            "README.md",
            "src/mod-03/x.txt",
            "src/pkg/mod-03/part/y.txt",
            "src/mod-30.txt",
        ]
        .map(String::from)
        .to_vec();
        let found = rank(&files, "mod-03");
        assert_eq!(found[0].path, "src/mod-03/x.txt");
        assert_eq!(found[0].positions, [4, 5, 6, 7, 8, 9]);
        assert_eq!(found.len(), 2);
        assert_eq!(rank(&files, "").len(), 4);
        let many: Vec<String> = (0..500).map(|n| format!("f/{n:03}.txt")).collect();
        let found = rank(&many, "txt");
        assert_eq!(found.len(), LIMIT);
        assert_eq!(found[0].path, "f/000.txt");
    }

    #[test]
    #[ignore = "benchmark; builds a 50,000-file fixture"]
    fn perf_index_and_rank_50k() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-file-fixture.py");
        let status = std::process::Command::new("python3")
            .arg("-I")
            .arg(script)
            .arg(&fixture)
            .arg("50000")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "fixture script failed");
        let start = Instant::now();
        let (files, truncated) = index_files(&fixture, MAX_FILES).unwrap();
        let indexed = start.elapsed();
        assert!(!truncated);
        assert!(files.iter().all(|f| !f.starts_with("build/")));
        let start = Instant::now();
        let found = rank(&files, "mod-03");
        let ranked = start.elapsed();
        eprintln!(
            "PERF mention_index_ms={:.1} mention_rank_ms={:.1} files={} first={}",
            indexed.as_secs_f64() * 1000.,
            ranked.as_secs_f64() * 1000.,
            files.len(),
            found[0].path,
        );
    }
}
