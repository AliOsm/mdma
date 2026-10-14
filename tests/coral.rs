//! Oracle programs for the Coral backend: `data/fixtures/coral/*` holds
//! FULLY_CONNECTED programs from the offline oracle (byte-identical to
//! edgetpu_compiler's), their parameters, an input and the
//! output the chip must produce. Every fixture is validated and linked on any host;
//! with `MDMA_CORAL_DEVICE` set (`/dev/apexN` on FreeBSD, `/dev/apex_N` on Linux)
//! they also run on the card.
#![cfg(feature = "coral")]

use mdma::backend::coral::codegen::layer::{
    LayerPlan, LayerQuant, ModelPrograms, gen_model, plan, quants_from_json,
};
use mdma::backend::coral::executable::{
    Bitstream, Description, Direction, Executable, ExecutableType, FieldOffset, Hint, Layer,
};
use mdma::backend::coral::link::{Addresses, link, read_u32};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
use std::sync::Mutex;

/// One test at a time may hold the device open.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
static DEVICE: Mutex<()> = Mutex::new(());

/// The card named by `MDMA_CORAL_DEVICE`, opened for one test at a time; `None`
/// (not on hardware) when the variable is unset.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn open_device() -> Option<(
    std::sync::MutexGuard<'static, ()>,
    mdma::backend::coral::tpu::Tpu,
)> {
    use mdma::backend::coral::tpu::{Options, Tpu};
    let Ok(device) = std::env::var("MDMA_CORAL_DEVICE") else {
        eprintln!("MDMA_CORAL_DEVICE not set: not running on hardware");
        return None;
    };
    let guard = DEVICE.lock().unwrap();
    Some((
        guard,
        Tpu::open(Options {
            device,
            ..Options::default()
        })
        .unwrap(),
    ))
}

struct Fixture {
    caching: Executable,
    execution: Executable,
    input: Vec<u8>,
    expected: Vec<u8>,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("data/fixtures/coral")
}

fn fixture_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("meta.json").is_file())
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "no fixtures in {}",
        fixtures_dir().display()
    );
    names
}

fn json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// tinycoral's whole-model fixture (`data/fixtures/coral/model`): its quantization
/// (the BOS position's and the others') and the manifest of its programs' hashes.
struct Oracle {
    dir: PathBuf,
    main: Vec<LayerQuant>,
    bos: Vec<LayerQuant>,
    manifest: Value,
}

impl Oracle {
    fn open() -> Self {
        let dir = fixtures_dir().join("model");
        let quants = json(&dir.join("quants.json"));
        Self {
            main: quants_from_json(&quants["main"]).unwrap(),
            bos: quants_from_json(&quants["bos"]).unwrap(),
            manifest: json(&dir.join("manifest.json")),
            dir,
        }
    }

    /// The programs of `p` positions, asserted equal to tinycoral's.
    fn programs(&self, p: usize, pl: &[LayerPlan]) -> ModelPrograms {
        let programs = gen_model(p, if p == 1 { &self.bos } else { &self.main }, pl).unwrap();
        let hashes: Vec<String> = programs
            .execution
            .bitstreams
            .iter()
            .map(|b| sha(&b.data))
            .collect();
        assert_eq!(
            Value::from(hashes),
            self.manifest["programs"][p.to_string()],
            "P={p}: the program differs from the oracle's"
        );
        programs
    }
}

/// Named program inputs as `Tpu::run` takes them.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn named(inputs: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
    inputs
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect()
}

