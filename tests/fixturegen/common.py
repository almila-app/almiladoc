import base64
import struct
import zipfile
from collections.abc import Iterable, Sequence
from pathlib import Path

ZIP_DATE = (2026, 1, 1, 0, 0, 0)

DECL = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
OFFICE_REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/"
OFFICE_CT = "application/vnd.openxmlformats-officedocument."

Part = tuple[str, str | bytes]
Rel = tuple[str, str, str] | tuple[str, str, str, bool]

DOT_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8"
    "z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)


def write_file(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def write_cfb(path: Path, streams: Sequence[tuple[str, bytes]]) -> None:
    """Minimal CFB v3 writer (512-byte sectors) for handmade OLE2 fixtures.

    Streams are padded to the 4096-byte mini-stream cutoff so everything
    lives in regular FAT sectors; no mini-FAT is written. `streams` is a
    list of (name, bytes) in CFB name order (shorter names first, then
    case-insensitive).
    """
    SECT = 512
    ENDOFCHAIN = 0xFFFFFFFE
    FATSECT = 0xFFFFFFFD
    FREESECT = NOSTREAM = 0xFFFFFFFF

    fat: list[int] = []
    sectors: list[bytes] = []

    def add_chain(data: bytes) -> int:
        if not data:
            return ENDOFCHAIN
        padded = data + b"\x00" * (-len(data) % SECT)
        first = len(sectors)
        count = len(padded) // SECT
        for i in range(count):
            sectors.append(padded[i * SECT : (i + 1) * SECT])
            fat.append(first + i + 1 if i + 1 < count else ENDOFCHAIN)
        return first

    def dir_entry(name: str, typ: int, right: int, child: int, start: int, size: int) -> bytes:
        raw = name.encode("utf-16-le") + b"\x00\x00"
        return (
            raw.ljust(64, b"\x00")
            + struct.pack("<HBBIII", len(raw), typ, 1, NOSTREAM, right, child)
            + bytes(36)  # clsid, state bits, ctime and mtime
            + struct.pack("<IQ", start, size)
        )

    entries = [dir_entry("Root Entry", 5, NOSTREAM, 1 if streams else NOSTREAM, ENDOFCHAIN, 0)]
    for sid, (name, data) in enumerate(streams, 1):
        if 0 < len(data) < 4096:
            data = data.ljust(4096, b"\x00")
        right = sid + 1 if sid < len(streams) else NOSTREAM
        entries.append(dir_entry(name, 2, right, NOSTREAM, add_chain(data), len(data)))
    dir_start = add_chain(b"".join(entries))

    fat_count = 1
    while (len(fat) + fat_count + 127) // 128 > fat_count:
        fat_count += 1
    fat_ids = list(range(len(sectors), len(sectors) + fat_count))
    free = fat_count * 128 - len(fat) - fat_count
    full_fat = fat + [FATSECT] * fat_count + [FREESECT] * free
    sectors.extend(
        struct.pack("<128I", *full_fat[i * 128 : (i + 1) * 128]) for i in range(fat_count)
    )

    difat = fat_ids + [FREESECT] * (109 - len(fat_ids))
    header = (
        b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"
        + bytes(16)
        + struct.pack(
            "<6H10I",
            0x003E,
            0x0003,
            0xFFFE,
            9,
            6,
            0,
            0,
            0,
            fat_count,
            dir_start,
            0,
            4096,
            ENDOFCHAIN,
            0,
            ENDOFCHAIN,
            0,
        )
        + struct.pack("<109I", *difat)
    )
    write_file(path, header + b"".join(sectors))


def write_zip(
    path: Path, entries: Iterable[Part], mimetype_first: str | bytes | None = None
) -> None:
    """Writes `entries` once, in order, with deterministic timestamps."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        if mimetype_first is not None:
            info = zipfile.ZipInfo("mimetype", date_time=ZIP_DATE)
            z.writestr(info, mimetype_first, compress_type=zipfile.ZIP_STORED)
        for name, data in entries:
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)


def zip_edit(src: Path, dst: Path, name: str, data: bytes | None = None) -> None:
    """Copies `src` to `dst` with member `name` replaced by `data`, or dropped when None."""
    with zipfile.ZipFile(src) as zin:
        entries = [
            (i.filename, data if i.filename == name else zin.read(i.filename))
            for i in zin.infolist()
        ]
    write_zip(dst, [(n, d) for n, d in entries if d is not None])


def rels(*rows: Rel) -> str:
    """A relationships part: rows are (id, type, target[, external]); a type without `://`
    is an Office relationship type."""
    body = ""
    for rid, kind, target, *external in rows:
        kind = kind if "://" in kind else OFFICE_REL + kind
        mode = ' TargetMode="External"' if any(external) else ""
        body += f'<Relationship Id="{rid}" Type="{kind}" Target="{target}"{mode}/>\n'
    return (
        f'{DECL}\n<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/'
        f'relationships">\n{body}</Relationships>'
    )


def default(extension: str, content_type: str) -> str:
    return f'<Default Extension="{extension}" ContentType="{content_type}"/>'


def override(partname: str, kind: str) -> str:
    return f'<Override PartName="{partname}" ContentType="{OFFICE_CT}{kind}+xml"/>'


PNG_DEFAULT = default("png", "image/png")
BIN_DEFAULT = default("bin", OFFICE_CT + "oleObject")
WORD_PARTS = (
    ("/word/document.xml", "wordprocessingml.document.main"),
    ("/word/styles.xml", "wordprocessingml.styles"),
)


def content_types(
    *overrides: tuple[str, str], defaults: Iterable[tuple[str, str]] = (), extra: str = ""
) -> str:
    """A content-types part: overrides are (partname, kind) with the Office `+xml` type of
    `kind`, defaults are (extension, content type); `extra` follows them inline."""
    lines = [
        default("rels", "application/vnd.openxmlformats-package.relationships+xml"),
        default("xml", "application/xml"),
        *(default(*d) for d in defaults),
        *(override(*o) for o in overrides),
    ]
    return (
        f'{DECL}\n<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">\n'
        + "".join(f"{line}\n" for line in lines)
        + f"{extra}</Types>"
    )


M_NS = 'xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"'


def omml_run(text: str) -> str:
    return f'<m:r><m:t xml:space="preserve">{text}</m:t></m:r>'


# x = (-b ± √(b² - 4ac)) / 2a
OMML_QUADRATIC = (
    "<m:oMath>"
    + omml_run("x")
    + omml_run("=")
    + "<m:f><m:num>"
    + omml_run("-b±")
    + '<m:rad><m:radPr><m:degHide m:val="1"/></m:radPr><m:deg/><m:e>'
    + "<m:sSup><m:e>"
    + omml_run("b")
    + "</m:e><m:sup>"
    + omml_run("2")
    + "</m:sup></m:sSup>"
    + omml_run("-4ac")
    + "</m:e></m:rad></m:num>"
    + "<m:den>"
    + omml_run("2a")
    + "</m:den></m:f></m:oMath>"
)
# ∑_{i=1}^{n} x_i = sin(θ)
OMML_SUM = (
    '<m:oMath><m:nary><m:naryPr><m:chr m:val="∑"/><m:limLoc m:val="undOvr"/></m:naryPr>'
    + "<m:sub>"
    + omml_run("i=1")
    + "</m:sub><m:sup>"
    + omml_run("n")
    + "</m:sup>"
    + "<m:e><m:sSub><m:e>"
    + omml_run("x")
    + "</m:e><m:sub>"
    + omml_run("i")
    + "</m:sub></m:sSub></m:e></m:nary>"
    + omml_run("=")
    + '<m:func><m:fName><m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t>sin</m:t></m:r></m:fName>'
    + "<m:e><m:d><m:e>"
    + omml_run("θ")
    + "</m:e></m:d></m:e></m:func></m:oMath>"
)
# where 𝐱 ∈ ℝ (normal text run plus styled alphanumerics)
OMML_WHERE = (
    '<m:oMath><m:r><m:rPr><m:nor/></m:rPr><m:t xml:space="preserve">where </m:t></m:r>'
    + omml_run("𝐱 ∈ ℝ")
    + "</m:oMath>"
)
