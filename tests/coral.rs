//! Oracle programs for the Coral backend: `data/fixtures/coral/*` holds
//! FULLY_CONNECTED programs from the offline oracle (byte-identical to
//! edgetpu_compiler's), their parameters, an input and the
//! output the chip must produce. Every fixture is validated and linked on any host;
//! with `MDMA_CORAL_DEVICE` set (FreeBSD, `/dev/apexN`) they also run on the card.
#![cfg(feature = "coral")]

use mdma::backend::coral::executable::{Bitstream, Description, Direction, Executable, ExecutableType, FieldOffset, Hint, Layer};
use mdma::backend::coral::link::{Addresses, link, read_u32};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(target_os = "freebsd")]
use std::sync::Mutex;

/// One test at a time may hold the device open.
#[cfg(target_os = "freebsd")]
static DEVICE: Mutex<()> = Mutex::new(());

/// The card named by `MDMA_CORAL_DEVICE`, opened for one test at a time; `None`
/// (not on hardware) when the variable is unset.
#[cfg(target_os = "freebsd")]
fn open_device() -> Option<(std::sync::MutexGuard<'static, ()>, mdma::backend::coral::tpu::Tpu)> {
    use mdma::backend::coral::tpu::{Options, Tpu};
    let Ok(device) = std::env::var("MDMA_CORAL_DEVICE") else {
        eprintln!("MDMA_CORAL_DEVICE not set: not running on hardware");
        return None;
    };
    let guard = DEVICE.lock().unwrap();
    Some((guard, Tpu::open(Options { device, ..Options::default() }).unwrap()))
}

struct Fixture {
    caching: Executable,
    execution: Executable,
    input: Vec<u8>,
    expected: Vec<u8>,
}

fn fixtures_dir() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("data/fixtures/coral") }

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir()).unwrap()
        .map(|entry| entry.unwrap().path()).filter(|path| path.join("meta.json").is_file())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    names.sort();
    assert!(!names.is_empty(), "no fixtures in {}", fixtures_dir().display());
    names
}

fn description(text: &str) -> Description {
    match text {
        "INPUT" => Description::InputActivation,
        "OUTPUT" => Description::OutputActivation,
        "PARAMETER" => Description::Parameter,
        "SCRATCH" => Description::Scratch,
        other => panic!("unknown description {other:?}"),
    }
}

fn executable(dir: &Path, meta: &Value) -> Executable {
    let string = |v: &Value| v.as_str().map(str::to_owned);
    let usize_of = |v: &Value| usize::try_from(v.as_u64().unwrap()).unwrap();
    let kind = match meta["type"].as_str().unwrap() {
        "PARAMETER_CACHING" => ExecutableType::ParameterCaching,
        "EXECUTION_ONLY" => ExecutableType::ExecutionOnly,
        "STAND_ALONE" => ExecutableType::StandAlone,
        other => panic!("unknown executable type {other:?}"),
    };
    let bitstreams = meta["bitstreams"].as_array().unwrap().iter().map(|b| Bitstream {
        data: std::fs::read(dir.join(b["file"].as_str().unwrap())).unwrap(),
        relocs: b["relocs"].as_array().unwrap().iter().map(|r| FieldOffset {
            desc: description(r["desc"].as_str().unwrap()),
            batch: usize_of(&r["batch"]),
            name: string(&r["name"]),
            upper: r["upper"].as_bool().unwrap(),
            offset_bit: usize_of(&r["offset_bit"]),
        }).collect(),
    }).collect();
    let hints = meta["hints"].as_array().unwrap().iter().map(|h| match h["kind"].as_str().unwrap() {
        "instruction" => Hint::Instruction { chunk: usize_of(&h["chunk"]) },
        "dma" => Hint::Dma {
            direction: if h["direction"] == "INFEED" { Direction::Infeed } else { Direction::Outfeed },
            desc: description(h["desc"].as_str().unwrap()),
            name: string(&h["name"]),
            offset: usize_of(&h["offset"]),
            size: usize_of(&h["size"]),
        },
        "interrupt" => Hint::Interrupt { id: u8::try_from(h["interrupt"].as_u64().unwrap()).unwrap() },
        "fence" => Hint::Fence,
        other => panic!("unknown hint {other:?}"),
    }).collect();
    let layers = |key: &str| meta[key].as_array().unwrap().iter()
        .map(|l| Layer { name: l["name"].as_str().unwrap().into(), size_bytes: usize_of(&l["size"]) }).collect();
    Executable {
        name: format!("{}:{}", dir.file_name().unwrap().to_string_lossy(), meta["type"].as_str().unwrap()),
        kind,
        batch_size: 1,
        scratch_size: 0,
        bitstreams,
        parameters: meta["parameters"].as_str().map(|file| std::fs::read(dir.join(file)).unwrap()).unwrap_or_default(),
        hints,
        inputs: layers("inputs"),
        outputs: layers("outputs"),
        caching_token: 0,
    }
}

fn load(name: &str) -> Fixture {
    let dir = fixtures_dir().join(name);
    let meta: Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    let executables = meta["executables"].as_array().unwrap();
    let caching = executable(&dir, &executables[0]);
    let execution = executable(&dir, &executables[1]);
    assert_eq!(caching.kind, ExecutableType::ParameterCaching);
    assert_eq!(execution.kind, ExecutableType::ExecutionOnly);
    let input = std::fs::read(dir.join(meta["input"]["x"].as_str().unwrap())).unwrap();
    let expected = std::fs::read(dir.join(meta["expected"]["y"].as_str().unwrap())).unwrap();
    assert_eq!(expected.len(), usize::try_from(meta["expected"]["valid_bytes"].as_u64().unwrap()).unwrap());
    Fixture { caching, execution, input, expected }
}

