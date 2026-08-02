//! Reading files for the model, resolved against the sandbox root.
//!
//! Unlike `<ai-harness-shell>`, a read is non-mutating and names exactly one
//! path, so it runs without the approval modal. That trade is only sound
//! because the policy here is *tighter* than the sandbox's: shell commands may
//! read anywhere outside the denylist, while a read element is confined to the
//! working-directory subtree. An auto-approved read ships file contents to
//! OpenRouter, so it must not be able to reach arbitrary paths.
//!
//! Resolving a single concrete path in-process is sound in a way that parsing a
//! shell command never is: `canonicalize` resolves `..` and symlinks before the
//! prefix check, which is the same "compare the *resolved* path" rule the
//! kernel applies (see [`crate::sandbox`]). It also skips a `sandbox-exec`
//! spawn, which matters when reads are meant to feel free.

use std::path::{Path, PathBuf};

use crate::sandbox::Sandbox;

/// Cap on a single read. Large enough for essentially any source file, small
/// enough that one read cannot dominate the context window.
pub const MAX_READ_BYTES: usize = 64 * 1024;

/// Cap on a file an edit may rewrite. An edit rebuilds and rewrites the *whole*
/// file, so — unlike a read — it must load every byte; refusing past this bound
/// keeps a huge file from being pulled into memory and, worse, written back
/// truncated. Well above any hand-edited source file.
pub const MAX_EDIT_BYTES: u64 = 4 * 1024 * 1024;

/// Cap on how far a read will scan just to report a file's total line count.
///
/// Knowing the total is what lets the model plan its next window, but it costs a
/// pass over the whole file. Worth it for source; not worth slurping a
/// multi-gigabyte log to fill in a header field, so past this the count is
/// reported as unknown rather than guessed at.
pub const MAX_COUNT_BYTES: u64 = MAX_EDIT_BYTES;

/// The outcome of a file read. Mirrors [`crate::exec::WriteOutcome`]: a failure
/// is data to hand back to the model, not an error that ends the turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadOutcome {
    /// The path as the model wrote it, for display and for the result message.
    pub path: String,
    pub contents: String,
    pub lines: usize,
    /// The window was cut short by [`MAX_READ_BYTES`] before it reached the
    /// requested line count. Distinct from [`ReadOutcome::has_more`]: this says
    /// the *window* was clipped, that says the *file* continues.
    pub truncated: bool,
    /// 1-based line this window starts at.
    ///
    /// `serde(default)` and no `session::VERSION` bump, the same way
    /// `Session::ledger` was added — but 0 is not a valid line, so
    /// [`ReadOutcome::first_line`] normalises it for sessions written before
    /// reads had windows.
    #[serde(default)]
    first_line: usize,
    /// Whether any line follows this window. Costs one peeked line, unlike
    /// `total_lines`, so it is always known.
    #[serde(default)]
    pub has_more: bool,
    /// Lines in the whole file. `None` when the file was too large to count —
    /// see [`MAX_COUNT_BYTES`].
    #[serde(default)]
    pub total_lines: Option<usize>,
    pub error: Option<String>,
}