/// (bytes that differ, largest difference).
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    let n = a.iter().zip(b).filter(|(x, y)| x != y).count();
    (
        n,
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0),
    )
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
    let bitstreams = meta["bitstreams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| Bitstream {
            data: std::fs::read(dir.join(b["file"].as_str().unwrap())).unwrap(),
            relocs: b["relocs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| FieldOffset {
                    desc: description(r["desc"].as_str().unwrap()),
                    batch: usize_of(&r["batch"]),
                    name: string(&r["name"]),
                    upper: r["upper"].as_bool().unwrap(),
                    offset_bit: usize_of(&r["offset_bit"]),
                })
                .collect(),
        })
        .collect();
    let hints = meta["hints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| match h["kind"].as_str().unwrap() {
            "instruction" => Hint::Instruction {
                chunk: usize_of(&h["chunk"]),
            },
            "dma" => Hint::Dma {
                direction: if h["direction"] == "INFEED" {
                    Direction::Infeed
                } else {
                    Direction::Outfeed
                },
                desc: description(h["desc"].as_str().unwrap()),
                name: string(&h["name"]),
                offset: usize_of(&h["offset"]),
                size: usize_of(&h["size"]),
            },
            "interrupt" => Hint::Interrupt {
                id: u8::try_from(h["interrupt"].as_u64().unwrap()).unwrap(),
            },
            "fence" => Hint::Fence,
            other => panic!("unknown hint {other:?}"),
        })
        .collect();
    let layers = |key: &str| {
        meta[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| Layer {
                name: l["name"].as_str().unwrap().into(),
                size_bytes: usize_of(&l["size"]),
            })
            .collect()
    };
    Executable {
        name: format!(
            "{}:{}",
            dir.file_name().unwrap().to_string_lossy(),
            meta["type"].as_str().unwrap()
        ),
        kind,
        batch_size: 1,
        scratch_size: 0,
        bitstreams,
        parameters: meta["parameters"]
            .as_str()
            .map(|file| std::fs::read(dir.join(file)).unwrap())
            .unwrap_or_default(),
        hints,
        inputs: layers("inputs"),
        outputs: layers("outputs"),
        caching_token: 0,
    }
}

fn load(name: &str) -> Fixture {
    let dir = fixtures_dir().join(name);
    let meta = json(&dir.join("meta.json"));
    let executables = meta["executables"].as_array().unwrap();
    let caching = executable(&dir, &executables[0]);
    let execution = executable(&dir, &executables[1]);
    assert_eq!(caching.kind, ExecutableType::ParameterCaching);
    assert_eq!(execution.kind, ExecutableType::ExecutionOnly);
    let input = std::fs::read(dir.join(meta["input"]["x"].as_str().unwrap())).unwrap();
    let expected = std::fs::read(dir.join(meta["expected"]["y"].as_str().unwrap())).unwrap();
    assert_eq!(
        expected.len(),
        usize::try_from(meta["expected"]["valid_bytes"].as_u64().unwrap()).unwrap()
    );
    Fixture {
        caching,
        execution,
        input,
        expected,
    }
}

#[test]
fn fixtures_validate_and_link() {
    for name in fixture_names() {
        let fixture = load(&name);
        fixture.caching.validate().unwrap();
        fixture.execution.validate().unwrap();
        assert_eq!(
            fixture.input.len(),
            fixture.execution.inputs[0].size_bytes,
            "{name}: input is padded to the DMA size"
        );
        assert!(
            fixture.expected.len() <= fixture.execution.outputs[0].size_bytes,
            "{name}"
        );
        let mut addresses = Addresses {
            parameter: Some(0x8000_0000_0010_0000),
            ..Addresses::default()
        };
        addresses
            .inputs
            .insert("x".into(), vec![0x8000_0000_0020_0000]);
        addresses
            .outputs
            .insert("y".into(), vec![0x8000_0000_0030_0000]);
        for (exe, expected_relocs) in [(&fixture.caching, 2), (&fixture.execution, 4)] {
            let bitstream = &exe.bitstreams[0];
            assert_eq!(bitstream.relocs.len(), expected_relocs, "{}", exe.name);
            let linked = link(bitstream, &addresses).unwrap();
            assert_eq!(linked.len(), bitstream.data.len());
            for reloc in &bitstream.relocs {
                assert_eq!(
                    read_u32(&bitstream.data, reloc.offset_bit).unwrap(),
                    0,
                    "{}: the oracle emits zero bases",
                    exe.name
                );
                let address = match reloc.desc {
                    Description::Parameter => 0x8000_0000_0010_0000u64,
                    Description::InputActivation => 0x8000_0000_0020_0000,
                    Description::OutputActivation => 0x8000_0000_0030_0000,
                    Description::Scratch => unreachable!(),
                };
                let half = if reloc.upper {
                    (address >> 32) as u32
                } else {
                    address as u32
                };
                assert_eq!(
                    read_u32(&linked, reloc.offset_bit).unwrap(),
                    half,
                    "{}: relocation at bit {}",
                    exe.name,
                    reloc.offset_bit
                );
            }
        }
    }
}

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn programs_run_bit_exact_on_the_card() {
    let Some((_guard, mut tpu)) = open_device() else {
        return;
    };
    for name in fixture_names() {
        let fixture = load(&name);
        let parameters = tpu
            .cache_parameters(&fixture.caching, &fixture.caching.parameters)
            .unwrap();
        for round in 0..3 {
            let run = tpu
                .run(
                    &fixture.execution,
                    Some(&parameters),
                    &[("x", &fixture.input)],
                )
                .unwrap();
            let (output_name, output) = &run.outputs[0];
            assert_eq!(output_name, "y");
            assert_eq!(
                &output[..fixture.expected.len()],
                &fixture.expected[..],
                "{name} round {round}: output differs from the oracle"
            );
            eprintln!(
                "{name} round {round}: bit-exact in {:?} ({} events)",
                run.elapsed,
                run.events.len()
            );
        }
        tpu.unload_parameters(parameters).unwrap();
    }
    tpu.close().unwrap();
}

