
# DO IN ORDER

[x] `<ai-harness-grep>` / `<ai-harness-glob>` — auto-approved, non-mutating search.
    Reads earned auto-approval because looking at four files should not interrupt
    you four times; finding those four files still costs a modal via shell `rg`.
    Confine exactly as `src/files.rs` confines a read: resolve inside the root,
    share the same denylist, cap the result size.
    Done in `src/search.rs`. Two follow-ups fell out of it:
      - Replace the hardcoded skip list with `.gitignore` parsing.
      - A quote-aware attribute-name tokenizer plus an `allowed_attrs(tag)` table,
        replacing the `takes_attrs` gate in `parse_reply`. Today an unknown
        attribute alongside a recognised one is silently ignored — `<grep dir=src
        case=yes>` is accepted — which predates this change but has more surface
        now that two more elements take attributes.

[ ] Context compaction. The whole conversation is resent every turn and `/clear`
    is the only escape, so a long session eventually dies against the model's
    limit. Both numbers are already there — `context_length` from the catalog
    (`src/openrouter.rs`) and the live context size in the status bar. Auto-compact
    at ~80%: summarise the old prefix into a harness→model block, keep recent turns
    verbatim, preserve the plan and any pending action. Add `/compact` to force it.

[ ] Load `AGENTS.md` from the project root at startup, appended to the contract in
    the same slot `--system` uses (never replacing it). `--system` is per-launch
    only; nothing picks up per-project conventions today.

[ ] Permission rules, replacing binary auto-approve. Add a third modal button —
    "Allow, and always allow this" — persisting a command prefix or path subtree to
    `.ai_harness/permissions.json`. Gets the twenty-step refactor without twenty
    keypresses, while still stopping on anything never approved.

[ ] Checkpoints and `/undo`. The sandbox root is what commands are confined *to*,
    so an auto-approved `rm -rf .` is inside the boundary. Snapshot touched files
    into `.ai_harness/checkpoints/<turn>/` before each approved write or edit and
    restore from there. Cheaper than git integration and works on a dirty tree.

[ ] Prompt cache breakpoints. The client sends no cache directives of its own, so
    for models where caching is opt-in via `cache_control` the 0% in `/cost` is the
    expected result rather than a provider quirk. Put a breakpoint after the system
    prompt and one at the last stable turn boundary.

[ ] Batched non-mutating actions: one top-level batch element whose children are
    read/fetch/grep only. Five reads is currently five round-trips, each re-sending
    the entire conversation. Keeps the invariant that matters — one mutating action,
    one approval — and the relaxation must not reach any other element, since strict
    whole-reply parsing is the point of the protocol.

[ ] Show reasoning deltas. `src/openrouter.rs` deliberately drops them, which is
    right for the parser, but a reasoning model leaves you watching `thinking…` for
    text the API is already streaming. Render dimmed; never parse it, never feed it
    back.

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
