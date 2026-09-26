# Pure-Rust Browser: Plan

Last updated: 2026-09-26
Status: planning complete, no code yet. Next step is Phase 0 in `04-roadmap.md`.

## For any session picking this up

Read in this order before doing anything:

1. `01-decisions.md`. Every decision there is **settled with the owner**. Do
   not re-ask them and do not reopen them. The only valid reason to reopen a
   decision is discovering during implementation that it is not doable; then
   raise it with the specific blocker, not as a general question.
2. `02-architecture.md` for how the pieces fit.
3. `03-crates.md` for what may be used and the rule for adding anything new.
4. `04-roadmap.md` for what to build next and when a phase is done.
5. `05-risks-and-open-items.md` for the short list of things genuinely still
   open. These are implementation details, not direction.

## What this project is

An open-source web browser written entirely in Rust. It exists because
existing browsers, including the ones that market themselves as private, come
with a catch: accounts, telemetry, a company's business interests, restrictions
on ad blocking, or a crypto scheme. This one has none of that.

Properties the owner requires, in priority order:

1. **Security through isolation.** Page content runs in memory, in a tab that
   is given only the capabilities we explicitly expose. Nothing a page does can
   reach the machine's filesystem or network except through our own code.
2. **Rust only.** No C or C++ anywhere in the build. No C++ toolchain
   required to compile. No system WebView. No engine borrowed from another
   browser.
3. **No bloat, no accounts, no telemetry.** No sync service, no login, no
   phone-home, ever.
4. **Fast.** Speed is one of the two reasons to use it over the others.
5. **Full ad blocking** with uBlock-style filter lists, unrestricted.
6. **All platforms.** Windows, Linux, macOS. This is why every dependency
   must be pure Rust: one toolchain, one build, every platform.

Everything else a normal browser does (JavaScript on by default, downloads,
logins, history, bookmarks) is expected. The difference is speed and security
without a company behind it.

## The crate rule

A crate may be used only if it is all three of:

- **standalone**: not designed to be embedded in one specific other project
  (this is why `stylo` is out: it is Servo's style system and is shaped around
  Servo and Gecko even though it is written in Rust),
- **pure Rust**: no C, C++, or assembly compiled or linked, no `cc`, `cmake`,
  or `pkg-config` in the build; calling OS APIs through Rust binding crates is
  allowed because there is no other way to reach hardware,
- **fast**: no interpreted glue, no needless copies, suitable for a browser
  hot path.

When a missing capability has a good crate meeting all three, use it rather
than writing it. When none exists, write it.

## Documents

| File | Contents |
|------|----------|
| `01-decisions.md` | Settled decisions with the reasoning, so they are not re-asked |
| `02-architecture.md` | Layers, threads, isolation model, storage model, data flow |
| `03-crates.md` | Approved crates with verified versions, banned crates, enforcement |
| `04-roadmap.md` | Phases 0 to 5 with exit criteria |
| `05-risks-and-open-items.md` | Risks and the few genuinely open implementation details |
| `06-unsafe-baseline.md` | Per-crate unsafe counts from Phase 0; compare at every phase boundary |
