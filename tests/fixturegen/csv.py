from pathlib import Path

from fixturegen.common import write_file


def generate(out: Path) -> None:
    csv = out / "csv"
    write_file(
        csv / "handmade-quoted.csv",
        b'name,desc,qty\n"  padded  ","comma, inside",3\nplain,"multi\nline",4\n',
    )
    write_file(csv / "handmade-semicolon.csv", b'a;b;c\n"1,5";"2,5";x\n"3,0";y;z\n')
    write_file(
        csv / "handmade-utf16.csv",
        b"\xff\xfe" + "col1,col2\nnaïve,café\nΑθήνα,数据\n".encode("utf-16-le"),
    )
