//! Turning text into vectors for similarity search.
//!
//! Two embedders share one interface:
//! * `Hashed`: a dependency-free feature-hashing embedder (word, word-pair and character-trigram
//!   features into 256 buckets). It needs no download and already finds paraphrases that share
//!   vocabulary, so memory works from the first message.
//! * `MiniLm`: sentence-transformers `all-MiniLM-L6-v2` (about 90 MB), a real sentence encoder run
//!   on the CPU in pure Rust with Candle (no ONNX runtime, no Python). It understands that "cheap
//!   flight" and "inexpensive plane ticket" mean the same thing and can say how strongly two texts
//!   are related. It is fetched from Hugging Face on request (Settings > Memory) and used from then
//!   on; stored vectors are re-made when the model changes.
//!
//! Neither runs for every keystroke or mouse move: memory is written when a milestone is
//! confirmed and read once per message.

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
};

use crate::chat_store;

pub trait Embedder: Send + Sync {
    fn name(&self) -> &str;
    fn embed(&self, text: &str) -> Vec<f32>;
    /// `(floor, ceiling)`: the cosine similarity of unrelated texts and of clearly matching ones.
    /// Similarity is rescaled between them into a 0 to 100% "related" figure.
    fn calibration(&self) -> (f32, f32);
}

pub fn normalize_unit(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|v| *v /= norm);
    }
    vector
}

// ---- hashed -----------------------------------------------------------------------------

const HASH_DIM: usize = 256;

pub struct Hashed;

fn fnv(text: &str, seed: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A crude stem so "installing", "installed" and "installs" meet.
fn stem(word: &str) -> String {
    let mut stem = word.to_owned();
    for suffix in ["ing", "edly", "ed", "es", "s", "ly"] {
        if word.len() > suffix.len() + 3 && word.ends_with(suffix) {
            stem = word[..word.len() - suffix.len()].to_owned();
            break;
        }
    }
    // "fixes" -> "fixe", "fix" -> "fix": dropping a final e on every word makes them meet.
    if stem.len() > 3 && stem.ends_with('e') {
        stem.pop();
    }
    stem
}

fn add(vector: &mut [f32], feature: &str, weight: f32) {
    let hash = fnv(feature, 0);
    let sign = if fnv(feature, 0x9e37_79b9_7f4a_7c15) & 1 == 0 { 1.0 } else { -1.0 };
    vector[(hash % HASH_DIM as u64) as usize] += sign * weight;
}

impl Embedder for Hashed {
    fn name(&self) -> &str {
        "hashed-256"
    }

    fn calibration(&self) -> (f32, f32) {
        (0.20, 0.65)
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; HASH_DIM];
        let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(|w| stem(&w.to_lowercase())).collect();
        for (index, word) in words.iter().enumerate() {
            add(&mut vector, word, 1.0);
            if let Some(next) = words.get(index + 1) {
                add(&mut vector, &format!("{word} {next}"), 0.7);
            }
            let padded = format!("^{word}$");
            let chars: Vec<char> = padded.chars().collect();
            for window in chars.windows(3) {
                add(&mut vector, &window.iter().collect::<String>(), 0.25);
            }
        }
        normalize_unit(vector)
    }
}

// ---- MiniLM -----------------------------------------------------------------------------

pub const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const MODEL_FILES: [&str; 3] = ["model.safetensors", "tokenizer.json", "config.json"];
pub const MODEL_NAME: &str = "all-MiniLM-L6-v2";
pub const MODEL_SIZE: &str = "about 90 MB";
/// Sentences longer than this are cut (the model's own training limit is 256 word pieces).
const MAX_TOKENS: usize = 256;

pub fn model_dir() -> PathBuf {
    chat_store::app_data_root().unwrap_or_else(std::env::temp_dir).join("memory").join("models").join(MODEL_NAME)
}

pub fn model_installed() -> bool {
    MODEL_FILES.iter().all(|file| model_dir().join(file).is_file())
}

pub struct MiniLm {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: candle_core::Device,
}

impl MiniLm {
    pub fn load() -> Result<Self, String> {
        use candle_nn::VarBuilder;
        use candle_transformers::models::bert::{BertModel, Config, DTYPE};
        let dir = model_dir();
        let device = candle_core::Device::Cpu;
        let config: Config = serde_json::from_slice(&fs::read(dir.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| format!("The embedding model's config is invalid: {e}"))?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| format!("The embedding model's tokenizer is invalid: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(|e| e.to_string())?;
        tokenizer.with_padding(None);
        // SAFETY: the weights file is only read, and nothing else in Rhea writes to it while it is mapped.
        let vars = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DTYPE, &device) }.map_err(|e| format!("Could not read the embedding weights: {e}"))?;
        let model = BertModel::load(vars, &config).map_err(|e| format!("Could not set up the embedding model: {e}"))?;
        Ok(Self { model, tokenizer, device })
    }

    fn try_embed(&self, text: &str) -> Result<Vec<f32>, String> {
        use candle_core::{DType, Tensor};
        let encoding = self.tokenizer.encode(text, true).map_err(|e| e.to_string())?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let length = ids.len();
        let input = Tensor::new(ids, &self.device).and_then(|t| t.unsqueeze(0)).map_err(|e| e.to_string())?;
        let types = input.zeros_like().map_err(|e| e.to_string())?;
        let mask = Tensor::new(encoding.get_attention_mask(), &self.device).and_then(|t| t.unsqueeze(0)).map_err(|e| e.to_string())?;
        let hidden = self.model.forward(&input, &types, Some(&mask)).map_err(|e| e.to_string())?;
        // Mean of the word-piece vectors, weighted by the attention mask (sentence-transformers pooling).
        let mask_f = mask.to_dtype(DType::F32).and_then(|m| m.unsqueeze(2)).map_err(|e| e.to_string())?;
        let summed = hidden.broadcast_mul(&mask_f).and_then(|t| t.sum(1)).map_err(|e| e.to_string())?;
        let pooled = (summed / (length as f64)).map_err(|e| e.to_string())?;
        pooled.squeeze(0).and_then(|t| t.to_vec1::<f32>()).map_err(|e| e.to_string())
    }
}

