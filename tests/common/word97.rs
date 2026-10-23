/// The `(WordDocument, 0Table)` streams of a Word 97 DOC: the FIB, its story lengths `ccps`
/// at their offsets and the language id `lid` when given, then `text` at `text_fc`, which the
/// Clx in the table stream names as `pieces` of `(cp_end, compressed fc, prm)`.
pub(crate) fn word97(
    text_fc: u32,
    ccps: &[(usize, u32)],
    text: &[u8],
    pieces: &[(u32, u32, u16)],
    lid: Option<u16>,
) -> (Vec<u8>, Vec<u8>) {
    let mut word_doc = vec![0u8; text_fc as usize];
    word_doc.extend_from_slice(text);
    // [MS-DOC] 2.5.1 Fib and 2.5.2 FibBase for nFib 0x00C1: fExtChar, nFibBack, csw, cslw
    // and cbRgFcLcb.
    let fib16 = [(0x00, 0xA5EC), (0x02, 0x00C1), (0x0A, 0x1000), (0x0C, 0x00BF), (0x20, 0x000E)];
    let lid = lid.map(|lid| (0x06, lid));
    for (off, value) in fib16.into_iter().chain([(0x3E, 0x0016), (0x98, 0x005D)]).chain(lid) {
        word_doc[off..off + 2].copy_from_slice(&u16::to_le_bytes(value));
    }
    let mut plc: Vec<u8> =
        [0].into_iter().chain(pieces.iter().map(|p| p.0)).flat_map(u32::to_le_bytes).collect();
    for &(_, fc, prm) in pieces {
        let fc = (fc * 2) | 0x4000_0000;
        plc.extend([&[0u8, 0][..], &fc.to_le_bytes(), &prm.to_le_bytes()].concat());
    }
    let table = [&[2u8][..], &u32::try_from(plc.len()).unwrap().to_le_bytes(), &plc].concat();
    let cb_mac = u32::try_from(word_doc.len()).unwrap();
    let lcb = u32::try_from(table.len()).unwrap();
    for (off, value) in ccps.iter().copied().chain([(0x40, cb_mac), (0x1A6, lcb)]) {
        word_doc[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }
    (word_doc, table)
}
