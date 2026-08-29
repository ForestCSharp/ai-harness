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
//! - plus the package-manager caches under `$HOME`, without which no build
//!   command works at all,
//! - reads open, minus an explicit denylist of secret locations,
//! - network allowed (the approval prompt is the control point).
//!
//! `(allow default)` with targeted denies is deliberate. A `(deny default)`
//! profile aborts processes during dyld startup with no usable diagnostic, so
//! the strict-looking option is in practice the broken one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[cfg(target_os = "macos")]
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
    ".cargo/credentials",
    ".cargo/credentials.toml",
    "Library/Keychains",
];

/// Files inside the working directory that stay unreadable even though the rest
/// of the tree is open. The harness's own key lives here.
const SECRET_ROOT_FILES: &[&str] = &[".env", ".env.local"];

/// Package-manager caches, relative to the user's home directory, that writes
/// are allowed to reach despite living outside the working directory.
///
/// Without these, no build command works: cargo, npm, pip and go keep their
/// downloaded packages and index state under `$HOME`, so a denied write there
/// fails the whole command with a bare `Operation not permitted` that reads as a
/// broken toolchain rather than as a policy decision.
///
/// Scoped to the cache directories rather than to each tool's home. `~/.cargo`
/// as a whole would include `bin/` — the `cargo` and `rustc` binaries you run
/// *outside* the sandbox, which is an escape — and the credentials file denied
/// above. A cache holds data the tool will re-fetch if it is corrupted; a
/// binary directory does not. `~/Library/Caches` and `~/.cache` are likewise
/// not taken wholesale: they are shared roots holding every other application's
/// state.
///
/// Not exhaustive, and deliberately so — an unlisted tool fails closed with a
/// path in the error, which is a fixable diagnostic. `~/.rustup` is left out:
/// installing a toolchain is not part of building a project.
const CACHE_HOME_SUBPATHS: &[&str] = &[
    ".cargo/registry",
    ".cargo/git",
    ".npm",
    ".cache/pip",
    ".cache/uv",
    "Library/Caches/pip",
    "Library/Caches/go-build",
    "go/pkg/mod",
];

/// Individual files under the home directory that writes may reach, on the same
/// grounds as [`CACHE_HOME_SUBPATHS`]. Cargo takes its inter-process lock on
/// `$CARGO_HOME/.package-cache`, which sits beside the caches rather than in
/// one, and cannot be acquired read-only.
const CACHE_HOME_FILES: &[&str] = &[".cargo/.package-cache"];

#[derive(Debug, Clone)]
pub struct Sandbox {
    /// Canonical working directory. Writes are confined to this subtree.
    root: PathBuf,
    home: Option<PathBuf>,
    /// When set, the one path writes may touch — the subtree allowance below is
    /// replaced by this single file. See [`Sandbox::writes_limited_to`].
    write_only: Option<PathBuf>,
    /// Whether commands are actually confined.
    ///
    /// False only for [`Sandbox::unconfined`], where something outside the
    /// harness is the boundary instead. Carried on the struct rather than
    /// decided at each call site so that every path which runs a command —
    /// `command`, `program`, and anything built on them — cannot disagree about
    /// it.
    confined: bool,
}

impl Sandbox {
    /// Build a sandbox rooted at `root`.
    ///
    /// The path is canonicalised because Seatbelt matches resolved paths: on
    /// macOS `/tmp` is a symlink to `/private/tmp`, so an uncanonicalised root
    /// would silently never match and confine nothing.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        preflight()?;

