//! The models in a folder: every `*.gguf`, and every subfolder holding a Laya checkpoint.
use super::{Error, Result, Source};
use std::path::Path;

/// Files a Laya checkpoint folder must contain.
const LAYA_FILES: [&str; 5] = [
    "model.safetensors",
    "rl_agent_config.json",
    "encoder/config.json",
    "tokenizer/tokenizer.json",
    "tokenizer/tokenizer_config.json",
];

/// Candidates sorted by name. Dotfiles, other files and names that are not
/// `[A-Za-z0-9._-]{1,128}` are skipped with a log line.
pub fn scan(dir: &Path) -> Result<Vec<(String, Source)>> {
    let unreadable = |e: std::io::Error| Error::Config(format!("{}: {e}", dir.display()));
    let entries = std::fs::read_dir(dir).map_err(unreadable)?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else { continue };
        if file_name.starts_with('.') {
            continue;
        }
        // Follow symlinks: a link to a GGUF or a checkpoint elsewhere is a model here.
        let Ok(metadata) = std::fs::metadata(&path) else {
            eprintln!("mdma: skipping {}: dangling link or unreadable", path.display());
            continue;
        };
        let (name, source) = if metadata.is_file() && path.extension().is_some_and(|ext| ext == "gguf") {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else { continue };
            (stem.to_owned(), Source::Gguf(path.clone()))
        } else if metadata.is_dir() && LAYA_FILES.iter().all(|file| path.join(file).is_file()) {
            (file_name.to_owned(), Source::Laya(path.clone()))
        } else {
            continue;
        };
        if let Err(e) = crate::federation::config::validate_model(&name) {
            eprintln!("mdma: skipping {}: {e}", path.display());
            continue;
        }
        found.push((name, source));
    }
    found.sort_by(|(a, _), (b, _)| a.cmp(b));
    if let Some(pair) = found.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(Error::Config(format!("model {:?} is both a GGUF file and a Laya folder in {}", pair[0].0, dir.display())));
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
