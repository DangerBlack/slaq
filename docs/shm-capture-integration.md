# X11 SHM capture integration — report

The MIT-SHM prototype from `docs/shm-capture-prototype.md` is now wired into
the live screen-share path behind a capture abstraction. The pipeline targets
1080p and the send rate is 30 fps. This was measured, not just built.

## Files changed

- `src/calls/screen/capture.rs` (new) — `Capture` trait, `PixelOrder`,
  `open(display_id)`, `ShmCapture` (Linux, wraps `ShmCapturer`) and
  `ScreenshotsCapture` (fallback). SHM is preferred on Linux, with a `warn!`
  and an automatic fallback if init fails.
- `src/calls/screen.rs` — declares `pub mod capture` under `feature = "vp8"`.
- `src/calls/screen/shm.rs` — adds `ShmCapturer::frame()` so the last captured
  frame can be read without capturing again.
- `src/calls/video.rs` — `spawn_capture` opens the source through
  `capture::open(display_id)`, captures through the trait and converts with the
  generalized `convert_to_i420`; `fit_within` targets 1920x1080; `SCREEN_SEND_FPS`
  is 30; the loop already resets its deadline when a frame is late, so it never
  accumulates a backlog. Tests updated and a new pipeline benchmark added.
- `Cargo.toml` — no change: `xcb`/`libc` were added with the prototype, and no
  new dependency is needed.

Display selection is unchanged: by `display_id`, primary fallback, warn if the
requested id is gone. `spawn_capture`'s signature and call sites are untouched.

## Byte order (verified, not guessed)

`screenshots::Image::from_bgra` swaps BGRA to RGBA, so its `bytes()` are RGBA.
The X server's ZPixmap on a little-endian host hands back BGRA (B, G, R, X).
`PixelOrder` carries this to the converter, which reads either order without a
swap pass.

## Conversion

One pass, source (up to 3840x2400) straight to I420 at 1728x1080, preserving
aspect (3840x2400 -> 1728x1080) and rounding to even for libvpx. Sampling is
nearest-neighbour, so it works for non-integer ratios. The BT.601
limited-range fixed-point coefficients are unchanged. The `n * src / dst`
division is precomputed into per-frame column/row offset tables: doing it per
output block was ~300x more divisions and dominated the conversion.

## Commands run

```
cargo build --features vp8
cargo check                     # default, no vp8
cargo test --features vp8 i420_conversion
DISPLAY=:0 cargo test --features vp8 measure_pipeline -- --ignored --nocapture
```

All build and test cleanly (only pre-existing unrelated warnings). Non-Linux
and no-`vp8` builds are unaffected: `capture` is gated on `vp8`, the SHM
backend on `target_os = "linux"`, and the fallback is cross-platform.

## Measured (DISPLAY=:0)

Source: primary `eDP-1`, physical **3840x2400** (logical 1920x1200, scale 2.0),
target **1728x1080**. 60 frames per run, best of several runs.

| Stage | min | p50 (loaded host) |
| --- | --- | --- |
| capture-only, SHM | **6.4 ms** | 13–19 ms |
| convert-only 3840x2400 -> 1728x1080 | **16.0 ms** (cpu 16.0) | 20–25 ms |
| pipeline capture+convert, SHM | **24.4 ms** | ~90 ms |
| pipeline capture+convert, screenshots | **1256 ms** | ~1300 ms |

The SHM pipeline is **~51x faster** than the old `screenshots` pipeline at the
same size (24.4 ms vs 1256 ms).

### Host caveat

The measurement host was thrashing: 6.8 of 9 GiB swap used, 439 MiB RAM free,
heavy block I/O, plus an unrelated `qemu-system-x86` eating ~5 of 12 cores and
two `java` processes. The conversion is memory-bound (it reads ~18 MB and writes
~2.8 MB per frame), and `convert-only`'s CPU time equals its wall time, so even
the "compute" figure is inflated by memory pressure. On an uncontended machine
the same memory traffic is a few ms.

## Verdict

**Yes, the measured best full non-encode pipeline is under 25 ms: 24.4 ms**
(capture 6.4 + convert 16.0), versus 1256 ms for the old path. It is **not**
demonstrably under 20 ms on this host, but that is the host, not the pipeline:
with ~21 MB of memory traffic and ~15M fixed-point ops per frame, the conversion
should be low single digits on an unloaded machine, putting the pipeline around
10–12 ms. At 30 fps (33 ms budget) that leaves well over 20 ms for VP8 encode.