/// A small deterministic generator for weights and inputs (xorshift64*).
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
struct Xorshift(u64);

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next_u64() >> 24) as u8).collect()
    }

    fn biases(&mut self, n: usize) -> Vec<i32> {
        (0..n)
            .map(|_| (self.next_u64() % 4001) as i32 - 2000)
            .collect()
    }
}

/// Generated FULLY_CONNECTED programs, run against the bit-exact reference.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn generated_programs_run_bit_exact_on_the_card() {
    use mdma::backend::coral::codegen::Quant;
    use mdma::backend::coral::codegen::fc::{FcProgram, fc_reference};
    let Some((_guard, mut tpu)) = open_device() else {
        return;
    };
    let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
    // (N, K, compute tile shift, parameter offset): the shapes exercise one input
    // tile, a west gather, a north relay chain, padded rows, all 16 compute tiles
    // with four passes each, and a shifted placement at a parameter offset.
    let cases = [
        (64, 64, 0, 0),
        (10, 300, 0, 0),
        (288, 288, 4, 4096),
        (1000, 320, 0, 0),
        (2045, 1330, 0, 0),
        (4096, 288, 0, 0),
        (1024, 1024, 0, 25600),
        (512, 512, 8, 0),
    ];
    for (n, k, shift, offset) in cases {
        let program = FcProgram::new(n, k);
        let quant = Quant::REFERENCE;
        let programs = program.programs(shift, offset, quant).unwrap();
        let w = rng.bytes(n * k);
        let bias = rng.biases(n);
        let x = rng.bytes(k);
        let expected = fc_reference(&quant, &x, &w, &bias, n, k);
        let parameters = tpu
            .cache_parameters(&programs.caching, &program.params(&w, &bias, quant.w_zp))
            .unwrap();
        let input = program.input_bytes(&x, quant.in_zp);
        let run = tpu
            .run(&programs.execution, Some(&parameters), &[("x", &input)])
            .unwrap();
        assert_eq!(
            program.output(&run.outputs[0].1),
            &expected[..],
            "fc {n}x{k} shift {shift} offset {offset}: output differs from the reference"
        );
        eprintln!(
            "generated fc {n}x{k} shift {shift} offset {offset}: bit-exact in {:?}",
            run.elapsed
        );
        tpu.unload_parameters(parameters).unwrap();
    }
    tpu.close().unwrap();
}

