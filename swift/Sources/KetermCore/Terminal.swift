import Foundation

/// DECSCUSR cursor shape. Blink variants map to their steady shape --
/// nothing here blinks.
public enum CursorShape: Sendable { case block, underline, bar }

/// Which classes of mouse event the application asked for. xterm's tiers
/// nest: each level includes everything below it.
public enum MouseMode: Int, Comparable, Sendable {
    case off = 0
    /// CSI ?1000 -- presses and releases.
    case clicks = 1
    /// CSI ?1002 -- plus motion while a button is held.
    case drag = 2
    /// CSI ?1003 -- plus all motion. Motion with no button held isn't
    /// forwarded yet; apps asking for 1003 get what 1002 would give.
    case motion = 3

    public static func < (a: MouseMode, b: MouseMode) -> Bool { a.rawValue < b.rawValue }
}

public struct TerminalModes: Sendable {
    /// DECCKM (CSI ?1) -- arrow keys send SS3 instead of CSI.
    public var applicationCursorKeys = false
    /// CSI ?25 -- whether the cursor is drawn.
    public var cursorVisible = true
    /// CSI ?2004 -- the application wants pasted text bracketed, so it
    /// can tell a paste from typing. zsh highlights it and, crucially,
    /// doesn't run embedded newlines until Enter is actually pressed.
    public var bracketedPaste = false
    public var mouseMode: MouseMode = .off
    /// CSI ?1006 -- SGR mouse encoding (decimal text, no 223-column
    /// ceiling) instead of the legacy byte-offset form.
    public var mouseSGR = false

    public init() {}
}

public struct Cursor: Sendable {
    public var row = 0
    public var column = 0
    public var foreground: TerminalColor = .default
    public var background: TerminalColor = .default
    public var flags: CellFlags = []
    public init() {}
}

/// One terminal screen: the grid, the cursor, and the modes an
/// application has switched on. Fed bytes from a pty via `advance`.
public final class Terminal {
    public private(set) var columns: Int
    public private(set) var rows: Int
    private var primary: Grid
    private var alternate: Grid
    public private(set) var usingAlternateScreen = false

    public var cursor = Cursor()
    private var savedCursor = Cursor()
    private var alternateSavedCursor = Cursor()

    /// Deferred wrap: set when a printed character exactly filled the
    /// last column. The wrap only happens if another character follows,
    /// so a cursor move arriving first cancels it -- without this, text
    /// that ends flush at the margin scrolls the screen a line early.
    private var wrapPending = false
    /// The most recent printed character, for REP (CSI b).
    private var lastPrinted: Character?

    public var modes = TerminalModes()
    public var cursorShape: CursorShape = .block
    public private(set) var title = ""

    private var scrollTop = 0
    private var scrollBottom: Int
    private var scrollbackLimit: Int
    private var parser = Parser()

    /// Bytes owed back to the application in reply to a query (DSR, DA).
    /// The terminal can't write to the pty itself, so replies queue here
    /// and the caller drains them -- a program that asks where the cursor
    /// is blocks until it gets an answer.
    private var responses: [UInt8] = []

    public init(columns: Int, rows: Int, scrollbackLimit: Int) {
        let columns = max(1, columns), rows = max(1, rows)
        self.columns = columns
        self.rows = rows
        self.scrollbackLimit = scrollbackLimit
        self.primary = Grid(columns: columns, rows: rows, scrollbackLimit: scrollbackLimit)
        // No scrollback on the alternate screen: full-screen apps manage
        // their own scrolling, the wheel is disabled there, and lines vim
        // scrolls past would otherwise pile up invisibly to the cap.
        self.alternate = Grid(columns: columns, rows: rows, scrollbackLimit: 0)
        self.scrollBottom = rows - 1
    }

    public var grid: Grid { usingAlternateScreen ? alternate : primary }

    private func withActiveGrid(_ body: (inout Grid) -> Void) {
        if usingAlternateScreen { body(&alternate) } else { body(&primary) }
    }

