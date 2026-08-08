/// The character grid: the live screen plus, for the primary screen, the
/// scrollback behind it.
///
/// Rows are addressed two ways, deliberately. `row(_:)` indexes the live
/// screen top-down, which is what the parser works in. Selections and
/// search results instead store a *distance from the bottom* (see
/// `distanceFromBottom`), because a screen row number stops meaning
/// anything the moment the view scrolls, while "three lines above the
/// live bottom" keeps pointing at the same text.
public struct Grid: Sendable {
    public private(set) var columns: Int
    public private(set) var rows: Int
    private var lines: [[Cell]]
    /// Parallel to `lines`: `wrapped[i]` means row `i` runs on into row
    /// `i + 1` because printing hit the right margin, as opposed to the
    /// program having sent a newline. Reflowing a column-count change
    /// needs this to know which rows are one logical line.
    private var wrapped: [Bool]
    public private(set) var scrollback: [[Cell]] = []
    public var scrollbackLimit: Int

    /// How many rows at the end of `scrollback` were hidden by a resize
    /// shrink and are still safe for a later grow to pull back, paired
    /// with the exact `scrollback.count` they were pushed at.
    ///
    /// Both are needed: without the count, a grow after the shell printed
    /// something in between would pop that new output back onto the
    /// screen instead of the row the shrink hid -- text already visible
    /// elsewhere reappearing at the top. Found the hard way in the Rust
    /// version; the pairing is what makes "is the tail still ours?"
    /// answerable.
    private var rowsPendingRestore = 0
    private var scrollbackCountWhenPending = 0

    public init(columns: Int, rows: Int, scrollbackLimit: Int) {
        self.columns = max(1, columns)
        self.rows = max(1, rows)
        self.scrollbackLimit = scrollbackLimit
        self.lines = Array(repeating: Array(repeating: Cell.blank, count: self.columns), count: self.rows)
        self.wrapped = Array(repeating: false, count: self.rows)
    }

    private var blankRow: [Cell] { Array(repeating: Cell.blank, count: columns) }

    public func row(_ index: Int) -> [Cell] { lines[index] }

    public subscript(row: Int, column: Int) -> Cell {
        get { lines[row][column] }
        set { lines[row][column] = newValue }
    }

    /// Mark whether printing wrapped off the end of `row` onto the next.
    public mutating func setWrapped(_ row: Int, _ value: Bool) {
        guard lines.indices.contains(row) else { return }
        wrapped[row] = value
    }

    /// Resolve a viewport row to its source line, accounting for how far
    /// `scrollOffset` has scrolled back. `viewRow` is clamped to
    /// `0..<rows` by the caller.
    public func line(viewRow: Int, scrollOffset: Int) -> [Cell] {
        let offset = min(scrollOffset, scrollback.count)
        if viewRow < offset {
            return scrollback[scrollback.count - offset + viewRow]
        }
        return lines[viewRow - offset]
    }

    /// A viewport row's distance from the live bottom: 0 is the bottom
    /// row, `rows - 1` the top one, `rows` the most recently scrolled-off
    /// line, and so on back through scrollback.
    public func distanceFromBottom(viewRow: Int, scrollOffset: Int) -> Int {
        scrollOffset + (rows - 1 - viewRow)
    }

    /// The inverse of `distanceFromBottom`, or `nil` once that line has
    /// fallen out of scrollback.
    public func absoluteLine(distanceFromBottom distance: Int) -> [Cell]? {
        if distance < rows {
            return lines[rows - 1 - distance]
        }
        let k = distance - rows
        guard k < scrollback.count else { return nil }
        return scrollback[scrollback.count - 1 - k]
    }

    /// Total addressable lines: the live screen plus scrollback.
    public var totalLines: Int { rows + scrollback.count }

    public mutating func clearAll() {
        for i in lines.indices { lines[i] = blankRow }
        for i in wrapped.indices { wrapped[i] = false }
    }

    public mutating func clearScrollback() {
        scrollback.removeAll()
        rowsPendingRestore = 0
    }

    public mutating func setScrollbackLimit(_ limit: Int) {
        scrollbackLimit = limit
        while scrollback.count > scrollbackLimit { scrollback.removeFirst() }
    }

    // MARK: - Scrolling