impl Embedder for MiniLm {
    fn name(&self) -> &str {
        MODEL_NAME
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        self.try_embed(text).map(normalize_unit).unwrap_or_default()
    }

    fn calibration(&self) -> (f32, f32) {
        // A question and a memory that answers it usually sit between 0.35 and 0.65.
        (0.20, 0.65)
    }
}

/// Download the model files (about 90 MB) into `~/.rhea/memory/models`.
pub fn download_model(progress: impl Fn(f32)) -> Result<(), String> {
    use std::io::Read;
    let client = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(900)).user_agent(format!("Rhea/{}", env!("CARGO_PKG_VERSION"))).build().map_err(|e| e.to_string())?;
    let dir = model_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    let sizes = [0.9f32, 0.07, 0.03];
    let mut done = 0.0f32;
    for (index, file) in MODEL_FILES.iter().enumerate() {
        let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{file}");
        let mut response = client.get(&url).send().and_then(|r| r.error_for_status()).map_err(|e| format!("Download of {file} failed: {e}"))?;
        let total = response.content_length().unwrap_or(0);
        let partial = dir.join(format!("{file}.part"));
        let mut out = fs::File::create(&partial).map_err(|e| e.to_string())?;
        let (mut written, mut chunk) = (0u64, [0u8; 64 * 1024]);
        loop {
            let read = response.read(&mut chunk).map_err(|e| format!("Download of {file} failed: {e}"))?;
            if read == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &chunk[..read]).map_err(|e| e.to_string())?;
            written += read as u64;
            if total > 0 {
                progress(done + sizes[index] * (written as f32 / total as f32));
            }
        }
        drop(out);
        fs::rename(&partial, dir.join(file)).map_err(|e| e.to_string())?;
        done += sizes[index];
    }
    Ok(())
}

// ---- the active embedder ----------------------------------------------------------------

fn cell() -> &'static RwLock<Arc<dyn Embedder>> {
    static ACTIVE: OnceLock<RwLock<Arc<dyn Embedder>>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(pick()))
}

fn pick() -> Arc<dyn Embedder> {
    if model_installed() {
        match MiniLm::load() {
            Ok(model) => return Arc::new(model),
            Err(error) => eprintln!("{error}"),
        }
    }
    Arc::new(Hashed)
}

pub fn active() -> Arc<dyn Embedder> {
    Arc::clone(&cell().read().unwrap_or_else(|p| p.into_inner()))
}

/// Pick the embedder again (after the model was downloaded or removed).
pub fn reload() -> String {
    let next = pick();
    let name = next.name().to_owned();
    *cell().write().unwrap_or_else(|p| p.into_inner()) = next;
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn vectors_have_unit_length() {
        let vector = Hashed.embed("Please always answer in short sentences");
        assert_eq!(vector.len(), HASH_DIM);
        assert!((cosine(&vector, &vector) - 1.0).abs() < 1e-4);
        assert!(Hashed.embed("").iter().all(|v| *v == 0.0));
    }

    #[test]
    fn related_texts_are_closer_than_unrelated_ones() {
        let query = Hashed.embed("how do I install the app on windows");
        let related = Hashed.embed("Installed the app on Windows using the installer");
        let unrelated = Hashed.embed("The user prefers oat milk in coffee");
        assert!(cosine(&query, &related) > cosine(&query, &unrelated) + 0.1, "{} vs {}", cosine(&query, &related), cosine(&query, &unrelated));
    }

    #[test]
    fn word_forms_share_a_stem() {
        assert_eq!(stem("installing"), stem("installed"));
        assert_eq!(stem("fixes"), stem("fix"));
        assert_eq!(stem("is"), "is");
    }

    /// Network check of the real model: downloads it into a throwaway folder and compares meanings.
    #[test]
    #[ignore = "downloads about 90 MB"]
    fn live_the_minilm_model_downloads_and_understands_meaning() {
        let home = std::env::temp_dir().join(format!("rhea-embed-{}", std::process::id()));
        // SAFETY: single-threaded ignored test pointing the data folder at a throwaway directory.
        unsafe { std::env::set_var("USERPROFILE", &home) };
        download_model(|_| {}).expect("download");
        assert!(model_installed());
        let model = MiniLm::load().expect("load");
        let started = std::time::Instant::now();
        let a = model.embed("a cheap flight to Lisbon");
        let b = model.embed("an inexpensive plane ticket to Portugal");
        let c = model.embed("compile the Rust project in release mode");
        let (floor, ceiling) = model.calibration();
        let percent = |x: f32| (((x - floor) / (ceiling - floor)).clamp(0.0, 1.0) * 100.0).round();
        println!("related {:.3} ({}%) unrelated {:.3} ({}%) dim {} in {:?}", cosine(&a, &b), percent(cosine(&a, &b)), cosine(&a, &c), percent(cosine(&a, &c)), a.len(), started.elapsed());
        assert_eq!(a.len(), 384);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-3);
        assert!(cosine(&a, &b) > cosine(&a, &c) + 0.25);
        let _ = std::fs::remove_dir_all(&home);
    }
}