/// stories15M's matmul shapes cached side by side at disjoint (tiles, wide
/// offset) placements before any of them runs, then invoked interleaved as
/// loaded programs: fresh inputs written in place and every compute tile's
/// quantization patched between invokes. Weights of one layer must survive the
/// caching and execution of the others.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn resident_programs_share_the_chip() {
    use mdma::backend::coral::codegen::fc::{FcProgram, fc_reference};
    use mdma::backend::coral::codegen::{Quant, patch_quant};
    use std::time::{Duration, Instant};
    let Some((_guard, mut tpu)) = open_device() else {
        return;
    };
    let mut rng = Xorshift(0x51_7cc1_b727_220a);
    // (N, K, compute tile shift, parameter offset): q and k side by side, the
    // gate/up width above them, the down projection on the last tiles.
    let cases = [
        (288, 288, 0, 0),
        (288, 288, 5, 0),
        (768, 288, 0, 18_688),
        (288, 768, 11, 37_376),
    ];
    let w_zp = Quant::REFERENCE.w_zp;
    let mut layers = Vec::new();
    for (n, k, shift, offset) in cases {
        let program = FcProgram::new(n, k);
        let programs = program.programs(shift, offset, Quant::REFERENCE).unwrap();
        let w = rng.bytes(n * k);
        let bias = rng.biases(n);
        let parameters = tpu
            .cache_parameters(&programs.caching, &program.params(&w, &bias, w_zp))
            .unwrap();
        layers.push((program, programs, w, bias, parameters));
    }
    let mut loaded: Vec<_> = layers
        .iter()
        .map(|(_, programs, _, _, parameters)| {
            tpu.load(&programs.execution, Some(parameters)).unwrap()
        })
        .collect();
    for round in 0..6u8 {
        for ((program, programs, w, bias, _), exe) in layers.iter().zip(&mut loaded) {
            let (n, k) = (program.n, program.k);
            // Round 0 keeps the generated quantization; later rounds give every
            // compute tile its own.
            let quants: Vec<Quant> = (0..programs.ops.len())
                .map(|t| {
                    if round == 0 {
                        return Quant::REFERENCE;
                    }
                    let salt = (round as usize * 31 + t * 7) as u8;
                    let mut q = Quant::new(
                        96 + salt % 64,
                        w_zp,
                        64 + salt % 128,
                        1.0 / (1024.0 + 256.0 * f32::from(salt % 16)),
                    );
                    (q.lo, q.hi) = (salt % 8, 255 - salt % 8);
                    q
                })
                .collect();
            for (&word, quant) in programs.ops.iter().zip(&quants) {
                patch_quant(exe.chunk_mut(0).unwrap(), word, quant).unwrap();
            }
            let x = rng.bytes(k);
            // Every tile reads the same x, so one input zero point serves all tiles;
            // the first tile's is the one the padding uses.
            let input = program.input_bytes(&x, quants[0].in_zp);
            exe.input_mut("x").unwrap().copy_from_slice(&input);
            tpu.invoke(exe).unwrap();
            let out = program.output(exe.output("y").unwrap());
            let per_tile = 64 * programs.geom.p();
            for (t, quant) in quants.iter().enumerate() {
                let expected = fc_reference(quant, &x, w, bias, n, k);
                let rows = per_tile * t..(per_tile * (t + 1)).min(n);
                assert_eq!(
                    &out[rows.clone()],
                    &expected[rows.clone()],
                    "fc {n}x{k} round {round} tile {t}: output differs from the reference"
                );
            }
        }
    }
    // Wall-clock per call of a loaded program: write the input, invoke, read the output.
    let (program, _, _, _, _) = &layers[0];
    let x = rng.bytes(program.k);
    let input = program.input_bytes(&x, Quant::REFERENCE.in_zp);
    let calls = 2000u32;
    let mut slowest = Duration::ZERO;
    let start = Instant::now();
    for _ in 0..calls {
        let call = Instant::now();
        loaded[0].input_mut("x").unwrap().copy_from_slice(&input);
        tpu.invoke(&loaded[0]).unwrap();
        std::hint::black_box(loaded[0].output("y").unwrap());
        slowest = slowest.max(call.elapsed());
    }
    eprintln!(
        "loaded fc 288x288: {:?} per call over {calls} calls, slowest {slowest:?}",
        start.elapsed() / calls
    );
    for exe in loaded {
        tpu.unload(exe).unwrap();
    }
    for (_, _, _, _, parameters) in layers {
        tpu.unload_parameters(parameters).unwrap();
    }
    tpu.close().unwrap();
}

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn argmax(logits: &[f32]) -> u32 {
    logits
        .iter()
        .enumerate()
        .fold((0, f32::NEG_INFINITY), |best, (i, &l)| {
            if l > best.1 { (i, l) } else { best }
        })
        .0 as u32
}