        let root = root.as_ref();
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("resolving sandbox root {}", root.display()))?;
        if !root.is_dir() {
            bail!("sandbox root {} is not a directory", root.display());
        }
        #[cfg(target_os = "macos")]
        check_profile_safe(&root)?;

        // A missing or odd home directory is not fatal; it only means there are
        // no home-relative denies to add. The `check_profile_safe` filter is a
        // Seatbelt concern — a home path it cannot express in SBPL — so it does
        // not apply to the Landlock backend, which takes paths as bytes.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| std::fs::canonicalize(h).ok())
            .filter(|h| {
                #[cfg(target_os = "macos")]
                {
                    check_profile_safe(h).is_ok()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = h;
                    true
                }
            });

        Ok(Self {
            root,
            home,
            write_only: None,
            confined: true,
        })
    }

    /// A sandbox that confines nothing, for running inside a container that is
    /// already the isolation boundary.
    ///
    /// This is the one way to run commands without Seatbelt, and it exists for
    /// benchmark runners: they hand each task its own container, so the
    /// confinement the harness would add is the confinement it already has. The
    /// harness cannot verify that claim from in here — which is exactly why
    /// [`crate::config::Args::validate`] refuses this outside `--headless`, and
    /// why the run record carries `unconfined: true` so no result can be read as
    /// confined when it was not.
    ///
    /// Unlike [`Sandbox::new`] this needs neither macOS nor `sandbox-exec`, but
    /// it still resolves the root: the working directory is where commands run
    /// and where reads are confined in-process, and neither of those stops
    /// mattering because the kernel is no longer involved.
    pub fn unconfined(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = std::fs::canonicalize(root)
            .with_context(|| format!("resolving sandbox root {}", root.display()))?;
        if !root.is_dir() {
            bail!("sandbox root {} is not a directory", root.display());
        }
        // Still resolved, because `denies_read` is built from it and the
        // credential denylist stays in force for in-process reads whether or not
        // the kernel is enforcing anything.
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|h| std::fs::canonicalize(h).ok());
        Ok(Self {
            root,
            home,
            write_only: None,
            confined: false,
        })
    }

    /// Whether the kernel is confining what commands can reach.
    pub fn is_confined(&self) -> bool {
        self.confined
    }

    /// The sandbox a test should build for the platform it is running on.
    ///
    /// Now simply [`Sandbox::new`] wherever a backend exists — macOS via
    /// Seatbelt, Linux via Landlock. It was briefly a stopgap that handed Linux
    /// an *unconfined* sandbox so that tests exercising the check loop, jobs and
    /// memory could run there at all; the Landlock backend is what retired that.
    /// Only a platform with no backend still gets the unconfined one, and its
    /// tests are honest about asserting nothing regarding confinement.
    #[cfg(test)]
    pub fn for_tests(root: impl AsRef<Path>) -> Self {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Self::new(root).expect("a sandbox for the test's working directory")
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self::unconfined(root).expect("an unconfined sandbox for the test's working directory")
        }
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

    /// This sandbox moved to `root`, keeping whether it confines anything.
    ///
    /// What `/cd` is built on. Confinement is preserved rather than re-decided:
    /// a `--sandbox none` run must not quietly acquire a boundary it was started
    /// without, and a confined one must not quietly lose the one it was started
    /// with — moving is a change of *where*, never of *whether*.
    ///
    /// `write_only` is deliberately dropped. It names one file in the tree being
    /// left behind, so carrying it over would leave a sandbox rooted here whose
    /// only writable path is somewhere else.
    ///
    /// Goes through the same constructors as a launch does, so the new root gets
    /// the same canonicalisation, the same "is it a directory", and on macOS the
    /// same `check_profile_safe` — a path that could not have been launched in
    /// cannot be reached by moving into it either.
    pub fn rooted_at(&self, root: impl AsRef<Path>) -> Result<Self> {
        if self.confined {
            Self::new(root)
        } else {
            Self::unconfined(root)
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
    /// The SBPL profile handed to `sandbox-exec`. macOS only: Landlock takes
    /// paths rather than a rendered policy, so there is nothing to render.
    #[cfg(target_os = "macos")]
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

        // Caches are granted only to the subtree policy, never to the narrowed
        // one: plan mode's promise is that the plan file is the only thing any
        // command can change, and nothing can build under it anyway — `target/`
        // is inside the root, which plan mode has already made read-only.
        let caches = match (&self.write_only, &self.home) {
            (None, Some(home)) => {
                let mut paths = String::new();
                for entry in CACHE_HOME_SUBPATHS {
                    paths.push_str(&format!(
                        "\n  (subpath \"{}\")",
                        escape(&home.join(entry).to_string_lossy())
                    ));
                }
                for file in CACHE_HOME_FILES {
                    paths.push_str(&format!(
                        "\n  (literal \"{}\")",
                        escape(&home.join(file).to_string_lossy())
                    ));
                }
                format!(
                    "\n;; Package-manager caches, so builds and installs work at all.\n\
                     (allow file-write*{paths})\n"
                )
            }
            _ => String::new(),
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
{caches}
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
        if !self.confined {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.arg("-c").arg(script).current_dir(&self.root);
            return command;
        }
        #[cfg(target_os = "linux")]
        {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.arg("-c").arg(script).current_dir(&self.root);
            self.landlock(&mut command);
            command
        }
        #[cfg(not(target_os = "linux"))]
        {
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
    }

    /// Run a program directly under the sandbox, with no shell in between. Each
    /// argument is passed literally, so a value like a file path can never be
    /// reinterpreted as shell syntax.
    pub fn program(&self, program: &str, args: &[&str]) -> tokio::process::Command {
        if !self.confined {
            let mut command = tokio::process::Command::new(program);
            command.args(args).current_dir(&self.root);
            return command;
        }
        #[cfg(target_os = "linux")]
        {
            let mut command = tokio::process::Command::new(program);
            command.args(args).current_dir(&self.root);
            self.landlock(&mut command);
            command
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut command = tokio::process::Command::new(SANDBOX_EXEC);
            command.arg("-p").arg(self.profile()).arg(program);
            command.args(args).current_dir(&self.root);
            command
        }
    }
}

/// Refuse to build a sandbox where one cannot be enforced.
///
/// Checked at startup rather than at the first command, so a machine that
/// cannot confine anything says so before a turn begins — the same reason the
/// Seatbelt check has always been here rather than in `command()`.
fn preflight() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if !Path::new(SANDBOX_EXEC).exists() {
            bail!("{SANDBOX_EXEC} not found; refusing to run commands unsandboxed");
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
        // Creating a ruleset asks the kernel for one; it restricts nothing until
        // `restrict_self`, so this is a safe probe to run in the harness's own
        // process. Hard-requiring the first ABI is the question worth asking: a
        // kernel that cannot do even that has no confinement to offer.
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|ruleset| ruleset.create())
            .map_err(|err| {
                anyhow::anyhow!(
                    "this kernel cannot enforce Landlock ({err}), so commands \
                     cannot be confined; refusing to run them unconfined. \
                     Landlock needs Linux 5.13 or newer with the LSM enabled at \
                     boot. Inside a container that is already the isolation \
                     boundary, `--headless --sandbox=none` is the deliberate way \
                     past this."
                )
            })?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!(
            "command execution is only sandboxed on macOS and Linux; refusing to \
             run commands rather than run them unconfined"
        )
    }
}

