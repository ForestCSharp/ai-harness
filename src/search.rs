//! Searching the working directory for the model — contents by regex,
//! filenames by glob.
//!
//! Like [`crate::files`], a search is non-mutating and confined to the
//! working-directory subtree, so it runs without the approval modal. The
//! reasoning is the read's: an agent that wants to look at four files should
//! not interrupt you four times, and *finding* those four files is the step
//! that comes first.
//!
//! What makes the trade sound is that every path this module reaches is
//! filtered by the same [`Sandbox::denies_read`] the kernel profile is rendered
//! from. A read resolves one concrete path; a walk assembles thousands, so the
//! check runs per entry rather than once at the root — a glob of `**/*` that
//! consulted the denylist only for its starting directory would list `.env`.
//!
//! Symlinks are never followed. That single rule closes three holes at once: a
//! link out of the root would escape confinement, a link back into the tree
//! would double-report, and `a -> .` would walk forever. It also saves a
//! `canonicalize` syscall per file, and it means every path the walk builds is
//! already canonical — the precondition `denies_read` documents.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use crate::sandbox::Sandbox;

/// Matches a grep returns before it stops looking.
pub const MAX_GREP_MATCHES: usize = 200;

/// Paths a glob returns before it stops looking. Higher than the grep cap
/// because a path costs a line and a match costs a line of source.
pub const MAX_GLOB_PATHS: usize = 500;

/// Directory entries a single search will visit. The backstop for a tree the
/// skip list does not cover.
pub const MAX_ENTRIES: usize = 20_000;

/// A file larger than this is passed over rather than searched. Above any
/// hand-written source file, below the minified bundles and lockfiles whose
/// one 2 MB line would be a match nobody can use.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Characters of a matched line kept before it is elided.
pub const MAX_LINE_CHARS: usize = 400;

/// Cap on the whole result body — the same ceiling one read gets, since both
/// spend from the same context window.
pub const MAX_RESULT_BYTES: usize = crate::files::MAX_READ_BYTES;

/// How long a search may run before it reports what it has.
pub const MAX_SEARCH_TIME: Duration = Duration::from_secs(5);

/// How deep the walk descends. Real trees are far shallower; this only stops a
/// pathological one from exhausting the stack.
const MAX_DEPTH: usize = 64;

/// Bytes of a file examined for a NUL before it is called binary.
const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// Ceiling on the compiled size of a model-authored pattern. `regex` is
/// finite-automata based with no backtracking, so a pattern cannot make the
/// harness hang the way one handed to a PCRE engine could; what it *can* do is
/// ask for a very large automaton, which this refuses at compile time.
const MAX_REGEX_SIZE: usize = 1 << 20;

/// Directory names the walk never descends into.
///
/// A heuristic about where source is not, and no part of the security boundary
/// — that is [`Sandbox::denies_read`], checked separately on every entry. The
/// list earns its place on cost: `target/` alone runs to gigabytes, and a
/// search that has to be narrowed by hand every time is one the model stops
/// reaching for.
///
/// [`crate::config::HARNESS_DIR`] is the entry that is easy to miss. A session
/// file holds an entire prior conversation, so without it a grep for any term
/// the user has typed would match the transcript of them typing it and hand
/// that back as a result.
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    "target",
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".direnv",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".parcel-cache",
    ".turbo",
    ".gradle",
    ".terraform",
    ".idea",
    ".vscode",
    "Pods",
    "DerivedData",
    crate::config::HARNESS_DIR,
];

/// Which of the two searches this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchKind {
    /// File contents, by regular expression.
    Grep,
    /// File names, by glob.
    Glob,
}

impl SearchKind {
    /// The element's name, for messages that have to say which search this was.
    pub fn label(self) -> &'static str {
        match self {
            SearchKind::Grep => "grep",
            SearchKind::Glob => "glob",
        }
    }
}

