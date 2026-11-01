
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

-   ~~**Read-only subagents.**~~ **Done**, as `<ai-harness-agent>`, and the entry
    was right about where the plumbing already was: `Slot` needed one field
    (`parent: Option<u64>`) and no new channel or `Update` variant at all.

    Two things it did not anticipate. The restriction is enforced at **dispatch**
    — the arm plan mode uses — and it has to include `<ai-harness-option>`, which
    mutates nothing: there is no user in a child conversation to answer a
    question, so a child that asked one would wait forever. And the slot list
    *is* the UI list, derived fresh every frame, so hiding a child meant filtering
    five separate readers. The one that would have hurt is `open_names`: it feeds
    the on-disk open set, so a leak there reopens the subagent **as a session** on
    the next launch.

    The argument for them also needs sharpening. Compaction's mechanical pass
    already stubs a 64 KB read result to ~220 bytes, so file contents were never
    the expensive part. What a subagent actually saves is *reasoning*: fifteen
    replies that survive the mechanical pass byte-for-byte and are compressed only
    by the lossy model pass.

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

---

# Done since

-   **`/evaluate` closes the improve loop**, which this file's own framing
    against the harness-engineering post said was a gap: the plan → execute →
    observe → improve cycle stopped at observe. Everything needed for the last
    step already existed and was only ever shown to a *person* — `/stats` draws
    the action counts, `/cost` the ledger, and `Entry::Malformed` has held every
    protocol failure with its parser reason all along.

    It is a composed prompt rather than a mode, which is what kept it small:
    `/plan <task>` already proved a command can hand text to `send_prompt`, so
    there is no new `App` state, no contract section, no sandbox change and
    nothing to clear in `cancel()`. `/compact`'s out-of-band path was the
    alternative and would have cost a new `Update` variant, a `pump_*`/`spawn_*`
    pair and a new `Status` — that shape exists because a summary must bypass
    the protocol parser, which is the opposite of what is wanted here.

    Reused rather than rebuilt: `stats::actions`, the ledger, and
    `headless::protocol_errors`, which turned out to be pure over the transcript
    and merely private. One guard worth keeping: the prompt reports **counts,
    not ratios**, because of the `lookup_ratio` direction problem below.

    Open: nobody has judged the output yet on a real session. The quality of
    this feature is the quality of its wording, so
    `cargo test show_the_evaluation_prompt -- --ignored --nocapture` prints the
    prompt for reading. If the reports come back vague, the fix is there and not
    in more machinery.

-   **Batched non-mutating actions**, as `<ai-harness-batch>`. The invariant held
    as this entry required: children are exactly the four auto-approved actions,
    and the relaxation reaches no other element.

    The parser was the cheap half — an extraction of `text_action` out of
    `parse_reply` so a batched read and a lone read cannot drift, plus a child
    loop modelled on `parse_option_children`. The real cost was the dispatch, and
    not where expected: `pending_fetch` and `pending_search` are single-slot
    `Option`s and `main`'s ladder takes exactly one parked thing per pass, so the
    children are drained **sequentially** through a `VecDeque`. That is not a
    compromise — `lookup_ratio` counts round-trips, not I/O, so draining in order
    collects the whole saving while leaving all three existing spawners untouched.

    One consequence worth keeping in view: a batch is one reply, so `iterations`
    no longer bounds filesystem work. `max_turn_bytes` is the only guard left,
    and it had to start being checked *between* children rather than before the
    action — it cannot stop a batch it has already waved through.

-   **The project check can run per write**, as `--check-each-write`, off by
    default. The turn-end trigger was already built; this moves it to the write
    so a broken edit is caught at the edit. `finish_check` had to learn which
    trigger fired, because a passing check mid-turn means *carry on* where a
    passing check at the end means *stop*.

    Per-file scoping was investigated and abandoned: no inferred command takes a
    path, `cargo check` and `go build ./...` have no single-file mode, and
    `tsc --noEmit` on one file ignores `tsconfig.json`.

## Open question this raised

`bench/metrics.py` pre-registers `lookup_ratio = lookups / iterations` as the
PRIMARY metric, described as "the share of round-trips spent gathering" and
"exactly what a batch element changes". It does change it — but **upward**.
Batching holds the lookup count and cuts `iterations`, so four reads go from
4/4 = 1.00 to 4/1 = 4.00. The baseline mean of 0.51 cannot be beaten downward
by the feature it was registered for, and the GATE (`min >= 0.5`) becomes
trivially satisfiable.

Deliberately not changed here. That file states that the metrics are fixed in it
"so a later run cannot quietly become a different experiment by picking a kinder
number", and rewriting the primary metric in the same commit as the feature it
judges is exactly that. `iterations_mean` is already computed and printed, moves
in the right direction, and is the honest headline; deciding that is a call for
whoever owns the registration.

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
