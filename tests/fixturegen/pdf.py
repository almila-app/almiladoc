# A minimal PDF: Helvetica as /F1 and a 1x1 gray image as /Im1 on every page,
# one content stream per page. Object numbers are fixed by position, so the
# xref table is exact rather than reconstructed by the reader.
def handmade_pdf(pages):
    n = len(pages)
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids [" + b" ".join(b"%d 0 R" % (5 + 2 * i) for i in range(n))
           + b"] /Count %d >>" % n,
        3: b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        4: b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray"
           b" /BitsPerComponent 8 /Length 1 >>\nstream\n\x80\nendstream",
    }
    for i, content in enumerate(pages):
        page, stream = 5 + 2 * i, 6 + 2 * i
        objs[page] = (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]"
                      b" /Resources << /Font << /F1 3 0 R >> /XObject << /Im1 4 0 R >> >>"
                      b" /Contents %d 0 R >>" % stream)
        objs[stream] = b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for num in sorted(objs):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % num + objs[num] + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += (b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n"
            % (len(objs) + 1, xref))
    return bytes(out)


TEXT_PAGE = b"BT /F1 24 Tf 72 700 Td (Text on the first page) Tj ET"
IMAGE_PAGE = b"q 468 0 0 648 72 72 cm /Im1 Do Q"


def ocr_pdfs(out):
    """A scan (image-only pages) and a mixed document (a text page then a scanned one)."""
    (out / "pdf" / "handmade-scanned.pdf").write_bytes(handmade_pdf([IMAGE_PAGE, IMAGE_PAGE]))
    (out / "pdf" / "handmade-mixed.pdf").write_bytes(handmade_pdf([TEXT_PAGE, IMAGE_PAGE]))


def generate(out):
    ocr_pdfs(out)
