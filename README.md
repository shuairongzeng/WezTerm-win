# Wez's Terminal

<img height="128" alt="WezTerm Icon" src="https://raw.githubusercontent.com/wezterm/wezterm/main/assets/icon/wezterm-icon.svg" align="left"> *A GPU-accelerated cross-platform terminal emulator and multiplexer written by <a href="https://github.com/wez">@wez</a> and implemented in <a href="https://www.rust-lang.org/">Rust</a>*

User facing docs and guide at: https://wezterm.org/

![Screenshot](docs/screenshots/two.png)

*Screenshot of wezterm on macOS, running vim*

## ⚡ Fork Notice: Windows Stability Improvements

This is a fork of WezTerm with **critical Windows stability fixes**, especially for heavy terminal output scenarios (e.g., running AI coding assistants like Claude Code, Codex, etc.).

### Key Fixes in This Fork

| Issue | Fix |
|-------|-----|
| **Input blocking during heavy output** | Added input priority mechanism with `AtomicBool` flag. When keyboard input is pending, `perform_actions` yields immediately to prevent input starvation. |
| **PTY connection errors (10053/10054)** | Added retry mechanism for transient socket errors. Panes no longer crash on temporary connection issues. |
| **Windows message pump freeze** | Added 500ms stuck detection with automatic recovery. UI no longer freezes during heavy rendering. |
| **Terminal lock contention** | Reduced batch size and added periodic yields during output processing. |

### Download

Download pre-built Windows binaries from [Releases](https://github.com/shuairongzeng/WezTerm-win/releases).

### Why This Fork?

The upstream WezTerm has issues on Windows where:
- Keyboard input becomes unresponsive when there's heavy terminal output
- Panes can freeze or crash with socket error 10053
- The UI may become unresponsive during rapid output

This fork has been tested with **90+ minutes of continuous AI output** without any input blocking issues.

---

## Installation

https://wezterm.org/installation

## Getting help

This is a spare time project, so please bear with me.  There are a couple of channels for support:

* You can use the [GitHub issue tracker](https://github.com/wezterm/wezterm/issues) to see if someone else has a similar issue, or to file a new one.
* Start or join a thread in our [GitHub Discussions](https://github.com/wezterm/wezterm/discussions); if you have general
  questions or want to chat with other wezterm users, you're welcome here!
* There is a [Matrix room via Element.io](https://app.element.io/#/room/#wezterm:matrix.org)
  for (potentially!) real time discussions.

The GitHub Discussions and Element/Gitter rooms are better suited for questions
than bug reports, but don't be afraid to use whichever you are most comfortable
using and we'll work it out.

## Supporting the Project

If you use and like WezTerm, please consider sponsoring it: your support helps
to cover the fees required to maintain the project and to validate the time
spent working on it!

[Read more about sponsoring](https://wezterm.org/sponsor.html).

* [![Sponsor WezTerm](https://img.shields.io/github/sponsors/wez?label=Sponsor%20WezTerm&logo=github&style=for-the-badge)](https://github.com/sponsors/wez)
* [Patreon](https://patreon.com/WezFurlong)
* [Ko-Fi](https://ko-fi.com/wezfurlong)
* [Liberapay](https://liberapay.com/wez)
