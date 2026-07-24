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

use std::io::Read;
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

/// The outcome of a file read. Mirrors [`crate::exec::WriteOutcome`]: a failure
/// is data to hand back to the model, not an error that ends the turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadOutcome {
    /// The path as the model wrote it, for display and for the result message.
    pub path: String,
    pub contents: String,
    pub lines: usize,
    /// The file was longer than [`MAX_READ_BYTES`] and only the head is here.
    pub truncated: bool,
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
            error: Some(error.into()),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none()
    }

    /// A short status line for the transcript header.
    pub fn summary(&self) -> String {
        if self.error.is_some() {
            "failed".to_string()
        } else if self.truncated {
            format!("{} line(s), truncated", self.lines)
        } else {
            format!("{} line(s), {} bytes", self.lines, self.contents.len())
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

/// Read a file for the model, bounded by [`MAX_READ_BYTES`].
///
/// Every failure mode comes back as a `ReadOutcome` carrying an error the model
/// can act on, so a bad path costs a round-trip rather than ending the turn.
pub fn read(sandbox: &Sandbox, path: &str) -> ReadOutcome {
    let resolved = match resolve(sandbox, path) {
        Ok(resolved) => resolved,
        Err(error) => return ReadOutcome::failed(path, error),
    };
    if resolved.is_dir() {
        return ReadOutcome::failed(path, format!("{path} is a directory, not a file"));
    }

    // Read one byte past the cap: enough to know the file was longer without
    // pulling a multi-gigabyte file into memory to find out.
    let mut buffer = Vec::new();
    let read = std::fs::File::open(&resolved).and_then(|file| {
        file.take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut buffer)
    });
    if let Err(e) = read {
        return ReadOutcome::failed(path, format!("{path}: {e}"));
    }

    let truncated = buffer.len() > MAX_READ_BYTES;
    if truncated {
        buffer.truncate(MAX_READ_BYTES);
    }
    // A NUL byte means this is not text. Sending a binary blob would waste the
    // context window and tell the model nothing it can use.
    if buffer.contains(&0) {
        return ReadOutcome::failed(path, format!("{path} looks like a binary file"));
    }

    // Lossy: a cut mid-character at the cap becomes U+FFFD rather than an error.
    let contents = String::from_utf8_lossy(&buffer).into_owned();
    ReadOutcome {
        path: path.to_string(),
        lines: contents.lines().count(),
        contents,
        truncated,
        error: None,
    }
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

/// Resolve an edit against the file on disk, replacing the single occurrence of
/// `old` with `new`.
///
/// Reads the *whole* file (not the truncating [`read`]) because the result is
/// written back in full — a truncated read would silently drop the tail. Every
/// failure is a message the model can act on: the match must exist and be
/// unique, and the fix for each case is spelled out.
pub fn plan_edit(sandbox: &Sandbox, path: &str, old: &str, new: &str) -> Result<EditPlan, String> {
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
    let contents = std::fs::read_to_string(&resolved)
        .map_err(|_| format!("{path} is not UTF-8 text and cannot be edited this way"))?;

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

        let out = read(&sandbox, "a.txt");
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
        assert_eq!(read(&sandbox, "src/main.rs").contents, "fn main() {}");
    }

    #[test]
    fn a_missing_file_is_reported_not_panicked() {
        let (sandbox, _dir) = sandbox_in("missing");
        let out = read(&sandbox, "nope.txt");
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("no such file"));
    }

    #[test]
    fn a_directory_is_rejected_distinctly() {
        let (sandbox, dir) = sandbox_in("isdir");
        std::fs::create_dir_all(dir.join("somedir")).unwrap();
        let out = read(&sandbox, "somedir");
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

        let out = read(&sandbox, "../ai-harness-outside-the-root.txt");
        assert!(!out.succeeded(), "traversal must not read outside the root");
        assert!(!out.contents.contains("secret"));
        assert!(out.error.unwrap().contains("outside the working directory"));
        let _ = std::fs::remove_file(&sibling);
    }

    #[test]
    fn traversal_to_a_missing_path_is_also_refused() {
        let (sandbox, _dir) = sandbox_in("traversal-missing");
        assert!(!read(&sandbox, "../../../etc/hosts").succeeded());
    }

    #[test]
    fn an_absolute_path_outside_the_root_is_rejected() {
        let (sandbox, _dir) = sandbox_in("absolute");
        let out = read(&sandbox, "/etc/hosts");
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

        let out = read(&sandbox, "escape");
        assert!(!out.succeeded(), "symlink escape must be refused: {out:?}");
        assert!(out.error.unwrap().contains("outside the working directory"));
    }

    #[test]
    fn the_key_file_stays_unreadable() {
        let (sandbox, dir) = sandbox_in("dotenv");
        std::fs::write(dir.join(".env"), "OPENROUTER_API_KEY=supersecret\n").unwrap();

        let out = read(&sandbox, ".env");
        assert!(!out.succeeded(), "the key file must not be readable");
        assert!(!out.contents.contains("supersecret"));
        assert!(out.error.unwrap().contains("credentials"));
    }

    #[test]
    fn a_large_file_is_truncated_and_says_so() {
        let (sandbox, dir) = sandbox_in("large");
        let body = "x".repeat(MAX_READ_BYTES * 2);
        std::fs::write(dir.join("big.txt"), &body).unwrap();

        let out = read(&sandbox, "big.txt");
        assert!(out.succeeded(), "{out:?}");
        assert!(out.truncated);
        assert_eq!(out.contents.len(), MAX_READ_BYTES);
    }

    #[test]
    fn a_binary_file_is_refused_rather_than_dumped() {
        let (sandbox, dir) = sandbox_in("binary");
        std::fs::write(dir.join("blob.bin"), [0x7f, 0x45, 0x4c, 0x00, 0x01]).unwrap();

        let out = read(&sandbox, "blob.bin");
        assert!(!out.succeeded());
        assert!(out.error.unwrap().contains("binary"));
    }

    #[test]
    fn an_empty_path_is_rejected() {
        let (sandbox, _dir) = sandbox_in("emptypath");
        assert!(!read(&sandbox, "   ").succeeded());
    }

    #[test]
    fn a_file_with_no_trailing_newline_keeps_its_bytes() {
        let (sandbox, dir) = sandbox_in("nonewline");
        std::fs::write(dir.join("b.txt"), "no trailing newline").unwrap();
        let out = read(&sandbox, "b.txt");
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
}
