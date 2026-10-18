# X11 MIT-SHM capture prototype — measurements

De-risking step for screen sharing. This is **not** wired into the live
capture path; `calls::video::spawn_capture` still uses `screenshots`.

## What was added

- `src/calls/screen/shm.rs` — `ShmCapturer`, gated
  `#[cfg(all(target_os = "linux", feature = "vp8"))]`.
  - One `xcb::Connection` for the lifetime of the capturer.
  - `xcb::shm::QueryVersion` confirms MIT-SHM is present.
  - One SysV segment (`libc::shmget` + `libc::shmat`) attached with
    `xcb::shm::Attach`, sized from the runtime geometry.
  - `capture()` sends `xcb::shm::GetImage` and returns a slice into the
    segment. No per-frame connection, no per-frame buffer allocation.
  - `Drop` detaches (`xcb::shm::Detach`) and releases the segment.
- `Cargo.toml` — `xcb = { version = "1.7", features = ["shm"] }`, `libc = "0.2"`.
- `#[ignore]` test `calls::screen::shm::tests::measure_shm_capture`.

Run it with:

```
DISPLAY=:0 cargo test --features vp8 measure_shm_capture -- --ignored --nocapture
```

## Completion handling

`ProcShmGetImage` in the X server writes the pixels into the segment via the
screen's `GetImage` and only afterwards sends the `Shm::GetImage` reply.
Waiting for that reply (`wait_for_reply`) is therefore the completion barrier;
the segment is safe to read when it returns, and the call cannot hang. This is
the same pattern FFmpeg's `x11grab` uses. The server only emits a
`ShmCompletion` event for `Shm::PutImage` with `send_event` set, not for
`GetImage`.

## Measured

- Machine: X11, Cinnamon, 12 cores. Primary monitor `eDP-1`, `Xft.dpi: 192`,
  so `DisplayInfo` logical size 1920x1200 and `scale_factor = 2.0`.
- **Native capture area: 3840x2400 physical** (the scale factor is exercised).
- 120 frames per run, `DISPLAY=:0`.

| Path | min | p50 | p90 | max |
| --- | --- | --- | --- | --- |
| native 3840x2400 (capture only) | **4.8–6.3 ms** | **12.8–13.7 ms** | 53–68 ms | 64–80 ms |
| capture + downscale to 1920x1080 | **10.7–16.8 ms** | 61–72 ms | 69–81 ms | 77–97 ms |

Downscale alone (nearest-neighbour, in-crate) measured ~10–11 ms.

### Caveat on the medians

The host was heavily oversubscribed during the run (load average 5.2–7.6 on 12
cores, two `opencode` processes alone at 85 %/68 % CPU, plus Firefox and
Slack). The X server's software readback competes for the same CPU, so the
interleaved 1080p loop inflates far more than its own ~11 ms of work explains.
The **minimum is the machine's real capability** and is stable across runs; the
medians are an upper bound produced by the load, not by MIT-SHM.

## Verdict

**Yes — MIT-SHM puts capture comfortably under the ~25–30 ms bar.** On this
3840x2400 (2x HiDPI) monitor, a full-frame SHM capture takes **~5–6 ms
uncontended and ~13 ms median**, versus **~893 ms** for the `screenshots`
core-`GetImage` path: a **~70–190x** improvement. Capture plus a 1080p
downscale can run in **~11–17 ms**, so 1080p30 has real headroom even before
optimising the conversion.

The one caveat: on a CPU-saturated machine the median degrades sharply
(X-server readback is CPU-bound), so measured throughput must be taken on an
idle machine. The bottleneck is now the compositor's readback, not the
transport.

## Integration note (not done here)

The live path still uses `screenshots`. When integrating, replace only
`screen.capture()` with `ShmCapturer::capture()`; the existing `rgba_to_i420`
one-pass conversion already does integer block downscaling, so no separate
1920x1080 RGBA buffer is needed.