/// Why a search stopped before it ran out of tree.
///
/// At most one is recorded — whichever fired first — so the note that reports
/// it stays one line, the way a read's continuation note does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Capped {
    /// Enough matches; there are more.
    Matches,
    /// Enough of the tree walked.
    Entries,
    /// Enough output to send.
    Bytes,
    /// Long enough spent looking.
    Time,
}

/// One result. `line` is `None` for a glob, which has no line to point at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    /// Root-relative and `/`-separated, so it can be handed straight to
    /// `<ai-harness-read>` without translation.
    pub path: String,
    pub line: Option<usize>,
    pub text: String,
}

/// What a search was asked to do.
///
/// Carried from the parsed action into the walk so the walk needs nothing from
/// [`crate::protocol::Action`], which is what lets it run on a blocking thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub kind: SearchKind,
    pub pattern: String,
    /// Subtree to search. The working directory when absent.
    pub dir: Option<String>,
    /// Filename filter on a grep. Never set for a glob, whose pattern is
    /// already a filename pattern.
    pub glob: Option<String>,
}

impl Request {
    /// A content search over the whole working directory.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn grep(pattern: &str) -> Self {
        Self {
            kind: SearchKind::Grep,
            pattern: pattern.to_string(),
            dir: None,
            glob: None,
        }
    }

    /// A filename search over the whole working directory.
    ///
    /// These three builders exist for the tests; production constructs the
    /// struct directly in `app`. Dead twice over, so the condition says both
    /// reasons at once: outside a test build nothing calls them, and inside one
    /// on a platform other than macOS their callers are compiled away with the
    /// gated test modules.
    #[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
    pub fn glob(pattern: &str) -> Self {
        Self {
            kind: SearchKind::Glob,
            pattern: pattern.to_string(),
            dir: None,
            glob: None,
        }
    }

    /// The same search, confined to `dir`.
    #[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
    pub fn in_dir(mut self, dir: &str) -> Self {
        self.dir = Some(dir.to_string());
        self
    }

    /// The same grep, restricted to filenames matching `glob`.
    #[cfg_attr(any(not(test), not(target_os = "macos")), allow(dead_code))]
    pub fn filtered(mut self, glob: &str) -> Self {
        self.glob = Some(glob.to_string());
        self
    }
}

/// The outcome of a search. Mirrors [`crate::files::ReadOutcome`]: a failure is
/// data to hand back to the model, not an error that ends the turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchOutcome {
    pub kind: SearchKind,
    /// The pattern as the model wrote it.
    pub pattern: String,
    pub dir: Option<String>,
    pub glob: Option<String>,
    pub hits: Vec<Hit>,
    /// Distinct files with at least one hit.
    pub files_matched: usize,
    /// Files whose contents a grep examined, or whose path a glob tested.
    pub files_scanned: usize,
    /// Files passed over as binary or over [`MAX_FILE_BYTES`]. Files excluded
    /// by the denylist are deliberately not counted here: a count is itself a
    /// hint that something is there.
    pub files_skipped: usize,
    pub capped: Option<Capped>,
    pub error: Option<String>,
}

