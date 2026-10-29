# Spill swap — implementation brief (delegated)

You are implementing the **log → spill** replacement for the LARA labeled store. Work **only** in this worktree
(`~/dev/gleaph-spill`). Do **not** touch `~/dev/gleaph`: it is the reviewer's checkout and must stay green
(a fresh `~/dev/gleaph` at the same commit is the reference for the original state).

## Read first, in this order

1. `design/adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md` — **the contract**. Every rule you
   need (ownership, shape, descriptor, lazy allocation, allocator classes, growth, operations, invariants, slot
   reuse, no-shift ordering, in-place tail growth) is there. If your work needs a rule the ADR does not state, stop
   and report instead of inventing one.
2. `design/implementation-gaps.md`, section **"Session handover index"**, then **"The spill design, in one place"**
   and **"File-by-file plan for the log-to-spill swap"** — the work list and the recommended slice order.
3. `AGENTS.md` (repository rules) and `.agents/skills/rust-workflow/SKILL.md` (validation commands and budgets).

## Scope

The swap is **one atomic change**: the log's memory region, the descriptor field's meaning, the insert producer, the
readers, the compaction paths, the batch reservation, the inline-property-bytes twin and the tests move together.
A half-applied swap cannot be green, so expect red mid-flight and do not try to keep the tree green between slices.

Slice order (from the work list): (1) layout/region and store construction, with a reopen test; (2) descriptor
`spill_run` for slab plus the insert producer (in-place tail growth first, then class growth); (3) reads and the
resident-content definition (the ordinal algebra in `slot_index.rs` must stay byte-for-byte identical — it is what
keeps readers compatible); (4) removes (the chain-splice logic disappears) and per-bucket compaction; (5) the batch
reservation and the values twin; (6) deletion of the log API and its tests, plus replaced tests/benches.

Start with `git switch -c spill-swap` in this worktree.

## Rules you must not break

- No `git stash`, no tree-wide state operations, no `cargo clean`, never `cargo fmt --all` (scoped `cargo fmt -p`
  only). Keep commits scoped; code and documentation in separate commits.