    /// Scroll `[top, bottom]` up by `n`, blanking the vacated rows.
    /// Lines leaving the top are kept in scrollback only when the region
    /// spans the whole screen: a scroll confined to a region (what vim
    /// does inside a split) never really left the screen, and pushing it
    /// would pollute the history with text the user can still see.
    public mutating func scrollUp(top: Int, bottom: Int, count n: Int) {
        rotateUp(top: top, bottom: bottom, count: n, keepingHistory: top == 0 && bottom == rows - 1)
    }

    /// Delete lines (DL): the same rotation, but the removed lines are
    /// discarded. An app deleting lines out of the middle of the screen
    /// isn't scrolling them off, and they must not turn up in scrollback.
    public mutating func deleteLines(top: Int, bottom: Int, count n: Int) {
        rotateUp(top: top, bottom: bottom, count: n, keepingHistory: false)
    }

    private mutating func rotateUp(top: Int, bottom: Int, count n: Int, keepingHistory: Bool) {
        let regionLength = bottom + 1 - top
        let n = min(n, regionLength)
        guard n > 0 else { return }
        for _ in 0..<n {
            let removed = lines.remove(at: top)
            wrapped.remove(at: top)
            if keepingHistory {
                scrollback.append(removed)
                if scrollback.count > scrollbackLimit { scrollback.removeFirst() }
            }
            lines.insert(blankRow, at: bottom)
            wrapped.insert(false, at: bottom)
        }
    }

    /// Scroll `[top, bottom]` down by `n` (reverse index), blanking the
    /// vacated rows at the top.
    public mutating func scrollDown(top: Int, bottom: Int, count n: Int) {
        let regionLength = bottom + 1 - top
        let n = min(n, regionLength)
        guard n > 0 else { return }
        for _ in 0..<n {
            lines.remove(at: bottom)
            wrapped.remove(at: bottom)
            lines.insert(blankRow, at: top)
            wrapped.insert(false, at: top)
        }
    }

    // MARK: - Resize

    /// Resize without reflowing: rows and columns are truncated or padded
    /// with blanks. Used for the alternate screen, where full-screen apps
    /// redraw themselves on `SIGWINCH` instead of expecting the terminal
    /// to preserve anything.
    public mutating func resizeTruncating(columns newColumns: Int, rows newRows: Int) {
        let newColumns = max(1, newColumns), newRows = max(1, newRows)
        guard newColumns != columns || newRows != rows else { return }
        for i in lines.indices {
            if lines[i].count > newColumns {
                lines[i].removeLast(lines[i].count - newColumns)
            } else {
                lines[i].append(contentsOf: Array(repeating: Cell.blank, count: newColumns - lines[i].count))
            }
        }
        columns = newColumns
        if lines.count > newRows {
            lines.removeLast(lines.count - newRows)
            wrapped.removeLast(wrapped.count - newRows)
        } else {
            lines.append(contentsOf: Array(repeating: blankRow, count: newRows - lines.count))
            wrapped.append(contentsOf: Array(repeating: false, count: newRows - wrapped.count))
        }
        rows = newRows
    }

    /// Resize the primary screen, reflowing rather than destroying:
    /// rows linked by `wrapped` are one logical line, re-wrapped at the
    /// new width, so narrowing the window moves text onto more rows
    /// instead of cutting it off -- and widening puts it back. Rows that
    /// no longer fit move through scrollback rather than being dropped.
    ///
    /// Takes and returns the cursor's `(row, column)` so it stays on the
    /// same character across the reflow.
    public mutating func resizeReflowing(columns newColumns: Int, rows newRows: Int, cursor: (row: Int, column: Int)) -> (row: Int, column: Int) {
        let newColumns = max(1, newColumns), newRows = max(1, newRows)
        guard newColumns != columns || newRows != rows else { return cursor }

        var placed = cursor
        if newColumns != columns {
            placed = reflowColumns(to: newColumns, cursor: cursor)
        }
        placed.row = adjustRowCount(to: newRows, cursorRow: placed.row)
        columns = newColumns
        rows = newRows
        return (min(placed.row, newRows - 1), min(placed.column, newColumns - 1))
    }

