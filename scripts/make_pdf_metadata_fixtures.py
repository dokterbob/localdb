#!/usr/bin/env python3
"""Generate PDF metadata test fixtures for the drafted `document_metadata()`
in extract/src/pdf.rs.

No external dependencies (no pikepdf) -- follows the same hand-rolled,
uncompressed-object style as extract/tests/fixtures/multipage.pdf (checked:
that file is a plain-text, non-compressed classic-xref PDF built by hand),
so the fixtures stay readable in a text editor and don't need a PDF library
to regenerate or inspect.

Produces two fixtures:

  metadata_full.pdf
    Info dict carries Title/Author/Subject/Keywords/CreationDate/Producer.
    An XMP packet is ALSO embedded with different (conflicting) dc:title/
    dc:creator/dc:description/dc:subject/xmp:CreateDate, plus dc:language
    and dc:rights (fields the Info dictionary has no equivalent for).
    Verifies: Info-dict wins over XMP wherever both are present; XMP is
    still used for language/rights, which Info can never supply.

  metadata_xmp_fallback.pdf
    Info dict carries ONLY /Title (mirrors multipage.pdf). No /Author,
    /Subject, /Keywords, or /CreationDate. XMP supplies dc:creator,
    dc:description, dc:subject, dc:language, dc:rights, xmp:CreateDate.
    Verifies: per-field XMP fallback when Info lacks that field.

Both fixtures reuse the exact three-page body of multipage.pdf, so page
count / text extraction is already covered by existing tests -- only the
Info/Metadata objects differ.

Run with: python3 make_pdf_metadata_fixtures.py
Writes both files next to this script. When integrating, copy them into
extract/tests/fixtures/.
"""
import pathlib

PAGE_CONTENTS = [
    (
        "Alpha Section",
        "The quick brown fox jumps over the lazy dog on page one.",
        "This first page discusses alpha topics in plain prose.",
    ),
    (
        "Bravo Section",
        "Sphinx of black quartz, judge my vow, says page two.",
        "The second page continues with bravo material and more prose.",
    ),
    (
        "Charlie Section",
        "Pack my box with five dozen liquor jugs on page three.",
        "The third page concludes with charlie findings.",
    ),
]


def content_stream(heading: str, line1: str, line2: str) -> bytes:
    return (
        f"BT /F1 24 Tf 72 700 Td ({heading}) Tj ET\n"
        f"BT /F1 11 Tf 72 660 Td ({line1}) Tj ET\n"
        f"BT /F1 11 Tf 72 644 Td ({line2}) Tj ET\n"
    ).encode("latin-1")


def build_pdf(objects: dict, root: int, info) -> bytes:
    """objects: obj_num -> object body (without the 'N 0 obj' / 'endobj'
    wrapper). Returns full PDF bytes with a correct classic xref table."""
    header = b"%PDF-1.4\n"
    size = max(objects) + 1

    offsets = {0: 0}
    body = bytearray()
    pos = len(header)
    for num in range(1, size):
        obj_bytes = objects[num]
        wrapped = f"{num} 0 obj\n".encode("latin-1") + obj_bytes + b"\nendobj\n"
        offsets[num] = pos
        body += wrapped
        pos += len(wrapped)

    xref_offset = len(header) + len(body)
    xref_lines = [f"xref\n0 {size}\n0000000000 65535 f \n"]
    for num in range(1, size):
        xref_lines.append(f"{offsets[num]:010d} 00000 n \n")
    xref_bytes = "".join(xref_lines).encode("latin-1")

    trailer_dict = f"<< /Size {size} /Root {root} 0 R"
    if info is not None:
        trailer_dict += f" /Info {info} 0 R"
    trailer_dict += " >>"
    trailer = f"trailer\n{trailer_dict}\nstartxref\n{xref_offset}\n%%EOF\n".encode(
        "latin-1"
    )

    return header + bytes(body) + xref_bytes + trailer


def page_objects(start_num: int, parent_num: int, font_num: int):
    """Returns ({obj_num: body}, page_num, content_num) for one page whose
    page object is at start_num and content stream at start_num + 1."""
    page_num = start_num
    content_num = start_num + 1
    objs = {
        page_num: (
            f"<< /Type /Page /Parent {parent_num} 0 R /MediaBox [0 0 612 792] "
            f"/Contents {content_num} 0 R /Resources << /Font << /F1 {font_num} 0 R >> >> >>"
        ).encode("latin-1")
    }
    return objs, page_num, content_num


