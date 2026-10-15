def math_rtf(out):
    """Word's rtf: a math zone nests `{\\*\\moMath ...}` (inside a
    `\\*\\moMathPara` when displayed) and a `\\mmathPict` picture fallback;
    run style is a parameter word on the run and flags are `on`."""
    def r(text):
        return "{\\mr\\mscr0\\msty2 " + text + "}"
    parts = [
        "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0 Cambria Math;}}",
        "\\pard The roots are {\\mmath{\\*\\moMath{\\rtlch\\fcs1 \\af0 \\ltrch\\fcs0 \\f0\\insrsid1 }" + r("x") + r("=")
        + "{\\mf{\\mfPr{\\mctrlPr\\f0 }}{\\mnum" + r("-b\\'b1") + "{\\mrad{\\mradPr{\\mdegHide on}{\\mctrlPr\\f0 }}{\\mdeg}"
        + "{\\me{\\msSup{\\me" + r("b") + "}{\\msup" + r("2") + "}}" + r("-4ac") + "}}}{\\mden" + r("2a") + "}}}"
        + "{\\mmathPict{\\*\\mmathPr}{\\pict\\pngblip\\picw1\\pich1 89504e47}}} for any a.\\par",
        "\\pard {\\mmath{\\*\\moMathPara{\\moMathParaPr{\\mjc centerGroup}}{\\*\\moMath"
        + "{\\mnary{\\mnaryPr{\\mchr \\u8721 ?}{\\mlimLoc undOvr}{\\mctrlPr\\f0 }}"
        + "{\\msub" + r("i=1") + "}{\\msup" + r("n") + "}{\\me{\\msSub{\\me" + r("x") + "}{\\msub" + r("i") + "}}}}" + r("=")
        + "{\\mfunc{\\mfName{\\mr\\mscr0\\msty0 sin}}{\\me{\\md{\\mdPr{\\mctrlPr\\f0 }}{\\me" + r("\\u952 ?") + "}}}}}"
        + "{\\*\\moMath{\\mr\\mnor where }" + r("\\u8722 ?1<x") + "}}}\\par",
        "\\pard A price of $5 or $6 is not math.\\par",
        "}",
    ]
    (out / "rtf" / "handmade-math.rtf").write_bytes("\n".join(parts).encode("cp1252"))


def blockstyle_rtf(out):
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
    (out / "rtf").mkdir(parents=True, exist_ok=True)
    (out / "rtf" / "handmade-blockstyle.rtf").write_bytes(b"\n".join(parts))


# ---------------------------------------------------------------------------
# R13: \sbasedon style chains; boundary-matched merges in nonuniform tables

def merge_rtf(out):
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
    (out / "rtf").mkdir(parents=True, exist_ok=True)
    (out / "rtf" / "handmade-merge.rtf").write_bytes(b"\n".join(parts))


# ---------------------------------------------------------------------------
# Handmade RTF: \bin payload, list table, nested table, per-font charset (H11, H12, M13)

def bin_rtf(out):
    payload = b"AB{C}D\\E}}{F.."  # 14 raw bytes full of grouping chars
    assert len(payload) == 14
    parts = [
        rb"{\rtf1\ansi\ansicpg1252\deff0",
        rb"{\fonttbl{\f0\fcharset0 Arial;}{\f1\fcharset204 Arial;}}",
        rb"{\stylesheet{\s1\outlinelevel0\b Heading One;}{\s0 Normal;}}",
        rb"{\*\listtable{\list\listtemplateid1{\listlevel\levelnfc2\levelstartat4"
        rb"{\leveltext \'02\'00.;}{\levelnumbers \'01;}}\listid100}}",
        rb"{\*\listoverridetable{\listoverride\listid100\listoverridecount0\ls1}"
        # S6: per-level override records - an overridden start, and an
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
    (out / "rtf").mkdir(parents=True, exist_ok=True)
    (out / "rtf" / "handmade-bin.rtf").write_bytes(data)
    # unbalanced variant: drop the final closing brace -> recovers
    (out / "malformed").mkdir(parents=True, exist_ok=True)
    (out / "malformed" / "unbalanced--recovers.rtf").write_bytes(data[: data.rfind(b"}")])


def generate(out):
    merge_rtf(out)
    blockstyle_rtf(out)
    bin_rtf(out)
    math_rtf(out)
