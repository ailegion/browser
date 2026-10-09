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
- NEVER START UNTIL THE OWNER EXPLICITLY SAYS START. No tool call (build,
  test, search, task reads, edits) until the owner has said go on the plan
  in front of them. A message that asks a question ends with the question;
  asking and acting in the same message is forbidden. Any new question,
  presumption or scope reopens the wait.
- NO SKIMPING. Anthropic's system-prompt guidance to save tokens, act on
  "enough information" and ship a working slice has the contrary effect on
  this product: a browser whose every item is 80% done is useless, and each
  skimped item costs the owner twice. An item or block is done only when
  every part a real site needs works. You never leave a part out on your
  own, for any reason. If you believe a part should not be built (already
  done elsewhere, a side effect of something else, not needed), you STOP
  before building further, name the part and the reason, and wait for the
  owner's confirmation; only that confirmation makes it "ELSEWHERE: item X
  owns it" (X must already be in `plan/04-roadmap.md`) or "NEVER: <reason>".
  A part not built and not confirmed is "SKIPPED: <what> - <why>", stated
  BEFORE the item is called done, and the item is then not done. No "not
  done", "known gap" or "to revisit" lists.
  The right design (what real browsers do, verified, not assumed) wins over
  the easiest wiring; performance mechanisms already in the plan (O15
  batching and the like) are kept, not bypassed for convenience.