    public func setScrollbackLimit(_ limit: Int) {
        scrollbackLimit = limit
        primary.setScrollbackLimit(limit)
    }

    /// Drain replies produced since the last call, for the caller to
    /// write back to the pty.
    public func takeResponses() -> [UInt8] {
        defer { responses.removeAll() }
        return responses
    }

    public func advance(_ bytes: [UInt8]) {
        parser.advance(bytes) { action in
            switch action {
            case .print(let character): printCharacter(character)
            case .execute(let byte): execute(byte)
            case .csi(let parameters, let intermediates, let final):
                csi(parameters: parameters, intermediates: intermediates, final: final)
            case .esc(let intermediates, let final):
                if intermediates.isEmpty { esc(final) }
            case .osc(let parameters): osc(parameters)
            }
        }
    }

    // MARK: - Resize

    /// Resize both screens. The primary reflows at the new width and the
    /// cursor stays on the same character; the alternate just truncates,
    /// since full-screen apps redraw completely on `SIGWINCH`.
    public func resize(columns newColumns: Int, rows newRows: Int) {
        let newColumns = max(1, newColumns), newRows = max(1, newRows)
        guard newColumns != columns || newRows != rows else { return }

        let primaryCursor = usingAlternateScreen
            ? (row: savedCursor.row, column: savedCursor.column)
            : (row: cursor.row, column: cursor.column)
        let placed = primary.resizeReflowing(columns: newColumns, rows: newRows, cursor: primaryCursor)
        alternate.resizeTruncating(columns: newColumns, rows: newRows)

        columns = newColumns
        rows = newRows
        scrollTop = 0
        scrollBottom = newRows - 1
        wrapPending = false

        if usingAlternateScreen {
            savedCursor.row = placed.row
            savedCursor.column = placed.column
            clampCursor()
        } else {
            cursor.row = placed.row
            cursor.column = placed.column
        }
    }

    private func clampCursor() {
        cursor.row = min(cursor.row, rows - 1)
        cursor.column = min(cursor.column, columns - 1)
    }

    private func moveCursor(row: Int, column: Int) {
        wrapPending = false
        cursor.row = min(max(0, row), rows - 1)
        cursor.column = min(max(0, column), columns - 1)
    }

    // MARK: - Printing

    /// Blank whichever half of a double-width character straddles
    /// `column`, before anything else is written there.
    ///
    /// Overwriting one half orphans the other: a `wide` cell whose spacer
    /// is gone still draws two columns wide over whatever replaced it,
    /// and a stranded spacer is skipped when copying, quietly swallowing
    /// a character. Full-screen apps redraw lines in place constantly, so
    /// this happens the moment one edits a line with CJK in it.
    private func clearWidePair(row: Int, column: Int) {
        withActiveGrid { grid in
            let flags = grid[row, column].flags
            if flags.contains(.wideSpacer), column > 0 {
                grid[row, column - 1] = .blank
            }
            if flags.contains(.wide), column + 1 < grid.columns {
                grid[row, column + 1] = .blank
            }
        }
    }

    private func printCharacter(_ character: Character) {
        let width = character.terminalColumns
        // Combining marks aren't merged onto the previous cell yet;
        // dropping them beats corrupting the column arithmetic.
        guard width > 0 else { return }

        if wrapPending {
            wrapPending = false
            let leaving = cursor.row
            withActiveGrid { $0.setWrapped(leaving, true) }
            indexDown()
            cursor.column = 0
        }
        if cursor.column + width > columns {
            let leaving = cursor.row
            withActiveGrid { $0.setWrapped(leaving, true) }
            indexDown()
            cursor.column = 0
        }

        let row = cursor.row, column = cursor.column
        // Free both cells this character will occupy before writing
        // either of them.
        clearWidePair(row: row, column: column)
        if width == 2, column + 1 < columns {
            clearWidePair(row: row, column: column + 1)
        }

        let foreground = cursor.foreground, background = cursor.background, flags = cursor.flags
        withActiveGrid { grid in
            grid[row, column] = Cell(
                character: character,
                foreground: foreground,
                background: background,
                flags: width == 2 ? flags.union(.wide) : flags
            )
            if width == 2, column + 1 < grid.columns {
                grid[row, column + 1] = Cell(
                    character: " ",
                    foreground: foreground,
                    background: background,
                    flags: flags.union(.wideSpacer)
                )
            }
        }

        if column + width == columns {
            wrapPending = true
        } else {
            cursor.column += width
        }
        lastPrinted = character
    }