impl ReadOutcome {
    /// A read that never happened, carrying the reason why.
    pub fn failed(path: &str, error: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            contents: String::new(),
            lines: 0,
            truncated: false,
            first_line: 1,
            has_more: false,
            total_lines: None,
            error: Some(error.into()),
        }
    }

    /// A successful whole-file read of `contents`.
    ///
    /// Used by tests and by callers that already hold a file's text.
    #[cfg_attr(not(test), allow(dead_code))]
    ///
    /// The window fields are bookkeeping that only [`read`] can fill in
    /// honestly, so callers that already hold a file's text say *that* rather
    /// than assembling a `ReadOutcome` field by field and getting them wrong.
    pub fn whole_file(path: &str, contents: impl Into<String>) -> Self {
        let contents = contents.into();
        let lines = contents.lines().count();
        Self {
            path: path.to_string(),
            contents,
            lines,
            truncated: false,
            first_line: 1,
            has_more: false,
            total_lines: Some(lines),
            error: None,
        }
    }

    /// Move this outcome to start at `first_line`, for a window that does not
    /// begin at the top of the file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn at_line(mut self, first_line: usize) -> Self {
        self.first_line = first_line.max(1);
        self
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    /// The 1-based first line of the window. Reads recorded before windows
    /// existed have no stored value and always started at the top.
    pub fn first_line(&self) -> usize {
        self.first_line.max(1)
    }

    /// The 1-based last line of the window, or `None` for an empty one.
    pub fn last_line(&self) -> Option<usize> {
        (self.lines > 0).then(|| self.first_line() + self.lines - 1)
    }

    /// Whether this is a plain whole-file read — the top of the file, with
    /// nothing after it. Windows are only worth naming when they are windows.
    pub fn is_whole_file(&self) -> bool {
        self.first_line() == 1 && !self.has_more && !self.truncated
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.error.is_some() {
            return "failed".to_string();
        }
        let bytes = self.contents.len();
        match (self.is_whole_file(), self.last_line()) {
            (true, _) => format!("{} line(s), {bytes} bytes", self.lines),
            // A window: which lines it covers matters more than how many.
            (false, Some(last)) => match self.total_lines {
                Some(total) => format!("lines {}-{last} of {total}", self.first_line()),
                None => format!("lines {}-{last}", self.first_line()),
            },
            // An offset past the end: empty, and the count says why.
            (false, None) => match self.total_lines {
                Some(total) => format!("empty, file has {total} line(s)"),
                None => "empty".to_string(),
            },
        }
    }
}

/// Resolve `path` against the sandbox root, rejecting anything outside it.
///
/// The path is canonicalised first, so `..` segments and symlinks are collapsed
/// before the prefix check rather than after — a symlink inside the root that
/// points outside it is refused, which a string comparison would wave through.
/// Canonicalising also proves the file exists, so a missing file is reported
/// here rather than surfacing later as a confusing read failure.
pub fn resolve(sandbox: &Sandbox, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("no path was given".to_string());
    }
    let root = sandbox.root();
    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let resolved = std::fs::canonicalize(&joined).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{path}: no such file"),
        std::io::ErrorKind::PermissionDenied => format!("{path}: permission denied"),
        _ => format!("{path}: {e}"),
    })?;

    // `Path::starts_with` compares whole components, so `/rootbeer` does not
    // count as being inside `/root`.
    if !resolved.starts_with(root) {
        return Err(format!(
            "{path} is outside the working directory; access is confined to {}",
            root.display()
        ));
    }
    if sandbox.denies_read(&resolved) {
        return Err(format!(
            "{path} holds credentials and is not readable by design"
        ));
    }
    Ok(resolved)
}

/// Resolve a path that need not exist yet, for comparing where a write would go.
///
/// [`resolve`] proves the file exists, which a write's target often does not, so
/// this canonicalises the *parent* and appends the file name. That keeps the
/// property that matters for a comparison: `./a/../plan.md` and a symlinked
/// directory both collapse to the same answer, so neither can masquerade as
/// another file. The parent must exist — a path whose directory is absent is not
/// the file we are looking for anyway.
///
/// This is for deciding what to *tell* the model, not for confinement. The kernel
/// is the boundary: see [`crate::sandbox`].
pub fn resolve_target(sandbox: &Sandbox, path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("no path was given".to_string());
    }
    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        sandbox.root().join(requested)
    };
    let name = joined
        .file_name()
        .ok_or_else(|| format!("{path}: not a file path"))?
        .to_os_string();
    let parent = joined.parent().unwrap_or(Path::new("/"));
    let parent =
        std::fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.to_string_lossy()))?;
    Ok(parent.join(name))
}

