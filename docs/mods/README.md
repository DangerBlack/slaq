# slaq mods

Design documents for a **mod system**: third-party extensions to the client,
sandboxed and capability-gated, that let users reshape their own Slack.

**Status: shipped.** The mod host is built with the `mods` cargo feature (off by
default in a source build, **on** in the official download). It has shipped in
the official download since 0.6; the current runtime dates from 0.8 and the API
was last extended in 0.9.

What exists today:
- **Isolation and consent:** a mod is a WebAssembly component on its own OS
  thread, sandboxed. It can only use what the user grants, per capability, in
  the in-app Mods dialog or with `slaq mods`.
- **The user stays in control:**
  - the user decides which channels a mod reads and which hosts it reaches;
  - its secrets live in the system keyring;
  - **Running mods** shows each mod's live state, with Stop and Reload.
- **What a mod can do:**
  - read messages and channel history;
  - decorate messages and add actions, views, dialogs, slash commands and
    themes;
  - keep a private KV, vector or SQL store;
  - reach allowed hosts and read mapped folders;
  - run in the background.

Examples:
- [`examples/light-theme/`](examples/light-theme/): a theme, as a complete
  buildable mod. It is loaded by slaq's test suite and published on the
  download page.
- [`examples/translate-mod.md`](examples/translate-mod.md): a worked
  translation mod, end to end.
- `tests/data/hello-mod/`: the smallest mod that exercises the host pipeline.

## Want to build a mod? (agent-friendly)

Point your coding agent at **[`HOW_TO_WRITE_MODS.md`](HOW_TO_WRITE_MODS.md)** — a
single, self-contained guide written for an LLM to read. It has the agent
interview the user about purpose and limits, pick the least-privilege
capabilities, and produce a working mod (manifest + Rust component + the exact
build/grant/run commands). Just tell your agent: *"read
`docs/mods/HOW_TO_WRITE_MODS.md` and help me build a slaq mod."*

## Normative sources (one source per axis)

To avoid the cross-document drift that plagued the early drafts, each fact has a
single home:

| Axis | File |
|---|---|
| Capability vocabulary, stability, consent groups, events, slices | [`capabilities.md`](capabilities.md) |
| Manifest shape | [`manifest.schema.json`](manifest.schema.json) |
| Runtime signatures (WIT) | [`api-v0.md`](api-v0.md) |

Everything else is narrative and points at these rather than restating them.

| Document | Contents |
|---|---|
| [`00-analysis.md`](00-analysis.md) | why mods, market evidence, native vs mod, execution model, trust model, staging |
| [`execution-model.md`](execution-model.md) | how mods **run**: one OS thread per mod, consent-not-budget limits, background lane, cancellable I/O, Running mods (shipped in 0.8; supersedes the execution/trust sections of `00-analysis.md`) |
| [`roadmap.md`](roadmap.md) | the engineering plan: the steps, what each proves, acceptance criteria |
| [`capabilities.md`](capabilities.md) | the capability registry (names, operations, stability, slice, group, risk), events, reserved names |
| [`manifest-v0.md`](manifest-v0.md) | the manifest's shape and behavioural rules |
| [`api-v0.md`](api-v0.md) | the WIT runtime contract and its design notes |
| [`manifest.schema.json`](manifest.schema.json) | machine-readable manifest schema (JSON Schema 2020-12) |
| [`HOW_TO_WRITE_MODS.md`](HOW_TO_WRITE_MODS.md) | the practical guide to writing a mod (written so a coding agent can follow it) |
| [`examples/light-theme/`](examples/light-theme/) | a complete theme mod: manifest, component source, built `mod.wasm` |
| [`examples/translate-mod.md`](examples/translate-mod.md) | a worked translation mod end to end |
