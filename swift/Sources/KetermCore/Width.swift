/// How many terminal columns a character occupies.
///
/// A table rather than libc's `wcwidth`, which answers from the process
/// locale: the same character would measure differently depending on an
/// environment variable, and every column calculation in the grid --
/// where a wide character's second half sits, where the cursor lands
/// after printing -- has to agree with what the *application* on the
/// other side of the pty assumed. Applications use the Unicode East
/// Asian Width property, so this does too.
extension Character {
    public var terminalColumns: Int {
        // A grapheme cluster's width is its base scalar's; the combining
        // marks after it add nothing.
        guard let scalar = unicodeScalars.first else { return 0 }
        return scalar.terminalColumns
    }
}

extension Unicode.Scalar {
    public var terminalColumns: Int {
        let value = self.value

        // C0/C1 controls occupy nothing -- they're commands, not text,
        // and the grid never stores them anyway.
        if value < 0x20 || (0x7F...0x9F).contains(value) { return 0 }

        // Combining marks, variation selectors, and other zero-width
        // scalars attach to the character before them.
        if Self.isZeroWidth(value) { return 0 }

        return Self.isWide(value) ? 2 : 1
    }

    private static func isZeroWidth(_ value: UInt32) -> Bool {
        switch value {
        case 0x0300...0x036F, // combining diacritical marks
             0x0483...0x0489,
             0x0591...0x05BD, 0x05BF, 0x05C1...0x05C2, 0x05C4...0x05C5, 0x05C7,
             0x0610...0x061A, 0x064B...0x065F, 0x0670,
             0x06D6...0x06DC, 0x06DF...0x06E4, 0x06E7...0x06E8, 0x06EA...0x06ED,
             0x0900...0x0902, 0x093C, 0x0941...0x0948, 0x094D,
             0x0951...0x0957, 0x0962...0x0963,
             0x0E31, 0x0E34...0x0E3A, 0x0E47...0x0E4E,
             0x1AB0...0x1AFF, // combining diacriticals extended
             0x1DC0...0x1DFF,
             0x200B...0x200F, // zero-width space through RTL mark
             0x2028...0x202E, // line/paragraph separators, bidi overrides
             0x2060...0x2064, // word joiner and invisible operators
             0x20D0...0x20F0, // combining marks for symbols
             0xFE00...0xFE0F, // variation selectors
             0xFE20...0xFE2F, // combining half marks
             0xFEFF,          // BOM / zero-width no-break space
             0xE0100...0xE01EF: // variation selectors supplement
            return true
        default:
            return false
        }
    }

    /// The Unicode East Asian Wide (W) and Fullwidth (F) ranges.
    private static func isWide(_ value: UInt32) -> Bool {
        switch value {
        case 0x1100...0x115F,   // Hangul Jamo initial consonants
             0x2E80...0x2EF3,   // CJK radicals supplement
             0x2F00...0x2FD5,   // Kangxi radicals
             0x2FF0...0x2FFB,   // ideographic description characters
             0x3000...0x303E,   // CJK symbols and punctuation
             0x3041...0x3096,   // Hiragana
             0x3099...0x30FF,   // combining marks, Katakana
             0x3105...0x312D,   // Bopomofo
             0x3131...0x318E,   // Hangul compatibility Jamo
             0x3190...0x31BA,   // Kanbun, Bopomofo extended
             0x31C0...0x31E3,   // CJK strokes
             0x31F0...0x321E,   // Katakana phonetic extensions, enclosed CJK
             0x3220...0x3247,
             0x3250...0x32FE,
             0x3300...0x4DBF,   // CJK compatibility, extension A
             0x4E00...0xA48C,   // CJK unified ideographs, Yi
             0xA490...0xA4C6,
             0xA960...0xA97C,   // Hangul Jamo extended-A
             0xAC00...0xD7A3,   // Hangul syllables
             0xF900...0xFAFF,   // CJK compatibility ideographs
             0xFE10...0xFE19,   // vertical forms
             0xFE30...0xFE52,   // CJK compatibility forms
             0xFE54...0xFE66,
             0xFE68...0xFE6B,
             0xFF01...0xFF60,   // fullwidth forms
             0xFFE0...0xFFE6,
             0x16FE0...0x16FE4, // Tangut, Nushu
             0x17000...0x18AFF,
             0x1B000...0x1B2FB, // Kana supplement/extended, Nushu
             0x1F004, 0x1F0CF,
             0x1F18E, 0x1F191...0x1F19A,
             0x1F200...0x1F320, // enclosed ideographic supplement, emoji
             0x1F32D...0x1F335,
             0x1F337...0x1F37C,
             0x1F37E...0x1F393,
             0x1F3A0...0x1F3CA,
             0x1F3CF...0x1F3D3,
             0x1F3E0...0x1F3F0,
             0x1F3F4,
             0x1F3F8...0x1F43E,
             0x1F440,
             0x1F442...0x1F4FC,
             0x1F4FF...0x1F53D,
             0x1F54B...0x1F54E,
             0x1F550...0x1F567,
             0x1F57A, 0x1F595...0x1F596, 0x1F5A4,
             0x1F5FB...0x1F64F,
             0x1F680...0x1F6C5,
             0x1F6CC,
             0x1F6D0...0x1F6D2,
             0x1F6EB...0x1F6EC,
             0x1F6F4...0x1F6FC,
             0x1F7E0...0x1F7EB,
             0x1F90C...0x1F93A,
             0x1F93C...0x1F945,
             0x1F947...0x1F9FF,
             0x1FA70...0x1FAFF,
             0x20000...0x3FFFD: // CJK extensions B and beyond
            return true
        default:
            return false
        }
    }
}
