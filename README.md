# terminal

A terminal emulator for macOS, written from scratch in Rust.

Not a wrapper around another emulator — it forks its own pty, parses ANSI/VT
escape sequences itself, and renders every cell with a GPU pipeline it owns.

## Stack

- [`nix`](https://docs.rs/nix) — pty creation (`forkpty`) and window-size ioctls
- [`vte`](https://docs.rs/vte) — ANSI/VT escape sequence parsing
- [`winit`](https://docs.rs/winit) — window and input event handling
- [`wgpu`](https://docs.rs/wgpu) — GPU-accelerated (Metal) rendering
- [`font-kit`](https://docs.rs/font-kit) + [`fontdue`](https://docs.rs/fontdue) — system font lookup and glyph rasterization

## Features

- Login shell spawned over a real pty (`$SHELL`, profile files sourced)
- SGR colors: 16 named, 256-indexed, and 24-bit truecolor
- Cursor movement, erase, scroll regions, deferred line wrapping
- Alternate screen buffer (full-screen apps like vim, less, htop work)
- DECCKM application cursor keys
- Retina-aware glyph rendering via a font atlas
- Window resizing kept in sync with the pty and the character grid
- Scrollback with mouse-wheel scrolling (suspended while an alt-screen app is active)

## Not implemented

Tabs/panes, a config file, clipboard copy-paste, font ligatures/fallback,
image protocols (sixel etc.), and OSC 8 hyperlinks. See the codebase's
module layout below for where these would plug in.

## Running

```
cargo run
```

Requires a stable Rust toolchain targeting `aarch64-apple-darwin` (or
`x86_64-apple-darwin`). No other setup needed — the system's monospace font
is looked up automatically (SF Mono, falling back to Menlo).

## Testing

```
cargo test
```

Covers the VT parser and Grid model (cursor movement, SGR, scroll regions,
alt screen, line wrapping, scrollback), key-to-byte-sequence encoding, and
font atlas construction against the real system font.

## Layout

```
src/
  main.rs        winit ApplicationHandler: owns the window, pty, and Term
  pty.rs         forkpty, login shell exec, TIOCSWINSZ resize
  input.rs       keyboard event -> pty byte sequence encoding
  term/
    mod.rs       Term: cursor, modes, active/alt Grid, line wrapping
    grid.rs      Grid/Cell/Row, scrollback, resize
    perform.rs   vte::Perform impl (CSI/OSC/ESC dispatch)
    color.rs     16/256-color palette, Color -> RGB resolution
  render/
    mod.rs       wgpu surface/device setup, Grid -> instance buffer
    pipeline.rs  instanced quad render pipeline
    font.rs      font-kit lookup + fontdue rasterization into a glyph atlas
shaders/
  cell.wgsl      vertex/fragment shader for cell background + glyph quads
```