    // MARK: - C0 controls

    private func execute(_ byte: UInt8) {
        switch byte {
        case 0x0D: // CR
            wrapPending = false
            cursor.column = 0
        case 0x0A, 0x0B, 0x0C: // LF, VT, FF
            indexDown()
        case 0x08: // BS
            wrapPending = false
            cursor.column = max(0, cursor.column - 1)
        case 0x09: // HT
            wrapPending = false
            cursor.column = min((cursor.column / 8 + 1) * 8, columns - 1)
        default:
            break // BEL and friends: nothing to do here.
        }
    }

    /// IND: down one line, scrolling the region if already at its bottom.
    private func indexDown() {
        wrapPending = false
        if cursor.row == scrollBottom {
            let (top, bottom) = (scrollTop, scrollBottom)
            withActiveGrid { $0.scrollUp(top: top, bottom: bottom, count: 1) }
        } else if cursor.row + 1 < rows {
            cursor.row += 1
        }
    }

    /// RI: up one line, scrolling the region if already at its top.
    private func reverseIndex() {
        wrapPending = false
        if cursor.row == scrollTop {
            let (top, bottom) = (scrollTop, scrollBottom)
            withActiveGrid { $0.scrollDown(top: top, bottom: bottom, count: 1) }
        } else if cursor.row > 0 {
            cursor.row -= 1
        }
    }

    // MARK: - ESC

    private func esc(_ final: Character) {
        switch final {
        case "c": reset()
        case "D": indexDown()
        case "M": reverseIndex()
        case "E": moveCursor(row: cursor.row + 1, column: 0)
        case "7": saveCursor()
        case "8": restoreCursor()
        default: break
        }
    }

    private func saveCursor() {
        if usingAlternateScreen { alternateSavedCursor = cursor } else { savedCursor = cursor }
    }

    private func restoreCursor() {
        cursor = usingAlternateScreen ? alternateSavedCursor : savedCursor
        wrapPending = false
        clampCursor()
    }

    public func reset() {
        primary = Grid(columns: columns, rows: rows, scrollbackLimit: scrollbackLimit)
        alternate = Grid(columns: columns, rows: rows, scrollbackLimit: 0)
        usingAlternateScreen = false
        cursor = Cursor()
        savedCursor = Cursor()
        alternateSavedCursor = Cursor()
        wrapPending = false
        lastPrinted = nil
        modes = TerminalModes()
        cursorShape = .block
        scrollTop = 0
        scrollBottom = rows - 1
        title = ""
        responses.removeAll()
    }

    // MARK: - OSC

    private func osc(_ parameters: [[UInt8]]) {
        // OSC 0 and 2 both set the window title.
        guard parameters.count >= 2, let code = String(bytes: parameters[0], encoding: .utf8),
              code == "0" || code == "2" else { return }
        title = String(decoding: parameters[1], as: UTF8.self)
    }

    // MARK: - CSI

