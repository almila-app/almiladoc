from pathlib import Path

from fixturegen.common import write_file


def math_rtf(out: Path) -> None:
    """Word's rtf: a math zone nests `{\\*\\moMath ...}` (inside a
    `\\*\\moMathPara` when displayed) and a `\\mmathPict` picture fallback;
    run style is a parameter word on the run and flags are `on`."""

    def r(text: str) -> str:
        return "{\\mr\\mscr0\\msty2 " + text + "}"

    parts = [
        "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0 Cambria Math;}}",
        "\\pard The roots are {\\mmath{\\*\\moMath"
        "{\\rtlch\\fcs1 \\af0 \\ltrch\\fcs0 \\f0\\insrsid1 }"
        + r("x")
        + r("=")
        + "{\\mf{\\mfPr{\\mctrlPr\\f0 }}{\\mnum"
        + r("-b\\'b1")
        + "{\\mrad{\\mradPr{\\mdegHide on}{\\mctrlPr\\f0 }}{\\mdeg}"
        + "{\\me{\\msSup{\\me"
        + r("b")
        + "}{\\msup"
        + r("2")
        + "}}"
        + r("-4ac")
        + "}}}{\\mden"
        + r("2a")
        + "}}}"
        + "{\\mmathPict{\\*\\mmathPr}{\\pict\\pngblip\\picw1\\pich1 89504e47}}} for any a.\\par",
        "\\pard {\\mmath{\\*\\moMathPara{\\moMathParaPr{\\mjc centerGroup}}{\\*\\moMath"
        + "{\\mnary{\\mnaryPr{\\mchr \\u8721 ?}{\\mlimLoc undOvr}{\\mctrlPr\\f0 }}"
        + "{\\msub"
        + r("i=1")
        + "}{\\msup"
        + r("n")
        + "}{\\me{\\msSub{\\me"
        + r("x")
        + "}{\\msub"
        + r("i")
        + "}}}}"
        + r("=")
        + "{\\mfunc{\\mfName{\\mr\\mscr0\\msty0 sin}}{\\me{\\md{\\mdPr{\\mctrlPr\\f0 }}{\\me"
        + r("\\u952 ?")
        + "}}}}}"
        + "{\\*\\moMath{\\mr\\mnor where }"
        + r("\\u8722 ?1<x")
        + "}}}\\par",
        "\\pard A price of $5 or $6 is not math.\\par",
        "}",
    ]
    write_file(out / "rtf" / "handmade-math.rtf", "\n".join(parts).encode("cp1252"))


def blockstyle_rtf(out: Path) -> None:
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\fcharset0 Arial;}}",
        # The style name is the trailing text of each stylesheet group; \s4
        # reaches its container only through the \sbasedon chain.
        rb"{\stylesheet{\s0 Normal;}{\s1 Preformatted Text;}"
        rb"{\s2 Quotations;}{\s3 Quote;}{\s4\sbasedon1 Listing;}}",
        rb"\pard\plain\s0 Body before.\par",
        rb"\pard\plain\s1 fn main() \{\par",
        rb"\pard\plain\s4     println!(\'22ok\'22);\par",
        rb"\pard\plain\s1 \}\par",
        rb"\pard\plain\s0 Body between.\par",
        rb"\pard\plain\s2 A quotation.\par",
        rb"\pard\plain\s3 Its second paragraph.\par",
        rb"\pard\plain\s0 Body after.\par",
        rb"}",
    ]
    write_file(out / "rtf" / "handmade-blockstyle.rtf", b"\n".join(parts))


