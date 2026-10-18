# slaq: what to install (quick reference)

Four things, in order:

1. the **system libraries** (graphical session + OpenGL + TLS + audio);
2. a **Chromium/Chrome browser** (needed for login);
3. **Rust** and the build tools (only if you compile);
4. a **login** for each person (credentials are not shared).

> **VP8 is mandatory.** Sending a screen-share uses VP8 (libvpx). In recent
> versions it's already in the default build; if not, compile with
> `--features vp8`. *Receiving* a screen-share uses AV1 (already included).

Built into `ffmpeg`-less: no extra multimedia package.

---

## 1. System libraries

### Debian / Ubuntu / Linux Mint
```bash
sudo apt update
sudo apt install -y \
  libxcb1 libxau6 libxdmcp6 libbsd0 libmd0 \
  libgl1 libegl1 \
  libssl3t64 libasound2t64
```
On older versions the names are `libssl3` and `libasound2`.

### Fedora
```bash
sudo dnf install -y \
  libxcb libXau libXdmcp libbsd libmd \
  mesa-libGL mesa-libEGL \
  openssl-libs alsa-lib
```

### Arch
```bash
sudo pacman -S --needed \
  libxcb libxau libxdmcp libbsd libmd \
  mesa openssl alsa-lib
```

What they cover: `libxcb`/`libXau`/`libXdmcp` (X11 window, and screen capture via
MIT-SHM), `libgl1`/`libegl1` (OpenGL, loaded at runtime), `libssl`/`libcrypto`
(TLS), `libasound` (huddle audio). The **fonts** are inside the binary: no need
to install them.

---

## 2. A browser for login

`slaq login` drives a real Chrome/Chromium (via CDP), so an installed Chromium
browser is required:

```bash
# Debian/Ubuntu/Mint
sudo apt install -y chromium-browser        # or google-chrome
# Fedora
sudo dnf install -y chromium
# Arch
sudo pacman -S chromium
```

---

## 3. Compiling (only if you don't use a ready-made binary)

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# C compiler and pkg-config (libvpx is built from source)
sudo apt install -y build-essential pkg-config   # Debian/Ubuntu/Mint
# Fedora: sudo dnf groupinstall -y "Development Tools"
# Arch:   sudo pacman -S --needed base-devel
```

### Build
```bash
cargo build --release --features mods  # GUI + TUI + VP8 + mods (as the download)
cargo build --release                 # GUI + TUI + VP8, without mods
cargo build --release --no-default-features   # TUI only (no graphics)
```
The first build compiles **libvpx from source**: ~15 minutes. Only the first time.

The binary lands in `target/release/slaq`.
The **debug build is huge (~650 MB)**: always share the **release** one.

---

## 4. First launch (each person for themselves)

```bash
./target/release/slaq login     # opens the browser, log in, saves the session
./target/release/slaq gui       # or: slaq tui
```

- Credentials (`xoxc`/`xoxd`) go into the system **keyring**, otherwise into
  `~/.config/slaq/credentials.json` (permissions `0600`).
- Message/channel cache: `~/.local/share/slaq/cache.db`.
- Avatars/emoji/previews: `~/.cache/slaq/images/`.
- Mods: installed under `~/.config/slaq/mods/`; their data under
  `~/.local/share/slaq/mods/`; their secrets in the system keyring.

**Don't share credentials**: each person runs their own `login`.

---

## If it doesn't start: what's missing

```bash
ldd ./target/release/slaq | grep "not found"
```
Each `not found` line is a missing library → install it from point 1.

## Minimum requirements
- **Linux x86_64**, graphical session (X11 or Wayland).
- **OpenGL** working (Mesa driver; software fallback llvmpipe ok).
- **glibc** compatible with the system the build was compiled on (if the
  build doesn't start with `GLIBC_x.xx not found`, recompile on the older
  of the two distros, or in a container).
- **VP8/libvpx** only at build-time (static: it's not a runtime dependency).
