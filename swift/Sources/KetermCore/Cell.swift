/// A character's color, kept symbolic rather than resolved to RGB so the
/// palette can change (a theme switch, a live settings save) without
/// rewriting every cell already on screen.
public enum TerminalColor: Equatable, Sendable {
    /// The palette's foreground or background, depending on which slot
    /// this color sits in.
    case `default`
    /// One of the 256 indexed colors: 0-7 normal, 8-15 bright, then the
    /// 6x6x6 cube and the greyscale ramp.
    case indexed(UInt8)
    case rgb(UInt8, UInt8, UInt8)
}

public struct CellFlags: OptionSet, Sendable {
    public let rawValue: UInt8
    public init(rawValue: UInt8) { self.rawValue = rawValue }

    public static let bold = CellFlags(rawValue: 1 << 0)
    public static let italic = CellFlags(rawValue: 1 << 1)
    public static let underline = CellFlags(rawValue: 1 << 2)
    public static let reverse = CellFlags(rawValue: 1 << 3)
    /// First column of a double-width glyph.
    public static let wide = CellFlags(rawValue: 1 << 4)
    /// Trailing placeholder column following a `wide` cell. Carries a
    /// space so it draws as nothing, and is skipped when extracting text
    /// -- it isn't a character, it's the other half of one.
    public static let wideSpacer = CellFlags(rawValue: 1 << 5)
}

public struct Cell: Equatable, Sendable {
    public var character: Character
    public var foreground: TerminalColor
    public var background: TerminalColor
    public var flags: CellFlags

    public init(
        character: Character = " ",
        foreground: TerminalColor = .default,
        background: TerminalColor = .default,
        flags: CellFlags = []
    ) {
        self.character = character
        self.foreground = foreground
        self.background = background
        self.flags = flags
    }

    public static let blank = Cell()

    /// How many columns this cell's character occupies. The trailing half
    /// of a wide character reports 0: it is not a character of its own.
    public var columns: Int {
        if flags.contains(.wideSpacer) { return 0 }
        return flags.contains(.wide) ? 2 : 1
    }
}