def hidden_rtf(out: Path) -> None:
    """Hidden text (`\\v`) set directly and by a paragraph style: it drops,
    a hidden paragraph mark joins the next paragraph of its container, and
    a hidden note reference leaves its note unread."""
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\fcharset0 Arial;}}",
        rb"{\stylesheet{\s0 Normal;}{\s1\outlinelevel0\b Heading 1;}{\s2\v Hidden Paragraph;}}",
        rb"\pard\plain\s1 Visible heading\par",
        rb"\pard\plain\s0 Shown text{\v  with a hidden aside} continues.\par",
        # The style hides the text and its mark, which joins the next paragraph.
        rb"\pard\plain\s2 This paragraph is hidden by its style.\par",
        rb"\pard\plain\s0 A hidden mark joins this paragraph{\v\par}",
        rb"\pard\plain\s0  to the next one.\par",
        rb"\pard\plain\s0 A hidden note reference{\v{\super\chftn}{\footnote\pard\plain\s0"
        rb" {\super\chftn} Never read.}} and a shown one{\super\chftn}{\footnote\pard\plain\s0"
        rb" {\super\chftn} A shown note{\v  with hidden words}.}.\par",
        rb"\pard\plain\s0 A hidden picture{\v{\*\shppict{\pict\pngblip 89504e47}}}"
        rb" and check box{\v{\field{\*\fldinst FORMCHECKBOX}{\fldrslt }}} drop;"
        rb" {\webhidden web-hidden text} shows.\par",
        rb"\trowd\cellx3000\cellx6000\pard\plain\intbl Cell one{\v\par} joined\cell"
        rb" Cell two{\v  hidden}\cell\row",
        rb"\pard\plain\s0 After the table.\par",
        rb"}",
    ]
    write_file(out / "rtf" / "handmade-hidden.rtf", b"\n".join(parts))


def merge_rtf(out: Path) -> None:
    """\\sbasedon style chains; boundary-matched merges in nonuniform tables."""
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\fcharset0 Arial;}}",
        rb"{\stylesheet{\s1\b\outlinelevel1 Base Bold;}"
        rb"{\s2\sbasedon1\i Child Italic;}}",
        # The paragraph carries only \s2: bold and the outline level must
        # arrive through the \sbasedon chain.
        rb"\pard\s2 Inherited heading\par",
        rb"\pard\plain Plain paragraph between tables.\par",
        # Nonuniform boundaries: row 2's wide cell spans what rows 1 and 3
        # divide differently; the \clvmrg chain matches by \cellx boundary.
        rb"\trowd\clvmgf\cellx3000\cellx6000\cellx9000 A1\cell B1\cell C1\cell\row",
        rb"\trowd\clvmrg\cellx3000\cellx9000 \cell Wide B2\cell\row",
        rb"\trowd\cellx4500\cellx9000 A3 wide\cell B3 wide\cell\row",
        # A stray \clvmrg with no matching chain above stays visible.
        rb"\trowd\clvmrg\cellx4500\cellx9000 stray continuation\cell X\cell\row",
        rb"}",
    ]
    write_file(out / "rtf" / "handmade-merge.rtf", b"\n".join(parts))


def bin_rtf(out: Path) -> None:
    """A \\bin payload, list table, nested table and per-font charset."""
    payload = b"AB{C}D\\E}}{F.."  # 14 raw bytes full of grouping chars
    assert len(payload) == 14
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\fcharset0 Arial;}{\f1\fcharset204 Arial;}}",
        rb"{\stylesheet{\s1\outlinelevel0\b Heading One;}{\s0 Normal;}}",
        rb"{\*\listtable{\list\listtemplateid1{\listlevel\levelnfc2\levelstartat4"
        rb"{\leveltext \'02\'00.;}{\levelnumbers \'01;}}\listid100}}",
        rb"{\*\listoverridetable{\listoverride\listid100\listoverridecount0\ls1}"
        # Per-level override records - an overridden start, and an
        # overriding format via an embedded \listlevel
        rb"{\listoverride\listid100\listoverridecount1"
        rb"{\lfolevel\listoverridestartat\levelstartat10}\ls2}"
        rb"{\listoverride\listid100\listoverridecount1"
        rb"{\lfolevel\listoverrideformat{\listlevel\levelnfc0\levelstartat7}}\ls3}}",
        rb"\pard\s1\b Heading via style\b0\par",
        rb"\pard Before the binary blob.\par",
        rb"{\*\pict\wmetafile8\bin14 " + payload + rb"}",
        rb"\pard After the binary blob: braces {stay} balanced.\par",
        rb"\pard{\listtext IV.\tab}\ls1\ilvl0 Roman four via list table\par",
        rb"\pard{\listtext V.\tab}\ls1\ilvl0 Roman five\par",
        rb"\pard{\listtext X.\tab}\ls2\ilvl0 Roman ten via start override\par",
        rb"\pard{\listtext XI.\tab}\ls2\ilvl0 Roman eleven\par",
        rb"\pard{\listtext 7.\tab}\ls3\ilvl0 Decimal seven via format override\par",
        # simple table with a merged first row and a header row
        rb"\trowd\trhdr\clmgf\cellx4000\clmrg\cellx8000 Wide header\cell\cell\row",
        rb"\trowd\cellx4000\cellx8000 L\cell R\cell\row",
        # nested table inside the first cell of an outer table
        rb"\trowd\cellx4000\cellx8000",
        rb"\pard\intbl\itap2 nested A\nestcell nested B\nestcell"
        rb"{\*\nesttableprops\trowd\cellx2000\cellx4000\nestrow}",
        rb"\pard\intbl outer second\cell\row",
        # cp1251 text under \f1 (fcharset204): bytes for Russian "Da"
        rb"\pard\f1 \'c4\'e0\f0\par",
        rb"\pard Persian join: \u1605?\u1740?\u8204?\u1582?\par",
        rb"}",
    ]
    data = b"\n".join(parts)
    write_file(out / "rtf" / "handmade-bin.rtf", data)
    # unbalanced variant: drop the final closing brace -> recovers
    write_file(out / "malformed" / "unbalanced--recovers.rtf", data[: data.rfind(b"}")])


