use super::*;
use crate::backend::cpu::CpuBackend;
use crate::gguf::tests::Fixture;
use crate::registry::{Class, Info, Kind};
use std::assert_matches;
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};

fn info() -> Info {
    Info {
        kind: Kind::Gguf,
        architecture: "internlm2".into(),
        n_ctx: 8,
        disk_bytes: 0,
        class: Class::Generative,
        capabilities: Vec::new(),
        chat_format: None,
    }
}

fn resident() -> Resident<CpuBackend> {
    let file = Fixture::new(1, 8).with_tokenizer().write();
    let mut gguf = crate::gguf::GgufModel::open(file.path()).unwrap();
    let tokenizer = Arc::new(Tokenizer::from_gguf(&gguf).unwrap());
    Resident::Decoder(Decoder {
        model: Arc::new(Model::load(&CpuBackend, &mut gguf).unwrap()),
        tokenizer,
    })
}

fn slot() -> Arc<Slot<CpuBackend>> {
    Arc::new(Slot::new(info(), Source::Laya(PathBuf::new())))
}

#[test]
fn concurrent_first_uses_share_one_load() {
    let slot = slot();
    let loads = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(4));
    let model = Arc::new(Mutex::new(Some(resident())));
    #[expect(
        clippy::needless_collect,
        reason = "start every thread before joining any"
    )]
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (slot, loads, start, model) =
                (slot.clone(), loads.clone(), start.clone(), model.clone());
            std::thread::spawn(move || {
                start.wait();
                slot.acquire(|| {
                    loads.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    Ok(model.lock().unwrap().take().expect("loaded once"))
                })
                .map(|loaded| Arc::as_ptr(&loaded.decoder().unwrap().model) as usize)
            })
        })
        .collect();
    let models: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap())
        .collect();
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(models.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(slot.phase(), (Phase::Resident, None));
}

#[test]
fn a_refusal_reaches_its_waiters_and_the_next_use_retries() {
    let slot = slot();
    let refused = slot.acquire(|| Err(LoadError::Memory("no room".into())));
    assert_matches!(refused.map(|_| ()), Err(LoadError::Memory(e)) if e == "no room");
    assert_eq!(slot.phase(), (Phase::Available, Some("no room".into())));
    assert!(slot.acquire(|| Ok(resident())).is_ok());
    assert_eq!(slot.phase(), (Phase::Resident, None));
}

#[test]
fn a_failure_is_permanent() {
    let slot = slot();
    assert_matches!(
        slot.acquire(|| Err(LoadError::Failed("corrupt".into())))
            .map(|_| ()),
        Err(LoadError::Failed(_))
    );
    let again = slot.acquire(|| panic!("a failed model must not load again"));
    assert_matches!(again.map(|_| ()), Err(LoadError::Failed(e)) if e == "corrupt");
    assert_eq!(slot.phase(), (Phase::Failed, Some("corrupt".into())));
}

#[test]
fn a_panicking_load_leaves_the_slot_loadable() {
    let slot = slot();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = slot.acquire(|| panic!("injected"));
    }));
    assert!(panicked.is_err());
    assert_eq!(slot.phase().0, Phase::Available);
    assert!(slot.acquire(|| Ok(resident())).is_ok());
}