impl SearchOutcome {
    /// A search that never ran, carrying the reason why.
    pub fn failed(request: &Request, error: impl Into<String>) -> Self {
        Self {
            kind: request.kind,
            pattern: request.pattern.clone(),
            dir: request.dir.clone(),
            glob: request.glob.clone(),
            hits: Vec::new(),
            files_matched: 0,
            files_scanned: 0,
            files_skipped: 0,
            capped: None,
            error: Some(error.into()),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.error.is_some() {
            return "failed".to_string();
        }
        let mut summary = match self.kind {
            SearchKind::Grep => format!(
                "{} match(es) in {} file(s), {} scanned",
                self.hits.len(),
                self.files_matched,
                self.files_scanned
            ),
            SearchKind::Glob => format!("{} file(s)", self.hits.len()),
        };
        if self.capped.is_some() {
            summary.push_str(", capped");
        }
        summary
    }

    /// The hits as one line each, in the same `path:line: text` form the model
    /// receives. Rendering and encoding share this so the two cannot drift.
    pub fn preview(&self) -> String {
        let mut out = String::new();
        for hit in &self.hits {
            match hit.line {
                Some(line) => out.push_str(&format!("{}:{}: {}\n", hit.path, line, hit.text)),
                None => out.push_str(&format!("{}\n", hit.path)),
            }
        }
        out
    }
}

/// Run a search against the sandbox.
///
/// Never returns `Err`: every failure mode comes back as a [`SearchOutcome`]
/// carrying a message the model can act on, so a bad pattern or an unreachable
/// directory costs a round-trip rather than ending the turn.
///
/// Blocking, and meant for [`tokio::task::spawn_blocking`]. `stop` is polled
/// per directory entry because a blocking task cannot be aborted, only asked to
/// stop; a cancelled turn therefore ends the walk within one `stat`.
pub fn run(sandbox: &Sandbox, request: &Request, stop: &AtomicBool) -> SearchOutcome {
    // A `dir=` is resolved by the same function a read uses, so a directory
    // outside the root or inside the denylist is refused in the wording the
    // model already knows from a failed read.
    let scope = match &request.dir {
        Some(dir) => match crate::files::resolve(sandbox, dir) {
            Ok(resolved) if resolved.is_dir() => resolved,
            Ok(_) => {
                return SearchOutcome::failed(request, format!("{dir} is not a directory"));
            }
            Err(error) => return SearchOutcome::failed(request, error),
        },
        None => sandbox.root().to_path_buf(),
    };

    let matcher = match request.kind {
        SearchKind::Grep => match compile(&request.pattern) {
            Ok(regex) => regex,
            Err(error) => return SearchOutcome::failed(request, error),
        },
        SearchKind::Glob => match glob_to_regex(&request.pattern) {
            Ok(regex) => regex,
            Err(error) => return SearchOutcome::failed(request, error),
        },
    };
    let filter = match &request.glob {
        Some(glob) => match glob_to_regex(glob) {
            Ok(regex) => Some(regex),
            Err(error) => return SearchOutcome::failed(request, error),
        },
        None => None,
    };

    let mut walk = Walk {
        sandbox,
        root: sandbox.root().to_path_buf(),
        kind: request.kind,
        matcher,
        filter,
        stop,
        deadline: Instant::now() + MAX_SEARCH_TIME,
        max_hits: match request.kind {
            SearchKind::Grep => MAX_GREP_MATCHES,
            SearchKind::Glob => MAX_GLOB_PATHS,
        },
        hits: Vec::new(),
        files_matched: 0,
        files_scanned: 0,
        files_skipped: 0,
        entries: 0,
        bytes: 0,
        capped: None,
    };
    walk.descend(&scope, 0);

    SearchOutcome {
        kind: request.kind,
        pattern: request.pattern.clone(),
        dir: request.dir.clone(),
        glob: request.glob.clone(),
        hits: walk.hits,
        files_matched: walk.files_matched,
        files_scanned: walk.files_scanned,
        files_skipped: walk.files_skipped,
        capped: walk.capped,
        error: None,
    }
}

/// Compile a model-authored regex, bounded so a pattern that would need an
/// enormous automaton is refused rather than allocated.
fn compile(pattern: &str) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("no pattern was given".to_string());
    }
    RegexBuilder::new(pattern)
        .size_limit(MAX_REGEX_SIZE)
        .build()
        // The crate's own message is unusually good at saying what is wrong
        // with a pattern, so it goes back to the model verbatim.
        .map_err(|e| format!("{pattern} is not a usable regular expression: {e}"))
}

