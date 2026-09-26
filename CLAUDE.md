# Project: pure-Rust web browser

Before doing anything in this repository, read `plan/00-README.md` and then
`plan/01-decisions.md`.

- Every decision in `plan/01-decisions.md` is settled with the owner. Do not
  re-ask it, do not propose alternatives, do not reopen it. If implementation
  proves one is not doable, raise the specific blocker.
- Crate rule: a dependency must be standalone, pure Rust, and fast. No C, no
  C++, no assembly, no `*-sys` crates that compile native code, no system
  WebView, no engine from another browser. Details and the ban list are in
  `plan/03-crates.md`.
- Isolation is by Rust memory safety, not OS sandboxing. Our crates use
  `#![forbid(unsafe_code)]` except `crates/platform`.
- No accounts, telemetry, sync, or any network request the user did not cause.
- Current phase and what to build next: `plan/04-roadmap.md`.
- Anything genuinely still open is in `plan/05-risks-and-open-items.md`;
  resolve it within the crate rule and record the result there.
- Do not run exhaustive crate compatibility checks up front. Check a specific
  crate fact only when a task hinges on it.
- Never run git. Not init, not add, not commit, not status. The owner does all
  git operations themselves. Do not ask about it and do not list it as a
  pending item.
- Work in small blocks. One roadmap item, or one crate, per turn: write it,
  build it, test it, report, stop. Do not chain several items into one long
  run; the owner wants to see each step land and the session must not hit
  its limit mid-work. Reading the plan is a block of its own.
