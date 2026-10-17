// SPDX-License-Identifier: Apache-2.0

//! The optional embedder: turning text into a vector, over someone else's model.
//!
//! # Why there is no model in this binary
//!
//! §9-D1 of the plan decides this, and the decision is worth restating where the code
//! lives: the vector leg is **bring your own embedder**. No ONNX runtime, no bundled
//! weights, no first-run download. A user with Ollama running gets semantic search by
//! editing one block of a config file; a user without one gets a full-text engine that
//! never touches the network. Option C (a bundled `bge-small` behind a cargo feature)
//! stays additive because the trait and the table are the seam.
//!
//! # What a vector is, and what two vectors are not
//!
//! Vectors are stored L2-normalised, so the read path is a dot product. They are
//! **only comparable within one model**: two different models produce vectors in
//! different spaces that still have plausible cosine similarity, so a mixed corpus
//! does not error, it just quietly gets worse. Every row is therefore keyed by model
//! name, the read path filters on exactly one model, and `doctor` says how much of the
//! corpus has no vector for the configured one.
//!
//! # Plain HTTP, and why
//!
//! The embedder URL must be `http:`. Every local server this is built for — Ollama, LM
//! Studio, `llama.cpp`, vLLM, text-embeddings-inference — listens on plain HTTP on
//! localhost, and TLS is not free: `ureq`'s rustls feature costs about twenty crates
//! and a C build, against a plan budget of sixty transitive crates and a 6 MB binary
//! (§7, Appendix B). An `https:` URL is refused by name with the workaround, rather
//! than failing somewhere inside a TLS handshake. The trigger to revisit is recorded in
//! the plan.

use std::io::Read as _;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The most text one embed call is given for one input, in characters.
///
/// Ported from the platform's `MAX_EMBED_CHARS`, and for the same reason: a model that
/// is handed a hundred kilobyte memory does not fail, it allocates, and the failure
/// arrives as a memory spike on someone else's machine. Truncation is reported rather
/// than silent — see [`EmbedOutcome::truncated`].
pub const MAX_EMBED_CHARS: usize = 20_000;

/// How many texts one HTTP call carries.
///
/// Ported from the platform's `DEFAULT_BATCH_SIZE`. Ollama embeds a batch in one pass
/// over the model, so this is the difference between one call per memory and one call
/// per reindex page.
pub const DEFAULT_BATCH: usize = 64;

/// How long one embed call may take before it is abandoned.
///
/// Generous on purpose. A cold Ollama loads the model on the first call, which is
/// seconds rather than milliseconds on a laptop, and a timeout that fired during the
/// one call that was about to succeed would make the whole leg look broken. A local
/// embedder that cannot answer in thirty seconds is not going to answer.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Which HTTP shape to speak.
///
/// Two shapes, because that is what the world has: Ollama's own API and the
/// OpenAI-compatible one that everything else implements (LM Studio, `llama.cpp`,
/// vLLM, text-embeddings-inference, and OpenAI itself behind a proxy). They differ in
/// the path, the request key (`input` vs `input`), and the response envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// `POST {url}/api/embed` → `{"embeddings": [[...]]}`. Ollama since 0.3.5.
    Ollama,
    /// `POST {url}/embeddings` → `{"data": [{"embedding": [...], "index": 0}]}`.
    OpenAi,
    /// Deterministic pseudo-vectors, no model, no network.
    ///
    /// Not a search feature. It exists so tests and `kiomon bench` can exercise the
    /// vector leg's plumbing — the query, the fusion, the blob encoding — on a machine
    /// with no model, deterministically and in microseconds. Its vectors are a hashed
    /// bag of words: two texts that share words are similar, two texts that mean the
    /// same thing are not. The model name it reports is `stub`, and `doctor` prints
    /// that the leg is a stub, so a brain that was built with it says so.
    Stub,
}

impl Provider {
    /// The word a user writes in the config file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::OpenAi => "openai",
            Self::Stub => "stub",
        }
    }

    /// The default URL, when the provider is one that has a conventional local port.
    ///
    /// Ollama's is 11434 and it is the whole point of the provider; the
    /// OpenAI-compatible one is whatever the user is running, so there is no default
    /// and a missing URL is a config error naming the key.
    #[must_use]
    pub const fn default_url(self) -> Option<&'static str> {
        match self {
            Self::Ollama => Some("http://127.0.0.1:11434"),
            Self::OpenAi | Self::Stub => None,
        }
    }
}

