# light-theme (example mod)

A **theme** mod: it overrides slaq's palette from its manifest, under
`ui.theme` (capability `ui.theme`, the only one it asks for). No host call is
involved. The host reads the table, merges it onto the built-in palette and
applies it at load; it reverts when the mod is revoked or removed.

This is the "no built-in light mode" answer: a light theme ships as a mod.

Contents:
- **`manifest.json`:** the theme (the mod's data).
- **`mod.wasm`:** the component. slaq instantiates a component for every mod;
  this one does nothing and imports nothing.
- **`component/`:** its source. It is built against slaq's own `wit/`, so it
  can't drift from the contract.

slaq's test `the_light_theme_example_loads_and_applies` loads this very folder,
so a change to the mod system that breaks it fails the build.

## Install

```sh
d="$HOME/.config/slaq/mods/light-theme"      # or "$SLAQ_MODS_DIR/light-theme"
mkdir -p "$d"
cp manifest.json mod.wasm "$d/"
slaq mods grant dev.danger.light-theme ui.theme
slaq gui
```

Revert: `slaq mods revoke dev.danger.light-theme ui.theme`, or delete the folder.

## Rebuild the component

```sh
cd component
cargo component build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/slaq_mod_light_theme.wasm ../mod.wasm
```

Release zip (what the download page serves):
`zip -j -X light-theme-<version>.zip manifest.json mod.wasm`.

## Notes

- Role names are the ones in `design-system/slaq/MASTER.md`. Unknown roles and
  malformed hex are ignored (lenient), so a partial theme is fine.
- There is **no contrast validation**: a theme that makes the app unreadable is
  the user's to remove.
