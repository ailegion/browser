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
- NO SKIP. SKIP = REJECT. A block is accepted only if every item in it
  behaves as Chrome and Firefox do in every case. Any deviation, however
  small, rare, or "edge", is a SKIP, and a block with a SKIP is REJECTED
  whole: not merged, not counted, redone from the report onward at your
  cost, not the owner's.
  - Choosing a design simpler than what browsers do IS the skip. It happens
    at the moment you choose it, not at the report. At that moment you
    stop, write "SKIPPED: <part> - <why>", and wait. No building past it.
  - Before the word "done" you print one line per item of the block, either
    "<item>: browser-equal in all cases" or "<item>: SKIPPED: <what>".
    "done" without this list is a lie. A false "browser-equal" line is a
    lie.
  - There is no word for a skip that is not a skip. "Limit",
    "simplification", "approximation", "edge case", "out of scope", "for
    now", "the common case", "not in the list": each of these in a report
    is a SKIP and rejects the block.
  - When you weigh "ask" against "continue": asking costs nothing, ever.
    Continuing past a skip costs the whole block. There is no case where
    continuing wins.
  - You say "I lied" and name the part the moment you notice you built
    past a skip, before anything else in the message.
