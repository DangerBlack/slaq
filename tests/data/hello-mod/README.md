# hello-mod (test fixture)

A tiny WASM component used by the mod host's end-to-end tests. Its source lives
in the separate `slaq-mod-hello` repo (the first example mod); this is a built
copy, vendored so the tests do not need `cargo-component` or a network.

It is built against `wit/slaq-mod.wit` (slice-1 contract) with:

```sh
cargo component build --release --target wasm32-unknown-unknown
# then copy target/wasm32-unknown-unknown/release/slaq_mod_hello.wasm here as mod.wasm
```

`manifest.json` is hand-written to match `docs/mods/manifest.schema.json`. It
requests `messages.read`, `store.kv`, `ui.message_decoration`, `ui.view`,
`ui.composer_action`, `ui.context_menu` and `ui.dialog`; the
host default-denies everything, so a run must grant those capabilities — the unit
tests pass them explicitly and `tests/mod_worker.rs` uses `--grant-all`. Its
`start` also fills the declared view (`set-view-content`) when `ui.view` is
granted, exercising that WIT round trip.

The vendored binary is rebuilt when a **required guest export** changes (a new
host *import* does not need a rebuild — a guest may import a subset; *changing* an
existing import's signature does, as `get-message` did when it moved to the
`message` record). It exports
`start`, `on-message`, `on-action`, `on-event`, `on-reaction`, `on-command`,
`on-composer-action` and
`on-dialog-action`; the source is the
separate `slaq-mod-hello` repo, which must add any new required export before the
fixture is rebuilt here.