/// Read a window of a file for the model, bounded by [`MAX_READ_BYTES`].
///
/// `offset` is the 1-based first line and `limit` the number of lines; `None`
/// for either means "from the top" and "as much as fits". Windowing is by line
/// rather than by byte because the model navigates with `grep -n` and reasons in
/// line numbers — a byte offset would be unusable for finding a function.
///
/// Every failure mode comes back as a `ReadOutcome` carrying an error the model
/// can act on, so a bad path costs a round-trip rather than ending the turn.
pub fn read(sandbox: &Sandbox, path: &str, offset: Option<usize>, limit: Option<usize>) -> ReadOutcome {
    let resolved = match resolve(sandbox, path) {
        Ok(resolved) => resolved,
        Err(error) => return ReadOutcome::failed(path, error),
    };
    if resolved.is_dir() {
        return ReadOutcome::failed(path, format!("{path} is a directory, not a file"));
    }

    let first_line = offset.unwrap_or(1).max(1);
    let file = match std::fs::File::open(&resolved) {
        Ok(file) => file,
        Err(e) => return ReadOutcome::failed(path, format!("{path}: {e}")),
    };

    match window(file, first_line, limit) {
        Ok(window) => {
            // A NUL byte means this is not text. Sending a binary blob would
            // waste the context window and tell the model nothing it can use.
            if window.bytes.contains(&0) {
                return ReadOutcome::failed(path, format!("{path} looks like a binary file"));
            }
            // Lossy: a cut mid-character at the cap becomes U+FFFD, not an error.
            let contents = String::from_utf8_lossy(&window.bytes).into_owned();
            ReadOutcome {
                path: path.to_string(),
                lines: window.lines,
                contents,
                truncated: window.truncated,
                first_line,
                has_more: window.has_more,
                total_lines: window.total_lines,
                error: None,
            }
        }
        Err(e) => ReadOutcome::failed(path, format!("{path}: {e}")),
    }
}

/// The slice of a file a read asked for, plus what surrounds it.
struct Window {
    bytes: Vec<u8>,
    lines: usize,
    /// The byte cap cut the window short of the requested line count.
    truncated: bool,
    has_more: bool,
    total_lines: Option<usize>,
}

/// Pull `limit` lines starting at `first_line`, then keep counting.
///
/// Reads line-wise rather than slurping: skipping to the window costs only the
/// bytes before it, and the tail is counted without being kept. The count stops
/// at [`MAX_COUNT_BYTES`] so a huge file reports an unknown total instead of
/// being read end to end for a header field.
fn window(file: std::fs::File, first_line: usize, limit: Option<usize>) -> std::io::Result<Window> {
    use std::io::BufRead;

    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut lines = 0usize;
    let mut seen = 0usize;
    let mut scanned = 0u64;
    let mut truncated = false;
    let mut has_more = false;
    let mut counted_all = true;

    loop {
        // Neither skipping to the window nor counting past it may read a huge
        // file to the end; the total is a nicety, not worth unbounded work.
        if scanned > MAX_COUNT_BYTES {
            counted_all = false;
            break;
        }
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        seen += 1;
        scanned += n as u64;

        if seen < first_line {
            continue;
        }
        if limit.is_some_and(|limit| lines >= limit) {
            // Past the window: nothing left to collect, only to count.
            has_more = true;
            continue;
        }
        let room = MAX_READ_BYTES.saturating_sub(bytes.len());
        if n <= room {
            bytes.extend_from_slice(&line);
            lines += 1;
            continue;
        }
        // Out of room. Break on a line boundary so the next offset names a line
        // the model can actually ask for — unless the very first line of the
        // window is itself over the cap, where a clean boundary would mean
        // returning nothing at all.
        truncated = true;
        has_more = true;
        if bytes.is_empty() {
            bytes.extend_from_slice(&line[..room]);
            lines += 1;
        }
    }

    Ok(Window {
        bytes,
        lines,
        truncated,
        has_more,
        total_lines: counted_all.then_some(seen),
    })
}

/// A prepared edit: the whole file with one span replaced, ready to be written.
///
/// Built before the approval modal so a hopeless edit (no match, or ambiguous)
/// never reaches the user — it goes straight back to the model to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    pub path: String,
    /// The full new file contents, with the one match replaced.
    pub updated: String,
    pub old_len: usize,
    pub new_len: usize,
}

/// Read a whole file, bounded by [`MAX_EDIT_BYTES`].
///
/// Unlike [`read`], which truncates for the model, this returns the file or
/// nothing: its callers either write the result back or diff against it, and a
/// silently truncated read would drop the tail either way. Every failure is a
/// message the caller can hand to the model or discard.
pub fn read_all(sandbox: &Sandbox, path: &str) -> Result<String, String> {
    let resolved = resolve(sandbox, path)?;
    if resolved.is_dir() {
        return Err(format!("{path} is a directory, not a file"));
    }
    // Guard on size before reading, so a giant file is refused rather than slurped.
    let len = std::fs::metadata(&resolved)
        .map_err(|e| format!("{path}: {e}"))?
        .len();
    if len > MAX_EDIT_BYTES {
        return Err(format!(
            "{path} is too large to edit in place; change it with a shell command instead"
        ));
    }
    std::fs::read_to_string(&resolved)
        .map_err(|_| format!("{path} is not UTF-8 text and cannot be edited this way"))
}