/// The Linux backend: a Landlock policy the child applies to itself before exec.
///
/// Deliberately the same *shape* as the Seatbelt profile rather than a different
/// architecture — a policy attached to one command, built from the same root,
/// home and `write_only` fields — so the two platforms cannot drift apart in
/// what they confine.
///
/// **The policy inverts, and tightens.** Seatbelt is `(allow default)` with a
/// credential denylist; Landlock is allowlist-only and cannot express "allow
/// everything except". So this grants read on the system hierarchies, the
/// workspace and the build caches, and grants nothing else under `$HOME` —
/// which excludes `~/.ssh`, `~/.aws` and the rest *by construction* rather than
/// by enumeration. That closes the gap the denylist openly has, and it is why
/// the Linux read confinement is stronger than the macOS one rather than a
/// weaker approximation of it.
#[cfg(target_os = "linux")]
mod linux {
    use super::{CACHE_HOME_FILES, CACHE_HOME_SUBPATHS, Sandbox};
    use std::path::PathBuf;

    /// System hierarchies a command has to read to be able to run at all: the
    /// interpreter, the shared libraries, the toolchain, the certificate store.
    /// Read-only — nothing here is a place a command has any business writing.
    const SYSTEM_READ: &[&str] = &[
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc",
        "/opt",
        "/proc",
        "/dev",
        "/run",
        "/var/lib",
        "/var/cache",
    ];

