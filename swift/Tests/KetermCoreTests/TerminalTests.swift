import Testing
@testable import KetermCore

/// The visible text of a live row, for asserting on what the screen says.
private func rowText(_ terminal: Terminal, _ row: Int) -> String {
    String(terminal.grid.row(row).map(\.character))
}

private func feed(_ terminal: Terminal, _ text: String) {
    terminal.advance(Array(text.utf8))
}

@Suite("Printing and wrapping")
struct PrintingTests {
    @Test("A character lands where the cursor is and moves it on")
    func printsAndAdvances() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "AB")
        #expect(rowText(terminal, 0) == "AB        ")
        #expect(terminal.cursor.column == 2)
    }

    @Test("A line that exactly fills the row wraps only once more is printed")
    func deferredWrap() {
        // The deferred wrap matters: without it, text ending flush at the
        // margin scrolls the screen a line early, and every full-width
        // line of output would leave a stray blank row behind it.
        let terminal = Terminal(columns: 5, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDE")
        #expect(terminal.cursor.row == 0, "still on the first row")
        feed(terminal, "F")
        #expect(rowText(terminal, 0) == "ABCDE")
        #expect(rowText(terminal, 1) == "F    ")
        #expect(terminal.cursor.row == 1)
    }

    @Test("A cursor move cancels a pending wrap")
    func pendingWrapCancelled() {
        let terminal = Terminal(columns: 5, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDE\u{1B}[1;1HX")
        #expect(rowText(terminal, 0) == "XBCDE")
        #expect(rowText(terminal, 1) == "     ", "nothing should have wrapped")
    }

    @Test("Scrolling off the top feeds scrollback")
    func scrollbackFills() {
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "11111\r\n22222\r\n33333")
        #expect(rowText(terminal, 0) == "22222")
        #expect(rowText(terminal, 1) == "33333")
        #expect(terminal.grid.scrollback.count == 1)
        #expect(String(terminal.grid.scrollback[0].map(\.character)) == "11111")
    }
}

@Suite("Wide characters")
struct WideCharacterTests {
    @Test("A double-width character claims two cells")
    func widePair() {
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "日x")
        #expect(terminal.grid[0, 0].flags.contains(.wide))
        #expect(terminal.grid[0, 1].flags.contains(.wideSpacer))
        #expect(terminal.grid[0, 2].character == "x", "the next character starts after both halves")
        #expect(terminal.cursor.column == 3)
    }

    @Test("Overwriting the right half clears the left")
    func overwriteRightHalf() {
        // What vim does constantly: redraw a line in place, landing a
        // narrow character on one half of a wide one. The orphaned half
        // kept its wide flag and went on drawing two columns wide, over
        // whatever replaced it.
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "日x\u{1B}[1;2Ha")
        #expect(terminal.grid[0, 1].character == "a")
        #expect(!terminal.grid[0, 0].flags.contains(.wide), "the left half still claims to be wide")
        #expect(terminal.grid[0, 0].character == " ", "and still draws its glyph")
    }

    @Test("Overwriting the left half clears the stranded spacer")
    func overwriteLeftHalf() {
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "日\u{1B}[1;1Ha")
        #expect(terminal.grid[0, 0].character == "a")
        #expect(
            !terminal.grid[0, 1].flags.contains(.wideSpacer),
            "a stranded spacer is skipped when copying, swallowing a real character"
        )
    }

    @Test("A wide character replacing two others clears both neighbours")
    func wideOverWide() {
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "日本\u{1B}[1;2H語") // 語 takes columns 1-2, straddling both pairs
        #expect(!terminal.grid[0, 0].flags.contains(.wide))
        #expect(terminal.grid[0, 1].flags.contains(.wide))
        #expect(terminal.grid[0, 2].flags.contains(.wideSpacer))
        #expect(!terminal.grid[0, 3].flags.contains(.wideSpacer), "the old pair's spacer is orphaned")
    }

    @Test("Erasing half a wide character takes the whole pair")
    func eraseHalf() {
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "あい\u{1B}[1;2H\u{1B}[1X")
        #expect(!terminal.grid[0, 0].flags.contains(.wide))
    }

    @Test("Width is measured by East Asian Width, not byte or scalar count")
    func widthTable() {
        #expect(Character("a").terminalColumns == 1)
        #expect(Character("日").terminalColumns == 2)
        #expect(Character("　").terminalColumns == 2, "ideographic space is fullwidth")
        #expect(Character("→").terminalColumns == 1)
        #expect(Unicode.Scalar(0x0301)!.terminalColumns == 0, "a combining accent adds no width")
    }
}

