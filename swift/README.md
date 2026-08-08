# keterm (Swift)

A native macOS rewrite of keterm: SwiftUI for the app shell, AppKit +
CoreText for the terminal grid itself.

## Why not SwiftUI all the way down

A terminal repaints thousands of cells at typing speed. SwiftUI's view
tree isn't built for that — a `Text` per cell is orders of magnitude too
slow, and there's no way to express "draw this glyph at this pixel" in
it. Every native terminal on the platform (Terminal.app, iTerm2, Warp)
draws its grid in a custom view for the same reason.

So the split is: SwiftUI owns the window, tab strips, sidebar, settings
and menus — everything that is genuinely a UI tree — and one
`NSViewRepresentable` hosts the grid view that draws text and takes
keyboard, mouse and input-method events.

## Layout

- `Sources/KetermCore` — no UI: the VT parser, the screen model, the
  pty, and the file/preview helpers. All of it testable without a
  window, which is the only reason the terminal emulation has tests.
- `Sources/keterm` — the SwiftUI app and the AppKit grid view.

## Status

Being built alongside the Rust version, which stays the shipping one
until this reaches parity. Done so far: the screen model (scrollback,
reflow, wide characters) and the VT parser.

Run the tests with `swift test` from this directory.