/// `-ln softmax(logits)[token]`.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
fn nll(logits: &[f32], token: u32) -> f64 {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let sum: f64 = logits.iter().map(|&l| f64::from(l - max).exp()).sum();
    sum.ln() - f64::from(logits[token as usize] - max)
}

/// stories15M (`MDMA_TEST_STORIES15M_GGUF`) decoded by the coral backend stays
/// close to the float model in both decode modes: teacher-forced on stories the
/// float model writes, the perplexity is within a quarter of the CPU's and the
/// argmax agrees with the CPU's on most tokens.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn stories15m_decodes_on_the_card() {
    use mdma::backend::Backend;
    use mdma::backend::coral::tpu::Options;
    use mdma::backend::coral::{CoralBackend, Decode};
    use mdma::backend::cpu::CpuBackend;
    use mdma::gguf::GgufModel;
    use mdma::model::Model;
    use std::time::Instant;
    let (Ok(device), Ok(path)) = (
        std::env::var("MDMA_CORAL_DEVICE"),
        std::env::var("MDMA_TEST_STORIES15M_GGUF"),
    ) else {
        eprintln!("MDMA_CORAL_DEVICE or MDMA_TEST_STORIES15M_GGUF not set: skipped");
        return;
    };
    let _guard = DEVICE.lock().unwrap();
    let cpu = CpuBackend;
    let mut gguf = GgufModel::open(&path).unwrap();
    let bos = gguf
        .metadata_u32("tokenizer.ggml.bos_token_id")
        .unwrap_or(1);
    let float = Model::load(&cpu, &mut gguf).unwrap();
    let n_ctx = float.config().n_ctx;
    // The float model writes the stories and scores them.
    let mut rng = 0x7e57_5701_1e5a_11ed;
    let mut stories = Vec::new();
    for _ in 0..4 {
        let mut kv = float.new_kv_cache(&cpu, n_ctx).unwrap();
        let mut ws = float.alloc_workspace(&cpu, 1).unwrap();
        let mut story = vec![bos];
        let mut float_logits = Vec::new();
        for pos in 0..n_ctx - 1 {
            let logits = float
                .forward(&cpu, &mut kv, &mut ws, story[pos], pos as u32)
                .unwrap();
            story.push(mdma::backend::coral::model::sample(&logits, 1.0, &mut rng));
            float_logits.push(logits);
        }
        stories.push((story, float_logits));
    }
    for decode in [Decode::Matmul, Decode::Model] {
        let coral = CoralBackend::with_decode(
            Options {
                device: device.clone(),
                ..Options::default()
            },
            decode,
        )
        .unwrap();
        let mut gguf = GgufModel::open(&path).unwrap();
        let chip = Model::load(&coral, &mut gguf).unwrap();
        let (mut float_nll, mut chip_nll, mut agree, mut scored) = (0.0, 0.0, 0usize, 0usize);
        let mut decode_time = std::time::Duration::ZERO;
        for (story, float_logits) in &stories {
            // The chip reads the same story after a one-token prefill.
            let mut kv = chip.new_kv_cache(&coral, n_ctx).unwrap();
            let mut prefill = chip.alloc_workspace(&coral, 1).unwrap();
            let mut logits = chip
                .prefill(&coral, &mut kv, &mut prefill, &story[..1], 0)
                .unwrap();
            let mut ws = chip.alloc_workspace(&coral, 1).unwrap();
            let mut decoder = coral.decoder(&chip, &mut kv, &mut ws).unwrap();
            for pos in 1..n_ctx {
                let next = story[pos];
                float_nll += nll(&float_logits[pos - 1], next);
                chip_nll += nll(&logits, next);
                agree += usize::from(argmax(&logits) == argmax(&float_logits[pos - 1]));
                scored += 1;
                if pos + 1 < n_ctx {
                    let start = Instant::now();
                    logits = decoder.forward(next, pos as u32).unwrap();
                    decode_time += start.elapsed();
                }
            }
        }
        let float_ppl = (float_nll / scored as f64).exp();
        let chip_ppl = (chip_nll / scored as f64).exp();
        let agreement = agree as f64 / scored as f64;
        eprintln!(
            "stories15M {decode:?} over {scored} tokens: perplexity {float_ppl:.3} float, {chip_ppl:.3} on the card; \
             argmax agreement {:.1}%; {:.1} tokens/s decoding",
            100.0 * agreement,
            (scored - stories.len()) as f64 / decode_time.as_secs_f64()
        );
        assert!(
            chip_ppl <= 1.25 * float_ppl,
            "{decode:?}: perplexity {chip_ppl:.3} on the card against {float_ppl:.3} in float"
        );
        assert!(
            agreement >= 0.75,
            "{decode:?}: argmax agreement {agreement:.3}"
        );
    }
}