/// What the config file may say about the embedder.
///
/// Deserialised from `[embedder]`. Every field is optional here and validated in
/// [`EmbedderConfig::build`], so a typo produces one message naming the key rather
/// than a serde error naming a field path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedderConfig {
    /// `ollama`, `openai`, or `stub`.
    pub provider: Option<String>,
    /// Base URL, without the path this implementation appends.
    pub url: Option<String>,
    /// The model to ask for, recorded with every vector.
    pub model: Option<String>,
    /// The dimension the model is expected to return, if the user knows it.
    ///
    /// Optional, and worth setting: it turns "the embedder answered with 768 numbers
    /// and the corpus holds 384" from a corpus that silently mixes spaces into one
    /// error at the first write.
    pub dim: Option<usize>,
    /// Texts per HTTP call.
    pub batch: Option<usize>,
    /// Seconds one call may take.
    pub timeout_secs: Option<u64>,
    /// Prepended to every embedded *document*, if the model wants one.
    ///
    /// `nomic-embed-text` is the reason this exists: without `search_document:` and
    /// `search_query:` prefixes its retrieval quality drops measurably, and nothing in
    /// the API tells you. Empty by default, because most models want nothing.
    pub document_prefix: Option<String>,
    /// Prepended to every embedded *query*.
    pub query_prefix: Option<String>,
    /// A bearer token, for an endpoint that wants one.
    ///
    /// Prefer `$KIOMON_EMBEDDER_API_KEY`, which is read by the CLI: a secret in a
    /// config file is a secret in every backup and every screenshot.
    pub api_key: Option<String>,
}

/// A built embedder, ready to be called.
pub struct Embedder {
    inner: Box<dyn Backend>,
    model: String,
    dim: Option<usize>,
    batch: usize,
    document_prefix: String,
    query_prefix: String,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("provider", &self.inner.provider())
            .field("model", &self.model)
            .field("dim", &self.dim)
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

/// One embedder's transport.
///
/// The trait is what `stub` and the two HTTP shapes share, and it is where a test
/// installs a model-shaped fake: a fixture that needs `deploy` to be near `ship` can
/// implement this and prove the vector leg returns a row the lexical leg cannot,
/// without a model on the machine.
pub trait Backend: Send + Sync {
    /// The provider name, for messages.
    fn provider(&self) -> &'static str;

    /// Where this backend sends, if it sends anywhere.
    fn endpoint(&self) -> Option<String> {
        None
    }

    /// Embed a batch, in order. The caller has already applied prefixes and truncation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the endpoint cannot be reached or answers
    /// something that is not embeddings.
    fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

impl Embedder {
    /// Validate a config block and build the client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the offending key when the provider is unknown,
    /// a URL is missing or not `http:`, the model is missing, or a count is zero.
    pub fn build(config: &EmbedderConfig) -> Result<Self> {
        let provider_name = config
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("ollama");
        let provider: Provider = match provider_name.to_ascii_lowercase().as_str() {
            "ollama" => Provider::Ollama,
            "openai" | "openai-compatible" => Provider::OpenAi,
            "stub" => Provider::Stub,
            other => {
                return Err(Error::Config(format!(
                    "unknown embedder provider `{other}` in the `provider` key of the \
                     [embedder] block. Expected one of: ollama, openai, stub."
                )));
            }
        };

        let model = config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        // The stub has a model name of its own: asking a user to invent one for a leg
        // that ignores it is a required flag that means nothing.
        let model = match (model, provider) {
            (Some(model), _) => model,
            (None, Provider::Stub) => "stub".to_owned(),
            (None, _) => {
                return Err(Error::Config(format!(
                    "the [embedder] block needs a `model` for provider `{}`: it is what \
                     the local server is asked for, and what every stored vector is \
                     labelled with.",
                    provider.as_str()
                )));
            }
        };

        let url = config
            .url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| provider.default_url().map(str::to_owned));
        let url = match (url, provider) {
            (Some(url), _) => check_scheme(&url)?,
            (None, Provider::OpenAi) => {
                return Err(Error::Config(
                    "the [embedder] block needs a `url` for provider `openai`: the \
                     OpenAI-compatible shape is served by many servers, and there is no \
                     port this program could guess sensibly. Example: \
                     `url = \"http://127.0.0.1:1234/v1\"`."
                        .to_owned(),
                ));
            }
            (None, _) => String::new(),
        };