    /// Writable outside the workspace — deliberately almost nothing.
    ///
    /// Kept to parity with the Seatbelt profile, which is `(deny file-write*)`
    /// followed by allowances for the workspace, the caches, and
    /// `file-write-data` on the terminal and null sinks. `/dev/null` is that
    /// last allowance; discarding output is not a write anyone means to confine.
    ///
    /// **`/tmp` is deliberately absent.** It was here at first, on the reasoning
    /// that toolchains need scratch space — and `plan_mode_keeps_the_workspace_read_only`
    /// caught what that actually costs: a workspace under `/tmp` inherits the
    /// grant, so plan mode stops confining writes at all. macOS has never
    /// allowed it and works; if a real toolchain turns out to need it, that is a
    /// change to make on both platforms at once rather than a divergence to
    /// leave here.
    const SYSTEM_WRITE: &[&str] = &["/dev/null"];

    impl Sandbox {
        /// Attach the policy to a command that has not been spawned yet.
        pub(super) fn landlock(&self, command: &mut tokio::process::Command) {
            let read = self.landlock_read_paths();
            let write = self.landlock_write_paths();
            // SAFETY: `pre_exec` runs between fork and exec, where only
            // async-signal-safe work is strictly permitted. Landlock is applied
            // here rather than in the parent because it must not restrict the
            // harness itself — the harness reads files the commands it runs may
            // not. This is what every Landlock sandboxer does, including the
            // crate's own example.
            unsafe {
                command.pre_exec(move || restrict(&read, &write));
            }
        }

        /// Everything a command may read. Notably absent: `$HOME` at large.
        fn landlock_read_paths(&self) -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
            paths.push(self.root.clone());
            // The workspace stays readable in plan mode — researching a codebase
            // is the whole point of the mode; it is writing that is narrowed.
            paths.extend(self.cache_paths());
            existing(paths)
        }

