//! Deterministic mutation smoke test: every corpus fixture is converted
//! after byte mutations at pseudo-random positions. Conversions may fail
//! with typed errors; they must never panic, hang, or exhaust memory.
//! (The cargo-fuzz targets under `fuzz/` run the same surfaces much harder.)

use crate::support::{converter, corpus, fixture_root, format_of};
use almiladoc::{ConvertError, Input};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: deterministic across runs and platforms.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn mutated_fixtures_never_panic() {
    let root = fixture_root();
    let converter = converter(1);
    let mut rng = Rng(0x5EED_1234_5678_9ABC);
    for path in corpus() {
        let rel = path.strip_prefix(&root).unwrap();
        let original = std::fs::read(&path).unwrap();
        let Some(format) = format_of(&path, &original) else {
            continue;
        };
        if original.is_empty() {
            continue;
        }
        for _ in 0..25 {
            let mut bytes = original.clone();
            for _ in 0..=(rng.next() % 8) {
                let pos = (rng.next() as usize) % bytes.len();
                bytes[pos] = rng.next() as u8;
            }
            if rng.next().is_multiple_of(4) {
                let cut = (rng.next() as usize) % bytes.len();
                bytes.truncate(cut.max(1));
            }
            let result = converter.convert(Input::new(&bytes, format));
            assert!(
                !matches!(result, Err(ConvertError::Internal)),
                "{}: a panic or internal failure",
                rel.display()
            );
        }
    }
}