        if let Some(dim) = config.dim {
            if dim == 0 {
                return Err(Error::Config(
                    "the [embedder] block sets `dim = 0`, which no model returns".to_owned(),
                ));
            }
        }
        let batch = config.batch.unwrap_or(DEFAULT_BATCH);
        if batch == 0 {
            return Err(Error::Config(
                "the [embedder] block sets `batch = 0`, which would embed nothing; omit \
                 it for the default."
                    .to_owned(),
            ));
        }
        let timeout = config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if timeout == 0 {
            return Err(Error::Config(
                "the [embedder] block sets `timeout_secs = 0`, which would abandon every \
                 call before the server could answer."
                    .to_owned(),
            ));
        }

        let inner: Box<dyn Backend> = match provider {
            Provider::Stub => Box::new(StubBackend),
            Provider::Ollama | Provider::OpenAi => Box::new(HttpBackend::new(
                provider,
                url,
                config.api_key.clone(),
                Duration::from_secs(timeout),
            )),
        };

        Ok(Self {
            inner,
            model,
            dim: config.dim,
            batch,
            document_prefix: config.document_prefix.clone().unwrap_or_default(),
            query_prefix: config.query_prefix.clone().unwrap_or_default(),
        })
    }

    /// An embedder over a caller-supplied backend.
    ///
    /// Two callers: the tests, which need similarity that means something rather than a
    /// hash of the words (so that "a semantic-only query" is a fixture rather than a
    /// hope), and a future `native-embed` build that links a model in-process instead of
    /// talking to a server. Nothing else should want it — the providers in
    /// [`Embedder::build`] are the supported ones.
    #[must_use]
    pub fn from_backend(backend: Box<dyn Backend>, model: impl Into<String>) -> Self {
        Self {
            inner: backend,
            model: model.into(),
            dim: None,
            batch: DEFAULT_BATCH,
            document_prefix: String::new(),
            query_prefix: String::new(),
        }
    }

    /// The model name every vector from this embedder is labelled with.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The dimension the user said the model returns, if they said.
    #[must_use]
    pub const fn dim(&self) -> Option<usize> {
        self.dim
    }

    /// The provider name, for messages and for `doctor`.
    #[must_use]
    pub fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    /// How many texts one call carries.
    #[must_use]
    pub const fn batch(&self) -> usize {
        self.batch
    }

    /// The URL this embedder sends to, if it sends anywhere.
    ///
    /// Reported by `doctor`, because "which address did it go to" is the first question
    /// when a model does not answer, and asserted by the config tests, which must not
    /// need a server to check that a trailing slash was handled.
    #[must_use]
    pub fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }

    /// Embed memories: the document prefix, the model's own text rule, truncation.
    ///
    /// # Errors
    ///
    /// As [`Backend::embed`], plus a dimension mismatch between the configured
    /// dimension and what the model returned.
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .iter()
            .map(|text| {
                let body = crate::embed::clip(text);
                if self.document_prefix.is_empty() {
                    body
                } else {
                    format!("{}{body}", self.document_prefix)
                }
            })
            .collect();
        self.embed_prefixed(&prefixed)
    }

    /// Embed one query: the query prefix, the same truncation rule.
    ///
    /// A query and a document are embedded by the same call with different prefixes,
    /// which is what the asymmetric retrieval models in this space expect.
    ///
    /// # Errors
    ///
    /// As [`Backend::embed`].
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let body = if self.query_prefix.is_empty() {
            clip(text)
        } else {
            format!("{}{}", self.query_prefix, clip(text))
        };
        let mut vectors = self.embed_prefixed(&[body])?;
        vectors.pop().ok_or_else(|| {
            Error::Internal("the embedder returned no vector for one query".to_owned())
        })
    }

    /// Whether an embedded text had to be truncated to fit [`MAX_EMBED_CHARS`].
    #[must_use]
    pub fn truncates(text: &str) -> bool {
        crate::embed::is_clipped(text)
    }

    /// One or more batches, in order, with every dimension checked.
    fn embed_prefixed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.batch) {
            let mut vectors = self.inner.embed(&self.model, batch)?;
            if vectors.len() != batch.len() {
                return Err(Error::Config(format!(
                    "the {} embedder returned {} vector(s) for {} input(s). The model \
                     `{}` is not answering the shape this program speaks.",
                    self.inner.provider(),
                    vectors.len(),
                    batch.len(),
                    self.model
                )));
            }
            for vector in vectors.drain(..) {
                if let Some(expected) = self.dim {
                    if vector.len() != expected {
                        return Err(Error::Config(format!(
                            "the embedder returned {} dimensions for model `{}`, and the \
                             config says `dim = {expected}`. Either the model name is not \
                             the one that was meant, or `dim` is left over from another \
                             model; vectors from two models are not comparable and would \
                             be mixed silently.",
                            vector.len(),
                            self.model
                        )));
                    }
                }
                out.push(normalize(vector));
            }
        }
        Ok(out)
    }
}