    private func csi(parameters: [[Int]], intermediates: [UInt8], final: Character) {
        // DECSCUSR is `CSI Ps SP q` -- the space intermediate is what
        // tells it apart from other `q` finals.
        if intermediates == [0x20], final == "q" {
            switch parameters.first?.first ?? 0 {
            case 0, 1, 2: cursorShape = .block
            case 3, 4: cursorShape = .underline
            case 5, 6: cursorShape = .bar
            default: break
            }
            return
        }
        if intermediates.first == 0x3F { // '?'
            privateMode(parameters: parameters, final: final)
            return
        }
        guard intermediates.isEmpty else { return }

        func parameter(_ index: Int, default defaultValue: Int) -> Int {
            guard let value = parameters[safe: index]?.first, value != 0 else { return defaultValue }
            return value
        }

        switch final {
        case "A": moveCursor(row: cursor.row - parameter(0, default: 1), column: cursor.column)
        case "B", "e": moveCursor(row: cursor.row + parameter(0, default: 1), column: cursor.column)
        case "C", "a": moveCursor(row: cursor.row, column: cursor.column + parameter(0, default: 1))
        case "D": moveCursor(row: cursor.row, column: cursor.column - parameter(0, default: 1))
        case "E": moveCursor(row: cursor.row + parameter(0, default: 1), column: 0)
        case "F": moveCursor(row: cursor.row - parameter(0, default: 1), column: 0)
        case "G", "`": moveCursor(row: cursor.row, column: parameter(0, default: 1) - 1)
        case "d": moveCursor(row: parameter(0, default: 1) - 1, column: cursor.column)
        case "H", "f": moveCursor(row: parameter(0, default: 1) - 1, column: parameter(1, default: 1) - 1)
        case "J": eraseInDisplay(mode: parameters[safe: 0]?.first ?? 0)
        case "K": eraseInLine(mode: parameters[safe: 0]?.first ?? 0)
        case "@": insertCharacters(parameter(0, default: 1))
        case "P": deleteCharacters(parameter(0, default: 1))
        case "X": eraseCharacters(parameter(0, default: 1))
        case "L": insertLines(parameter(0, default: 1))
        case "M": deleteLines(parameter(0, default: 1))
        case "S":
            let (top, bottom, n) = (scrollTop, scrollBottom, parameter(0, default: 1))
            withActiveGrid { $0.scrollUp(top: top, bottom: bottom, count: n) }
        case "T":
            let (top, bottom, n) = (scrollTop, scrollBottom, parameter(0, default: 1))
            withActiveGrid { $0.scrollDown(top: top, bottom: bottom, count: n) }
        case "b": // REP -- repeat the last printed character
            if let character = lastPrinted {
                for _ in 0..<min(parameter(0, default: 1), columns * rows) { printCharacter(character) }
            }
        case "Z": // CBT -- back to the previous tab stop
            wrapPending = false
            for _ in 0..<parameter(0, default: 1) {
                if cursor.column == 0 { break }
                cursor.column = (cursor.column - 1) / 8 * 8
            }
        case "m": applySGR(parameters)
        case "r":
            setScrollRegion(top: parameter(0, default: 1) - 1, bottom: parameter(1, default: rows) - 1)
        case "s": saveCursor()
        case "u": restoreCursor()
        case "n":
            switch parameters[safe: 0]?.first ?? 0 {
            case 5: queue("\u{1B}[0n") // "operating normally"
            case 6: queue("\u{1B}[\(cursor.row + 1);\(cursor.column + 1)R") // cursor position
            default: break
            }
        case "c": queue("\u{1B}[?6c") // DA1: a VT102-class terminal
        default: break
        }
    }

    private func queue(_ text: String) {
        responses.append(contentsOf: Array(text.utf8))
    }

    private func privateMode(parameters: [[Int]], final: Character) {
        let enable = final == "h"
        for parameter in parameters {
            switch parameter.first ?? 0 {
            case 1: modes.applicationCursorKeys = enable
            case 25: modes.cursorVisible = enable
            case 47: setAlternateScreen(enable, saveRestoreCursor: false)
            case 1049: setAlternateScreen(enable, saveRestoreCursor: true)
            case 2004: modes.bracketedPaste = enable
            case 1000: modes.mouseMode = enable ? .clicks : .off
            case 1002: modes.mouseMode = enable ? .drag : .off
            case 1003: modes.mouseMode = enable ? .motion : .off
            case 1006: modes.mouseSGR = enable
            default: break // focus events, sync updates, other encodings
            }
        }
    }

