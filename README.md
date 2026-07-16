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
- [`muda`](https://docs.rs/muda) — native macOS menu bar
- [`egui`](https://docs.rs/egui) + `egui-winit` + `egui-wgpu` — the Preferences window
- [`toml`](https://docs.rs/toml) + `serde` — config file read/write

## Features

- Login shell spawned over a real pty (`$SHELL`, profile files sourced)
- SGR colors: 16 named, 256-indexed, and 24-bit truecolor
- Cursor movement, erase, scroll regions, deferred line wrapping
- Alternate screen buffer (full-screen apps like vim, less, htop work)
- DECCKM application cursor keys
- Retina-aware glyph rendering via a font atlas
- Window resizing kept in sync with the pty and the character grid
- Scrollback with mouse-wheel scrolling (suspended while an alt-screen app is active)
- Configurable via `~/.terminal.config.toml`, editable either by hand or
  through Terminal > Preferences... (⌘,) in the menu bar

## Configuration

Settings live in `~/.terminal.config.toml`. See [config.example.toml](config.example.toml)
for every field and its default. The file is optional — a missing or
malformed one just falls back to defaults (a parse error is printed to
the terminal that launched the app, but never blocks startup).

Open **Terminal > Preferences...** (⌘,) from the menu bar for a GUI form
covering the same fields (font, colors, shell, scrollback), laid out as a
category sidebar rather than one long page. Saving writes back to the same
TOML file and applies every field immediately — no restart needed.

- **Colors**: pick a built-in theme (Default Dark/Light, Dracula, Nord,
  Solarized Dark/Light) to set background/foreground/ANSI all at once, then
  hand-tweak any swatch afterward.
- **Font**: a dropdown of the system's installed monospace fonts (detected
  via font-kit), or type a custom family name. Changing it rebuilds the
  glyph atlas and re-fits the grid to the window on the spot.
- **Shell**: a dropdown of the login shells listed in `/etc/shells`, or type
  a custom path. Changing it restarts the pty session: the running shell
  gets `SIGHUP` (same as a real terminal closing) and a new one starts in
  its place, so anything running in the old shell is interrupted. The
  screen clears for the fresh session; old scrollback is discarded.

If you edit `~/.terminal.config.toml` by hand while the app is running, pick
**Terminal > Reload Config** (⌘R) from the menu bar to pick up the change
without restarting the app (same live-apply behavior as saving from
Preferences, including the shell-restart caveat above).

## Not implemented

Tabs/panes, clipboard copy-paste, font ligatures/fallback, image protocols
(sixel etc.), and OSC 8 hyperlinks. See the codebase's module layout below
for where these would plug in.

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
alt screen, line wrapping, scrollback), key-to-byte-sequence encoding, font
atlas construction against the real system font, and config TOML
parsing/round-tripping (including validating `config.example.toml` itself).

## Layout

```
src/
  main.rs        winit ApplicationHandler: owns the window, pty, and Term
  pty.rs         forkpty, login shell exec, TIOCSWINSZ resize
  input.rs       keyboard event -> pty byte sequence encoding
  config.rs      Config/FontConfig/ColorConfig/ShellConfig, TOML load/save
  menu.rs        native macOS menu bar (muda), Preferences (Cmd+,)
  settings_ui.rs egui Preferences window: sidebar form UI, its own wgpu device
  settings_ui/
    themes.rs    built-in color theme presets (Dracula, Nord, Solarized, ...)
  term/
    mod.rs       Term: cursor, modes, active/alt Grid, line wrapping
    grid.rs      Grid/Cell/Row, scrollback, resize
    perform.rs   vte::Perform impl (CSI/OSC/ESC dispatch)
    color.rs     16/256-color palette (Palette), Color -> RGB resolution
  render/
    mod.rs       wgpu surface/device setup, Grid -> instance buffer
    pipeline.rs  instanced quad render pipeline
    font.rs      font-kit lookup + fontdue rasterization into a glyph atlas
shaders/
  cell.wgsl      vertex/fragment shader for cell background + glyph quads
config.example.toml  every config field documented with its default
```

### Why the Preferences window has its own GPU device

`egui-wgpu` 0.35 depends on `wgpu` 29.x while the terminal renderer uses
`wgpu` 30.x — two distinct, type-incompatible copies of the crate. Rather
than hold the whole terminal renderer back on an older wgpu, the settings
window gets its own small, independent wgpu instance/device instead of
sharing the main window's.
