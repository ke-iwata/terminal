/// What the parser recognised. The parser knows escape-sequence
/// *syntax* and nothing about screens or cursors; `Terminal` decides what
/// each one means.
public enum ParserAction: Equatable, Sendable {
    /// A printable character.
    case print(Character)
    /// A C0 control byte (CR, LF, BS, HT, BEL, ...).
    case execute(UInt8)
    /// `CSI params intermediates final`. Parameters are already split on
    /// `;`, with sub-parameters (split on `:`) kept together so
    /// `38:2:r:g:b` arrives as one group.
    case csi(parameters: [[Int]], intermediates: [UInt8], final: Character)
    /// `ESC intermediates final`.
    case esc(intermediates: [UInt8], final: Character)
    /// `OSC params ST`, split on `;`.
    case osc([[UInt8]])
}

/// A byte-at-a-time VT/ANSI parser, structured after the DEC state
/// diagram every terminal uses (the same one the `vte` crate implements).
///
/// Byte-at-a-time and resumable on purpose: input arrives from a pty in
/// arbitrary chunks, and an escape sequence -- or a multi-byte UTF-8
/// character -- is routinely split across two reads.
public struct Parser: Sendable {
    private enum State: Sendable {
        case ground
        case escape
        case escapeIntermediate
        case csiEntry
        case csiParam
        case csiIntermediate
        /// A sequence we recognise the shape of but don't implement;
        /// consumed to its terminator so its bytes never reach the screen.
        case csiIgnore
        case oscString
        /// DCS and friends: skipped wholesale until ST.
        case dcsIgnore
    }

    private var state: State = .ground
    private var parameters: [[Int]] = []
    private var currentParameter: [Int] = []
    private var intermediates: [UInt8] = []
    private var oscParameters: [[UInt8]] = []
    private var currentOSC: [UInt8] = []
    private var utf8 = UTF8Decoder()

    /// Guard against a runaway sequence (a binary file catted to the
    /// terminal) growing these buffers without bound.
    private static let maxParameters = 32
    private static let maxOSCBytes = 8192

    public init() {}

    /// Feed bytes, calling `handler` for each action recognised.
    public mutating func advance(_ bytes: [UInt8], _ handler: (ParserAction) -> Void) {
        for byte in bytes {
            step(byte, handler)
        }
    }

    private mutating func step(_ byte: UInt8, _ handler: (ParserAction) -> Void) {
        // ESC, CAN and SUB abort whatever was in progress from any state
        // -- that is how a terminal recovers from a truncated sequence
        // instead of swallowing everything after it.
        switch byte {
        case 0x1B:
            beginEscape()
            return
        case 0x18, 0x1A:
            state = .ground
            utf8.reset()
            return
        default:
            break
        }

        switch state {
        case .ground:
            ground(byte, handler)
        case .escape:
            escape(byte, handler)
        case .escapeIntermediate:
            if (0x20...0x2F).contains(byte) {
                intermediates.append(byte)
            } else if (0x30...0x7E).contains(byte) {
                handler(.esc(intermediates: intermediates, final: Character(UnicodeScalar(byte))))
                state = .ground
            }
        case .csiEntry, .csiParam, .csiIntermediate:
            csi(byte, handler)
        case .csiIgnore:
            if (0x40...0x7E).contains(byte) { state = .ground }
        case .oscString:
            osc(byte, handler)
        case .dcsIgnore:
            // ST is ESC \, and the ESC is caught above; a bare BEL also
            // ends these in practice.
            if byte == 0x07 { state = .ground }
        }
    }

    private mutating func beginEscape() {
        state = .escape
        parameters = []
        currentParameter = []
        intermediates = []
        oscParameters = []
        currentOSC = []
        utf8.reset()
    }

    private mutating func ground(_ byte: UInt8, _ handler: (ParserAction) -> Void) {
        if byte < 0x20 || byte == 0x7F {
            handler(.execute(byte))
            return
        }
        // Everything else is text, decoded as UTF-8 across calls.
        if let scalar = utf8.push(byte) {
            handler(.print(Character(scalar)))
        }
    }