/// The text a memory is embedded as.
///
/// Title, then body, falling back to the excerpt when there is no body. The hosted
/// product embeds a document's body alone, with `title + excerpt` only when a body is
/// missing, because it chunks long documents and the chunk is the unit of meaning. A
/// local memory *is* the unit, and its title is the most semantic line its writer gave
/// it — dropping it would make "migrations are manual on staging" unfindable by a query
/// about a migration whenever the body is a stack trace.
#[must_use]
pub fn memory_text(title: &str, body: &str, excerpt: &str) -> String {
    let title = title.trim();
    let body = body.trim();
    if !body.is_empty() {
        if title.is_empty() {
            return body.to_owned();
        }
        return format!("{title}\n{body}");
    }
    let excerpt = excerpt.trim();
    if title.is_empty() {
        return excerpt.to_owned();
    }
    if excerpt.is_empty() {
        return title.to_owned();
    }
    format!("{title}\n{excerpt}")
}

/// Clamp to [`MAX_EMBED_CHARS`] characters, on a character boundary.
fn clip(text: &str) -> String {
    if !is_clipped(text) {
        return text.to_owned();
    }
    text.chars().take(MAX_EMBED_CHARS).collect()
}

/// Whether [`clip`] would remove anything.
fn is_clipped(text: &str) -> bool {
    text.chars().count() > MAX_EMBED_CHARS
}

/// L2-normalise, so the read path can use a dot product.
///
/// A zero vector stays zero rather than becoming NaN: it can only come from a model
/// that returned zeros, and a NaN in the corpus would make every comparison with it
/// false in a way nobody could trace back here.
fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let mut sum = 0.0f64;
    for value in &vector {
        sum += f64::from(*value) * f64::from(*value);
    }
    if sum <= 0.0 || !sum.is_finite() {
        return vector;
    }
    let inverse = 1.0 / sum.sqrt();
    for value in &mut vector {
        // Every coordinate of a unit vector is in `-1.0..=1.0`, where `f32` has more than
        // enough precision for a cosine that only ever decides an ordering.
        #[allow(clippy::cast_possible_truncation)]
        {
            *value = (f64::from(*value) * inverse) as f32;
        }
    }
    vector
}

/// Refuse anything but plain HTTP, and say why rather than failing inside a handshake.
fn check_scheme(url: &str) -> Result<String> {
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("https://") {
        if rest.trim_matches('/').is_empty() {
            return Err(Error::Config(format!(
                "the [embedder] `url` is `{url}`, which has no host"
            )));
        }
        return Err(Error::Config(format!(
            "the [embedder] `url` is `{url}`, and this build speaks only plain HTTP to an \
             embedder: the default build has no TLS. Ollama, LM Studio, llama.cpp and \
             text-embeddings-inference all serve plain HTTP on localhost, and a hosted \
             endpoint can be reached through a local proxy. Set `url` to a `http://` \
             address, or run a model locally."
        )));
    }
    if !lower.starts_with("http://") {
        return Err(Error::Config(format!(
            "the [embedder] `url` must start with `http://`, and it starts with `{url}`"
        )));
    }
    // A trailing slash would produce `//api/embed`, which most servers treat as a
    // different path, and the ones that tolerate it are not the ones worth debugging.
    Ok(url.trim_end_matches('/').to_owned())
}

// ---------------------------------------------------------------------------
// The two HTTP shapes
// ---------------------------------------------------------------------------