/// Resolve an edit against the file on disk, replacing the single occurrence of
/// `old` with `new`.
///
/// Every failure is a message the model can act on: the match must exist and be
/// unique, and the fix for each case is spelled out.
pub fn plan_edit(sandbox: &Sandbox, path: &str, old: &str, new: &str) -> Result<EditPlan, String> {
    let contents = read_all(sandbox, path)?;

    match contents.matches(old).count() {
        0 => Err(format!(
            "the text to replace was not found in {path}; re-read the file and copy \
             the text exactly, including whitespace"
        )),
        1 => Ok(EditPlan {
            path: path.to_string(),
            updated: contents.replacen(old, new, 1),
            old_len: old.len(),
            new_len: new.len(),
        }),
        n => Err(format!(
            "the text to replace appears {n} times in {path}; include more \
             surrounding lines so it matches exactly one place"
        )),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A sandbox over a fresh temp directory. The counter keeps parallel tests
    /// from sharing a directory and clobbering each other's fixtures.
    fn sandbox_in(name: &str) -> (Sandbox, PathBuf) {
        static N: AtomicU32 = AtomicU32::new(0);
        let unique = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-files-{name}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let dir = std::fs::canonicalize(&dir).unwrap();
        (Sandbox::new(&dir).unwrap(), dir)
    }

    #[test]
    fn reads_a_file_verbatim() {
        let (sandbox, dir) = sandbox_in("plain");
        let body = "line one\nline two\n";
        std::fs::write(dir.join("a.txt"), body).unwrap();

        let out = read(&sandbox, "a.txt", None, None);
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.contents, body);
        assert_eq!(out.lines, 2);
        assert!(!out.truncated);
    }

    #[test]
    fn reads_through_a_subdirectory() {
        let (sandbox, dir) = sandbox_in("subdir");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(read(&sandbox, "src/main.rs", None, None).contents, "fn main() {}");
    }

    #[test]
    fn a_missing_file_is_reported_not_panicked() {
        let (sandbox, _dir) = sandbox_in("missing");
        let out = read(&sandbox, "nope.txt", None, None);
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("no such file"));
    }

    #[test]
    fn a_directory_is_rejected_distinctly() {
        let (sandbox, dir) = sandbox_in("isdir");
        std::fs::create_dir_all(dir.join("somedir")).unwrap();
        let out = read(&sandbox, "somedir", None, None);
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("is a directory"));
    }

    /// A `..` that lands on a file which really exists, so the prefix check is
    /// what refuses it rather than the file simply being absent.
    #[test]
    fn traversal_to_a_real_file_outside_the_root_is_rejected() {
        let (sandbox, dir) = sandbox_in("traversal");
        let sibling = dir
            .parent()
            .unwrap()
            .join("ai-harness-outside-the-root.txt");
        std::fs::write(&sibling, "secret").unwrap();

        let out = read(&sandbox, "../ai-harness-outside-the-root.txt", None, None);
        assert!(!out.succeeded(), "traversal must not read outside the root");
        assert!(!out.contents.contains("secret"));
        assert!(out.error.unwrap().contains("outside the working directory"));
        let _ = std::fs::remove_file(&sibling);
    }

    #[test]
    fn traversal_to_a_missing_path_is_also_refused() {
        let (sandbox, _dir) = sandbox_in("traversal-missing");
        assert!(!read(&sandbox, "../../../etc/hosts", None, None).succeeded());
    }

    #[test]
    fn an_absolute_path_outside_the_root_is_rejected() {
        let (sandbox, _dir) = sandbox_in("absolute");
        let out = read(&sandbox, "/etc/hosts", None, None);
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("outside the working directory"));
    }

    /// The case a string-prefix check would wave through: the path is textually
    /// inside the root, but resolves outside it.
    #[test]
    fn a_symlink_pointing_out_of_the_root_is_rejected() {
        let (sandbox, dir) = sandbox_in("symlink");
        let link = dir.join("escape");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();

        let out = read(&sandbox, "escape", None, None);
        assert!(!out.succeeded(), "symlink escape must be refused: {out:?}");
        assert!(out.error.unwrap().contains("outside the working directory"));
    }

    #[test]
    fn the_key_file_stays_unreadable() {
        let (sandbox, dir) = sandbox_in("dotenv");
        std::fs::write(dir.join(".env"), "OPENROUTER_API_KEY=supersecret\n").unwrap();

        let out = read(&sandbox, ".env", None, None);
        assert!(!out.succeeded(), "the key file must not be readable");
        assert!(!out.contents.contains("supersecret"));
        assert!(out.error.unwrap().contains("credentials"));
    }

    #[test]
    fn a_large_file_is_truncated_and_says_so() {
        let (sandbox, dir) = sandbox_in("large");
        let body = "x".repeat(MAX_READ_BYTES * 2);
        std::fs::write(dir.join("big.txt"), &body).unwrap();

        let out = read(&sandbox, "big.txt", None, None);
        assert!(out.succeeded(), "{out:?}");
        assert!(out.truncated);
        assert_eq!(out.contents.len(), MAX_READ_BYTES);
    }

    /// A file of numbered lines, so a window's contents identify themselves.
    fn numbered(dir: &Path, name: &str, lines: usize) {
        let body: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_window_returns_exactly_the_lines_asked_for() {
        let (sandbox, dir) = sandbox_in("window");
        numbered(&dir, "n.txt", 500);

        let out = read(&sandbox, "n.txt", Some(200), Some(3));
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.contents, "line 200\nline 201\nline 202\n");
        assert_eq!(out.lines, 3);
        assert_eq!(out.first_line(), 200);
        assert_eq!(out.last_line(), Some(202));
        assert_eq!(out.total_lines, Some(500));
        assert!(out.has_more, "497 lines follow the window");
        assert!(!out.truncated, "the byte cap was nowhere near");
    }

    #[test]
    fn a_window_reaching_the_end_reports_nothing_more() {
        let (sandbox, dir) = sandbox_in("window-end");
        numbered(&dir, "n.txt", 10);

        let out = read(&sandbox, "n.txt", Some(8), Some(3));
        assert_eq!(out.contents, "line 8\nline 9\nline 10\n");
        assert!(!out.has_more, "the window ends exactly at EOF");
        assert_eq!(out.total_lines, Some(10));
    }

    #[test]
    fn a_limit_past_the_end_returns_what_exists() {
        let (sandbox, dir) = sandbox_in("window-over");
        numbered(&dir, "n.txt", 5);

        let out = read(&sandbox, "n.txt", Some(4), Some(999));
        assert_eq!(out.lines, 2);
        assert!(!out.has_more);
    }

    /// The end of a paging loop, not an error: answering it with a protocol
    /// failure would cost a correction round-trip for correct behaviour.
    #[test]
    fn an_offset_past_the_end_succeeds_and_is_empty() {
        let (sandbox, dir) = sandbox_in("window-past");
        numbered(&dir, "n.txt", 5);

        let out = read(&sandbox, "n.txt", Some(99), None);
        assert!(out.succeeded(), "{out:?}");
        assert!(out.contents.is_empty());
        assert_eq!(out.lines, 0);
        assert_eq!(out.last_line(), None);
        assert!(!out.has_more);
        assert_eq!(out.total_lines, Some(5));
    }

    /// The byte cap has to break on a line boundary, or the next `offset` names
    /// a line the model has already half-seen.
    #[test]
    fn the_byte_cap_clips_a_window_on_a_line_boundary() {
        let (sandbox, dir) = sandbox_in("window-cap");
        // Each line is 100 bytes, so the cap lands mid-file, not mid-line.
        let line = format!("{}\n", "y".repeat(99));
        let body: String = std::iter::repeat_n(line.as_str(), 2000).collect();
        std::fs::write(dir.join("wide.txt"), &body).unwrap();

        let out = read(&sandbox, "wide.txt", None, Some(2000));
        assert!(out.truncated, "the cap should have cut this short");
        assert!(out.has_more);
        assert!(out.contents.len() <= MAX_READ_BYTES);
        assert!(
            out.contents.ends_with('\n'),
            "a clipped window must end on a line boundary"
        );
        assert_eq!(out.lines, out.contents.lines().count());

        // The advertised next line is genuinely the next unseen one.
        let next = out.last_line().unwrap() + 1;
        let rest = read(&sandbox, "wide.txt", Some(next), Some(1));
        assert_eq!(rest.contents, line);
    }

    /// The whole point, against the file that motivated it: `src/app.rs` is far
    /// past the 64 KB cap, so before windows existed its tail was unreachable —
    /// and a model that hit the cap could only read the same head again.
    #[test]
    fn the_tail_of_an_oversized_file_is_reachable() {
        let sandbox = Sandbox::new(env!("CARGO_MANIFEST_DIR")).unwrap();

        let head = read(&sandbox, "src/app.rs", None, None);
        assert!(head.succeeded(), "{head:?}");
        assert!(head.has_more, "src/app.rs should not fit in one read");
        let total = head.total_lines.expect("countable");
        assert!(
            head.last_line().unwrap() < total,
            "the head should stop short of the end"
        );

        // Page to the very end, which the old contract could never reach.
        let last = read(&sandbox, "src/app.rs", Some(total), Some(1));
        assert_eq!(last.lines, 1);
        assert_eq!(last.first_line(), total);
        assert!(!last.has_more, "nothing follows the last line");
        assert!(
            !last.contents.is_empty(),
            "the final line should have content"
        );
    }

    /// A single line over the cap still has to return something, or the read is
    /// an infinite loop of empty windows.
    #[test]
    fn one_oversized_line_is_clipped_rather_than_dropped() {
        let (sandbox, dir) = sandbox_in("window-huge-line");
        std::fs::write(dir.join("one.txt"), "z".repeat(MAX_READ_BYTES * 2)).unwrap();

        let out = read(&sandbox, "one.txt", None, None);
        assert!(out.succeeded(), "{out:?}");
        assert_eq!(out.contents.len(), MAX_READ_BYTES);
        assert_eq!(out.lines, 1);
        assert!(out.truncated);
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_dumped() {
        let (sandbox, dir) = sandbox_in("binary");
        std::fs::write(dir.join("blob.bin"), [0x7f, 0x45, 0x4c, 0x00, 0x01]).unwrap();

        let out = read(&sandbox, "blob.bin", None, None);
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("binary"));
    }

    #[test]
    fn an_empty_path_is_rejected() {
        let (sandbox, _dir) = sandbox_in("emptypath");
        assert!(!read(&sandbox, "   ", None, None).succeeded());
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_its_bytes() {
        let (sandbox, dir) = sandbox_in("nonewline");
        std::fs::write(dir.join("b.txt"), "no trailing newline").unwrap();
        let out = read(&sandbox, "b.txt", None, None);
        assert_eq!(out.contents, "no trailing newline");
        assert_eq!(out.lines, 1);
    }

    #[test]
    fn plan_edit_replaces_a_unique_span_and_leaves_the_rest() {
        let (sandbox, dir) = sandbox_in("edit-unique");
        let before = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        std::fs::write(dir.join("m.rs"), before).unwrap();

        let plan = plan_edit(&sandbox, "m.rs", "let x = 1;", "let x = 2;").unwrap();
        assert_eq!(
            plan.updated,
            "fn main() {\n    let x = 2;\n    println!(\"{x}\");\n}\n"
        );
        // Pre-flight does not touch disk; only the later write does.
        assert_eq!(std::fs::read_to_string(dir.join("m.rs")).unwrap(), before);
    }

    #[test]
    fn plan_edit_reports_a_missing_span() {
        let (sandbox, dir) = sandbox_in("edit-nomatch");
        std::fs::write(dir.join("m.rs"), "let x = 1;\n").unwrap();
        let err = plan_edit(&sandbox, "m.rs", "let y = 9;", "z").unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn plan_edit_refuses_an_ambiguous_span() {
        let (sandbox, dir) = sandbox_in("edit-ambiguous");
        std::fs::write(dir.join("m.rs"), "a\na\na\n").unwrap();
        let err = plan_edit(&sandbox, "m.rs", "a", "b").unwrap_err();
        assert!(err.contains("3 times"), "{err}");
    }

    #[test]
    fn plan_edit_can_delete_by_replacing_with_nothing() {
        let (sandbox, dir) = sandbox_in("edit-delete");
        std::fs::write(dir.join("m.rs"), "keep\nDROP ME\nkeep\n").unwrap();
        let plan = plan_edit(&sandbox, "m.rs", "DROP ME\n", "").unwrap();
        assert_eq!(plan.updated, "keep\nkeep\n");
    }

    #[test]
    fn plan_edit_matches_a_multi_line_span_exactly() {
        let (sandbox, dir) = sandbox_in("edit-multiline");
        let before = "one\ntwo\nthree\n";
        std::fs::write(dir.join("m.txt"), before).unwrap();
        let plan = plan_edit(&sandbox, "m.txt", "two\nthree\n", "two\nTHREE\nfour\n").unwrap();
        assert_eq!(plan.updated, "one\ntwo\nTHREE\nfour\n");
    }

    #[test]
    fn plan_edit_honours_crlf_in_the_span() {
        let (sandbox, dir) = sandbox_in("edit-crlf");
        // A CRLF file: the span must include the \r or it will not match.
        std::fs::write(dir.join("m.txt"), "a\r\nb\r\n").unwrap();
        assert!(plan_edit(&sandbox, "m.txt", "b\n", "c\n").is_err());
        let plan = plan_edit(&sandbox, "m.txt", "b\r\n", "c\r\n").unwrap();
        assert_eq!(plan.updated, "a\r\nc\r\n");
    }

    #[test]
    fn plan_edit_refuses_a_path_outside_the_root() {
        let (sandbox, dir) = sandbox_in("edit-escape");
        let outside = dir.parent().unwrap().join("ai-harness-edit-escape.txt");
        std::fs::write(&outside, "secret").unwrap();
        let err =
            plan_edit(&sandbox, "../ai-harness-edit-escape.txt", "secret", "pwned").unwrap_err();
        assert!(err.contains("outside the working directory"), "{err}");
        // The file on disk is untouched.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn plan_edit_refuses_a_non_utf8_file() {
        let (sandbox, dir) = sandbox_in("edit-binary");
        // 0xFF/0xFE never appear in valid UTF-8, so this cannot be edited as text
        // — and must fail rather than write back a mangled, lossy rewrite.
        std::fs::write(dir.join("b.bin"), [0xFF, 0xFE, 0x00]).unwrap();
        let err = plan_edit(&sandbox, "b.bin", "x", "y").unwrap_err();
        assert!(err.contains("not UTF-8"), "{err}");
    }

    #[test]
    fn plan_edit_refuses_a_file_too_large_to_rewrite() {
        let (sandbox, dir) = sandbox_in("edit-toobig");
        let big = "x".repeat(MAX_EDIT_BYTES as usize + 1);
        std::fs::write(dir.join("big.txt"), &big).unwrap();
        let err = plan_edit(&sandbox, "big.txt", "x", "y").unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn read_all_returns_the_whole_file_untruncated() {
        // Unlike `read`, which bounds what the model sees: this one's callers
        // write the result back or diff against it, so a silent truncation
        // would drop the tail either way.
        let (sandbox, dir) = sandbox_in("read-all");
        let body = "line\n".repeat(MAX_READ_BYTES / 2);
        std::fs::write(dir.join("long.txt"), &body).unwrap();

        assert_eq!(read_all(&sandbox, "long.txt").unwrap(), body);
    }

    #[test]
    fn read_all_refuses_what_it_cannot_return_whole() {
        let (sandbox, dir) = sandbox_in("read-all-refusals");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.bin"), [0xFF, 0xFE]).unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(MAX_EDIT_BYTES as usize + 1)).unwrap();

        assert!(read_all(&sandbox, "sub").unwrap_err().contains("directory"));
        assert!(
            read_all(&sandbox, "b.bin")
                .unwrap_err()
                .contains("not UTF-8")
        );
        assert!(
            read_all(&sandbox, "big.txt")
                .unwrap_err()
                .contains("too large")
        );
        assert!(read_all(&sandbox, "nope.txt").is_err());
    }

    #[test]
    fn read_all_cannot_escape_the_sandbox() {
        // It shares `resolve` with every other read, so a write's pre-flight
        // diff is confined exactly as `<ai-harness-read>` is.
        let (sandbox, _dir) = sandbox_in("read-all-escape");
        assert!(read_all(&sandbox, "../outside.txt").is_err());
        assert!(read_all(&sandbox, "/etc/hosts").is_err());
    }
}