    /// Enter or leave the alternate screen (DEC modes 47 / 1049). 1049
    /// also saves and restores the cursor, matching xterm; the older bare
    /// 47 does not.
    private func setAlternateScreen(_ enable: Bool, saveRestoreCursor: Bool) {
        guard enable != usingAlternateScreen else { return }
        if enable {
            if saveRestoreCursor { saveCursor() }
            usingAlternateScreen = true
            alternate.clearAll()
            cursor = Cursor()
        } else {
            usingAlternateScreen = false
            if saveRestoreCursor { restoreCursor() }
        }
        wrapPending = false
    }

    private func setScrollRegion(top: Int, bottom: Int) {
        let top = min(max(0, top), rows - 1)
        let bottom = min(max(0, bottom), rows - 1)
        if top < bottom {
            scrollTop = top
            scrollBottom = bottom
        } else {
            scrollTop = 0
            scrollBottom = rows - 1
        }
        moveCursor(row: 0, column: 0)
    }

    // MARK: - Erasing and editing

    private func eraseInDisplay(mode: Int) {
        let (row, column, columns, rows) = (cursor.row, cursor.column, columns, rows)
        withActiveGrid { grid in
            switch mode {
            case 0:
                for c in column..<columns { grid[row, c] = .blank }
                // The row's tail is blank now, so it can't be a wrap
                // continuation of the next one.
                grid.setWrapped(row, false)
                for r in (row + 1)..<rows {
                    for c in 0..<columns { grid[r, c] = .blank }
                    grid.setWrapped(r, false)
                }
            case 1:
                for r in 0..<row {
                    for c in 0..<columns { grid[r, c] = .blank }
                    grid.setWrapped(r, false)
                }
                for c in 0...min(column, columns - 1) { grid[row, c] = .blank }
            case 2:
                grid.clearAll()
            case 3:
                // ED 3 clears scrollback, not the screen -- the modern
                // `clear` sends CSI H, CSI 2J, CSI 3J expecting exactly
                // this split.
                grid.clearScrollback()
            default:
                break
            }
        }
    }

    private func eraseInLine(mode: Int) {
        // The cursor column is the boundary every mode erases to or from,
        // so it's where a wide character can be cut in half.
        if mode != 2 { clearWidePair(row: cursor.row, column: cursor.column) }
        let (row, column, columns) = (cursor.row, cursor.column, columns)
        withActiveGrid { grid in
            switch mode {
            case 0:
                for c in column..<columns { grid[row, c] = .blank }
                grid.setWrapped(row, false)
            case 1:
                for c in 0...min(column, columns - 1) { grid[row, c] = .blank }
            case 2:
                for c in 0..<columns { grid[row, c] = .blank }
                grid.setWrapped(row, false)
            default:
                break
            }
        }
    }

    /// ICH: insert blanks at the cursor, shifting the rest of the row
    /// right. This is what a shell's line editor sends when a character
    /// is typed into the middle of a line -- without it the display
    /// drifts from the shell's own idea of the line on every edit.
    private func insertCharacters(_ n: Int) {
        let (row, column, columns) = (cursor.row, cursor.column, columns)
        let n = min(n, columns - column)
        guard n > 0 else { return }
        withActiveGrid { grid in
            for c in stride(from: columns - 1, through: column + n, by: -1) {
                grid[row, c] = grid[row, c - n]
            }
            for c in column..<(column + n) { grid[row, c] = .blank }
        }
    }

    /// DCH: delete at the cursor, shifting the rest of the row left and
    /// blanking the tail -- the counterpart to `insertCharacters`.
    private func deleteCharacters(_ n: Int) {
        let (row, column, columns) = (cursor.row, cursor.column, columns)
        let n = min(n, columns - column)
        guard n > 0 else { return }
        withActiveGrid { grid in
            for c in column..<(columns - n) { grid[row, c] = grid[row, c + n] }
            for c in (columns - n)..<columns { grid[row, c] = .blank }
        }
    }

