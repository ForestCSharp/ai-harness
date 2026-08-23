//! Working out what this project's check command is.
//!
//! `--check` gives the harness something to run after a turn that wrote files
//! ([`crate::app::App::should_check`]). A flag nobody passes is a feature nobody
//! has, though, and the default it leaves behind — a turn ending on the model's
//! word — is exactly what the flag exists to stop. So where a project's check is
//! **inferable**, it is inferred, and the flag becomes the override rather than
//! the switch.
//!
//! ```text
//! Cargo.toml at the root   →  cargo check --all-targets
//! go.mod                   →  go build ./...
//! package.json script      →  npm run typecheck | npm run check
//! tsconfig.json            →  npx --no-install tsc --noEmit
//! Ruff configured          →  ruff check .
//! ```
//!
//! **Conservative on purpose.** A default that guesses wrong is worse than no
//! default at all: it produces confident failures about code that is fine,
//! teaches the model to distrust the check, and costs a command run every time.
//! So this recognises a handful of unambiguous markers and returns `None` for
//! everything else — an unverified project is an honest outcome, a
//! wrongly-verified one is not.
//!
//! Two things are deliberately **not** inferred. A `build` script produces
//! artifacts, and a `test` script is usually slow; the check is paid on every
//! writing turn, so both are the wrong trade to make on someone's behalf. They
//! are still perfectly good values for `--check` when a person chooses them.

use std::path::Path;

/// What to run for a project we recognise, or `None`.
///
/// First match in the order written wins, which matters for a repository
/// carrying more than one marker — a Rust workspace with a JS front end gets
/// the Rust check, because the ordering is what decides and an arbitrary one
/// would be worse than a stated one.
pub fn detect(root: &Path) -> Option<String> {
    if root.join("Cargo.toml").is_file() {
        return Some("cargo check --all-targets".to_string());
    }
    if root.join("go.mod").is_file() {
        return Some("go build ./...".to_string());
    }
    // A `package.json` script comes before `tsconfig.json` deliberately: a
    // project that named its own check has said what it wants run, and guessing
    // past that would override an answer rather than supply a missing one.
    if let Some(script) = npm_script(root) {
        return Some(script);
    }
    if root.join("tsconfig.json").is_file() {
        // `--noEmit` is the whole reason this clears the bar: it is a typecheck
        // with no artifacts. `--no-install` so a missing toolchain fails
        // immediately instead of silently downloading one mid-turn.
        return Some("npx --no-install tsc --noEmit".to_string());
    }
    ruff(root)
}

/// Files that mean this project has chosen Ruff.
///
/// Python has no universal check the way Cargo and Go do — `python -m
/// compileall` writes bytecode, a test suite is slow, and neither is safe to
/// pick on someone's behalf. A Ruff configuration is different: it is the
/// project saying which linter it runs, and Ruff is fast enough to pay for on
/// every writing turn. So Python is inferred exactly when it has been named,
/// and left alone otherwise.
const RUFF_CONFIGS: &[&str] = &["ruff.toml", ".ruff.toml"];

fn ruff(root: &Path) -> Option<String> {
    let configured = RUFF_CONFIGS.iter().any(|name| root.join(name).is_file())
        || pyproject_names_ruff(root).unwrap_or(false);
    configured.then(|| "ruff check .".to_string())
}

/// Whether `pyproject.toml` carries a `[tool.ruff]` section.
///
/// Matched as text rather than parsed: there is no TOML parser in this binary,
/// adding one to answer a yes/no question would be the tail wagging the dog, and
/// the section header is unambiguous enough that a substring is the honest
/// implementation. Bounded like [`npm_script`], for the same reason.
fn pyproject_names_ruff(root: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(root.join("pyproject.toml")).ok()?;
    if text.len() > MAX_PACKAGE_JSON {
        return None;
    }
    Some(
        text.lines()
            .map(str::trim)
            .any(|line| line == "[tool.ruff]" || line.starts_with("[tool.ruff.")),
    )
}

/// The scripts a `package.json` may offer, in the order they are preferred.
///
/// Both are conventionally fast and side-effect free, which is the whole bar for
/// something run automatically. Nothing else in a `package.json` clears it.
const NPM_SCRIPTS: &[&str] = &["typecheck", "check"];