        /// Everything a command may write, which is the half that differs
        /// between an ordinary turn and plan mode.
        fn landlock_write_paths(&self) -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
            match &self.write_only {
                // Plan mode. Landlock grants access to a path that exists, so a
                // plan file not yet written falls back to its directory — which
                // is wider than the single `(literal …)` Seatbelt gets, and is
                // the one place the two platforms genuinely differ. The session
                // directory is still a far smaller target than the workspace.
                Some(plan) => {
                    if plan.exists() {
                        paths.push(plan.clone());
                    } else if let Some(parent) = plan.parent() {
                        paths.push(parent.to_path_buf());
                    }
                }
                None => {
                    paths.push(self.root.clone());
                    paths.extend(self.cache_paths());
                }
            }
            existing(paths)
        }

        /// Build caches under the home directory. Without these no build
        /// command works, which is the same reason the Seatbelt profile carves
        /// them out.
        fn cache_paths(&self) -> Vec<PathBuf> {
            let Some(home) = &self.home else {
                return Vec::new();
            };
            CACHE_HOME_SUBPATHS
                .iter()
                .chain(CACHE_HOME_FILES)
                .map(|entry| home.join(entry))
                .collect()
        }
    }

    /// Landlock rules are taken on an open descriptor, so a path that is not
    /// there yet cannot be granted. Dropping them is right rather than fatal:
    /// a machine without `/opt` or without a cargo registry is ordinary.
    fn existing(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().filter(|p| p.exists()).collect()
    }

    /// Apply the policy to the calling process. Runs in the child.
    fn restrict(read: &[PathBuf], write: &[PathBuf]) -> std::io::Result<()> {
        use landlock::{
            ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
            path_beneath_rules,
        };

        // Fixed at compile time, not probed. The crate is explicit that building
        // an ABI from the running kernel "can lead to unreliable sandboxing where
        // rules might differ between executions" — so ask for the newest set and
        // let best-effort mode, which is the default, drop whatever this kernel
        // lacks. The floor that matters was already hard-required in `preflight`.
        let abi = ABI::V9;
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .and_then(|r| r.create())
            .and_then(|r| r.add_rules(path_beneath_rules(read, AccessFs::from_read(abi))))
            .and_then(|r| r.add_rules(path_beneath_rules(write, AccessFs::from_all(abi))))
            .and_then(|r| r.restrict_self())
            .map_err(std::io::Error::other)?;

        // Fail closed. `preflight` already established that this kernel has
        // Landlock, so reaching here unenforced means something went wrong that
        // the harness cannot see — and a command that believes it is confined
        // when it is not is worse than one that refuses to start.
        if status.ruleset == RulesetStatus::NotEnforced {
            return Err(std::io::Error::other(
                "Landlock reported the ruleset was not enforced; refusing to run \
                 this command unconfined",
            ));
        }
        Ok(())
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
#[cfg(target_os = "macos")]
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

    // Only the macOS-gated tests below call this; on Linux they are compiled
    // out and it is left with no callers rather than being wrong.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-sbtest-{name}"));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::canonicalize(&dir).unwrap()
    }

    // `escape` renders SBPL string literals, which only the Seatbelt backend
    // produces.
    #[cfg(target_os = "macos")]
    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape(r"a\b"), r"a\\b");
        assert_eq!(escape("/plain/path"), "/plain/path");
    }

    /// A unique directory per test, so a parallel run cannot have two tests
    /// creating and removing the same path.
    fn moving_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-cd-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("inner"));
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn rooted_at_moves_the_root() {
        let dir = moving_root("moves");
        let sandbox = Sandbox::for_tests(&dir);
        let moved = sandbox.rooted_at(dir.join("inner")).unwrap();
        assert_eq!(moved.root(), dir.join("inner"));
        assert_eq!(sandbox.root(), dir, "the original is left alone");
    }

    /// The invariant worth a test of its own: moving changes *where* a sandbox
    /// confines, never *whether* it does. A `--sandbox none` run must not
    /// silently acquire a boundary by moving into a directory.
    #[test]
    fn rooted_at_keeps_whether_it_confines() {
        let dir = moving_root("confinement");

        let loose = Sandbox::unconfined(&dir).unwrap();
        assert!(!loose.rooted_at(dir.join("inner")).unwrap().is_confined());

        let tight = Sandbox::for_tests(&dir);
        let moved = tight.rooted_at(dir.join("inner")).unwrap();
        assert_eq!(moved.is_confined(), tight.is_confined());
    }

    /// `write_only` names one file in the tree being left behind, so carrying it
    /// across a move would leave a sandbox rooted here whose only writable path
    /// is somewhere else entirely.
    #[test]
    fn rooted_at_drops_the_write_only_path() {
        let dir = moving_root("writeonly");
        let planning = Sandbox::for_tests(&dir).writes_limited_to(dir.join("plan.md"));
        assert!(planning.write_only.is_some());
        let moved = planning.rooted_at(dir.join("inner")).unwrap();
        assert!(moved.write_only.is_none());
    }

    /// The claim `/cd` actually makes, checked where it is enforced: a command
    /// built from the moved sandbox is spawned in the new directory. Asserted on
    /// the working directory of the process about to be spawned rather than on
    /// `root()`, because that is the field the kernel acts on.
    #[test]
    fn a_command_from_a_moved_sandbox_runs_in_the_new_directory() {
        let dir = moving_root("runs-in");
        let moved = Sandbox::for_tests(&dir)
            .rooted_at(dir.join("inner"))
            .unwrap();

        let command = moved.command("pwd");
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(dir.join("inner").as_path())
        );
        let program = moved.program("pwd", &[]);
        assert_eq!(
            program.as_std().get_current_dir(),
            Some(dir.join("inner").as_path())
        );
    }

    #[test]
    fn rooted_at_refuses_what_is_not_a_directory() {
        let dir = moving_root("refuses");
        std::fs::write(dir.join("file"), "x").unwrap();
        let sandbox = Sandbox::for_tests(&dir);
        assert!(sandbox.rooted_at(dir.join("file")).is_err());
        assert!(sandbox.rooted_at(dir.join("nope")).is_err());
        assert_eq!(sandbox.root(), dir, "a refused move changes nothing");
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
    fn profile_allows_the_package_manager_caches() {
        // Regression: a cargo build died on EPERM writing ~/.cargo/registry,
        // which reads as a broken toolchain rather than as a policy decision.
        let root = temp_root("caches");
        let sandbox = Sandbox::new(&root).unwrap();
        let Some(home) = sandbox.home.clone() else {
            return; // No home resolved; there is nothing to assert.
        };
        let profile = sandbox.profile();

        for entry in [".cargo/registry", ".npm", "go/pkg/mod"] {
            assert!(
                profile.contains(&format!(
                    "(subpath \"{}\")",
                    home.join(entry).to_string_lossy()
                )),
                "{entry} must be writable:\n{profile}"
            );
        }
        // Cargo's lock file sits beside the caches, not inside one.
        assert!(profile.contains(&format!(
            "(literal \"{}\")",
            home.join(".cargo/.package-cache").to_string_lossy()
        )));
        // The allowance is scoped: the binaries you run outside the sandbox
        // stay read-only, or the confinement leaks straight out of the box.
        assert!(
            !profile.contains(&format!(
                "(subpath \"{}\")",
                home.join(".cargo").to_string_lossy()
            )),
            "~/.cargo must not be writable wholesale:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_narrowed_profile_grants_no_caches() {
        // Plan mode's guarantee is that one file is the only writable path, and
        // nothing can build under it regardless — `target/` is inside the root.
        let root = temp_root("plan-caches");
        let sandbox = Sandbox::new(&root).unwrap();
        let Some(home) = sandbox.home.clone() else {
            return;
        };
        let profile = sandbox.writes_limited_to(root.join("plan.md")).profile();
        assert!(
            !profile.contains(&format!(
                "(subpath \"{}\")",
                home.join(".cargo/registry").to_string_lossy()
            )),
            "plan mode must not widen writes to the caches:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cargo_credentials_stay_denied_beside_the_cache_allowance() {
        // The allow and the deny both name paths under ~/.cargo; the deny block
        // comes last, so the token is unreadable even though the registry
        // beside it is writable.
        let root = temp_root("cargo-creds");
        let sandbox = Sandbox::new(&root).unwrap();
        let Some(home) = sandbox.home.clone() else {
            return;
        };
        let profile = sandbox.profile();

        let allow_at = profile.find("(allow file-write*").unwrap();
        let deny_at = profile.find("(deny file-read* file-write*").unwrap();
        assert!(deny_at > allow_at, "deny rules must come last:\n{profile}");
        assert!(profile.contains(&format!(
            "(subpath \"{}\")",
            home.join(".cargo/credentials.toml").to_string_lossy()
        )));
        assert!(sandbox.denies_read(&home.join(".cargo/credentials.toml")));
        assert!(!sandbox.denies_read(&home.join(".cargo/registry")));
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

#[cfg(test)]
mod unconfined_tests {
    use super::*;

    /// The whole point of the mode: no `sandbox-exec` in front of the command.
    /// Asserted on the program actually being spawned rather than on a flag,
    /// because the flag is not what confines anything.
    #[test]
    fn an_unconfined_command_does_not_go_through_seatbelt() {
        let sandbox =
            Sandbox::unconfined(std::env::temp_dir()).expect("temp dir should be a valid root");
        assert!(!sandbox.is_confined());
        let command = sandbox.command("echo hi");
        assert_eq!(command.as_std().get_program(), "/bin/sh");
    }

    #[test]
    fn an_unconfined_program_runs_directly() {
        let sandbox =
            Sandbox::unconfined(std::env::temp_dir()).expect("temp dir should be a valid root");
        let command = sandbox.program("echo", &["hi"]);
        assert_eq!(command.as_std().get_program(), "echo");
    }

    /// Unconfined is about the kernel, not about secrets: the credential
    /// denylist is what `crate::files` consults for an in-process read, and it
    /// costs nothing to keep enforcing.
    #[test]
    fn the_credential_denylist_survives_going_unconfined() {
        let root = std::env::temp_dir();
        let sandbox = Sandbox::unconfined(&root).expect("temp dir should be a valid root");
        assert!(
            sandbox.denies_read(&sandbox.root().join(".env")),
            "a secret is still a secret when the container is the boundary"
        );
    }

    #[test]
    fn a_root_that_is_not_a_directory_is_refused() {
        let file = std::env::temp_dir().join("ai-harness-unconfined-root-test");
        std::fs::write(&file, b"x").expect("writing the fixture");
        let result = Sandbox::unconfined(&file);
        let _ = std::fs::remove_file(&file);
        assert!(result.is_err(), "a file is not a workspace");
    }
}

/// Does the Landlock policy actually stop anything?
///
/// The tests above prove a sandbox can be *built*; these prove a command is
/// *confined*, which is the only claim worth making. They spawn real processes
/// and assert on what the kernel allowed, not on what a path check computed.
#[cfg(all(test, target_os = "linux"))]
mod landlock_tests {
    use super::*;

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-ll-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[tokio::test]
    async fn a_write_inside_the_workspace_is_allowed() {
        let dir = workspace("inside");
        let sandbox = Sandbox::new(&dir).expect("landlock should be available");
        let out = sandbox
            .command("echo hi > inside.txt")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.join("inside.txt").is_file(), "the write should land");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/var/tmp` is granted neither read nor write — unlike `/tmp`, which every
    /// toolchain needs. A write there is the kernel refusing, not a path check.
    #[tokio::test]
    async fn a_write_outside_the_workspace_is_refused() {
        let dir = workspace("outside");
        let escape = Path::new("/var/tmp/ai-harness-landlock-escape.txt");
        let _ = std::fs::remove_file(escape);
        let sandbox = Sandbox::new(&dir).expect("landlock should be available");
        let out = sandbox
            .command(&format!("echo escaped > {}", escape.display()))
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "the write should have been refused");
        assert!(!escape.exists(), "and nothing should have been created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The claim that makes the Linux policy *stronger* than the macOS one: the
    /// home directory is not granted at all, so credentials under it are
    /// excluded by construction rather than by an admittedly partial denylist.
    /// Probed by writing a harmless name — if the policy holds, nothing is
    /// created, and if it does not, the test says so rather than the harness
    /// discovering it later.
    #[tokio::test]
    async fn the_home_directory_is_not_writable() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return; // No home to confine; nothing to assert.
        };
        let dir = workspace("home");
        let probe = home.join(".ai-harness-landlock-probe");
        let _ = std::fs::remove_file(&probe);
        let sandbox = Sandbox::new(&dir).expect("landlock should be available");
        let out = sandbox
            .command(&format!("echo probe > {}", probe.display()))
            .output()
            .await
            .unwrap();
        assert!(
            !out.status.success(),
            "a home write should have been refused"
        );
        assert!(!probe.exists(), "and nothing should have been created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reading the toolchain has to keep working, or every command fails for a
    /// reason that has nothing to do with policy.
    #[tokio::test]
    async fn system_paths_stay_readable() {
        let dir = workspace("system");
        let sandbox = Sandbox::new(&dir).expect("landlock should be available");
        let out = sandbox
            .command("ls /usr/bin > /dev/null")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan mode narrows writes to the plan file's directory. Wider than the
    /// single `(literal …)` Seatbelt gets — see `landlock_write_paths` — but the
    /// workspace itself must still be read-only.
    #[tokio::test]
    async fn plan_mode_keeps_the_workspace_read_only() {
        let dir = workspace("plan");
        let plans = dir.join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let plan = plans.join("plan.md");
        std::fs::write(&plan, "draft").unwrap();

        let sandbox = Sandbox::new(&dir)
            .expect("landlock should be available")
            .writes_limited_to(&plan);
        let refused = sandbox
            .command("echo nope > elsewhere.txt")
            .output()
            .await
            .unwrap();
        assert!(!refused.status.success(), "the workspace must be read-only");
        assert!(!dir.join("elsewhere.txt").exists());

        let allowed = sandbox
            .command(&format!("echo written > {}", plan.display()))
            .output()
            .await
            .unwrap();
        assert!(
            allowed.status.success(),
            "the plan file must stay writable: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
