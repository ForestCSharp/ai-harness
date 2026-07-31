//! Kernel-enforced confinement for model-authored shell commands.
//!
//! Commands run under macOS Seatbelt via `sandbox-exec`. Confinement is done by
//! the kernel rather than by inspecting the command string: a shell command is
//! an arbitrary program, so no amount of parsing can make `rm -rf ..` safe.
//! Seatbelt checks the *resolved* path on every filesystem operation, which also
//! closes symlink escapes that a Rust-side path check would miss.
//!
//! The policy is:
//!
//! - writes confined to the working-directory subtree — or, under
//!   [`Sandbox::writes_limited_to`], to a single file,
//! - reads open, minus an explicit denylist of secret locations,
//! - network allowed (the approval prompt is the control point).
//!
//! `(allow default)` with targeted denies is deliberate. A `(deny default)`
//! profile aborts processes during dyld startup with no usable diagnostic, so
//! the strict-looking option is in practice the broken one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Paths denied for both read and write, relative to the user's home directory.
/// Not exhaustive — it covers well-known credential stores.
const SECRET_HOME_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "Library/Keychains",
];

/// Files inside the working directory that stay unreadable even though the rest
/// of the tree is open. The harness's own key lives here.
const SECRET_ROOT_FILES: &[&str] = &[".env", ".env.local"];

#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Canonical working directory. Writes are confined to this subtree.
    root: PathBuf,
    home: Option<PathBuf>,
    /// When set, the one path writes may touch — the subtree allowance below is
    /// replaced by this single file. See [`Sandbox::writes_limited_to`].
    write_only: Option<PathBuf>,
}