/// Compile a glob into an anchored regex over root-relative paths.
///
/// `*` matches within one path segment, `**` across segments, `?` is one
/// character that is not `/`, and `[abc]` is a class; everything else is
/// literal. A pattern naming no directory is matched at any depth, so `*.rs`
/// finds `src/app.rs` — ripgrep's `--glob` rule, and the one a model will
/// assume.
fn glob_to_regex(pattern: &str) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("no pattern was given".to_string());
    }
    let mut out = String::from("^");
    if !pattern.contains('/') {
        out.push_str("(?:.*/)?");
    }

    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**/` spans zero or more directories, so `**/*.rs` finds
                    // a file at the top of the tree as well as a nested one.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        out.push_str("(?:.*/)?");
                    } else {
                        out.push_str(".*");
                    }
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            '[' => push_class(&mut chars, &mut out),
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');

    RegexBuilder::new(&out)
        .size_limit(MAX_REGEX_SIZE)
        .build()
        .map_err(|e| format!("{pattern} is not a usable pattern: {e}"))
}

/// Translate a `[…]` class, or emit a literal `[` if it is never closed.
///
/// The body is passed through so ranges like `[a-z]` keep working, with `\`
/// escaped so a class cannot smuggle an escape sequence into the output.
fn push_class(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    let mut body = String::new();
    let mut closed = false;
    let negated = matches!(chars.peek(), Some('!') | Some('^'));
    if negated {
        chars.next();
    }
    for c in chars.by_ref() {
        if c == ']' {
            closed = true;
            break;
        }
        if c == '\\' {
            body.push_str("\\\\");
        } else {
            body.push(c);
        }
    }
    if !closed || body.is_empty() {
        // An unclosed or empty class is not a class; treat what we consumed as
        // the literal text it looks like.
        out.push_str(&regex::escape("["));
        if negated {
            out.push_str(&regex::escape("!"));
        }
        out.push_str(&regex::escape(&body));
        return;
    }
    out.push('[');
    if negated {
        out.push('^');
    }
    out.push_str(&body);
    out.push(']');
}

/// The state a walk carries as it descends.
struct Walk<'a> {
    sandbox: &'a Sandbox,
    root: PathBuf,
    kind: SearchKind,
    /// Grep: the content pattern. Glob: the path pattern.
    matcher: Regex,
    /// Grep only: which filenames to bother opening.
    filter: Option<Regex>,
    stop: &'a AtomicBool,
    deadline: Instant,
    max_hits: usize,
    hits: Vec<Hit>,
    files_matched: usize,
    files_scanned: usize,
    files_skipped: usize,
    /// Directory entries visited, which is what [`MAX_ENTRIES`] bounds.
    entries: usize,
    /// Bytes the encoded hits will take, which is what [`MAX_RESULT_BYTES`]
    /// bounds.
    bytes: usize,
    capped: Option<Capped>,
}

