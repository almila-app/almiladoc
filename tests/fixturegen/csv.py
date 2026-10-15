def csvs(out):
    d = out / "csv"
    d.mkdir(parents=True, exist_ok=True)
    (d / "handmade-quoted.csv").write_text(
        'name,desc,qty\n"  padded  ","comma, inside",3\nplain,"multi\nline",4\n',
        encoding="utf-8", newline="")
    (d / "handmade-semicolon.csv").write_text(
        'a;b;c\n"1,5";"2,5";x\n"3,0";y;z\n', encoding="utf-8", newline="")
    (d / "handmade-utf16.csv").write_bytes(
        b"\xff\xfe" + "col1,col2\nnaïve,café\nΑθήνα,数据\n".encode("utf-16-le"))


def generate(out):
    csvs(out)
