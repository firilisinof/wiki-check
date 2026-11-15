//! Local sentence embeddings and the single worker that computes them.
//!
//! One thread owns the model. Query embeddings are served before background
//! indexing, so a search never waits behind a long indexing queue.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Context;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::Tokenizer;

use crate::notes::sha256_hex;

/// input limit of all-MiniLM-L6-v2 in word pieces, including the two special tokens
pub const MAX_TOKENS: usize = 256;
/// changes whenever pooling or normalization changes, so old vectors stop matching
const PREPROCESSING: &str = "mean-pooling+l2";
const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];
const MAX_QUEUED_QUERIES: usize = 16;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EmbedError {
    #[error("input has {tokens} tokens, above the model limit of {limit}")]
    Overlength { tokens: usize, limit: usize },
    #[error("embedding failed: {0}")]
    Failed(String),
}

pub trait Embedder: Send + 'static {
    /// Identifies the weights, tokenizer, and preprocessing that produced a vector.
    fn model_id(&self) -> &str;
    /// A normalized vector for the whole input. Overlength input is an error, never truncated.
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

pub struct CandleEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    model_id: String,
}

impl CandleEmbedder {
    /// Load `config.json`, `tokenizer.json`, and `model.safetensors` from a directory.
    pub fn load(directory: &Path) -> anyhow::Result<Self> {
        let mut identity = PREPROCESSING.to_string();
        for name in MODEL_FILES {
            let path = directory.join(name);
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            identity.push_str(&format!("\n{name}:{}", sha256_hex(&bytes)));
        }
        let config: Config = serde_json::from_slice(&std::fs::read(directory.join("config.json"))?)
            .context("parsing config.json")?;
        let mut tokenizer = Tokenizer::from_file(directory.join("tokenizer.json"))
            .map_err(|error| anyhow::anyhow!("loading tokenizer.json: {error}"))?;
        // the published tokenizer truncates and pads to 128 tokens, which would
        // silently drop the end of a long input
        tokenizer
            .with_truncation(None)
            .map_err(|error| anyhow::anyhow!("disabling truncation: {error}"))?;
        tokenizer.with_padding(None);
        let weights = unsafe {
            // the file is read-only model data owned by the deployment
            VarBuilder::from_mmaped_safetensors(
                &[directory.join("model.safetensors")],
                DType::F32,
                &Device::Cpu,
            )
        }
        .context("loading model.safetensors")?;
        let model = BertModel::load(weights, &config).context("building the model")?;
        Ok(Self {
            model,
            tokenizer,
            model_id: format!("minilm:{}", &sha256_hex(identity.as_bytes())[..16]),
        })
    }

    fn forward(&self, token_ids: &[u32]) -> candle_core::Result<Vec<f32>> {
        let ids = Tensor::new(token_ids, &Device::Cpu)?.unsqueeze(0)?;
        let token_types = ids.zeros_like()?;
        let states = self.model.forward(&ids, &token_types, None)?;
        // one unpadded sequence, so the plain mean equals the masked mean
        let pooled = states.mean(1)?.squeeze(0)?;
        let norm = pooled.sqr()?.sum_all()?.sqrt()?;
        pooled.broadcast_div(&norm)?.to_vec1()
    }
}

impl Embedder for CandleEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| EmbedError::Failed(format!("tokenizing: {error}")))?;
        let token_ids = encoding.get_ids();
        if token_ids.len() > MAX_TOKENS {
            return Err(EmbedError::Overlength {
                tokens: token_ids.len(),
                limit: MAX_TOKENS,
            });
        }
        self.forward(token_ids)
            .map_err(|error| EmbedError::Failed(error.to_string()))
    }
}

type Reply = Box<dyn FnOnce(Result<Vec<f32>, EmbedError>) + Send>;

struct Job {
    text: String,
    reply: Reply,
}

#[derive(Default)]
struct Queues {
    queries: VecDeque<Job>,
    background: VecDeque<Job>,
}

struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
}

/// Handle to the embedding thread.
#[derive(Clone)]
pub struct EmbedWorker {
    shared: Arc<Shared>,
    model_id: String,
}

