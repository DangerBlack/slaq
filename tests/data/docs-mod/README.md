# docs-mod (test fixture)

A WASM component used by the mod host's end-to-end tests to exercise the
`filesystem` capability. Its source lives in the separate `slaq-mod-docs` repo
(the `filesystem` example mod); this is a built copy, vendored so the tests do
not need `cargo-component` or a network.

It is built against `wit/slaq-mod.wit` (slice-1 contract) with:

```sh
cargo component build --release --target wasm32-unknown-unknown
# then copy target/wasm32-unknown-unknown/release/slaq_mod_docs.wasm here as mod.wasm
```

`manifest.json` is hand-written to match `docs/mods/manifest.schema.json`. It
requests `messages.read` (`scope: "all"`), `filesystem` (one `read` slot named
`docs`) and `ui.message_decoration`. On each non-empty message it calls
`fs-search("docs", "", text, 5)` and decorates the first hit as
`docs: N hit(s) — path:line`; an unmapped slot fails closed (no decoration).

The host default-denies everything, so a run must grant those capabilities. The
unit test `the_docs_example_searches_a_mapped_slot` loads it with an explicit
`--fs-slot`-equivalent mapping; the one-shot runner uses `--fs-slot docs=DIR`
with the vendored `slaq-mod-docs` README recipe.

The vendored binary is rebuilt when a **required guest export** changes or when
the WIT import signatures it uses change. It exports
`start`, `on-message`, `on-action`, `on-event`, `on-reaction`, `on-command`,
`on-composer-action` and `on-dialog-action`.