@Suite("Editing sequences")
struct EditingTests {
    @Test("ICH opens a gap and shifts the rest right")
    func insertCharacters() {
        // The mid-line-editing bug: a shell's line editor inserts a typed
        // character with ICH, and dropping it left the display diverged
        // from the shell's real buffer.
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDEF\u{1B}[4G\u{1B}[2@")
        #expect(rowText(terminal, 0) == "ABC  DEF  ")
        feed(terminal, "xy")
        #expect(rowText(terminal, 0) == "ABCxyDEF  ")
    }

    @Test("DCH removes and shifts left")
    func deleteCharacters() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDEF\u{1B}[3G\u{1B}[2P")
        #expect(rowText(terminal, 0) == "ABEF      ")
    }

    @Test("ECH blanks in place without shifting")
    func eraseCharacters() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDEF\u{1B}[2G\u{1B}[3X")
        #expect(rowText(terminal, 0) == "A   EF    ")
    }

    @Test("ICH and DCH clamp at the right edge instead of overrunning")
    func editingClamps() {
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "ABCDE\u{1B}[4G\u{1B}[99@")
        #expect(rowText(terminal, 0) == "ABC  ")
        feed(terminal, "\u{1B}[99P")
        #expect(rowText(terminal, 0) == "ABC  ")
    }

    @Test("IL and DL shift lines within the scroll region")
    func insertDeleteLines() {
        let terminal = Terminal(columns: 3, rows: 4, scrollbackLimit: 100)
        feed(terminal, "AA\r\nBB\r\nCC\r\nDD")
        feed(terminal, "\u{1B}[2;1H\u{1B}[1L")
        #expect(rowText(terminal, 0) == "AA ")
        #expect(rowText(terminal, 1) == "   ")
        #expect(rowText(terminal, 2) == "BB ")
        #expect(rowText(terminal, 3) == "CC ", "DD was pushed off the bottom")

        feed(terminal, "\u{1B}[2;1H\u{1B}[1M")
        #expect(rowText(terminal, 1) == "BB ")
        #expect(terminal.grid.scrollback.isEmpty, "DL discards lines, it never feeds scrollback")
    }

    @Test("REP repeats the last printed character")
    func repeatCharacter() {
        let terminal = Terminal(columns: 10, rows: 2, scrollbackLimit: 100)
        feed(terminal, "A\u{1B}[3b")
        #expect(rowText(terminal, 0) == "AAAA      ")
    }
}

@Suite("Modes and queries")
struct ModeTests {
    @Test("DSR reports the cursor position, once")
    func deviceStatusReport() {
        let terminal = Terminal(columns: 10, rows: 5, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[3;5H\u{1B}[6n")
        #expect(String(decoding: terminal.takeResponses(), as: UTF8.self) == "\u{1B}[3;5R")
        #expect(terminal.takeResponses().isEmpty, "drained once, gone")
    }

    @Test("DA identifies the terminal")
    func deviceAttributes() {
        let terminal = Terminal(columns: 10, rows: 5, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[c")
        #expect(String(decoding: terminal.takeResponses(), as: UTF8.self) == "\u{1B}[?6c")
    }

    @Test("Private modes toggle")
    func privateModes() {
        let terminal = Terminal(columns: 10, rows: 5, scrollbackLimit: 100)
        #expect(terminal.modes.mouseMode == .off)
        feed(terminal, "\u{1B}[?1000h\u{1B}[?1006h\u{1B}[?2004h\u{1B}[?1l")
        #expect(terminal.modes.mouseMode == .clicks)
        #expect(terminal.modes.mouseSGR)
        #expect(terminal.modes.bracketedPaste)
        #expect(!terminal.modes.applicationCursorKeys)
        feed(terminal, "\u{1B}[?1000l\u{1B}[?2004l")
        #expect(terminal.modes.mouseMode == .off)
        #expect(!terminal.modes.bracketedPaste)
    }

    @Test("DECSCUSR sets the cursor shape, and a bare CSI q does not")
    func cursorShape() {
        let terminal = Terminal(columns: 10, rows: 5, scrollbackLimit: 100)
        #expect(terminal.cursorShape == .block)
        feed(terminal, "\u{1B}[5 q")
        #expect(terminal.cursorShape == .bar)
        feed(terminal, "\u{1B}[4 q")
        #expect(terminal.cursorShape == .underline)
        feed(terminal, "\u{1B}[0 q")
        #expect(terminal.cursorShape == .block)
        feed(terminal, "\u{1B}[5 q\u{1B}[q")
        #expect(terminal.cursorShape == .bar, "the space intermediate is what makes it DECSCUSR")
    }

    @Test("ED 3 clears scrollback without touching the screen")
    func eraseSavedLines() {
        // The modern `clear` sends CSI H, CSI 2J, CSI 3J as a trio and
        // expects exactly this split.
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "11111\r\n22222\r\n33333")
        #expect(terminal.grid.scrollback.count == 1)
        feed(terminal, "\u{1B}[3J")
        #expect(terminal.grid.scrollback.isEmpty)
        #expect(rowText(terminal, 0) == "22222", "the visible screen is untouched")
    }

    @Test("The alternate screen keeps no scrollback and restores the primary")
    func alternateScreen() {
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "primary")
        feed(terminal, "\u{1B}[?1049h")
        #expect(terminal.usingAlternateScreen)
        feed(terminal, "aaaaa\r\nbbbbb\r\nccccc\r\nddddd")
        #expect(terminal.grid.scrollback.isEmpty)
        feed(terminal, "\u{1B}[?1049l")
        #expect(!terminal.usingAlternateScreen)
        #expect(rowText(terminal, 0).hasPrefix("prima"), "the primary screen is still there")
    }

    @Test("SGR sets colors and attributes, and 0 clears them")
    func graphics() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[1;31mA\u{1B}[0mB")
        #expect(terminal.grid[0, 0].flags.contains(.bold))
        #expect(terminal.grid[0, 0].foreground == .indexed(1))
        #expect(terminal.grid[0, 1].flags == [])
        #expect(terminal.grid[0, 1].foreground == .default)
    }

    @Test("Extended colors parse in both the semicolon and colon spellings")
    func extendedColors() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[38;5;196mZ")
        #expect(terminal.grid[0, 0].foreground == .indexed(196))
        feed(terminal, "\u{1B}[38;2;10;20;30mY")
        #expect(terminal.grid[0, 1].foreground == .rgb(10, 20, 30))
        feed(terminal, "\u{1B}[38:2:1:2:3mW")
        #expect(terminal.grid[0, 2].foreground == .rgb(1, 2, 3), "the colon form arrives as one parameter group")
    }

    @Test("OSC sets the window title")
    func windowTitle() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "\u{1B}]0;日本語 title\u{07}")
        #expect(terminal.title == "日本語 title")
    }
}

@Suite("Resize")
struct ResizeTests {
    @Test("Narrowing reflows a wrapped line instead of truncating it")
    func reflowNarrower() {
        // The bug this guards: shrinking used to cut every row to the new
        // width, destroying what didn't fit -- and widening again didn't
        // bring it back, because it was already gone.
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDEFGHIJKLMNO") // 15 characters: a real wrap at column 10

        terminal.resize(columns: 5, rows: 3)
        #expect(rowText(terminal, 0) == "ABCDE")
        #expect(rowText(terminal, 1) == "FGHIJ")
        #expect(rowText(terminal, 2) == "KLMNO")

        terminal.resize(columns: 10, rows: 3)
        #expect(rowText(terminal, 0) == "ABCDEFGHIJ")
        #expect(rowText(terminal, 1) == "KLMNO     ")
    }

    @Test("Separately-entered lines never merge on reflow")
    func hardBreaksSurvive() {
        let terminal = Terminal(columns: 5, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDE\r\nFG")
        terminal.resize(columns: 3, rows: 4)
        #expect(rowText(terminal, 0) == "ABC")
        #expect(rowText(terminal, 1) == "DE ")
        #expect(rowText(terminal, 2) == "FG ", "a real newline is not a wrap")
    }

    @Test("The cursor stays on the same character across a reflow")
    func cursorFollowsReflow() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "ABCDEFGHIJKLMNO")
        terminal.resize(columns: 5, rows: 5)
        #expect(terminal.cursor.row == 2)
        #expect(terminal.cursor.column == 4)
        #expect(terminal.grid[2, 4].character == "O")
    }

    @Test("Shrinking rows pushes to scrollback and growing pulls back")
    func rowCountUsesScrollback() {
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "11111\r\n22222")
        terminal.resize(columns: 5, rows: 1)
        #expect(rowText(terminal, 0) == "22222")
        #expect(terminal.grid.scrollback.count == 1)

        terminal.resize(columns: 5, rows: 2)
        #expect(rowText(terminal, 0) == "11111")
        #expect(rowText(terminal, 1) == "22222")
        #expect(terminal.grid.scrollback.isEmpty)
    }

    @Test("Output between a shrink and a grow is not resurrected")
    func noStaleRestore() {
        // A resize drag shrinks by a row, the shell prints, then the drag
        // grows back. A naive pending-row counter pops whatever is now at
        // the back of scrollback -- text already visible one row up --
        // duplicating it at the top.
        let terminal = Terminal(columns: 5, rows: 2, scrollbackLimit: 100)
        feed(terminal, "11111\r\n22222")
        terminal.resize(columns: 5, rows: 1)
        feed(terminal, "\r\n33333")
        #expect(rowText(terminal, 0) == "33333")
        #expect(terminal.grid.scrollback.count == 2)

        terminal.resize(columns: 5, rows: 2)
        // The new row is padded in at the bottom, leaving what was on
        // screen where it was. Nothing is pulled back out of scrollback:
        // its tail is real output that scrolled off for good, not the
        // row the shrink hid.
        #expect(rowText(terminal, 0) == "33333")
        #expect(rowText(terminal, 1) == "     ", "a padded blank, not a resurrected row")
        #expect(terminal.grid.scrollback.count == 2)
    }
}

@Suite("Parser")
struct ParserTests {
    @Test("A sequence split across reads is still recognised")
    func splitAcrossReads() {
        // Pty reads land on arbitrary boundaries; a parser that only
        // worked on whole sequences would corrupt the screen whenever one
        // straddled two chunks.
        let terminal = Terminal(columns: 10, rows: 5, scrollbackLimit: 100)
        terminal.advance(Array("\u{1B}[3".utf8))
        terminal.advance(Array(";5H".utf8))
        terminal.advance(Array("X".utf8))
        #expect(terminal.grid[2, 4].character == "X")
    }

    @Test("A multi-byte character split across reads is decoded, not mangled")
    func utf8SplitAcrossReads() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        let bytes = Array("日".utf8)
        terminal.advance([bytes[0]])
        terminal.advance([bytes[1]])
        #expect(terminal.grid[0, 0].character == " ", "nothing drawn from a partial character")
        terminal.advance([bytes[2]])
        #expect(terminal.grid[0, 0].character == "日")
    }

    @Test("An unfinished sequence is abandoned when a new one starts")
    func truncatedSequenceRecovers() {
        // This is how a terminal recovers from garbage instead of
        // swallowing everything that follows it.
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[12;\u{1B}[1;1HX")
        #expect(terminal.grid[0, 0].character == "X")
    }

    @Test("Unimplemented sequences are consumed, never printed")
    func unknownSequencesAreSwallowed() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        feed(terminal, "\u{1B}[>4;2mA\u{1B}P1$rok\u{1B}\\B")
        #expect(rowText(terminal, 0) == "AB        ", "no escape-sequence debris on screen")
    }

    @Test("Invalid UTF-8 becomes a replacement character rather than vanishing")
    func invalidUTF8() {
        let terminal = Terminal(columns: 10, rows: 3, scrollbackLimit: 100)
        terminal.advance([0xFF, 0x41])
        #expect(terminal.grid[0, 0].character == "\u{FFFD}")
        #expect(terminal.grid[0, 1].character == "A")
    }
}