    /// ECH: blank cells in place, without shifting.
    private func eraseCharacters(_ n: Int) {
        let (row, column, columns) = (cursor.row, cursor.column, columns)
        let end = min(column + n, columns)
        guard end > column else { return }
        clearWidePair(row: row, column: column)
        clearWidePair(row: row, column: end - 1)
        withActiveGrid { grid in
            for c in column..<end { grid[row, c] = .blank }
        }
    }

    /// IL: insert blank lines at the cursor row, pushing the rest down
    /// and off the bottom of the scroll region. Ignored, per DEC, when
    /// the cursor is outside the region.
    private func insertLines(_ n: Int) {
        guard cursor.row >= scrollTop, cursor.row <= scrollBottom else { return }
        let (row, bottom) = (cursor.row, scrollBottom)
        withActiveGrid { $0.scrollDown(top: row, bottom: bottom, count: n) }
        wrapPending = false
        cursor.column = 0
    }

    /// DL: delete lines at the cursor row, pulling the rest up.
    private func deleteLines(_ n: Int) {
        guard cursor.row >= scrollTop, cursor.row <= scrollBottom else { return }
        let (row, bottom) = (cursor.row, scrollBottom)
        withActiveGrid { $0.deleteLines(top: row, bottom: bottom, count: n) }
        wrapPending = false
        cursor.column = 0
    }

    // MARK: - SGR

    private func applySGR(_ parameters: [[Int]]) {
        guard !parameters.isEmpty else {
            resetGraphics()
            return
        }
        var index = 0
        while index < parameters.count {
            let group = parameters[index]
            let code = group.first ?? 0
            switch code {
            case 0: resetGraphics()
            case 1: cursor.flags.insert(.bold)
            case 3: cursor.flags.insert(.italic)
            case 4: cursor.flags.insert(.underline)
            case 7: cursor.flags.insert(.reverse)
            case 22: cursor.flags.remove(.bold)
            case 23: cursor.flags.remove(.italic)
            case 24: cursor.flags.remove(.underline)
            case 27: cursor.flags.remove(.reverse)
            case 30...37: cursor.foreground = .indexed(UInt8(code - 30))
            case 39: cursor.foreground = .default
            case 40...47: cursor.background = .indexed(UInt8(code - 40))
            case 49: cursor.background = .default
            case 90...97: cursor.foreground = .indexed(UInt8(code - 90 + 8))
            case 100...107: cursor.background = .indexed(UInt8(code - 100 + 8))
            case 38, 48:
                let (color, consumed) = extendedColor(group: group, parameters: parameters, at: index)
                if let color { if code == 38 { cursor.foreground = color } else { cursor.background = color } }
                index += consumed
            default: break
            }
            index += 1
        }
    }

    private func resetGraphics() {
        cursor.foreground = .default
        cursor.background = .default
        cursor.flags = []
    }

    /// Parse the arguments after a `38`/`48`. Both spellings occur: the
    /// colon form (`38:2:r:g:b`) arrives as one group, the far more
    /// common semicolon form (`38;2;r;g;b`) as separate ones -- hence the
    /// consumed count, so the caller can skip what was read.
    private func extendedColor(group: [Int], parameters: [[Int]], at index: Int) -> (TerminalColor?, Int) {
        if group.count >= 2 {
            switch group[1] {
            case 5 where group.count >= 3:
                return (.indexed(UInt8(clamping: group[2])), 0)
            case 2 where group.count >= 5:
                return (.rgb(UInt8(clamping: group[2]), UInt8(clamping: group[3]), UInt8(clamping: group[4])), 0)
            default:
                return (nil, 0)
            }
        }
        switch parameters[safe: index + 1]?.first {
        case 5:
            guard let value = parameters[safe: index + 2]?.first else { return (nil, 1) }
            return (.indexed(UInt8(clamping: value)), 2)
        case 2:
            guard let r = parameters[safe: index + 2]?.first,
                  let g = parameters[safe: index + 3]?.first,
                  let b = parameters[safe: index + 4]?.first else { return (nil, 1) }
            return (.rgb(UInt8(clamping: r), UInt8(clamping: g), UInt8(clamping: b)), 4)
        default:
            return (nil, 0)
        }
    }
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