    private mutating func reflowColumns(to newColumns: Int, cursor: (row: Int, column: Int)) -> (row: Int, column: Int) {
        let cursorRow = min(cursor.row, max(0, lines.count - 1))
        let cursorColumn = min(cursor.column, max(0, columns - 1))

        // Gather rows into logical lines, remembering where the cursor
        // falls within the flattened run.
        var logical: [[Cell]] = []
        var cursorLogical = 0
        var cursorOffset = 0
        var index = 0
        while index < lines.count {
            var cells: [Cell] = []
            let lineStart = logical.count
            while true {
                if index == cursorRow {
                    cursorLogical = lineStart
                    cursorOffset = cells.count + cursorColumn
                }
                cells.append(contentsOf: lines[index])
                let continues = index < wrapped.count && wrapped[index]
                index += 1
                if !continues || index >= lines.count { break }
            }
            // Trailing blanks are only meaningful at the very end of a
            // logical line -- interior rows are full by definition, which
            // is why they wrapped.
            while cells.last == Cell.blank { cells.removeLast() }
            if logical.count == cursorLogical { cursorOffset = min(cursorOffset, cells.count) }
            logical.append(cells)
        }

        // Drop trailing blank logical lines instead of re-wrapping each
        // into a padding row: keeping them would inflate the row count
        // every time a narrower resize adds wrapped rows, pushing real
        // content into scrollback for nothing. The one holding the cursor
        // stays, so it has somewhere to land.
        while let last = logical.last, last.isEmpty, logical.count - 1 != cursorLogical {
            logical.removeLast()
        }
        if logical.isEmpty {
            logical = [[]]
            cursorLogical = 0
            cursorOffset = 0
        }

        var rebuilt: [[Cell]] = []
        var rebuiltWrapped: [Bool] = []
        var newCursor = (row: 0, column: 0)

        for (logicalIndex, cells) in logical.enumerated() {
            let isCursorLine = logicalIndex == cursorLogical
            var offset = 0
            while true {
                let end = min(offset + newColumns, cells.count)
                var row = Array(cells[offset..<end])
                row.append(contentsOf: Array(repeating: Cell.blank, count: newColumns - row.count))

                if isCursorLine, cursorOffset >= offset, cursorOffset < offset + newColumns {
                    newCursor = (rebuilt.count, cursorOffset - offset)
                }
                let more = end < cells.count
                rebuiltWrapped.append(more)
                rebuilt.append(row)
                if !more {
                    if isCursorLine, cursorOffset >= end {
                        newCursor = (rebuilt.count - 1, min(cursorOffset - offset, newColumns - 1))
                    }
                    break
                }
                offset += newColumns
            }
        }
        if rebuilt.isEmpty {
            rebuilt = [Array(repeating: Cell.blank, count: newColumns)]
            rebuiltWrapped = [false]
        }

        lines = rebuilt
        wrapped = rebuiltWrapped
        columns = newColumns
        return newCursor
    }

    /// Grow or shrink the row count by moving rows through scrollback
    /// rather than truncating. Rows only ever leave or arrive at the top,
    /// where scrollback connects, so the cursor row shifts with them.
    private mutating func adjustRowCount(to newRows: Int, cursorRow: Int) -> Int {
        var cursorRow = cursorRow
        while lines.count > newRows {
            let removed = lines.removeFirst()
            wrapped.removeFirst()
            scrollback.append(removed)
            if scrollback.count > scrollbackLimit { scrollback.removeFirst() }
            rowsPendingRestore += 1
            scrollbackCountWhenPending = scrollback.count
            cursorRow = max(0, cursorRow - 1)
        }
        while lines.count < newRows {
            // Two conditions, both required. The width must still match:
            // scrollback isn't reflowed on a column change, and splicing
            // a differently-sized row back in would corrupt every
            // column-indexed access after it. And the tail must still be
            // what this shrink pushed -- see `rowsPendingRestore`.
            let widthMatches = scrollback.last?.count == columns
            let pendingIntact = rowsPendingRestore > 0 && scrollback.count == scrollbackCountWhenPending
            if widthMatches && pendingIntact {
                lines.insert(scrollback.removeLast(), at: 0)
                wrapped.insert(false, at: 0)
                rowsPendingRestore -= 1
                scrollbackCountWhenPending -= 1
                cursorRow += 1
            } else {
                rowsPendingRestore = 0
                // Padding at the bottom doesn't shift any row's index.
                lines.append(blankRow)
                wrapped.append(false)
            }
        }
        return cursorRow
    }
}