impl Sandbox {
    /// Build a sandbox rooted at `root`.
    ///
    /// The path is canonicalised because Seatbelt matches resolved paths: on
    /// macOS `/tmp` is a symlink to `/private/tmp`, so an uncanonicalised root
    /// would silently never match and confine nothing.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            bail!(
                "command execution is only sandboxed on macOS; refusing to run \
                 commands rather than run them unconfined"
            );
        }
        if !Path::new(SANDBOX_EXEC).exists() {
            bail!("{SANDBOX_EXEC} not found; refusing to run commands unsandboxed");
        }

        let root = root.as_ref();
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("resolving sandbox root {}", root.display()))?;
        if !root.is_dir() {
            bail!("sandbox root {} is not a directory", root.display());
        }
        check_profile_safe(&root)?;

        // A missing or odd home directory is not fatal; it only means there are
        // no home-relative denies to add.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| std::fs::canonicalize(h).ok())
            .filter(|h| check_profile_safe(h).is_ok());

        Ok(Self {
            root,
            home,
            write_only: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A clone whose only writable path is `path`.
    ///
    /// This is how plan mode holds still: while a plan is being written, the plan
    /// file is the only thing any command can change, so researching a codebase
    /// cannot modify it. Enforced by the kernel like every other rule here, which
    /// is why it covers a shell command as well as an `<ai-harness-write>` — no
    /// inspection of the command string is involved.
    ///
    /// `path` need not exist yet; Seatbelt matches the path, not an inode. It is
    /// caller's business to keep it representable — see [`check_profile_safe`],
    /// which the caller runs before offering the mode.
    pub fn writes_limited_to(&self, path: impl AsRef<Path>) -> Self {
        Self {
            write_only: Some(path.as_ref().to_path_buf()),
            ..self.clone()
        }
    }

    /// Whether an already-resolved path falls under the read denylist.
    ///
    /// Built from the same constants the SBPL profile is rendered from, so an
    /// in-process read (see `crate::files`) and a shelled-out one can never
    /// disagree about what counts as a secret. Expects a canonical path — the
    /// kernel matches resolved paths, and so must this.
    pub fn denies_read(&self, path: &Path) -> bool {
        if let Some(home) = &self.home {
            // `(subpath …)`: the directory itself and everything beneath it.
            for entry in SECRET_HOME_SUBPATHS {
                if path.starts_with(home.join(entry)) {
                    return true;
                }
            }
        }
        // `(literal …)`: an exact match only.
        SECRET_ROOT_FILES
            .iter()
            .any(|file| path == self.root.join(file))
    }

    /// Render the Seatbelt (SBPL) profile.
    ///
    /// Deny rules come last so they take precedence over the broad `allow`.
    pub fn profile(&self) -> String {
        // Plan mode's narrowing: one file in place of the whole subtree. Phrased
        // as a comment the reader of a leaked profile can understand, since this
        // is the rule that will surprise someone whose build suddenly fails.
        let writes = match &self.write_only {
            Some(path) => format!(
                ";; Plan mode: this file is the only writable path.\n\
                 (allow file-write* (literal \"{}\"))",
                escape(&path.to_string_lossy())
            ),
            None => format!(
                ";; Writes are confined to the working-directory subtree.\n\
                 (allow file-write* (subpath \"{}\"))",
                escape(&self.root.to_string_lossy())
            ),
        };

        let mut denies = String::new();
        if let Some(home) = &self.home {
            for entry in SECRET_HOME_SUBPATHS {
                let path = home.join(entry);
                denies.push_str(&format!(
                    "\n  (subpath \"{}\")",
                    escape(&path.to_string_lossy())
                ));
            }
        }
        for file in SECRET_ROOT_FILES {
            let path = self.root.join(file);
            denies.push_str(&format!(
                "\n  (literal \"{}\")",
                escape(&path.to_string_lossy())
            ));
        }

        format!(
            r#"(version 1)
(allow default)

(deny file-write*)
{writes}

;; Terminal and null sinks, so ordinary commands can still emit output.
(allow file-write-data
  (literal "/dev/null")
  (literal "/dev/stdout")
  (literal "/dev/stderr")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))

;; Credential stores stay unreadable even though reads are otherwise open.
;; Listed last so these denies win over the (allow default) above.
(deny file-read* file-write*{denies})
"#
        )
    }

    /// Build the sandboxed command. `script` is passed to `sh -c` inside the
    /// sandbox, so the confinement applies to it and every process it spawns.
    pub fn command(&self, script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(SANDBOX_EXEC);
        command
            .arg("-p")
            .arg(self.profile())
            .arg("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(&self.root);
        command
    }

    /// Run a program directly under the sandbox, with no shell in between. Each
    /// argument is passed literally, so a value like a file path can never be
    /// reinterpreted as shell syntax.
    pub fn program(&self, program: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(self.profile()).arg(program);
        command.args(args).current_dir(&self.root);
        command
    }
}

/// Whether `path` can appear in a profile at all.
///
/// Public so a caller can decline to offer a mode whose confinement it could not
/// express, rather than discovering it when the first command fails.
pub fn path_is_safe(path: &Path) -> bool {
    check_profile_safe(path).is_ok()
}

/// Escape a path for an SBPL string literal.
fn escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Reject paths that cannot be represented safely in a profile.
///
/// Quotes and backslashes are escaped, but control characters would corrupt the
/// profile in ways that are hard to reason about, so those are refused outright
/// rather than emitting a policy we cannot vouch for.
fn check_profile_safe(path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    if text.chars().any(|c| c.is_control()) {
        bail!(
            "path {} contains a control character and cannot be sandboxed safely",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-sbtest-{name}"));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape(r"a\b"), r"a\\b");
        assert_eq!(escape("/plain/path"), "/plain/path");
    }

    #[test]
    fn rejects_control_characters_in_paths() {
        assert!(check_profile_safe(Path::new("/tmp/ok")).is_ok());
        assert!(check_profile_safe(Path::new("/tmp/ba\nd")).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn root_is_canonicalised() {
        // /tmp is a symlink to /private/tmp; an unresolved root would confine nothing.
        let sandbox = Sandbox::new("/tmp").unwrap();
        assert_eq!(sandbox.root(), Path::new("/private/tmp"));
        assert!(sandbox.profile().contains("/private/tmp"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn profile_confines_writes_to_the_root() {
        let root = temp_root("profile");
        let profile = Sandbox::new(&root).unwrap().profile();
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            root.to_string_lossy()
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_narrowed_profile_allows_one_file_and_not_the_tree() {
        // Plan mode's guarantee. The subtree allowance must be *replaced*, not
        // added to, or researching a codebase could still rewrite it.
        let root = temp_root("plan-only");
        let plan = root.join("plan.md");
        let profile = Sandbox::new(&root)
            .unwrap()
            .writes_limited_to(&plan)
            .profile();

        assert!(profile.contains(&format!(
            "(allow file-write* (literal \"{}\"))",
            plan.to_string_lossy()
        )));
        assert!(
            !profile.contains(&format!(
                "(allow file-write* (subpath \"{}\"))",
                root.to_string_lossy()
            )),
            "the root subtree must not stay writable:\n{profile}"
        );
        assert!(profile.contains("(deny file-write*)"));
        // Output still has to go somewhere, or every command breaks.
        assert!(profile.contains("/dev/null"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn narrowing_keeps_the_secret_denies_last() {
        let root = temp_root("plan-secrets");
        let profile = Sandbox::new(&root)
            .unwrap()
            .writes_limited_to(root.join("plan.md"))
            .profile();
        let allow_at = profile.find("(allow default)").unwrap();
        let deny_at = profile.find("(deny file-read* file-write*").unwrap();
        assert!(deny_at > allow_at, "deny rules must still come last");
        assert!(profile.contains(".ssh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn profile_denies_secret_locations() {
        let root = temp_root("secrets");
        let profile = Sandbox::new(&root).unwrap().profile();
        assert!(profile.contains(".ssh"), "ssh keys must be denied");
        assert!(profile.contains("Keychains"), "keychains must be denied");
        assert!(
            profile.contains(&format!("{}/.env", root.to_string_lossy())),
            "the harness's own key file must be denied"
        );
        // The deny block must come after the broad allow, or it never applies.
        let allow_at = profile.find("(allow default)").unwrap();
        let deny_at = profile.find("(deny file-read* file-write*").unwrap();
        assert!(
            deny_at > allow_at,
            "deny rules must come last to take effect"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_a_nonexistent_root() {
        assert!(Sandbox::new("/tmp/definitely-does-not-exist-ai-harness").is_err());
    }
}
