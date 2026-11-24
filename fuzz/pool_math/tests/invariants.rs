use std::fs::{read, read_dir};
use std::path::Path;

use stellarlend_pool_math_fuzz::{
    decode, deterministic_cases, extreme_seeds, run, uniform_input, INPUT_LEN,
};

#[test]
fn short_inputs_are_ignored() {
    for length in 0..INPUT_LEN {
        assert!(run(&vec![0xFF; length]).is_ok());
        assert!(decode(&vec![0xFF; length]).is_none());
    }
}

#[test]
fn saturation_inputs_do_not_break_invariants() {
    for value in [0x00_u8, 0x01, 0x7F, 0x80, 0xFE, 0xFF] {
        assert!(run(&uniform_input(value)).is_ok(), "byte pattern {value:#04x}");
    }
    for seed in extreme_seeds() {
        assert!(run(&stellarlend_pool_math_fuzz::deterministic_input(seed)).is_ok());
    }
}

#[test]
fn deterministic_fuzz_cases_preserve_pool_invariants() {
    for case in deterministic_cases(2_000) {
        assert_eq!(case.len(), INPUT_LEN);
        assert!(decode(&case).is_some());
        assert!(run(&case).is_ok(), "violation for case {}", hex(&case));
    }
}

#[test]
fn checked_in_corpus_seeds_are_replayable() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus").join("pool_math");
    let mut seeds = read_dir(corpus).unwrap().filter_map(Result::ok).collect::<Vec<_>>();
    assert!(!seeds.is_empty(), "expected checked-in corpus seeds");
    seeds.sort_by_key(|entry| entry.file_name());
    for seed in seeds {
        let bytes = read(seed.path()).unwrap();
        assert_eq!(bytes.len(), INPUT_LEN);
        assert!(run(&bytes).is_ok(), "corpus seed {:?} violated an invariant", seed.file_name());
    }
}

#[test]
fn hostile_percentages_are_normalised_instead_of_panicking() {
    let mut case = uniform_input(0xFF);
    case[8 * 16..].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(run(&case).is_ok());

    let mut case = uniform_input(0x00);
    case[5 * 16..6 * 16].copy_from_slice(&u128::MAX.to_le_bytes());
    case[6 * 16..7 * 16].copy_from_slice(&u128::MAX.to_le_bytes());
    case[7 * 16..8 * 16].copy_from_slice(&u128::MAX.to_le_bytes());
    assert!(run(&case).is_ok());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}