impl EmbedWorker {
    pub fn spawn(embedder: impl Embedder) -> Self {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            ready: Condvar::new(),
        });
        let model_id = embedder.model_id().to_string();
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("embeddings".to_string())
            .spawn(move || {
                loop {
                    let job = {
                        let mut queues = worker.queues.lock().expect("embedding queue lock");
                        loop {
                            if let Some(job) = queues.queries.pop_front() {
                                break job;
                            }
                            if let Some(job) = queues.background.pop_front() {
                                break job;
                            }
                            queues = worker.ready.wait(queues).expect("embedding queue lock");
                        }
                    };
                    (job.reply)(embedder.embed(&job.text));
                }
            })
            .expect("starting the embedding thread");
        Self { shared, model_id }
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Embed a search query ahead of any background work.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        {
            let mut queues = self.shared.queues.lock().expect("embedding queue lock");
            if queues.queries.len() >= MAX_QUEUED_QUERIES {
                return Err(EmbedError::Failed(format!(
                    "{MAX_QUEUED_QUERIES} queries are already waiting for the embedding worker"
                )));
            }
            queues.queries.push_back(Job {
                text: text.to_string(),
                reply: Box::new(move |result| {
                    let _ = sender.send(result);
                }),
            });
        }
        self.shared.ready.notify_one();
        receiver
            .await
            .map_err(|_| EmbedError::Failed("the embedding worker stopped".to_string()))?
    }

    /// Embed an indexing input, blocking the calling thread until it is done.
    pub fn embed_background(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.shared
            .queues
            .lock()
            .expect("embedding queue lock")
            .background
            .push_back(Job {
                text: text.to_string(),
                reply: Box::new(move |result| {
                    let _ = sender.send(result);
                }),
            });
        self.shared.ready.notify_one();
        receiver
            .recv()
            .map_err(|_| EmbedError::Failed("the embedding worker stopped".to_string()))?
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Maps a text to a fixed two-dimensional vector by its first known keyword.
    pub struct FakeEmbedder {
        pub directions: Vec<(&'static str, [f32; 2])>,
    }

    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> &str {
            "fake-model"
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            if text.contains("overlength") {
                return Err(EmbedError::Overlength {
                    tokens: MAX_TOKENS + 1,
                    limit: MAX_TOKENS,
                });
            }
            let lower = text.to_lowercase();
            self.directions
                .iter()
                .find(|(keyword, _)| lower.contains(keyword))
                .map(|(_, vector)| vector.to_vec())
                .ok_or_else(|| EmbedError::Failed(format!("no direction for {text:?}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_worker_serves_queries_and_background_inputs() {
        let worker = EmbedWorker::spawn(testing::FakeEmbedder {
            directions: vec![("deploy", [1.0, 0.0])],
        });
        assert_eq!(worker.model_id(), "fake-model");
        assert_eq!(
            worker.embed_query("Deploy it").await.unwrap(),
            vec![1.0, 0.0]
        );
        let background = worker.clone();
        let result = tokio::task::spawn_blocking(move || background.embed_background("overlength"))
            .await
            .unwrap();
        assert!(matches!(result, Err(EmbedError::Overlength { .. })));
    }

    /// Runs only where the pinned model files are present.
    #[test]
    fn minilm_embeds_similar_sentences_closer() {
        let Ok(directory) = std::env::var("WIKI_MCP_MODEL_DIR") else {
            eprintln!("skipped: WIKI_MCP_MODEL_DIR is not set");
            return;
        };
        let embedder = CandleEmbedder::load(Path::new(&directory)).unwrap();
        let embed = |text: &str| embedder.embed(text).unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let deploy = embed("How to deploy the server configuration");
        assert_eq!(deploy.len(), 384);
        assert!((dot(&deploy, &deploy) - 1.0).abs() < 1e-4);
        let near = embed("Steps for rolling out a new system build");
        let far = embed("A recipe for chocolate cake");
        assert!(dot(&deploy, &near) > dot(&deploy, &far));
        // reference value from sentence-transformers for this exact pair
        let pair = dot(
            &embed("That is a happy person"),
            &embed("That is a very happy person"),
        );
        assert!((pair - 0.9429).abs() < 0.01, "similarity {pair}");
        let long = "word ".repeat(400);
        assert!(matches!(
            embedder.embed(&long),
            Err(EmbedError::Overlength { .. })
        ));
    }
}