def xmp_packet(
    *, title, creator, description, subjects, language, rights, create_date
) -> bytes:
    subj_items = "".join(f"<rdf:li>{s}</rdf:li>" for s in subjects)
    xml = f"""<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
  <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
    <rdf:Description rdf:about=""
        xmlns:dc="http://purl.org/dc/elements/1.1/"
        xmlns:xmp="http://ns.adobe.com/xap/1.0/">
      <dc:title><rdf:Alt><rdf:li xml:lang="x-default">{title}</rdf:li></rdf:Alt></dc:title>
      <dc:creator><rdf:Seq><rdf:li>{creator}</rdf:li></rdf:Seq></dc:creator>
      <dc:description><rdf:Alt><rdf:li xml:lang="x-default">{description}</rdf:li></rdf:Alt></dc:description>
      <dc:subject><rdf:Bag>{subj_items}</rdf:Bag></dc:subject>
      <dc:language>{language}</dc:language>
      <dc:rights><rdf:Alt><rdf:li xml:lang="x-default">{rights}</rdf:li></rdf:Alt></dc:rights>
      <xmp:CreateDate>{create_date}</xmp:CreateDate>
    </rdf:Description>
  </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"""
    return xml.encode("utf-8")


def build_fixture(*, info_extra: str, xmp_kwargs: dict, filename: str, out_dir: pathlib.Path):
    """Common skeleton: catalog(1) -> pages(2) -> 3 pages+contents(3..8) ->
    font(9) -> Info(10) -> Metadata stream(11)."""
    objects = {}

    font_num = 9
    objects[font_num] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"

    page_nums = []
    next_num = 3
    for heading, l1, l2 in PAGE_CONTENTS:
        objs, page_num, content_num = page_objects(next_num, 2, font_num)
        objects.update(objs)
        stream = content_stream(heading, l1, l2)
        objects[content_num] = (
            f"<< /Length {len(stream)} >>\nstream\n".encode("latin-1")
            + stream
            + b"endstream"
        )
        page_nums.append(page_num)
        next_num += 2

    objects[2] = (
        f"<< /Type /Pages /Kids [{' '.join(f'{n} 0 R' for n in page_nums)}] "
        f"/Count {len(page_nums)} >>"
    ).encode("latin-1")

    metadata_num = 11
    objects[1] = (
        f"<< /Type /Catalog /Pages 2 0 R /Metadata {metadata_num} 0 R >>"
    ).encode("latin-1")

    info_num = 10
    objects[info_num] = f"<< {info_extra} >>".encode("latin-1")

    xmp_xml = xmp_packet(**xmp_kwargs)
    objects[metadata_num] = (
        b"<< /Type /Metadata /Subtype /XML /Length "
        + str(len(xmp_xml)).encode("latin-1")
        + b" >>\nstream\n"
        + xmp_xml
        + b"\nendstream"
    )

    pdf_bytes = build_pdf(objects, root=1, info=info_num)
    out_path = out_dir / filename
    out_path.write_bytes(pdf_bytes)
    print(f"wrote {out_path} ({len(pdf_bytes)} bytes)")


def main():
    out_dir = pathlib.Path(__file__).parent

    # Fixture A: Info dict is fully populated and deliberately conflicts
    # with XMP wherever both can supply a field -- Info must win.
    build_fixture(
        out_dir=out_dir,
        filename="metadata_full.pdf",
        info_extra=(
            "/Title (Info Title) /Author (Info Author) "
            "/Subject (Info Subject text) "
            "/Keywords (alpha, beta;gamma) "
            "/CreationDate (D:20210102153000+05'30') "
            "/Producer (localdb tests producer)"
        ),
        xmp_kwargs=dict(
            title="XMP Title",
            creator="XMP Creator",
            description="XMP Description",
            subjects=["xmp-subject-1", "xmp-subject-2"],
            language="en",
            rights="XMP Rights statement",
            create_date="2019-06-01T00:00:00Z",
        ),
    )

    # Fixture B: Info dict carries ONLY /Title (like multipage.pdf). Every
    # other field must come from XMP.
    build_fixture(
        out_dir=out_dir,
        filename="metadata_xmp_fallback.pdf",
        info_extra="/Title (Fallback Fixture Title)",
        xmp_kwargs=dict(
            title="Should Not Be Used (Info wins for title)",
            creator="XMP Fallback Creator",
            description="XMP Fallback Description",
            subjects=["fallback-subject-1", "fallback-subject-2"],
            language="fr",
            rights="XMP Fallback Rights",
            create_date="2018-03-04T09:00:00Z",
        ),
    )


if __name__ == "__main__":
    main()
