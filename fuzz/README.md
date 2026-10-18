# Fuzzers

Coverage-guided fuzzing (`cargo-fuzz` / libFuzzer) for the parsers that consume
untrusted input. Two reasons this matters here:

- the release build is `panic = "abort"`, so **a panic closes the whole client**;
- a pathological input can also make a parser loop or allocate without bound —
  a message that arrives should never be able to freeze the app.

## Setup (once)

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Run

```bash
cargo +nightly fuzz run mrkdwn      # Slack mrkdwn parsing
cargo +nightly fuzz run signaling   # Chime signaling frame (protobuf) decode
```

Fuzzing has no end: run it for a bounded time locally or on a schedule.

```bash
# a quick pass
cargo +nightly fuzz run mrkdwn -- -max_total_time=120

# a crash (if any) is written here, with the exact command to reproduce it
ls fuzz/artifacts/mrkdwn/
```

The evolving corpus lives in `fuzz/corpus/<target>/` (not committed: a run adds
thousands of files). The classic bad inputs — [`big-list-of-naughty-strings`][blns]
(RTL, ZWJ, combining marks, control characters…) — are instead asserted on every
`cargo test` by `naughty_strings_never_panic` in `src/main.rs`.

[blns]: https://github.com/minimaxir/big-list-of-naughty-strings

## The targets

| Target | What it feeds |
|---|---|
| `mrkdwn` | arbitrary text to `app::mrkdwn::{parse, parse_spans, to_plain, to_editable}` |
| `signaling` | one tag byte + protobuf bytes to `SdkSignalFrame::decode` |

They are deliberately **self-contained**: `mrkdwn` compiles the parser source
file directly (`#[path]`), and `signaling` regenerates the proto with
`prost-build`, so neither depends on the `slaq` crate. That keeps the sanitizer
build small (no GUI, libvpx, WebRTC) and independent of the app's other
dependencies.
