
# From the harness-engineering post

Untriaged — read against Lilian Weng's ["Harness Engineering"][post], which sorts
harness design into three patterns: a closed plan → execute → observe → improve
loop, the file system as persistent memory, and subagents with background jobs
whose logs live as files. We are ahead of the post on the second one; the memory
index in `src/memory.rs` is a more careful design than "put things in files".
The other two are where the gaps are. Nothing here is ordered against the queue
below.

Two are **done**: background jobs (`<ai-harness-shell background=true>`,
`src/jobs.rs`) and the verification step (`--check`, `App::should_check`). Both
have a section under step 4 of `docs/request-lifecycle.md`.

[post]: https://lilianweng.github.io/posts/2026-07-04-harness/

-   **Read-only subagents.** `src/sessions.rs` already holds a `Slot` per running
    conversation sharing one channel, which is most of the plumbing. The argument
    for them is context isolation: compaction is currently the only defence
    against growth and it is lossy for the whole conversation, where a subagent
    spends its own context on a search and returns a paragraph. Restrict the
    first version to read/grep/glob/fetch. A subagent that could write or shell
    would break "one mutating action, one approval", and confining it to the
    actions that never reach the modal preserves that invariant structurally
    rather than by discipline.

-   **Tell the model its own history exists.** The post counts error traces and
    past trajectories as things that belong on disk. `compaction-NNN.json` and
    the checkpoints are both recovery artifacts for the user — they sit under the
    root and are readable, but nothing says so. One line in the contract pointing
    at the session folder would make prior-turn history reachable for near-zero
    standing cost. Note that it would have to name paths: `.ai_harness/` is in
    `search::SKIP_DIRS`, so a read gets in and a grep does not — the same
    asymmetry the jobs section had to spell out.

-   **Notes are write-only.** Eviction in `memory::within` is least-recently-
    modified first, and nothing ever revises or merges a note, so a long project
    accumulates stale ones that keep costing index lines. A `/memory`
    consolidation pass — the model reads some notes and proposes merges and
    deletions through the ordinary approval path — is the post's "update the
    playbook by delta rather than appending" in the shape we already have.

-   ~~**Keep the protocol failures we already compute.**~~ **Done**, and more
    cheaply than this entry assumed: `roll_back_retries` prunes only the
    *model's* copy of the conversation, so `Entry::Malformed` was never actually
    thrown away — the transcript had every attempt all along. `headless.rs`
    derives the tally from it at the end of a run, grouped by parser reason, and
    the smoke job in `bench.yml` fails a commit that introduces one on a trivial
    prompt.

    The dismissal in the second half of this entry needs revising too. The
    population-search literature it waves off (ADAS, AlphaEvolve, DGM) is not
    the only shape this comes in: Claw-SWE-Bench measured a **12.5–27.4 point**
    Pass@1 spread from varying the harness alone on a fixed model, against 29.4
    points across nine models on a fixed harness — the scaffold is a
    model-tier-sized variable — and it did so with a single lineage and no
    population machinery at all. The evaluator this project "has no reason to
    build" is now three published ones it can be plugged into; see
    `bench/README.md`.

Two things the post argues for that are already here, worth not regressing: the
approval modal and the `iterations` cap are exactly its "permission controls
outside the loop", and its bottlenecks on reward hacking and on human oversight
at the right abstraction level are the standing reason `--auto-approve` should
stay a deliberate choice.

---

# DO IN ORDER

-   code indexing support, also stored in .ai_harness/

-   more organized context management. Don't just keep appending, but store in a structured manner.

-   Batched non-mutating actions: one top-level batch element whose children are
    read/fetch/grep only. Five reads is currently five round-trips, each re-sending
    the entire conversation. Keeps the invariant that matters — one mutating action,
    one approval — and the relaxation must not reach any other element, since strict
    whole-reply parsing is the point of the protocol.

---

# Future work

## Cross-platform sandbox support

**Linux is done, via Landlock rather than the bubblewrap this entry used to
recommend.** `src/sandbox.rs` now has two backends behind the same two choke
points (`command`, `program`): Seatbelt on macOS, Landlock on Linux. Everything
else — `root`, `denies_read`, `writes_limited_to`, the path constants — was
already platform-independent, so no trait or enum was needed; the seam this
entry proposed building turned out to already exist.

Why Landlock beat the recommendation here:

-   It is a policy a process applies to **itself** before exec, which is the same
    shape as Seatbelt. Bubblewrap re-parents the process under a helper binary,
    which is a different architecture wearing the same hat.
-   No binary to install, and no user namespaces — which are restricted in
    exactly the container and CI environments the benchmarks run in.
-   Verified unprivileged inside a container at ABI 8.

**The policy inverts, and comes out stronger.** Seatbelt is `(allow default)`
plus a credential denylist the README admits is "not exhaustive". Landlock is
allowlist-only and cannot express that, so the Linux backend grants read on the
system hierarchies, the workspace and the build caches, and grants nothing else
under `$HOME` — which excludes `~/.ssh`, `~/.aws` and everything like them by
construction. `sandbox::landlock_tests` proves it against the kernel rather than
against a path check.

Two things worth knowing:

-   **Landlock is absent under x86_64 emulation on Apple Silicon** (`ENOSYS`;
    ABI 8 natively on arm64). SWE-bench images are x86_64, so `--sandbox=none`
    remains necessary for benchmark containers on a Mac. It is not a stopgap for
    Linux any more — it is for the emulated case.
-   **Plan mode is slightly wider on Linux.** Landlock grants access to paths
    that exist, so a plan file not yet written falls back to its directory where
    Seatbelt gets a single `(literal …)`. Still far smaller than the workspace,
    and the only place the two backends genuinely differ.

### Windows

Still open, and still the shape this entry originally described: **Job Objects**
with `CreateRestrictedToken` — CPU/memory caps, kill-on-close, no breakaway.
Filesystem and network isolation are weaker than either backend above, and that
should be documented honestly rather than papered over. Nothing here is blocked
on it; `Sandbox::new` fails closed on any platform without a backend, which is
the correct behaviour until one exists.

The crate survey that used to live here (nanosandbox, openclaw-rs, gaol) is
retired: two backends are written, and neither needed a dependency beyond the
`landlock` crate itself.