impl Walk<'_> {
    /// Walk `dir`, depth-first and in name order.
    ///
    /// Sorting is not cosmetic: `read_dir` order is filesystem-dependent, so
    /// without it the same search returns its results shuffled and any test
    /// asserting on them flakes.
    fn descend(&mut self, dir: &Path, depth: usize) {
        if depth > MAX_DEPTH || self.capped.is_some() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            // An unreadable directory is not a failed search — the rest of the
            // tree is still worth reporting.
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            if !self.keep_going() {
                return;
            }
            self.entries += 1;

            // `file_type` reports the link itself rather than its target, which
            // is what lets a symlink be skipped instead of followed.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if self.sandbox.denies_read(&path) {
                continue;
            }

            if kind.is_dir() {
                let name = entry.file_name();
                if SKIP_DIRS.iter().any(|skip| name == *skip) {
                    continue;
                }
                self.descend(&path, depth + 1);
            } else if kind.is_file() {
                self.visit_file(&path);
            }
        }
    }

    /// Whether the walk may take another step, recording the cap that stopped
    /// it if not.
    fn keep_going(&mut self) -> bool {
        if self.capped.is_some() {
            return false;
        }
        if self.stop.load(Ordering::Relaxed) {
            // A cancelled turn discards the outcome, so which cap is recorded
            // never reaches anyone; stopping is the whole point.
            self.capped = Some(Capped::Time);
            return false;
        }
        if self.entries >= MAX_ENTRIES {
            self.capped = Some(Capped::Entries);
            return false;
        }
        if Instant::now() >= self.deadline {
            self.capped = Some(Capped::Time);
            return false;
        }
        true
    }

    fn visit_file(&mut self, path: &Path) {
        let Some(relative) = self.relative(path) else {
            return;
        };
        match self.kind {
            SearchKind::Glob => {
                self.files_scanned += 1;
                if self.matcher.is_match(&relative) {
                    self.push(Hit {
                        path: relative,
                        line: None,
                        text: String::new(),
                    });
                }
            }
            SearchKind::Grep => {
                if let Some(filter) = &self.filter
                    && !filter.is_match(&relative)
                {
                    return;
                }
                self.grep_file(path, relative);
            }
        }
    }

    /// The path as the model should see it: relative to the root, with `/`
    /// separators, so it can be pasted into an `<ai-harness-read>`.
    fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut out = String::new();
        for (i, part) in relative.components().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(&part.as_os_str().to_string_lossy());
        }
        Some(out)
    }

    fn grep_file(&mut self, path: &Path, relative: String) {
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                self.files_skipped += 1;
                return;
            }
            Ok(_) => {}
            Err(_) => return,
        }
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        // The same test a read uses: a NUL means this is not text, and sending
        // a binary blob would spend context to tell the model nothing.
        let probe = bytes.len().min(BINARY_PROBE_BYTES);
        if bytes[..probe].contains(&0) {
            self.files_skipped += 1;
            return;
        }

        self.files_scanned += 1;
        let mut matched = false;
        for (index, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if self.capped.is_some() {
                break;
            }
            // Lossy per line: one stretch of invalid UTF-8 costs that line its
            // exact bytes, not the whole file its search.
            let text = String::from_utf8_lossy(line);
            let text = text.strip_suffix('\r').unwrap_or(&text);
            if !self.matcher.is_match(text) {
                continue;
            }
            matched = true;
            self.push(Hit {
                path: relative.clone(),
                line: Some(index + 1),
                text: clip(text),
            });
        }
        if matched {
            self.files_matched += 1;
        }
    }

    /// Record a hit, or record the cap that means this one does not fit.
    fn push(&mut self, hit: Hit) {
        // What the encoder will spend on this row, near enough: the path, the
        // line number and its punctuation, the text, and the newline.
        let cost = hit.path.len() + hit.text.len() + 12;
        if self.bytes + cost > MAX_RESULT_BYTES {
            self.capped = Some(Capped::Bytes);
            return;
        }
        self.bytes += cost;
        self.hits.push(hit);
        if self.hits.len() >= self.max_hits {
            self.capped = Some(Capped::Matches);
        }
    }
}