/// The URL this platform's own HTTP client sends to.
struct HttpBackend {
    provider: Provider,
    url: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl HttpBackend {
    fn new(provider: Provider, url: String, api_key: Option<String>, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // A local server answers in milliseconds and a failed one refuses the
            // connection immediately; following a redirect is a way to end up talking to
            // something that is not the embedder the user configured.
            .max_redirects(0)
            .build();
        Self {
            provider,
            url,
            api_key,
            agent: config.into(),
        }
    }

    /// The path each shape appends to the configured base URL.
    fn path(&self) -> String {
        match self.provider {
            Provider::Ollama => format!("{}/api/embed", self.url),
            Provider::OpenAi => format!("{}/embeddings", self.url),
            Provider::Stub => unreachable!("the stub never reaches HTTP"),
        }
    }

    /// Parse whichever response envelope this provider uses.
    fn parse(&self, model: &str, body: &str) -> Result<Vec<Vec<f32>>> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Err(Error::Config(format!(
                "the {} embedder at {} returned an empty body for model `{model}`",
                self.provider.as_str(),
                self.url
            )));
        }
        match self.provider {
            Provider::Ollama => {
                #[derive(Deserialize)]
                struct Response {
                    embeddings: Vec<Vec<f32>>,
                }
                let parsed: Response = serde_json::from_str(trimmed).map_err(|err| {
                    Error::Config(format!(
                        "the Ollama embedder at {} answered something that is not \
                         `{{\"embeddings\": [[...]]}}`: {err}. This program needs Ollama \
                         0.3.5 or newer, which serves `POST /api/embed`.",
                        self.url
                    ))
                })?;
                Ok(parsed.embeddings)
            }
            Provider::OpenAi => {
                #[derive(Deserialize)]
                struct Entry {
                    embedding: Vec<f32>,
                    #[serde(default)]
                    index: usize,
                }
                #[derive(Deserialize)]
                struct Response {
                    data: Vec<Entry>,
                }
                let parsed: Response = serde_json::from_str(trimmed).map_err(|err| {
                    Error::Config(format!(
                        "the OpenAI-compatible embedder at {} answered something that is \
                         not `{{\"data\": [{{\"embedding\": [...]}}]}}`: {err}",
                        self.url
                    ))
                })?;
                // Sorted by `index` rather than trusted: the order of the vectors has to
                // pair with the order of the texts, and pairing the wrong vector with the
                // wrong text is the one failure here that no later check could notice.
                let mut entries = parsed.data;
                entries.sort_by_key(|entry| entry.index);
                Ok(entries.into_iter().map(|entry| entry.embedding).collect())
            }
            Provider::Stub => unreachable!("the stub never reaches HTTP"),
        }
    }
}

impl Backend for HttpBackend {
    fn provider(&self) -> &'static str {
        self.provider.as_str()
    }

    fn endpoint(&self) -> Option<String> {
        Some(self.path())
    }

    fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let payload = serde_json::json!({ "model": model, "input": texts }).to_string();
        let mut request = self
            .agent
            .post(self.path())
            .header("content-type", "application/json")
            // No `accept-encoding`: this build has no decompressor, and a server that
            // gzipped a response anyway would be read as a corrupt one. Asking for
            // nothing is how the header protocol is meant to work.
            .header("accept", "application/json");
        if let Some(key) = &self.api_key {
            request = request.header("authorization", &format!("Bearer {key}"));
        }

        let mut response = request.send(payload.as_bytes()).map_err(|err| {
            Error::Config(format!(
                "the {} embedder at {} could not be reached for model `{model}`: {err}. \
                 Is it running, and is `url` in the [embedder] block right?",
                self.provider.as_str(),
                self.url
            ))
        })?;

        // Read as bytes and convert by hand: a body that is not UTF-8 is a body that is
        // not JSON, and `ureq`'s own text conversion would report it as a charset error
        // instead of as the API contract it actually is.
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(64 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|err| {
                Error::Config(format!(
                    "the {} embedder at {} stopped mid-response for model `{model}`: {err}",
                    self.provider.as_str(),
                    self.url
                ))
            })?;

        // Non-2xx lands here, and the server's own message is the useful part: Ollama
        // says `model "nope" not found, try pulling it first`, which is the whole answer.
        if let Some(status) = response_error(&response) {
            let body = String::from_utf8_lossy(&bytes);
            let body = body.trim();
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(": {}", truncate_for_message(body))
            };
            return Err(Error::Config(format!(
                "the {} embedder at {} answered HTTP {status} for model `{model}`{detail}",
                self.provider.as_str(),
                self.url
            )));
        }

        let body = String::from_utf8(bytes).map_err(|_| {
            Error::Config(format!(
                "the {} embedder at {} returned a body that is not UTF-8",
                self.provider.as_str(),
                self.url
            ))
        })?;
        let vectors = self.parse(model, &body)?;
        if vectors.is_empty() {
            return Err(Error::Config(format!(
                "the {} embedder at {} returned no vectors for model `{model}`",
                self.provider.as_str(),
                self.url
            )));
        }
        Ok(vectors)
    }
}

