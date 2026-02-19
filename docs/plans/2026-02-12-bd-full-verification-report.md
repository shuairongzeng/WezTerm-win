# BD Full Verification Report (2026-02-12)

## Scope
Validated all tracked issue domains in `bd` using the same method:
1. root-cause style review
2. failing/targeted tests where feasible
3. minimal fix
4. regression/build verification
5. runtime smoke checks

## Issue Matrix

- `wezterm-ysa` (IN_PROGRESS)
  - Status: Verified by automated checks; two additional hidden bugs fixed during verification.
  - New fixes added:
    - pending-input cleanup when pane missing (`wezterm-gui/src/termwindow/mod.rs`)
    - PaneRemoved runtime-state cleanup (`wezterm-gui/src/termwindow/mod.rs`)
  - Evidence:
    - `cargo test -p wezterm-gui --bin wezterm-gui pending_input_cleanup -- --nocapture` (4 passed)

- `wezterm-xka` / `wezterm-ntj` (TCP keepalive/socketpair)
  - Status: Automated regression verified.
  - Evidence:
    - `cargo test -p filedescriptor --lib -- --nocapture` (1 passed: socketpair)

- `wezterm-bgu` (CLI socket path discovery)
  - Status: Security + path reconstruction tests verified.
  - Added tests:
    - shared memory path length boundary
    - parent traversal rejection (`..`)
    - relative path reconstruction via `RUNTIME_DIR`
  - Evidence:
    - `cargo test -p wezterm-client --lib -- --nocapture` (5 passed)
    - `target/release/wezterm.exe cli list` succeeds

- `wezterm-wum` (listener recovery)
  - Status: Error-classification behavior now unit-tested.
  - Added tests:
    - windows error-code classification matrix (retry/rebuild)
  - Evidence:
    - `cargo test -p wezterm-mux-server-impl --lib -- --nocapture` (1 passed)

- `wezterm-9qa` (parser recovery)
  - Status: Covered by crate regression and workspace build; no new failure observed.
  - Evidence:
    - `cargo test -p mux --lib -- --nocapture` (4 passed)

- `wezterm-ilp` / `wezterm-4a4` / `wezterm-57p` (input queue + stability bundle)
  - Status: Verified by targeted termwindow tests + runtime CLI stress smoke.
  - Evidence:
    - `cargo test -p wezterm-gui --bin wezterm-gui pending_input_cleanup -- --nocapture`
    - runtime smoke: spawn pane + 120x `cli send-text` + `get-text` + kill-pane

- `wezterm-y8z` (build script)
  - Status: Found and fixed blocking script parser bug; build script now validated.
  - Fix:
    - Replaced broken `build-wezterm.ps1` with clean, parseable script.
  - Evidence:
    - `powershell -ExecutionPolicy Bypass -File .\build-wezterm.ps1 -Help`
    - `powershell -ExecutionPolicy Bypass -File .\build-wezterm.ps1` (release build success)

- `wezterm-vt9` (stability test report)
  - Status: Extended with new verification run (this report + bd comments).

## Build/Binary Validation

- Release build artifacts produced by script:
  - `target/release/wezterm.exe`
  - `target/release/wezterm-gui.exe`
  - `target/release/wezterm-mux-server.exe`
- Version/boot smoke:
  - `target/release/wezterm.exe -V`
  - `target/release/wezterm-mux-server.exe -V`
  - `target/release/wezterm-gui.exe -V`

## Remaining Manual (Interactive) Checks

The only items not fully automatable in this headless CLI workflow are pure GUI interaction scenarios:
- mouse behavior in vim/tmux under heavy output
- real clipboard paste under GUI focus transitions
- crash-recovery behavior with intentional GUI process kill while external clients reconnect

These should be executed as manual QA on Windows desktop session and appended back into `bd` as evidence.
