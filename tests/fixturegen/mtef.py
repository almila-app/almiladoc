"""MathType (MTEF v5) and Equation Editor 3.x (MTEF v3) equations written from the MTEF
record, typeface and template tables, wrapped as the OLE objects Office embeds."""

import struct

from fixturegen.common import cfb_bytes

V3 = 3
V5 = 5
WINDOWS = 1
MATHTYPE = 0
EQUATION_EDITOR = 1
END = 0
LINE = 1
CHAR = 2
TMPL = 3
SIZE_FULL = 10
LINE_NULL = 0x01
EQUATION_INLINE = 0x01
TYPEFACE_BIAS = 128
FN_VARIABLE = 3
FN_SYMBOL = 6
FN_NUMBER = 8
V3_ROOT, V3_FRACT, V3_SCRIPT, V3_SUPER = 13, 14, 15, 0
V5_ROOT, V5_FRACT, V5_SUP = 10, 11, 28
EQNOLEFILEHDR_BYTES = 28
EQUATION_NATIVE = "Equation Native"
EQN_VERSION = 0x00020000
EQN_CLIPBOARD_FORMAT = 0xC1C6


class Mtef:
    def __init__(self, version: int) -> None:
        self.version = version

    def tag(self, kind: int, options: int = 0) -> bytes:
        if self.version == V3:
            return bytes([kind | options << 4])
        return bytes([kind, options])

    def line(self, *items: bytes) -> bytes:
        return self.tag(LINE) + b"".join(items) + bytes([END])

    def null_line(self) -> bytes:
        return self.tag(LINE, LINE_NULL)

    def chars(self, typeface: int, text: str) -> bytes:
        return b"".join(
            self.tag(CHAR) + struct.pack("<BH", typeface + TYPEFACE_BIAS, ord(c)) for c in text
        )

    def tmpl(self, selector: int, variation: int, *parts: bytes) -> bytes:
        return self.tag(TMPL) + bytes([selector, variation, 0]) + b"".join(parts) + bytes([END])

    def equation(self, *items: bytes, inline: bool = False) -> bytes:
        if self.version == V3:
            header = bytes([V3, WINDOWS, EQUATION_EDITOR, 3, 0])
        else:
            header = bytes([V5, WINDOWS, MATHTYPE, 6, 0]) + b"DSMT4\0"
            header += bytes([EQUATION_INLINE * inline])
        return header + bytes([SIZE_FULL]) + self.line(*items) + bytes([END])


def quadratic(version: int) -> bytes:
    """x = (-b ± √(b² - 4ac)) / 2a in either version's own template selectors."""
    m = Mtef(version)
    if version == V3:
        root, fract, sup = V3_ROOT, V3_FRACT, (V3_SCRIPT, V3_SUPER)
    else:
        root, fract, sup = V5_ROOT, V5_FRACT, (V5_SUP, 0)
    squared = m.tmpl(*sup, m.null_line(), m.line(m.chars(FN_NUMBER, "2")))
    radicand = m.line(
        m.chars(FN_VARIABLE, "b"),
        squared,
        m.chars(FN_SYMBOL, "−"),
        m.chars(FN_NUMBER, "4"),
        m.chars(FN_VARIABLE, "ac"),
    )
    numerator = m.line(
        m.chars(FN_SYMBOL, "−"),
        m.chars(FN_VARIABLE, "b"),
        m.chars(FN_SYMBOL, "±"),
        m.tmpl(root, 0, radicand, m.null_line()),
    )
    denominator = m.line(m.chars(FN_NUMBER, "2"), m.chars(FN_VARIABLE, "a"))
    return m.equation(
        m.chars(FN_VARIABLE, "x"),
        m.chars(FN_SYMBOL, "="),
        m.tmpl(fract, 0, numerator, denominator),
    )


def energy() -> bytes:
    """E = mc² as MathType writes it when marked inline."""
    m = Mtef(V5)
    sup = m.tmpl(V5_SUP, 0, m.null_line(), m.line(m.chars(FN_NUMBER, "2")))
    return m.equation(
        m.chars(FN_VARIABLE, "E"),
        m.chars(FN_SYMBOL, "="),
        m.chars(FN_VARIABLE, "mc"),
        sup,
        inline=True,
    )


def ole_object(mtef: bytes) -> bytes:
    """The CFB an Equation.3 or Equation.DSMT4 object embeds: its Equation Native stream is
    the EQNOLEFILEHDR then the MTEF."""
    header = struct.pack(
        "<HIHI16x", EQNOLEFILEHDR_BYTES, EQN_VERSION, EQN_CLIPBOARD_FORMAT, len(mtef)
    )
    return cfb_bytes([(EQUATION_NATIVE, header + mtef)])
