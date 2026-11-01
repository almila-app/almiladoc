pub(crate) const WINGDINGS: [char; 256] = table(include_str!("dingbats/wingdings.tsv"));
pub(crate) const WINGDINGS_2: [char; 256] = table(include_str!("dingbats/wingdings2.tsv"));
pub(crate) const WINGDINGS_3: [char; 256] = table(include_str!("dingbats/wingdings3.tsv"));
pub(crate) const WEBDINGS: [char; 256] = table(include_str!("dingbats/webdings.tsv"));
pub(crate) const ZAPF_DINGBATS: [char; 256] = table(include_str!("dingbats/zapfdingbats.tsv"));

#[expect(clippy::panic, reason = "a const fn fails the build on a malformed table by panicking")]
const fn table(tsv: &str) -> [char; 256] {
    let tsv = tsv.as_bytes();
    let mut table = ['\0'; 256];
    let mut next_code = 0;
    let mut start = 0;
    while start < tsv.len() {
        let code_end = field_end(tsv, start);
        let char_end = next_field_end(tsv, code_end);
        let name_end = next_field_end(tsv, char_end);
        let mut end = next_field_end(tsv, name_end);
        if tsv[end] == b'\t' {
            end = field_end(tsv, end + 1);
        }
        assert!(tsv[end] == b'\n', "a dingbats row has four or five fields");

        assert!(code_end - start == 2, "a dingbats code is two hex digits");
        let code = hex(tsv, start, code_end) as usize;
        assert!(code >= next_code, "dingbats codes ascend without repeating");
        next_code = code + 1;

        let char_start = code_end + 1;
        if char_end - char_start != 1 || tsv[char_start] != b'-' {
            let digits = char_end - char_start;
            assert!(
                digits >= 6
                    && digits <= 8
                    && tsv[char_start] == b'U'
                    && tsv[char_start + 1] == b'+',
                "a dingbats character is `-` or U+ and four to six hex digits"
            );
            table[code] = match char::from_u32(hex(tsv, char_start + 2, char_end)) {
                Some(c) if c != '\0' => c,
                _ => panic!("a dingbats character is U+0000 or no scalar value"),
            };
        }
        start = end + 1;
    }
    table
}

const fn field_end(tsv: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < tsv.len() && tsv[end] != b'\t' && tsv[end] != b'\n' {
        end += 1;
    }
    assert!(end > start, "a dingbats row has an empty field");
    assert!(end < tsv.len(), "a dingbats row ends in a newline");
    end
}

const fn next_field_end(tsv: &[u8], end: usize) -> usize {
    assert!(tsv[end] == b'\t', "a dingbats row has four or five fields");
    field_end(tsv, end + 1)
}

#[expect(clippy::panic, reason = "a const fn fails the build on a malformed table by panicking")]
const fn hex(tsv: &[u8], start: usize, end: usize) -> u32 {
    let mut value = 0;
    let mut at = start;
    while at < end {
        let Some(digit) = (tsv[at] as char).to_digit(16) else {
            panic!("a dingbats code or character holds a non-hex digit");
        };
        value = value * 16 + digit;
        at += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_file_fills_the_codes_its_rows_give() {
        let tsv =
            "00\t-\t-\tw-1255\tnot encoded\n41\tU+1F577\tSPIDER\tw-0033\nFF\tU+10FFFF\tLAST\tz\n";
        let mut expected = ['\0'; 256];
        expected[0x41] = '\u{1f577}';
        expected[0xFF] = '\u{10ffff}';
        assert_eq!(table(tsv), expected);
        assert_eq!(table(""), ['\0'; 256]);
    }

    fn rejection(tsv: &str) -> Option<String> {
        let payload = std::panic::catch_unwind(|| table(tsv)).err()?;
        payload
            .downcast_ref::<&str>()
            .map(|reason| (*reason).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
    }

    #[test]
    fn a_malformed_data_file_is_rejected_with_its_reason() {
        const CODE: &str = "a dingbats code is two hex digits";
        const HEX: &str = "a dingbats code or character holds a non-hex digit";
        const ORDER: &str = "dingbats codes ascend without repeating";
        const CHAR: &str = "a dingbats character is `-` or U+ and four to six hex digits";
        const SCALAR: &str = "a dingbats character is U+0000 or no scalar value";
        const EMPTY: &str = "a dingbats row has an empty field";
        const NEWLINE: &str = "a dingbats row ends in a newline";
        const FIELDS: &str = "a dingbats row has four or five fields";
        for (tsv, reason) in [
            ("F\tU+2713\tA\tw\n", CODE),
            ("0FC\tU+2713\tA\tw\n", CODE),
            ("G1\tU+2713\tA\tw\n", HEX),
            ("FC\tU+2713\tA\tw\nFB\tU+2717\tB\tw\n", ORDER),
            ("FC\tU+2713\tA\tw\nFC\tU+2717\tB\tw\n", ORDER),
            ("FC\t002713\tA\tw\n", CHAR),
            ("FC\tV+2713\tA\tw\n", CHAR),
            ("FC\tU-2713\tA\tw\n", CHAR),
            ("FC\tU+271\tA\tw\n", CHAR),
            ("FC\tU+0002713\tA\tw\n", CHAR),
            ("FC\t--\tA\tw\n", CHAR),
            ("FC\tU+27G3\tA\tw\n", HEX),
            ("FC\tU+D800\tA\tw\n", SCALAR),
            ("FC\tU+110000\tA\tw\n", SCALAR),
            ("FC\tU+0000\tA\tw\n", SCALAR),
            ("\n", EMPTY),
            ("FC\t\tA\tw\n", EMPTY),
            ("FC\tU+2713\t\tw\n", EMPTY),
            ("FC\tU+2713\tA\t\n", EMPTY),
            ("FC\tU+2713\tA\tw\t\n", EMPTY),
            ("FC\tU+2713\tA\tw", NEWLINE),
            ("FC\tU+2713\tA\tw\tnote", NEWLINE),
            ("FC\tU+2713\tA\n", FIELDS),
            ("FC\tU+2713\tA\tw\tnote\textra\n", FIELDS),
            ("FC\tU+2713\tA\tw\tnote\tFD\tU+2717\tB\tw\n", FIELDS),
        ] {
            assert_eq!(rejection(tsv).as_deref(), Some(reason), "{tsv:?}");
        }
    }
}