/// The whole stories15M model as one program per position: the oracle's
/// quantization (`data/fixtures/coral/model/quants.json`) gives the oracle's
/// parameter blob and programs, and on the card the programs reproduce the
/// oracle's bit model at P = 1, 2, 5 and 9 byte for byte.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
#[test]
fn whole_model_runs_bit_exact_on_the_card() {
    use mdma::backend::coral::codegen::attention::heads;
    use mdma::backend::coral::float_weights;
    use mdma::backend::coral::model::{model_params, rope_inputs};
    use mdma::backend::coral::whole::{Kv8, Whole};
    use mdma::backend::cpu::CpuBackend;
    use mdma::gguf::GgufModel;
    use mdma::model::Model;
    let Ok(path) = std::env::var("MDMA_TEST_STORIES15M_GGUF") else {
        eprintln!("MDMA_TEST_STORIES15M_GGUF not set: skipped");
        return;
    };
    let oracle = Oracle::open();
    let (dir, main) = (&oracle.dir, &oracle.main);
    let mut gguf = GgufModel::open(&path).unwrap();
    let model = Model::load(&CpuBackend, &mut gguf).unwrap();
    let weights = float_weights(&model).unwrap().unwrap();
    let blob = model_params(&weights, main);
    assert_eq!(
        sha(&blob),
        oracle.manifest["params"],
        "the parameter blob differs from the oracle's"
    );
    let pl = plan(6).unwrap();
    let Some((_guard, mut tpu)) = open_device() else {
        return;
    };
    let caching = gen_model(2, main, &pl).unwrap().caching;
    let parameters = tpu.cache_parameters(&caching, &blob).unwrap();
    for p in [1usize, 2, 5, 9] {
        let programs = oracle.programs(p, &pl);
        let vector = std::fs::read(dir.join(format!("vectors/P{p}.bin"))).unwrap();
        let (dm, rows) = (288usize, 288 * (p - 1));
        let mut at = 0;
        let mut take = |n: usize| {
            let s = &vector[at..at + n];
            at += n;
            s.to_vec()
        };
        let (x, cos, sin) = (take(dm), take(dm), take(dm));
        let mut inputs: Vec<(String, Vec<u8>)> =
            vec![("x".into(), x), ("cos".into(), cos), ("sin".into(), sin)];
        if p > 1 {
            for l in 0..6 {
                for kv in ["k", "v"] {
                    let mut block = take(rows);
                    block.resize(dm * p, 0);
                    inputs.push((format!("{kv}{l}"), heads(&block, p)));
                }
            }
        }
        let (x_out, kv) = (take(dm), take(6 * 2 * dm));
        let run = tpu
            .run(&programs.execution, Some(&parameters), &named(&inputs))
            .unwrap();
        let output = |name: &str| {
            run.outputs
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b.clone())
                .unwrap()
        };
        let mut report = vec![format!("out {:?}", diff(&output("out"), &x_out))];
        for l in 0..6 {
            let ours = output(&format!("kv{l}"));
            let theirs = &kv[l * 2 * dm..(l + 1) * 2 * dm];
            report.push(format!(
                "k{l} {:?} v{l} {:?}",
                diff(&ours[..dm], &theirs[..dm]),
                diff(&ours[dm..], &theirs[dm..])
            ));
        }
        eprintln!("P={p}: {}", report.join(", "));
        // The scalar core's exp2 and reciprocal are not correctly rounded: at P = 5 a
        // softmax value next to a rounding tie lands one code off in layer 3, and the
        // difference carries through the later layers.
        let outputs: Vec<(Vec<u8>, &[u8])> = (0..6)
            .map(|l| (output(&format!("kv{l}")), &kv[l * 2 * dm..(l + 1) * 2 * dm]))
            .chain(std::iter::once((output("out"), x_out.as_slice())))
            .collect();
        if p == 5 {
            let first = outputs
                .iter()
                .find(|(a, b)| a != b)
                .map(|(a, b)| diff(a, b).1);
            assert_eq!(
                first,
                Some(1),
                "P={p}: the first difference is not a tie: {report:?}"
            );
            assert!(
                outputs.iter().all(|(a, b)| diff(a, b).1 <= 8),
                "P={p}: the tie spread too far: {report:?}"
            );
        } else {
            assert!(
                outputs.iter().all(|(a, b)| a == b),
                "P={p}: differs from the bit model: {report:?}"
            );
        }
        eprintln!("whole model P={p}: {:?} per call", run.elapsed);
    }
    tpu.unload_parameters(parameters).unwrap();
    // The decoder's runtime: every program resident, the KV rows from the uint8
    // cache, the host's embedding and RoPE inputs.
    let mut whole = Whole::build(&mut tpu, weights, main.clone(), 128, 0, None).unwrap();
    let mut kv8 = Kv8::new(6, 128);
    for p in [2usize, 5, 9] {
        let vector = std::fs::read(dir.join(format!("vectors/P{p}.bin"))).unwrap();
        let (dm, rows) = (288usize, 288 * (p - 1));
        let x = &vector[..dm];
        let (cos, sin) = rope_inputs(p - 1, &main[0]);
        assert_eq!(
            (&cos[..], &sin[..]),
            (&vector[dm..2 * dm], &vector[2 * dm..3 * dm]),
            "P={p}: the host's RoPE inputs differ from the oracle's"
        );
        let mut at = 3 * dm;
        for l in 0..6 {
            let (k, v) = (&vector[at..at + rows], &vector[at + rows..at + 2 * rows]);
            for pos in 0..p - 1 {
                kv8.put(
                    l,
                    pos,
                    &k[pos * dm..(pos + 1) * dm],
                    &v[pos * dm..(pos + 1) * dm],
                );
            }
            at += 2 * rows;
        }
        let step = whole.run(&mut tpu, &kv8, x, &cos, &sin, p).unwrap();
        let kv: Vec<u8> = step.kv.iter().flatten().copied().collect();
        let reference = &vector[at + dm..at + dm + 6 * 2 * dm];
        let (differing, _) = diff(&kv, reference);
        eprintln!(
            "whole model runtime P={p}: {differing} of {} k'|v bytes differ",
            kv.len()
        );
        if p != 5 {
            assert_eq!(
                &kv[..],
                reference,
                "P={p}: the resident program differs from the bit model"
            );
        }
    }
    // Late positions: the resident programs (inputs written after mapping) answer
    // like freshly mapped ones on the same random inputs.
    let mut rng = Xorshift(0x0bad_5eed_1234_5678);
    for p in [64usize, 127] {
        let (x, cos, sin) = (rng.bytes(288), rng.bytes(288), rng.bytes(288));
        let mut kv8 = Kv8::new(6, 128);
        let mut inputs: Vec<(String, Vec<u8>)> = vec![
            ("x".into(), x.clone()),
            ("cos".into(), cos.clone()),
            ("sin".into(), sin.clone()),
        ];
        for l in 0..6 {
            let mut rows = [rng.bytes(288 * (p - 1)), rng.bytes(288 * (p - 1))];
            for pos in 0..p - 1 {
                let span = pos * 288..(pos + 1) * 288;
                kv8.put(l, pos, &rows[0][span.clone()], &rows[1][span]);
            }
            for (name, block) in ["k", "v"].iter().zip(&mut rows) {
                block.resize(288 * p, 0);
                inputs.push((format!("{name}{l}"), heads(block, p)));
            }
        }
        let fresh = gen_model(p, main, &pl).unwrap();
        let run = tpu.run(&fresh.execution, None, &named(&inputs)).unwrap();
        let resident = whole.run(&mut tpu, &kv8, &x, &cos, &sin, p).unwrap();
        for (l, row) in resident.kv.iter().enumerate() {
            let name = format!("kv{l}");
            let fresh_row = &run.outputs.iter().find(|(n, _)| *n == name).unwrap().1;
            assert_eq!(
                &row[..],
                &fresh_row[..],
                "P={p}: layer {l}'s resident k'|v differs from a fresh run"
            );
        }
        eprintln!("whole model runtime P={p}: resident equals fresh");
    }
    whole.release(&mut tpu).unwrap();
    tpu.close().unwrap();
}

