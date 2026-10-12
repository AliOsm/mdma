use super::*;
use crate::backend::coral::isa::{self, opcode, split};
use std::path::PathBuf;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/fixtures/coral/fc_corpus")
}

/// The first differing instruction, decoded for the field names.
fn first_diff(ours: &[u8], reference: &[u8]) -> Option<String> {
    if ours == reference {
        return None;
    }
    let (a, b) = (split(ours), split(reference));
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if x != y {
            let op = opcode(y.1[0]);
            let layout = [
                &*isa::scalar::SYNC,
                &*isa::scalar::SCSYNC,
                &*isa::scalar::POP,
                &*isa::scalar::INFEED,
                &*isa::scalar::OUTFEED,
                &*isa::op::OP,
                &*isa::wide_narrow::W2N,
                &*isa::wide_narrow::N2W,
                &*isa::ring_mesh::RING_PRODUCER,
                &*isa::ring_mesh::RING_CONSUMER,
                &*isa::ring_mesh::MESH,
            ]
            .into_iter()
            .find(|l| l.ops.contains(&op));
            let detail = layout
                .filter(|l| x.1.len() == l.nwords && y.1.len() == l.nwords)
                .map_or_else(
                    || format!("ours {:x?} ref {:x?}", x.1, y.1),
                    |l| {
                        let (da, db) = (l.decode(&x.1), l.decode(&y.1));
                        da.iter()
                            .filter(|(name, v)| db.get(name) != Some(*v))
                            .map(|(name, v)| {
                                format!("{name}: ours {v} ref {}", db.get(name).unwrap_or(0))
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                );
            return Some(format!(
                "instruction {i} (word {}, opcode {op:#x}): {detail}",
                y.0
            ));
        }
    }
    Some(format!(
        "ours has {} instructions, the reference {}",
        a.len(),
        b.len()
    ))
}

#[test]
fn programs_match_the_oracle_corpus() {
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(corpus().join("index.json")).unwrap()).unwrap();
    let cases = index.as_array().unwrap();
    assert!(cases.len() >= 30);
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let usize_of = |key: &str| usize::try_from(case[key].as_u64().unwrap()).unwrap();
        let mut quant = Quant::REFERENCE;
        if let Some(q) = case["quant"].as_object() {
            quant.w_zp = u8::try_from(q["w_zp"].as_u64().unwrap()).unwrap();
            quant.in_zp = u8::try_from(q["in_zp"].as_u64().unwrap()).unwrap();
            quant.out_zp = u8::try_from(q["out_zp"].as_u64().unwrap()).unwrap();
            quant.mult = q["mult"].as_f64().unwrap() as f32;
            if let Some(lo) = q.get("clamp_min") {
                quant.lo = (lo.as_f64().unwrap() + f64::from(quant.out_zp)) as u8;
                quant.hi = (q["clamp_max"].as_f64().unwrap() + f64::from(quant.out_zp)) as u8;
            }
        }
        let spec = FcSpec {
            n: usize_of("n"),
            k: usize_of("k"),
            param_offset: usize_of("param_offset"),
            tile_shift: usize_of("tile_shift"),
            move_gather: case["move_gather"].as_bool().unwrap(),
            quant,
            bias: case["bias"].as_bool().unwrap(),
        };
        let programs = build_fc(&spec).unwrap();
        for (kind, exe) in [
            ("caching", &programs.caching),
            ("exec", &programs.execution),
        ] {
            let reference = std::fs::read(corpus().join(format!("{name}_{kind}.bin"))).unwrap();
            if let Some(diff) = first_diff(&exe.bitstreams[0].data, &reference) {
                failures.push(format!("{name} {kind}: {diff}"));
            }
            exe.validate().unwrap();
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn relocations_point_at_the_base_movis() {
    let programs = build_fc(&FcSpec::new(64, 64)).unwrap();
    let relocs = |exe: &Executable| {
        exe.bitstreams[0]
            .relocs
            .iter()
            .map(|r| (r.desc, r.upper, r.offset_bit))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        relocs(&programs.caching),
        vec![
            (Description::Parameter, true, 128 * 5 + 70),
            (Description::Parameter, false, 128 * 4 + 70)
        ]
    );
    assert_eq!(
        relocs(&programs.execution),
        vec![
            (Description::InputActivation, true, 128 * 14 + 70),
            (Description::InputActivation, false, 128 * 15 + 70),
            (Description::OutputActivation, true, 12230),
            (Description::OutputActivation, false, 12358),
        ]
    );
}

type LayoutCase = ((usize, usize), (i64, i64, i64));

#[test]
fn narrow_layout_matches_the_fitted_rule() {
    const CASES: &[LayoutCase] = &[
        ((64, 64), (768, 0, 64)),
        ((704, 188), (768, 0, 704)),
        ((752, 128), (784, 0, 752)),
        ((1000, 320), (1032, 0, 1000)),
        ((2045, 1330), (2048, 0, 3380)),
        ((288, 288), (0, 288, 576)),
        ((1, 64), (768, 0, 0)),
        ((4, 300), (0, 332, 300)),
        ((64, 1000), (0, 1000, 1064)),
        ((1024, 4096), (0, 4096, 5120)),
    ];
    for ((n, k), expected) in CASES {
        assert_eq!(narrow_layout(*n, *k), *expected, "{n}x{k}");
    }
}

#[test]
fn host_side_matches_the_oracle_fixture() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/fixtures/coral/fc_10x300");
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    let program = FcProgram::new(10, 300);
    assert_eq!(program.rows, 64);
    let params = std::fs::read(dir.join("params.bin")).unwrap();
    let input = std::fs::read(dir.join("input.bin")).unwrap();
    assert_eq!(params.len(), program.geom().param_bytes());
    assert_eq!(input.len(), program.geom().s());
    let q = Quant::REFERENCE;
    assert_eq!(meta["quant"]["in_zp"].as_u64().unwrap(), u64::from(q.in_zp));
    // Recover x and the weights from the fixture's blob and check the reference against the expected output.
    let x = &input[..program.k];
    let group_rows = 64;
    let mut w = vec![0u8; program.rows * program.k];
    let mut bias = vec![0i32; program.rows];
    for row in 0..program.rows {
        bias[row] = i32::from_le_bytes(params[4 * row..4 * row + 4].try_into().unwrap());
        for col in 0..program.k {
            let (word, j) = (col / 4, col % 4);
            w[row * program.k + col] = params[4 * group_rows + word * group_rows * 4 + row * 4 + j];
        }
    }
    let expected = std::fs::read(dir.join("expected.bin")).unwrap();
    let computed = fc_reference(&q, x, &w, &bias, program.rows, program.k);
    assert_eq!(program.output(&computed), &expected[..]);
    assert_eq!(
        program.params(&w[..10 * program.k], &bias[..10], q.w_zp),
        params,
        "the blob layout round-trips"
    );
    assert_eq!(program.input_bytes(x, q.in_zp), input);
}

#[test]
fn programs_must_fit_the_tiles() {
    assert!(FcProgram::new(1024, 1024).fits(25600));
    assert!(FcProgram::new(4096, 288).fits(0));
    assert!(
        !FcProgram::new(4096, 4096).fits(0),
        "1 MB of weights per tile, twice the wide memory"
    );
    assert!(!FcProgram::new(1024, 1024).fits(WIDE_BYTES - 1024));
    let message = FcProgram::new(4096, 4096)
        .programs(0, 0, Quant::REFERENCE)
        .unwrap_err()
        .to_string();
    assert!(message.contains("exceed"), "{message}");
}

#[test]
fn generator_rejects_what_the_compiler_cannot_do() {
    let cases = [
        (
            FcSpec {
                param_offset: 128,
                ..FcSpec::new(64, 64)
            },
            "multiple of 256",
        ),
        (
            FcSpec {
                tile_shift: 9,
                ..FcSpec::new(2048, 1024)
            },
            "not legal",
        ),
        (FcSpec::new(2048, 4), "another op schedule"),
        (FcSpec::new(0, 64), "empty"),
    ];
    for (spec, expected) in cases {
        let message = build_fc(&spec).unwrap_err().to_string();
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?}"
        );
    }
}