    private mutating func escape(_ byte: UInt8, _ handler: (ParserAction) -> Void) {
        switch byte {
        case 0x5B: // [
            state = .csiEntry
        case 0x5D: // ]
            state = .oscString
        case 0x50, 0x58, 0x5E, 0x5F: // P (DCS), X (SOS), ^ (PM), _ (APC)
            state = .dcsIgnore
        case 0x20...0x2F:
            intermediates.append(byte)
            state = .escapeIntermediate
        case 0x30...0x7E:
            handler(.esc(intermediates: [], final: Character(UnicodeScalar(byte))))
            state = .ground
        default:
            state = .ground
        }
    }

    private mutating func csi(_ byte: UInt8, _ handler: (ParserAction) -> Void) {
        switch byte {
        case 0x30...0x39: // digit
            if currentParameter.isEmpty { currentParameter = [0] }
            let last = currentParameter.count - 1
            // Saturate rather than overflow on an absurd parameter.
            currentParameter[last] = min(currentParameter[last] &* 10 &+ Int(byte - 0x30), 65535)
            state = .csiParam
        case 0x3A: // ':' -- sub-parameter, stays in the same group
            currentParameter.append(0)
            state = .csiParam
        case 0x3B: // ';' -- next parameter
            pushParameter()
            state = .csiParam
        case 0x3C...0x3F: // private markers ? > = <
            if state == .csiEntry {
                intermediates.append(byte)
                state = .csiParam
            } else {
                state = .csiIgnore
            }
        case 0x20...0x2F: // intermediates
            intermediates.append(byte)
            state = .csiIntermediate
        case 0x40...0x7E: // final byte
            pushParameter()
            handler(.csi(parameters: parameters, intermediates: intermediates, final: Character(UnicodeScalar(byte))))
            state = .ground
        default:
            state = .csiIgnore
        }
    }

    private mutating func pushParameter() {
        if parameters.count < Self.maxParameters {
            parameters.append(currentParameter.isEmpty ? [0] : currentParameter)
        }
        currentParameter = []
    }

    private mutating func osc(_ byte: UInt8, _ handler: (ParserAction) -> Void) {
        if byte == 0x07 { // BEL terminates, the common form
            oscParameters.append(currentOSC)
            handler(.osc(oscParameters))
            state = .ground
            return
        }
        if byte == 0x3B { // ';'
            oscParameters.append(currentOSC)
            currentOSC = []
            return
        }
        if currentOSC.count < Self.maxOSCBytes { currentOSC.append(byte) }
    }
}

/// Incremental UTF-8 decoding.
///
/// Its own thing rather than `String(decoding:)` because a pty read can
/// end mid-character: the leading bytes have to be held until the rest
/// arrives, not turned into replacement characters.
struct UTF8Decoder: Sendable {
    private var pending: [UInt8] = []
    private var needed = 0

    mutating func reset() {
        pending.removeAll()
        needed = 0
    }

    /// Feed one byte; returns a scalar once a complete character has
    /// arrived. Malformed input yields U+FFFD rather than being dropped,
    /// so a binary file catted to the screen still advances the cursor
    /// the way other terminals show it.
    mutating func push(_ byte: UInt8) -> UnicodeScalar? {
        if needed == 0 {
            switch byte {
            case 0x00...0x7F:
                return UnicodeScalar(byte)
            case 0xC2...0xDF:
                pending = [byte]; needed = 1
            case 0xE0...0xEF:
                pending = [byte]; needed = 2
            case 0xF0...0xF4:
                pending = [byte]; needed = 3
            default:
                return UnicodeScalar(0xFFFD)
            }
            return nil
        }

        // Continuation bytes only; anything else means the sequence was
        // truncated, and the offending byte still needs handling.
        guard (0x80...0xBF).contains(byte) else {
            reset()
            return UnicodeScalar(0xFFFD)
        }
        pending.append(byte)
        needed -= 1
        guard needed == 0 else { return nil }

        let bytes = pending
        reset()
        var scalar: UInt32
        switch bytes.count {
        case 2: scalar = UInt32(bytes[0] & 0x1F)
        case 3: scalar = UInt32(bytes[0] & 0x0F)
        default: scalar = UInt32(bytes[0] & 0x07)
        }
        for continuation in bytes.dropFirst() {
            scalar = (scalar << 6) | UInt32(continuation & 0x3F)
        }
        return UnicodeScalar(scalar) ?? UnicodeScalar(0xFFFD)
    }
}