- Do not weaken a test to make an implementation pass. When a mechanism changes, re-derive the test **and say why**
  in the commit message (this session's examples: the publish owns the overflow-log fold; slack is a hint).
- The spill store's first two slices already exist and are tested (`spill_run_store.rs`, 7 tests). Extend it only
  where the ADR requires (level-2 hand-off if your slice needs it); do not redesign it.
- The invariants must hold end to end: one owner per row; a published width **and a spill length** must be backed;
  ordinals of existing rows never change; resident content is `prefix + spill`; **a spill run exists only because
  rows exist** (no pre-allocation for growth).

## Definition of done (report both)

1. `cargo test -p ic-stable-lara --lib` → 621 or more passed, 0 failed (the log-specific tests you delete may lower
   the count; new spill tests should more than replace them).
2. `cargo fmt -p ic-stable-lara` clean and `cargo clippy -p ic-stable-lara --lib` with no lib warnings.
3. The space audit from GAP-006 re-run (the ledger's audit note describes the probe): the log's ~696 KB of fixed
   capacity must read zero, and the report should say whether the previously measured +19 % on
   `bench_l_nt_bp_ins_*` disappeared (`canbench bench_l_nt_bp`, no `--persist`).

## Report format (keep it short)

Per slice: files changed, tests added/deleted with their names, the command output that proves the state, and any
decision you had to make that the ADR did not cover. At the end: the two gate results, the audit numbers, the
canbench numbers, and an explicit list of what you could not verify. The reviewer will re-run the gates and decide
the commits that land on `main`.

## Review loop discipline (added after a reviewer process failure)

The reviewer skipped one implementer report and sent a new directive while a slice was in flight.
The rule is now explicit, in both directions:

1. Every implementer report is read in full, its claims are **independently reproduced** by the
   reviewer (tests re-run in a throwaway worktree, diffs read, arithmetic checked), and the verdict
   plus the next instruction follow only after that.
2. A directive never silently supersedes an in-flight slice. If a stop is intended for one workstream
   only (for example "no wire width changes" while a conversion runs), the directive says which
   workstream it stops and which one continues.
3. Reports state the **current** branch head hash and, when the branch was rebased, that it was and why;
   the reviewer reviews by content diff, so stale hashes cost time.
4. Anything the reviewer has not reproduced is labelled "not verified by the reviewer" in the response,
   so the distinction between the implementer's evidence and the reviewer's evidence stays visible.

---

# REMAINING WORK (handover, written by the reviewer at branch head 941135b0b)

The previous implementer session ran out of turns. Everything below is scoped, decided and verified
by the reviewer; execute one MECHANISM per turn, commit it, report two lines (hash + kind delta),
then stop. Small commits are wanted. Evidence per turn: the kind delta of `cargo check -p
ic-stable-lara --lib` (census 160 at 941135b0b), and `cargo test -p ic-stable-lara --lib
labeled::spill_run_store` (21) in the isolated tree main + labeled/spill_run_store.rs + traits.rs.

Hard rules: no raw-u32 escape hatch (typed SpillRunId/ArenaRows only); no accessor, trait method,
enum variant or match arm left alive-and-dead; the row is the single source of truth for where a
bucket's rows live; deletion tombstones in place (ADR 0097 section 7).

## Mechanism A (next): BucketEdgeDeleteLocation's OverflowLog -> Spill type change (remove.rs)
- Enum at :28-33: OverflowLog { leaf, entry_idx, newer_entry_idx, moves } -> Spill { run, index }.
  A tombstone in place carries no successor and no moves, so the enum stops being able to express a
  chain splice on the delete path.
- Apply arm :700-713: tombstone in place, return empty moves; the head-publication step goes with it.
- removed_from_log: delete it if no caller reads it after that change (report the site if one does).
- Locate path :601-632; second locate loop :1371-1410 (which also computes moves and newer_entry_idx).
- Test helper chain read at :1790 becomes a run read.
- overflow_chain_slot_moves_after_delete is deleted in THIS commit with its last callers :613/:1395,
  unless a caller survives - then report that instead.

## Mechanism B: bucket.rs's nine sites.
## Mechanism C: access.rs's two sites.
## Mechanism D (last of item 2): delete the DeleteTarget::Log variant together with its match arm in
tombstone_overflow_log_delete_target (:303).

## Mechanism E (item 3): wrapper contract tests + store re-run
- spill_read_row rejecting index >= used_len; spill_append's class-full signal; spill_grow's single
  promotion outcome for class-cap and arena-limit exhaustion. Then re-run the store filter and report
  its count (21 expected; DOWN is a failure, new names are welcome).

## Mechanism F (T2b, the last conversion slice, may need splitting - say so first)
- Producer: insert_edge_inner's slab-vs-spill decision and insert_into_log_with_layout (:229) become
  insert_into_spill_with_layout: run = v.spill_run(); none -> allocate(class_rows(1)) + write row 0;
  else try_append_row, and on class-full either grow_to(run, used, used+1) (used < MAX_ROWS) or the
  promotion outcome; publish try_with_spill_run(new)?.try_grow_packed_slab_by_one().
- Promotion wiring: route the ArenaExhausted/RunIdOutOfRange outcome to the single mode-generic entry
  (rename promote_bypass_to_tree_mode -> promote_bucket_to_tree_mode, promote.rs:75), deleting the
  pre-promotion "fold a non-empty overflow log into the slab" step and the I2 drain guard it protected.
- Remaining labeled readers: compact.rs, traverse.rs, batch_write.rs, values.rs, invariants.rs, promote.rs.
- Acceptance for F: census 158-ish -> 0, full isolated suite green, fmt/clippy clean, and the G6
  byte-shape numbers measured (derived table: 149R/44W, 105/44, 105/0, 411R/72W) - a measured
  disagreement is reported, never absorbed into the test.

## After F
The reviewer runs the full independent verification (tests, fmt, clippy, space audit, canbench) and
decides the landing. SPILL_SWAP_BRIEF.md is deleted before landing.

## Follow-up surfaced at 55e1cbcc5 (do not lose)
remove.rs's by-directedness scan kept both arms deliberately: collapsing them would change
no-spill ordinal ordering, which cannot be verified mid-flight. The separate mechanism that
finishes it also touches that iterator's ordinal-order replay - do it AFTER F, when the lib is
green and the ordered-replay tests can actually run, and report the ordering contract it changes.

## F's deletion debt (named at 1446dc72f - these must be gone at F)
The fold/drain/segment-release choreography, with its remaining callers:
rebalance_edge_log_leaf_for_labeled, ensure_leaf_overflow_logs_drained, release_leaf_overflow_log,
fold_label_bucket_edges_to_slab, prepare_vertex_edge_span_for_overflow_log_fold.
Callers: compact.rs:1179,1284,2854,3018,3063,3069; insert.rs:657; values.rs:1998,2016;
traverse.rs:6852; plus test/bench sites. If deleting one at F leaves a caller with nothing to do,
say so there rather than keeping it alive.
