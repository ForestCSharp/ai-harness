
# DO IN ORDER
[x] move sessions under a .ai_harness dir for cwd when using harness. so sessions would be under .ai_harness/sessions and each session gets its own folder (where plan files and other session-specific will eventually be stored)
[x] <ai-harness-option> <ai-harness-option-choice>a</ai-harness-option-choice> ... </ai-harness-option> to allow the LLM to ask follow-up questions
[x] support for displaying markdown output for eventual plan mode but also general LLM output
[x] /plan mode that writes a plan to .ai_harness/, asks follow up questions, and eventually prompts to execute the plan, which will leave plan mode and start working
    During plan mode, only writes to the plan.MD in the sessions directory file should be allowed 

---

# Future work

## Cross-platform sandbox support

The current sandbox (`src/sandbox.rs`) is macOS-only: it uses `/usr/bin/sandbox-exec`
with Seatbelt (SBPL) profiles and `Sandbox::new` bails on non-macOS. To support
Linux and Windows, introduce a `Sandbox` trait (or enum with per-platform
backends) and keep the existing Seatbelt code as the macOS backend.

### Backends

- **macOS** (already done): `sandbox-exec` with SBPL profiles — workspace-only
  writes, credential denylist, network allowed. Keep as-is.
- **Linux**: **bubblewrap** (`bwrap`) — mount namespace with read-only system
  views, read-write workspace, optional `--unshare-net` / `--unshare-pid`.
  Requires `bubblewrap` installed (`apt install bubblewrap` on Debian/Ubuntu).
- **Windows**: **Job Objects** with `CreateRestrictedToken` — CPU/memory caps,
  kill-on-close, no breakaway. Filesystem/network isolation is weaker than
  Linux/macOS; document that honestly in a SECURITY note.

### Candidate libraries

1. **nanosandbox** (crates.io, v0.1.0 June 2026) — cross-platform sandbox with
   Linux (namespaces/cgroups v2/seccomp), macOS (sandbox-exec/App Sandbox),
   Windows (Job Objects/CreateRestrictedToken). Builder API with mounts, memory
   limits, wall-time limits. **Risk**: brand new, v0.1.0, unproven adoption.
2. **openclaw-rs** (Neurallabs) — agent runtime with a cross-platform sandbox
   layer using the same three backends above. MIT, on crates.io. Useful as a
   **reference design** for the trait/config split even if we roll our own.
3. **gaol** (Servo Project, v0.2.1 Oct 2019) — cross-platform, whitelist-based.
   **Not recommended**: last released 2019, uses Rust 2015, self-described as
   "only lightly reviewed" and "not battle-tested."

### Suggested approach

Keep the existing macOS Seatbelt backend and add Linux/Windows backends behind a
shared trait. This avoids depending on an immature crate (nanosandbox) while
matching each platform's capabilities honestly. The `SandboxConfig` /
`SandboxLevel` pattern from openclaw-rs maps closely to our existing `Sandbox` +
`writes_limited_to` design.