/// The whole-model programs this crate generates from the oracle's quantization
/// (`data/fixtures/coral/model`) equal tinycoral's byte for byte (sha256 in the
/// manifest) at a spread of positions, and every input and output buffer is linked
/// through exactly one pair of relocated MOVIs.
#[test]
fn whole_model_programs_match_the_oracle() {
    use mdma::backend::coral::codegen::layer::{Placement, model_io};
    use mdma::backend::coral::isa::{bytes_to_words, scalar::IMM_BIT, scalar::WORD};
    let oracle = Oracle::open();
    let expected_plan = json(&oracle.dir.join("plan.json"));
    let pl = plan(6).unwrap();
    for (l, d) in pl.iter().enumerate() {
        for (m, placement) in d {
            let e = &expected_plan[l][m.name()];
            let ours = match placement {
                Placement::Spread(o) => serde_json::json!(["spread", o]),
                Placement::Stream(ps) => serde_json::json!([
                    "stream",
                    ps.iter().map(|(t, o)| [t, o]).collect::<Vec<_>>()
                ]),
            };
            assert_eq!(&ours, e, "layer {l} {}: placement", m.name());
        }
    }
    for p in [1usize, 2, 3, 4, 5, 8, 9, 16, 17, 64, 127, 128, 255, 256] {
        let programs = oracle.programs(p, &pl);
        assert_eq!(
            sha(&programs.caching.bitstreams[0].data),
            oracle.manifest["caching"],
            "the caching program differs from the oracle's"
        );
        programs.execution.validate().unwrap();
        let mut linked = Vec::new();
        for b in &programs.execution.bitstreams {
            let words = bytes_to_words(&b.data);
            for r in &b.relocs {
                let d = WORD.decode(&[words[(r.offset_bit - IMM_BIT) / 128]]);
                assert_eq!(
                    (d.get("s_op"), d.get("imm_scalar")),
                    (Some(0x2f), Some(0)),
                    "P={p}: relocation {r:?} is not a MOVI of zero"
                );
                linked.push((r.name.clone().unwrap(), r.upper));
            }
        }
        let (inputs, outputs) = model_io(p, 6);
        for layer in inputs.iter().chain(&outputs) {
            for upper in [false, true] {
                let n = linked
                    .iter()
                    .filter(|(n, u)| *n == layer.name && *u == upper)
                    .count();
                assert_eq!(
                    n, 1,
                    "P={p}: {} (upper {upper}) is linked {n} times",
                    layer.name
                );
            }
        }
        assert_eq!(linked.len(), 2 * (inputs.len() + outputs.len()));
    }
}
