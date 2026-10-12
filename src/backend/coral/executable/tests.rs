use super::*;

fn executable() -> Executable {
    Executable {
        name: "fc".into(),
        kind: ExecutableType::ExecutionOnly,
        batch_size: 1,
        scratch_size: 0,
        bitstreams: vec![Bitstream {
            data: vec![0; 3 * WORD_BYTES],
            relocs: vec![
                FieldOffset {
                    desc: Description::InputActivation,
                    batch: 0,
                    name: Some("x".into()),
                    upper: false,
                    offset_bit: 70,
                },
                FieldOffset {
                    desc: Description::OutputActivation,
                    batch: 0,
                    name: Some("y".into()),
                    upper: true,
                    offset_bit: 198,
                },
            ],
        }],
        parameters: Vec::new(),
        hints: vec![
            Hint::Instruction { chunk: 0 },
            Hint::Dma {
                direction: Direction::Infeed,
                desc: Description::InputActivation,
                name: Some("x".into()),
                offset: 0,
                size: 64,
            },
            Hint::Dma {
                direction: Direction::Outfeed,
                desc: Description::OutputActivation,
                name: Some("y".into()),
                offset: 0,
                size: 64,
            },
            Hint::Interrupt { id: 0 },
        ],
        inputs: vec![Layer {
            name: "x".into(),
            size_bytes: 64,
        }],
        outputs: vec![Layer {
            name: "y".into(),
            size_bytes: 64,
        }],
        caching_token: 0,
    }
}

#[test]
fn valid_program_passes() {
    executable().validate().unwrap();
}

type Mutation = Box<dyn Fn(&mut Executable)>;

#[test]
fn invalid_programs_are_named() {
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "not a whole number",
            Box::new(|e| e.bitstreams[0].data.truncate(40)),
        ),
        (
            "outside 48 bytes",
            Box::new(|e| e.bitstreams[0].relocs[0].offset_bit = 48 * 8 - 31),
        ),
        (
            "relocation batch 1 of 1",
            Box::new(|e| e.bitstreams[0].relocs[0].batch = 1),
        ),
        (
            "unknown input \"z\"",
            Box::new(|e| e.bitstreams[0].relocs[0].name = Some("z".into())),
        ),
        (
            "needs a layer name",
            Box::new(|e| e.bitstreams[0].relocs[0].name = None),
        ),
        (
            "must not carry a name",
            Box::new(|e| e.bitstreams[0].relocs[0].desc = Description::Parameter),
        ),
        (
            "instruction chunk 1 of 1",
            Box::new(|e| e.hints[0] = Hint::Instruction { chunk: 1 }),
        ),
        (
            "outside the 64-byte",
            Box::new(|e| {
                e.hints[1] = Hint::Dma {
                    direction: Direction::Infeed,
                    desc: Description::InputActivation,
                    name: Some("x".into()),
                    offset: 8,
                    size: 64,
                }
            }),
        ),
        (
            "interrupt 4 of 4",
            Box::new(|e| e.hints[3] = Hint::Interrupt { id: 4 }),
        ),
        ("batch size is zero", Box::new(|e| e.batch_size = 0)),
    ];
    for (expected, mutate) in cases {
        let mut exe = executable();
        mutate(&mut exe);
        let message = exe.validate().unwrap_err().to_string();
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?}"
        );
    }
}