/// How much of a `package.json` is read looking for its scripts.
///
/// Bounded like [`crate::memory::description_of`] and [`crate::jobs::head`]: a
/// `package.json` is normally small, but nothing here should be the reason a
/// repository with a pathological one hangs at startup.
const MAX_PACKAGE_JSON: usize = 256 * 1024;

fn npm_script(root: &Path) -> Option<String> {
    let path = root.join("package.json");
    let text = std::fs::read_to_string(&path).ok()?;
    if text.len() > MAX_PACKAGE_JSON {
        return None;
    }
    // A `package.json` that does not parse is not a project we understand well
    // enough to run commands in.
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = parsed.get("scripts")?.as_object()?;
    NPM_SCRIPTS
        .iter()
        .find(|name| scripts.contains_key(**name))
        .map(|name| format!("npm run {name}"))
}

/// The check this session will run, given the two flags and the project.
///
/// Precedence, highest first: `--no-check` turns it off whatever else is set;
/// `--check` is used exactly as written; otherwise the project decides; and a
/// project we do not recognise has none.
///
/// Kept here rather than in `main` so the ordering is testable without building
/// an `App`, and so there is one place to read when a check turns out not to be
/// the one someone expected.
pub fn resolve(root: &Path, flag: Option<&str>, disabled: bool) -> Option<String> {
    if disabled {
        return None;
    }
    match flag {
        // An empty `--check ""` reads as "none" rather than as a command that
        // would fail on every turn.
        Some(command) if !command.trim().is_empty() => Some(command.trim().to_string()),
        Some(_) => None,
        None => detect(root),
    }
}