#[test]
fn fixtures_validate_and_link() {
    for name in fixture_names() {
        let fixture = load(&name);
        fixture.caching.validate().unwrap();
        fixture.execution.validate().unwrap();
        assert_eq!(fixture.input.len(), fixture.execution.inputs[0].size_bytes, "{name}: input is padded to the DMA size");
        assert!(fixture.expected.len() <= fixture.execution.outputs[0].size_bytes, "{name}");
        let mut addresses = Addresses { parameter: Some(0x8000_0000_0010_0000), ..Addresses::default() };
        addresses.inputs.insert("x".into(), vec![0x8000_0000_0020_0000]);
        addresses.outputs.insert("y".into(), vec![0x8000_0000_0030_0000]);
        for (exe, expected_relocs) in [(&fixture.caching, 2), (&fixture.execution, 4)] {
            let bitstream = &exe.bitstreams[0];
            assert_eq!(bitstream.relocs.len(), expected_relocs, "{}", exe.name);
            let linked = link(bitstream, &addresses).unwrap();
            assert_eq!(linked.len(), bitstream.data.len());
            for reloc in &bitstream.relocs {
                assert_eq!(read_u32(&bitstream.data, reloc.offset_bit).unwrap(), 0, "{}: the oracle emits zero bases", exe.name);
                let address = match reloc.desc {
                    Description::Parameter => 0x8000_0000_0010_0000u64,
                    Description::InputActivation => 0x8000_0000_0020_0000,
                    Description::OutputActivation => 0x8000_0000_0030_0000,
                    Description::Scratch => unreachable!(),
                };
                let half = if reloc.upper { (address >> 32) as u32 } else { address as u32 };
                assert_eq!(read_u32(&linked, reloc.offset_bit).unwrap(), half, "{}: relocation at bit {}", exe.name, reloc.offset_bit);
            }
        }
    }
}

#[cfg(target_os = "freebsd")]
#[test]
fn programs_run_bit_exact_on_the_card() {
    let Some((_guard, mut tpu)) = open_device() else { return };
    for name in fixture_names() {
        let fixture = load(&name);
        let parameters = tpu.load_parameters(&fixture.caching.parameters, 0).unwrap();
        let cached = tpu.run(&fixture.caching, Some(&parameters), &[]).unwrap();
        eprintln!("{name}: parameters cached in {:?} ({} events)", cached.elapsed, cached.events.len());
        for round in 0..3 {
            let run = tpu.run(&fixture.execution, Some(&parameters), &[("x", &fixture.input)]).unwrap();
            let (output_name, output) = &run.outputs[0];
            assert_eq!(output_name, "y");
            assert_eq!(&output[..fixture.expected.len()], &fixture.expected[..], "{name} round {round}: output differs from the oracle");
            eprintln!("{name} round {round}: bit-exact in {:?} ({} events)", run.elapsed, run.events.len());
        }
        tpu.unload_parameters(parameters).unwrap();
    }
    tpu.close().unwrap();
}

/// A small deterministic generator for weights and inputs (xorshift64*).
#[cfg(target_os = "freebsd")]
struct Xorshift(u64);

#[cfg(target_os = "freebsd")]
impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> { (0..n).map(|_| (self.next_u64() >> 24) as u8).collect() }

    fn biases(&mut self, n: usize) -> Vec<i32> { (0..n).map(|_| (self.next_u64() % 4001) as i32 - 2000).collect() }
}

/// Generated FULLY_CONNECTED programs, run against the bit-exact reference.
#[cfg(target_os = "freebsd")]
#[test]
fn generated_programs_run_bit_exact_on_the_card() {
    use mdma::backend::coral::codegen::Quant;
    use mdma::backend::coral::codegen::fc::{FcProgram, fc_reference};
    let Some((_guard, mut tpu)) = open_device() else { return };
    let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
    // (N, K, compute tile shift, parameter offset): the shapes exercise one input
    // tile, a west gather, a north relay chain, padded rows, all 16 compute tiles
    // with four passes each, and a shifted placement at a parameter offset.
    let cases = [(64, 64, 0, 0), (10, 300, 0, 0), (288, 288, 4, 4096), (1000, 320, 0, 0), (2045, 1330, 0, 0), (4096, 288, 0, 0), (1024, 1024, 0, 25600), (512, 512, 8, 0)];
    for (n, k, shift, offset) in cases {
        let program = FcProgram::new(n, k);
        let quant = Quant::REFERENCE;
        let programs = program.programs(shift, offset, quant).unwrap();
        let w = rng.bytes(n * k);
        let bias = rng.biases(n);
        let x = rng.bytes(k);
        let expected = fc_reference(&quant, &x, &w, &bias, n, k);
        let parameters = tpu.load_parameters(&program.params(&w, &bias, quant.w_zp), 0).unwrap();
        let cached = tpu.run(&programs.caching, Some(&parameters), &[]).unwrap();
        let input = program.input_bytes(&x, quant.in_zp);
        let run = tpu.run(&programs.execution, Some(&parameters), &[("x", &input)]).unwrap();
        assert_eq!(program.output(&run.outputs[0].1), &expected[..], "fc {n}x{k} shift {shift} offset {offset}: output differs from the reference");
        eprintln!("generated fc {n}x{k} shift {shift} offset {offset}: bit-exact, cache {:?}, call {:?}", cached.elapsed, run.elapsed);
        tpu.unload_parameters(parameters).unwrap();
    }
    tpu.close().unwrap();
}