def mac_rtf(out: Path) -> None:
    """A Mac-era RTF: \\mac names Mac Roman, \\fcharset89 a Mac Cyrillic font."""

    def hex_escaped(text: str, codec: str) -> str:
        return "".join(c if c.isascii() else f"\\'{c.encode(codec)[0]:02x}" for c in text)

    roman = "Caf\u00e9 cr\u00e8me br\u00fbl\u00e9e, na\u00efve, Gr\u00f6\u00dfe: 5 \u20ac, \u03a9."
    quoted = "\u201cQuoted\u201d \u2026 done"
    cyrillic = "\u0414\u0430, \u041c\u0438\u0440"
    parts = [
        "{\\rtf1\\mac\\deff0",
        "{\\fonttbl{\\f0\\froman Times;}{\\f1\\fcharset89 Charcoal CY;}}",
        "\\pard " + hex_escaped(roman, "mac_roman") + "\\par",
        "\\pard " + hex_escaped(quoted, "mac_roman") + "\\par",
        "\\pard\\f1 " + hex_escaped(cyrillic, "mac_cyrillic") + "\\f0\\par",
        "}",
    ]
    write_file(out / "rtf" / "handmade-mac.rtf", "\n".join(parts).encode("ascii"))


def scientific_rtf(out: Path) -> None:
    """Symbol-font characters: Symbol letters and operators, Wingdings ballot boxes and a C1
    code, and a private-use code after a symbol font."""
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\froman\fcharset0 Times New Roman;}{\f1\froman\fcharset2 Symbol;}"
        rb"{\f2\fnil\fcharset2{\*\panose 05000000000000000000}Wingdings;}}",
        rb"\pard\plain Particles of 10 {\f1 m}m {\f1\'b1} 2 {\f1 m}m settle at a rate"
        rb" {\f1\'b5} {\f1\u-3998?}.\par",
        rb"\pard\plain A field pasted as Unicode keeps the last symbol font: 5\u-3987?m.\par",
        rb"\pard\plain {\f2\'fe} Calibrated {\f2\'6f} Replicated {\f2\'fc} Reviewed\par",
        rb"\pard\plain Step {\f2\'81} precedes step {\f2\'82}{\v  and {\f2\'83}}.\par",
        rb"}",
    ]
    write_file(out / "rtf" / "handmade-scientific.rtf", b"\n".join(parts))


def abuse(out: Path) -> None:
    """HYPERLINK fields nested 300 deep, each two groups: past the default
    nesting limit of 256."""
    depth = 300
    field = '{\\field{\\*\\fldinst HYPERLINK "https://example.com"}{\\fldrslt '
    data = "{\\rtf1\\ansi " + field * depth + "x" + "}}" * depth + "\\par}"
    write_file(out / "abuse" / "max_nesting_depth--fieldnest--errors.rtf", data.encode("ascii"))


def generate(out: Path) -> None:
    merge_rtf(out)
    blockstyle_rtf(out)
    hidden_rtf(out)
    bin_rtf(out)
    math_rtf(out)
    mac_rtf(out)
    scientific_rtf(out)
    abuse(out)