/// What to tell the user at startup about how their turns will end.
///
/// Said out loud, and said in **both** cases. An unverified turn currently looks
/// exactly like a verified one — which is why answering "did we actually check?"
/// for a past session meant reading its `session.json` by hand — and the cheapest
/// fix for that is to name the state once, at the moment somebody could still do
/// something about it. The auto-approve notice earns its place on the same
/// argument.
pub fn startup_notice(check: Option<&str>) -> String {
    match check {
        Some(command) => format!(
            "Project check: `{command}` — it runs after any turn that writes a \
             file, and a failure goes back to the model. --check changes it, \
             --no-check turns it off."
        ),
        None => "No project check: turns that write files will end unverified. \
                 Set --check '<command>' to have one run automatically."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let unique = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ai-harness-check-detect-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        dir
    }

    #[test]
    fn a_cargo_project_type_checks() {
        let root = temp_root("cargo", &[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        assert_eq!(detect(&root).as_deref(), Some("cargo check --all-targets"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_go_module_builds() {
        let root = temp_root("go", &[("go.mod", "module x\n")]);
        assert_eq!(detect(&root).as_deref(), Some("go build ./..."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_json_uses_its_typecheck_or_check_script() {
        let typecheck = temp_root(
            "npm-tc",
            &[(
                "package.json",
                r#"{"scripts": {"typecheck": "tsc --noEmit", "check": "x"}}"#,
            )],
        );
        assert_eq!(
            detect(&typecheck).as_deref(),
            Some("npm run typecheck"),
            "typecheck is preferred over check"
        );

        let check = temp_root(
            "npm-check",
            &[("package.json", r#"{"scripts": {"check": "biome check"}}"#)],
        );
        assert_eq!(detect(&check).as_deref(), Some("npm run check"));
        let _ = std::fs::remove_dir_all(&typecheck);
        let _ = std::fs::remove_dir_all(&check);
    }

    /// A `build` produces artifacts and a `test` is usually slow. Both are fine
    /// things for a person to choose and bad things to choose for them, since
    /// the cost lands on every writing turn.
    #[test]
    fn build_and_test_scripts_are_not_inferred() {
        let root = temp_root(
            "npm-build",
            &[(
                "package.json",
                r#"{"scripts": {"build": "vite build", "test": "vitest run"}}"#,
            )],
        );
        assert_eq!(detect(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_we_do_not_recognise_gets_no_check() {
        let bare = temp_root("bare", &[("README.md", "# hi")]);
        assert_eq!(detect(&bare), None);

        // Malformed, and a `package.json` with no scripts at all.
        let broken = temp_root("broken", &[("package.json", "{ not json")]);
        assert_eq!(detect(&broken), None);
        let empty = temp_root("noscripts", &[("package.json", r#"{"name": "x"}"#)]);
        assert_eq!(detect(&empty), None);

        for dir in [bare, broken, empty] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A repository can carry several markers. Which one wins is decided by the
    /// order in `detect`, and it is pinned here so it stays a decision.
    #[test]
    fn the_first_marker_in_order_wins() {
        let root = temp_root(
            "both",
            &[
                ("Cargo.toml", "[package]\nname = \"x\"\n"),
                ("go.mod", "module x\n"),
                ("package.json", r#"{"scripts": {"typecheck": "tsc"}}"#),
            ],
        );
        assert_eq!(detect(&root).as_deref(), Some("cargo check --all-targets"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_flags_beat_the_project_and_no_check_beats_everything() {
        let root = temp_root("resolve", &[("Cargo.toml", "[package]\nname = \"x\"\n")]);

        // Nothing given: the project decides.
        assert_eq!(
            resolve(&root, None, false).as_deref(),
            Some("cargo check --all-targets")
        );
        // An explicit command is used verbatim.
        assert_eq!(
            resolve(&root, Some("just lint"), false).as_deref(),
            Some("just lint")
        );
        // And --no-check wins over both.
        assert_eq!(resolve(&root, None, true), None);
        assert_eq!(resolve(&root, Some("just lint"), true), None);
        // An empty command means none, not a command that fails every turn.
        assert_eq!(resolve(&root, Some("   "), false), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The fixtures above are all one-file directories. This points the same
    /// function at a real repository — this one — because "recognises a project"
    /// is the claim being made, and a table that only works on fixtures would
    /// pass every test above and still do nothing on the day it matters.
    #[test]
    fn it_recognises_this_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            detect(root).as_deref(),
            Some("cargo check --all-targets"),
            "the harness should check itself by default"
        );
    }

    /// Both branches say something. Silence in the `None` case is the bug this
    /// notice exists to close.
    #[test]
    fn the_startup_notice_names_the_command_or_says_there_is_none() {
        let configured = startup_notice(Some("cargo check"));
        assert!(configured.contains("cargo check"), "{configured}");
        assert!(configured.contains("--no-check"), "{configured}");

        let absent = startup_notice(None);
        assert!(absent.contains("unverified"), "{absent}");
        assert!(absent.contains("--check"), "{absent}");
    }

    /// A project that named its own check has answered the question; inferring
    /// past it would override the answer rather than supply a missing one.
    #[test]
    fn a_package_json_script_beats_tsconfig() {
        let dir = tempdir("script-beats-tsconfig");
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"typecheck":"tsc -p ."}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(detect(&dir).as_deref(), Some("npm run typecheck"));
        clean(&dir);
    }

    #[test]
    fn tsconfig_alone_is_a_typecheck() {
        let dir = tempdir("tsconfig-alone");
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            detect(&dir).as_deref(),
            Some("npx --no-install tsc --noEmit")
        );
        clean(&dir);
    }

    #[test]
    fn ruff_is_inferred_from_its_own_config() {
        let dir = tempdir("ruff-toml");
        std::fs::write(dir.join("ruff.toml"), "line-length = 100\n").unwrap();
        assert_eq!(detect(&dir).as_deref(), Some("ruff check ."));
        clean(&dir);
    }

    #[test]
    fn ruff_is_inferred_from_a_pyproject_section() {
        let dir = tempdir("ruff-pyproject");
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"x\"\n\n[tool.ruff.lint]\nselect = [\"E\"]\n",
        )
        .unwrap();
        assert_eq!(detect(&dir).as_deref(), Some("ruff check ."));
        clean(&dir);
    }

    /// The conservative half of the rule, and the one worth protecting: a Python
    /// project that has not named a linter gets no check rather than a guessed
    /// one. `--check` is how someone chooses for it.
    #[test]
    fn python_without_a_named_linter_is_left_alone() {
        let dir = tempdir("bare-python");
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("setup.py"), "").unwrap();
        assert_eq!(detect(&dir), None, "a guess here would be worse than none");
        clean(&dir);
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-harness-check-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn clean(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