/// The status of a response that is not a success.
///
/// `ureq` treats 4xx and 5xx as errors with a payload, and this build wants the body, so
/// the status is read rather than matched on.
fn response_error(response: &ureq::http::Response<ureq::Body>) -> Option<u16> {
    let status = response.status().as_u16();
    (status >= 400).then_some(status)
}

/// Keep a server's error body to one line and one line's worth of characters.
fn truncate_for_message(body: &str) -> String {
    let flattened = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() > 500 {
        flattened.chars().take(500).collect::<String>() + "…"
    } else {
        flattened
    }
}

// ---------------------------------------------------------------------------
// The stub
// ---------------------------------------------------------------------------

/// Deterministic pseudo-vectors: a hashed bag of words, L2-normalised.
///
/// The dimension is fixed rather than configurable because nothing compares two stub
/// corpora across dimensions, and a fixed one makes the vector column's width — and so
/// the blob encoding's cost — the same in every test and every bench run.
const STUB_DIM: usize = 384;

/// FNV-1a, 64-bit: the same family as the content hash, for the same reason.
const FNV64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv64(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = FNV64_OFFSET_BASIS ^ seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV64_PRIME);
    }
    hash
}

/// The stub's embedding of one text.
///
/// Each word contributes to four dimensions of the vector, which is what makes two
/// texts that share words come out similar; the vector is then normalised. The values
/// are drawn from the hash itself, so the whole thing is a pure function of the text.
fn stub_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; STUB_DIM];
    for word in text.split_whitespace() {
        let normalized = crate::hash::normalize(word);
        if normalized.is_empty() {
            continue;
        }
        // Three chip dimensions per word rather than one: one dimension collides into
        // cancellation often enough that two unrelated long texts can come out
        // orthogonal-but-zero, and the point of the stub is that the numbers are stable
        // and comparable, not that they are good.
        for lane in 0..3u64 {
            let hash = fnv64(normalized.as_bytes(), lane);
            // `STUB_DIM` is a small constant, so the remainders fit a `usize` on every
            // target this builds for.
            #[allow(clippy::cast_possible_truncation)]
            let index = (hash % STUB_DIM as u64) as usize;
            let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
            vector[index] += sign;
        }
    }
    normalize(vector)
}

/// The stub backend.
struct StubBackend;

