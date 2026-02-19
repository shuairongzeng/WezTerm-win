# Input Queue Missing Pane Recovery Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Prevent stale pending input from causing repeated timer retries after a pane has already been removed from mux.

**Architecture:** Introduce an explicit cleanup path in `flush_pending_input` for missing panes, so queued input is dropped deterministically instead of remaining in `pane_state`. Add small helper utilities to avoid implicit `pane_state` insertion when only checking pending status. Cover the cleanup behavior with focused unit tests before implementation and validate with targeted crate tests.

**Tech Stack:** Rust, `wezterm-gui` (`termwindow`), `cargo test`, `cargo check`

---

### Task 1: Add failing tests for pending-input cleanup

**Files:**
- Modify: `wezterm-gui/src/termwindow/mod.rs`
- Test: `wezterm-gui/src/termwindow/mod.rs` (new `#[cfg(test)]` block)

**Step 1: Write the failing tests**

Add tests that assert a missing-pane cleanup helper:
- clears queued `InputOp` entries for an existing pane state
- returns `0` and keeps map unchanged when pane state is absent

**Step 2: Run test to verify it fails**

Run: `cargo test -p wezterm-gui --lib pending_input_cleanup -- --nocapture`
Expected: FAIL (helper function not found / behavior not implemented)

**Step 3: Commit checkpoint**

Do not commit yet; continue to Task 2 in same working change.

### Task 2: Implement missing-pane queue cleanup in flush path

**Files:**
- Modify: `wezterm-gui/src/termwindow/mod.rs`

**Step 1: Add minimal helper implementation**

Implement helper that takes `&mut HashMap<PaneId, PaneState>` and a `PaneId`, clears `pending_input`, and returns dropped count.

**Step 2: Integrate helper into runtime path**

Update `flush_pending_input`:
- if `Mux::get().get_pane(pane_id)` is `None`, clear pending queue for that pane and return
- log dropped operation count for observability

Update timer retry checks:
- replace `pane_state(pane_id)` lookup used only for `still_pending` checks with a non-inserting read helper

**Step 3: Run targeted tests to verify pass**

Run: `cargo test -p wezterm-gui --lib pending_input_cleanup -- --nocapture`
Expected: PASS

### Task 3: Validate no regressions in impacted crates

**Files:**
- Modify: `wezterm-gui/src/termwindow/mod.rs` (already listed)

**Step 1: Build/check changed crates**

Run: `cargo check -p wezterm-gui -p mux -p wezterm-client -p wezterm-mux-server-impl`
Expected: PASS (existing unrelated warnings acceptable)

**Step 2: Summarize verification evidence**

Capture command outputs and key assertions:
- unit test proves pending queue cleanup for missing pane state path
- crate checks still pass

**Step 3: Session landing**

After code is stable, run `bd sync` and update issue state according to verification outcome.