/// Cut a matched line to [`MAX_LINE_CHARS`], on a character boundary.
fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A sandbox over a fresh temp directory. The counter keeps parallel tests
    /// from sharing a directory and clobbering each other's fixtures.
    fn sandbox_in(name: &str) -> (Sandbox, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let unique = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-search-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let dir = std::fs::canonicalize(&dir).unwrap();
        (Sandbox::new(&dir).unwrap(), dir)
    }

    fn write(dir: &Path, path: &str, body: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    fn search(sandbox: &Sandbox, request: &Request) -> SearchOutcome {
        run(sandbox, request, &AtomicBool::new(false))
    }

    fn paths(outcome: &SearchOutcome) -> Vec<&str> {
        outcome.hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn grep_finds_a_match_and_reports_its_line() {
        let (sandbox, dir) = sandbox_in("basic");
        write(&dir, "src/app.rs", "fn one() {}\nfn parse_reply() {}\n");

        let out = search(&sandbox, &Request::grep("fn parse_reply"));
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].path, "src/app.rs");
        assert_eq!(out.hits[0].line, Some(2));
        assert_eq!(out.hits[0].text, "fn parse_reply() {}");
        assert_eq!(out.files_matched, 1);
    }

    #[test]
    fn paths_are_relative_to_the_root_and_usable_as_a_read() {
        let (sandbox, dir) = sandbox_in("relative");
        write(&dir, "src/deep/nested.rs", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["src/deep/nested.rs"]);
        // The whole point of the relative form: it round-trips through a read.
        assert!(crate::files::read(&sandbox, &out.hits[0].path, None, None).succeeded());
    }

    #[test]
    fn a_grep_with_no_matches_is_a_success_not_a_failure() {
        let (sandbox, dir) = sandbox_in("empty");
        write(&dir, "a.rs", "nothing here\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert!(out.succeeded(), "{out:?}");
        assert!(out.hits.is_empty());
        assert_eq!(out.files_scanned, 1);
    }

    /// The test this change exists to keep passing.
    #[test]
    fn grep_never_reads_a_denied_file() {
        let (sandbox, dir) = sandbox_in("denied-grep");
        write(&dir, ".env", "OPENROUTER_API_KEY=needle\n");
        write(&dir, "ok.txt", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["ok.txt"]);
        assert!(
            !out.preview().contains(".env"),
            "the denylist must hold for a walk: {out:?}"
        );
    }

    #[test]
    fn a_glob_of_everything_does_not_leak_dot_env() {
        let (sandbox, dir) = sandbox_in("denied-glob");
        write(&dir, ".env", "OPENROUTER_API_KEY=secret\n");
        write(&dir, "src/app.rs", "fn main() {}\n");

        let out = search(&sandbox, &Request::glob("**/*"));
        assert!(paths(&out).contains(&"src/app.rs"));
        assert!(
            !paths(&out).contains(&".env"),
            "a glob of everything must still respect the denylist: {out:?}"
        );
    }

    #[test]
    fn a_symlink_out_of_the_root_is_not_followed() {
        let (sandbox, dir) = sandbox_in("symlink-out");
        let outside = dir.parent().unwrap().join("ai-harness-search-outside.txt");
        std::fs::write(&outside, "classified\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link.txt")).unwrap();

        let out = search(&sandbox, &Request::grep("classified"));
        assert!(
            out.hits.is_empty(),
            "a symlink out of the root must not be followed: {out:?}"
        );
    }

    /// Passing at all is the assertion: a followed loop would never return.
    #[test]
    fn a_symlink_loop_terminates() {
        let (sandbox, dir) = sandbox_in("symlink-loop");
        write(&dir, "a.txt", "needle\n");
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["a.txt"]);
    }

    #[test]
    fn the_skip_list_holds() {
        let (sandbox, dir) = sandbox_in("skips");
        write(&dir, "target/debug/x.rs", "needle\n");
        write(&dir, "node_modules/y.js", "needle\n");
        write(&dir, ".git/config", "needle\n");
        write(&dir, "src/keep.rs", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["src/keep.rs"]);
    }

    /// A session file holds the whole prior conversation, so a search that
    /// walked into it would feed the transcript back to the model.
    #[test]
    fn the_harness_directory_is_skipped() {
        let (sandbox, dir) = sandbox_in("harness-dir");
        write(
            &dir,
            &format!("{}/sessions/s/session.json", crate::config::HARNESS_DIR),
            "needle\n",
        );
        write(&dir, "src/a.rs", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["src/a.rs"]);
    }

    #[test]
    fn binary_files_are_skipped_and_counted() {
        let (sandbox, dir) = sandbox_in("binary");
        std::fs::write(dir.join("blob.bin"), b"needle\0more needle\n").unwrap();
        write(&dir, "ok.txt", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["ok.txt"]);
        assert_eq!(out.files_skipped, 1);
    }

    #[test]
    fn a_file_over_the_size_cap_is_skipped_and_counted() {
        let (sandbox, dir) = sandbox_in("oversize");
        let huge = "needle\n".repeat((MAX_FILE_BYTES as usize / 7) + 16);
        std::fs::write(dir.join("huge.txt"), huge).unwrap();
        write(&dir, "small.txt", "needle\n");

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(paths(&out), vec!["small.txt"]);
        assert_eq!(out.files_skipped, 1);
    }

    #[test]
    fn the_match_cap_stops_the_walk_and_says_so() {
        let (sandbox, dir) = sandbox_in("match-cap");
        let body = "needle\n".repeat(MAX_GREP_MATCHES + 50);
        std::fs::write(dir.join("many.txt"), body).unwrap();

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(out.capped, Some(Capped::Matches));
        assert_eq!(out.hits.len(), MAX_GREP_MATCHES);
        assert!(out.summary().contains("capped"), "{}", out.summary());
    }

    #[test]
    fn a_long_matched_line_is_clipped() {
        let (sandbox, dir) = sandbox_in("clip");
        let body = format!("needle{}\n", "x".repeat(MAX_LINE_CHARS * 2));
        std::fs::write(dir.join("long.txt"), body).unwrap();

        let out = search(&sandbox, &Request::grep("needle"));
        assert_eq!(out.hits.len(), 1);
        assert!(out.hits[0].text.ends_with('…'));
        assert_eq!(out.hits[0].text.chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn an_invalid_regex_is_an_outcome_not_a_panic() {
        let (sandbox, _dir) = sandbox_in("bad-regex");
        let out = search(&sandbox, &Request::grep("fn ("));
        assert!(!out.succeeded());
        assert!(
            out.error
                .unwrap()
                .contains("not a usable regular expression"),
            "the model needs to be told what was wrong with its pattern"
        );
    }

    #[test]
    fn a_dir_outside_the_root_is_refused_in_the_reads_wording() {
        let (sandbox, _dir) = sandbox_in("bad-dir");
        let out = search(&sandbox, &Request::grep("needle").in_dir(".."));
        assert!(!out.succeeded());
        assert!(
            out.error.unwrap().contains("outside the working directory"),
            "a bad dir should read exactly as a bad read path does"
        );
    }

    #[test]
    fn a_dir_that_is_a_file_is_refused_distinctly() {
        let (sandbox, dir) = sandbox_in("dir-is-file");
        write(&dir, "a.txt", "x\n");
        let out = search(&sandbox, &Request::grep("x").in_dir("a.txt"));
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("is not a directory"));
    }

    /// The skip list bounds where the walk *wanders*, not where it may be
    /// pointed. Naming a skipped directory explicitly is the escape hatch.
    #[test]
    fn dir_scoping_into_a_skipped_directory_is_honoured() {
        let (sandbox, dir) = sandbox_in("scoped-skip");
        write(&dir, "target/x.rs", "needle\n");

        let out = search(&sandbox, &Request::grep("needle").in_dir("target"));
        assert_eq!(paths(&out), vec!["target/x.rs"]);
    }

    #[test]
    fn a_dir_scope_excludes_the_rest_of_the_tree() {
        let (sandbox, dir) = sandbox_in("scoped");
        write(&dir, "src/a.rs", "needle\n");
        write(&dir, "docs/b.md", "needle\n");

        let out = search(&sandbox, &Request::grep("needle").in_dir("src"));
        assert_eq!(paths(&out), vec!["src/a.rs"]);
    }

    #[test]
    fn results_are_ordered_deterministically() {
        let (sandbox, dir) = sandbox_in("order");
        for name in ["c.txt", "a.txt", "b.txt"] {
            write(&dir, name, "needle\n");
        }
        let first = paths(&search(&sandbox, &Request::grep("needle")))
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(first, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn a_grep_filter_restricts_which_files_are_opened() {
        let (sandbox, dir) = sandbox_in("filter");
        write(&dir, "src/a.rs", "needle\n");
        write(&dir, "src/b.md", "needle\n");

        let out = search(&sandbox, &Request::grep("needle").filtered("*.rs"));
        assert_eq!(paths(&out), vec!["src/a.rs"]);
        assert_eq!(out.files_scanned, 1, "a filtered-out file is never opened");
    }

    #[test]
    fn glob_matches_bare_names_at_any_depth() {
        let (sandbox, dir) = sandbox_in("glob-depth");
        write(&dir, "src/app.rs", "x\n");
        write(&dir, "top.rs", "x\n");
        write(&dir, "src/notes.md", "x\n");

        let out = search(&sandbox, &Request::glob("*.rs"));
        assert_eq!(paths(&out), vec!["src/app.rs", "top.rs"]);
    }

    #[test]
    fn a_double_star_prefix_also_matches_the_top_level() {
        let (sandbox, dir) = sandbox_in("glob-doublestar");
        write(&dir, "top.rs", "x\n");
        write(&dir, "src/app.rs", "x\n");

        let out = search(&sandbox, &Request::glob("**/*.rs"));
        assert_eq!(paths(&out), vec!["src/app.rs", "top.rs"]);
    }

    #[test]
    fn a_single_star_does_not_cross_a_directory_boundary() {
        let (sandbox, dir) = sandbox_in("glob-boundary");
        write(&dir, "src/app.rs", "x\n");
        write(&dir, "src/deep/nested.rs", "x\n");

        let out = search(&sandbox, &Request::glob("src/*.rs"));
        assert_eq!(paths(&out), vec!["src/app.rs"]);
    }

    #[test]
    fn a_question_mark_matches_one_character() {
        let (sandbox, dir) = sandbox_in("glob-question");
        write(&dir, "a1.rs", "x\n");
        write(&dir, "a12.rs", "x\n");

        let out = search(&sandbox, &Request::glob("a?.rs"));
        assert_eq!(paths(&out), vec!["a1.rs"]);
    }

    #[test]
    fn a_glob_class_matches_its_members() {
        let (sandbox, dir) = sandbox_in("glob-class");
        write(&dir, "a.rs", "x\n");
        write(&dir, "b.rs", "x\n");
        write(&dir, "c.rs", "x\n");

        let out = search(&sandbox, &Request::glob("[ab].rs"));
        assert_eq!(paths(&out), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn a_dot_in_a_glob_is_literal_not_a_wildcard() {
        let (sandbox, dir) = sandbox_in("glob-dot");
        write(&dir, "a.rs", "x\n");
        write(&dir, "axrs", "x\n");

        let out = search(&sandbox, &Request::glob("a.rs"));
        assert_eq!(paths(&out), vec!["a.rs"]);
    }

    #[test]
    fn case_insensitivity_comes_from_the_inline_flag() {
        let (sandbox, dir) = sandbox_in("case");
        write(&dir, "a.txt", "TODO: fix\n");

        assert!(search(&sandbox, &Request::grep("todo")).hits.is_empty());
        assert_eq!(search(&sandbox, &Request::grep("(?i)todo")).hits.len(), 1);
    }

    #[test]
    fn a_cancelled_walk_stops_early() {
        let (sandbox, dir) = sandbox_in("cancelled");
        for i in 0..50 {
            write(&dir, &format!("f{i}.txt"), "needle\n");
        }
        let stop = AtomicBool::new(true);
        let out = run(&sandbox, &Request::grep("needle"), &stop);
        assert!(
            out.hits.is_empty(),
            "a walk asked to stop should not keep collecting: {out:?}"
        );
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        let (sandbox, _dir) = sandbox_in("empty-pattern");
        assert!(!search(&sandbox, &Request::grep("")).succeeded());
        assert!(!search(&sandbox, &Request::glob("")).succeeded());
    }
}