impl Backend for StubBackend {
    fn provider(&self) -> &'static str {
        "stub"
    }

    fn embed(&self, _model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| stub_vector(text)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: Provider, model: &str) -> EmbedderConfig {
        EmbedderConfig {
            provider: Some(provider.as_str().to_owned()),
            model: Some(model.to_owned()),
            ..EmbedderConfig::default()
        }
    }

    #[test]
    fn the_default_provider_is_ollama_at_its_own_port() {
        // A block with no provider and no URL is the documented minimal config: Ollama,
        // on the port Ollama documents, with the model the user names.
        let embedder = Embedder::build(&EmbedderConfig {
            model: Some("nomic-embed-text".to_owned()),
            ..EmbedderConfig::default()
        })
        .expect("ollama with a model is a complete config");
        assert_eq!(embedder.provider(), "ollama");
        assert_eq!(
            embedder.endpoint().as_deref(),
            Some("http://127.0.0.1:11434/api/embed")
        );

        // With no model there is nothing to ask for, and the message has to name the key
        // a user has to write rather than the field serde wanted.
        let err = Embedder::build(&EmbedderConfig::default()).unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
    }
    #[test]
    fn an_unknown_provider_names_the_ones_that_exist() {
        let err = Embedder::build(&EmbedderConfig {
            provider: Some("openrouter".to_owned()),
            model: Some("m".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("openrouter"), "{message}");
        assert!(message.contains("ollama, openai, stub"), "{message}");
    }

    #[test]
    fn a_missing_model_is_a_config_error_that_names_the_key() {
        let err = Embedder::build(&EmbedderConfig {
            provider: Some("openai".to_owned()),
            url: Some("http://127.0.0.1:1234/v1".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
    }

    #[test]
    fn https_is_refused_by_name_with_the_reason() {
        let err = Embedder::build(&EmbedderConfig {
            provider: Some("openai".to_owned()),
            url: Some("https://api.example.com/v1".to_owned()),
            model: Some("m".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("plain HTTP"), "{message}");
        assert!(message.contains("no TLS"), "{message}");
    }

    #[test]
    fn a_trailing_slash_does_not_double_the_path() {
        // `//api/embed` is a different path to most servers, and the ones that tolerate
        // it are not the ones worth debugging.
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("ollama".to_owned()),
            url: Some("http://127.0.0.1:11434/".to_owned()),
            model: Some("m".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap();
        assert_eq!(
            embedder.endpoint().as_deref(),
            Some("http://127.0.0.1:11434/api/embed")
        );
    }

    #[test]
    fn the_openai_provider_appends_its_own_path_to_the_base_url() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("openai".to_owned()),
            url: Some("http://127.0.0.1:1234/v1".to_owned()),
            model: Some("text-embedding-3-small".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap();
        assert_eq!(
            embedder.endpoint().as_deref(),
            Some("http://127.0.0.1:1234/v1/embeddings")
        );

        // And it has no default URL: the port is the user's business, and guessing one
        // sends a request to whatever happens to be listening there.
        let err = Embedder::build(&EmbedderConfig {
            provider: Some("openai".to_owned()),
            model: Some("text-embedding-3-small".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("`url`"), "{err}");
    }
    #[test]
    fn the_stub_needs_no_model_and_produces_the_same_vector_twice() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap();
        assert_eq!(embedder.model(), "stub");
        let first = embedder
            .embed_documents(&["the deploy failed".to_owned()])
            .unwrap();
        let second = embedder
            .embed_documents(&["the deploy failed".to_owned()])
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0].len(), STUB_DIM);
        let length: f32 = first[0].iter().map(|value| value * value).sum();
        assert!((length - 1.0).abs() < 1e-4, "not normalised: {length}");
    }

    #[test]
    fn the_stub_is_a_bag_of_words_which_is_the_whole_point_of_it() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap();
        let vectors = embedder
            .embed_documents(&[
                "the staging deploy needs a manual migration".to_owned(),
                "a manual migration before the staging deploy".to_owned(),
                "the cat sat on the mat".to_owned(),
            ])
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        assert!(
            dot(&vectors[0], &vectors[1]) > 0.5,
            "the same words in a different order are not similar: {}",
            dot(&vectors[0], &vectors[1])
        );
        assert!(
            dot(&vectors[0], &vectors[2]).abs() < 0.4,
            "unrelated texts are too similar: {}",
            dot(&vectors[0], &vectors[2])
        );
    }

    #[test]
    fn the_prefixes_are_per_side_and_only_where_they_are_set() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            query_prefix: Some("search_query: ".to_owned()),
            ..EmbedderConfig::default()
        })
        .unwrap();
        let document = embedder
            .embed_documents(&["migrations".to_owned()])
            .unwrap();
        let query = embedder.embed_query("migrations").unwrap();
        assert_ne!(
            document[0], query,
            "the query prefix has to reach the vector, or it is decoration"
        );
    }

    #[test]
    fn a_text_over_the_cap_is_clipped_on_a_character_boundary() {
        // Multi-byte on purpose: clipping by bytes would panic or produce mojibake.
        let text = "日".repeat(MAX_EMBED_CHARS + 5);
        assert!(Embedder::truncates(&text));
        let clipped = clip(&text);
        assert_eq!(clipped.chars().count(), MAX_EMBED_CHARS);
        assert!(clipped.is_char_boundary(clipped.len()));
    }

    #[test]
    fn the_memory_text_is_the_title_and_the_body_or_what_there_is() {
        assert_eq!(
            memory_text("Deploy", "run the script", ""),
            "Deploy\nrun the script"
        );
        assert_eq!(memory_text("Deploy", "", "one line"), "Deploy\none line");
        assert_eq!(memory_text("Deploy", "", ""), "Deploy");
        assert_eq!(memory_text("", "the body", ""), "the body");
        assert_eq!(memory_text("", "", "the excerpt"), "the excerpt");
        // Whitespace-only fields are absent fields, not empty lines in the text.
        assert_eq!(
            memory_text(" Deploy ", "  ", " excerpt "),
            "Deploy
excerpt"
        );
    }

    #[test]
    fn the_ollama_envelope_is_read() {
        let backend = HttpBackend::new(
            Provider::Ollama,
            "http://127.0.0.1:11434".to_owned(),
            None,
            Duration::from_secs(1),
        );
        let vectors = backend
            .parse(
                "nomic-embed-text",
                r#"{"model":"nomic-embed-text","embeddings":[[0.1,-0.2],[0.3,0.4]]}"#,
            )
            .unwrap();
        assert_eq!(vectors, vec![vec![0.1, -0.2], vec![0.3, 0.4]]);
    }

    #[test]
    fn the_openai_envelope_is_read_in_index_order() {
        let backend = HttpBackend::new(
            Provider::OpenAi,
            "http://127.0.0.1:1234/v1".to_owned(),
            None,
            Duration::from_secs(1),
        );
        // Deliberately out of order: the vectors have to pair with the texts, and a
        // server that returns them shuffled would otherwise pair every vector with the
        // wrong memory and still look like it worked.
        let vectors = backend
            .parse(
                "text-embedding-3-small",
                r#"{"data":[{"index":1,"embedding":[2.0]},{"index":0,"embedding":[1.0]}]}"#,
            )
            .unwrap();
        assert_eq!(vectors, vec![vec![1.0], vec![2.0]]);
    }

    #[test]
    fn an_unparseable_body_says_what_shape_was_expected() {
        let backend = HttpBackend::new(
            Provider::Ollama,
            "http://127.0.0.1:11434".to_owned(),
            None,
            Duration::from_secs(1),
        );
        let err = backend
            .parse("m", "<html>404</html>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("embeddings"), "{err}");
        assert!(err.contains("0.3.5"), "{err}");
    }

    #[test]
    fn a_dimension_that_contradicts_the_config_is_refused_at_the_first_write() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            dim: Some(768),
            ..EmbedderConfig::default()
        })
        .unwrap();
        let err = embedder
            .embed_documents(&["migrations".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("384"), "{err}");
        assert!(err.contains("dim = 768"), "{err}");
    }

    #[test]
    fn batches_are_split_and_reassembled_in_order() {
        let embedder = Embedder::build(&EmbedderConfig {
            provider: Some("stub".to_owned()),
            batch: Some(2),
            ..EmbedderConfig::default()
        })
        .unwrap();
        assert_eq!(embedder.batch(), 2);
        let texts: Vec<String> = (0..5).map(|index| format!("memory {index}")).collect();
        let vectors = embedder.embed_documents(&texts).unwrap();
        assert_eq!(vectors.len(), 5);
        // The batch boundary must not change what any single text becomes.
        let one = embedder.embed_documents(&["memory 2".to_owned()]).unwrap();
        assert_eq!(vectors[2], one[0]);
    }

    #[test]
    fn a_zero_batch_or_timeout_is_a_config_error_naming_the_key() {
        for (config, key) in [
            (
                EmbedderConfig {
                    provider: Some("stub".to_owned()),
                    batch: Some(0),
                    ..EmbedderConfig::default()
                },
                "`batch = 0`",
            ),
            (
                EmbedderConfig {
                    provider: Some("stub".to_owned()),
                    timeout_secs: Some(0),
                    ..EmbedderConfig::default()
                },
                "`timeout_secs = 0`",
            ),
            (
                EmbedderConfig {
                    provider: Some("stub".to_owned()),
                    dim: Some(0),
                    ..EmbedderConfig::default()
                },
                "`dim = 0`",
            ),
        ] {
            let err = Embedder::build(&config).unwrap_err().to_string();
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn the_stub_is_told_apart_from_a_model_everywhere_a_user_can_look() {
        let embedder = Embedder::build(&config(Provider::Stub, "stub")).unwrap();
        assert_eq!(embedder.provider(), "stub");
        assert_eq!(embedder.model(), "stub");
    }

    #[test]
    fn a_server_error_keeps_its_own_words_in_one_line() {
        assert_eq!(
            truncate_for_message("model not found\n  try pulling it"),
            "model not found try pulling it"
        );
        let long = "x".repeat(600);
        let truncated = truncate_for_message(&long);
        assert_eq!(truncated.chars().count(), 501);
        assert!(truncated.ends_with('…'));
    }
}
