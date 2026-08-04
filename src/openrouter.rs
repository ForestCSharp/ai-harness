//! Minimal OpenRouter chat-completions client.
//!
//! Requests are always streamed (Server-Sent Events). [`Client::open_stream`]
//! exposes the token deltas as they arrive; [`Client::complete`] drains that
//! stream to a single [`Completion`] for callers that only want the final text.

use anyhow::{Context, Result, anyhow};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

const API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// OpenRouter uses these for attribution on its leaderboards; both are optional.
const REFERER: &str = "https://github.com/local/ai-harness";
const TITLE: &str = "ai-harness";

/// Endpoints the client talks to. Overridable so tests can point at a local
/// server and assert on the exact request we put on the wire.
#[derive(Debug, Clone)]
struct Endpoints {
    chat: String,
    models: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            chat: API_URL.to_string(),
            models: MODELS_URL.to_string(),
        }
    }
}

/// A single turn in the conversation, in the shape the API expects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: String,
    model: String,
    endpoints: Endpoints,
}

/// One entry from OpenRouter's model catalog.
///
/// Everything past the id is `#[serde(default)]`: this is a third-party payload
/// listing hundreds of models from dozens of providers, and a field one of them
/// stops sending must not cost us the whole catalog.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// Human-readable name, e.g. "Anthropic: Claude Opus 4.5". Matched against
    /// alongside the id, since it is what people remember.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

/// Per-token prices, quoted as decimal strings in USD.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Pricing {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub completion: String,
}

impl ModelInfo {
    /// Prompt and completion price in dollars per million tokens, when the
    /// catalog quotes both as numbers. Per million rather than per token because
    /// that is how the prices are advertised, and how `/cost` reports them.
    pub fn price_per_million(&self) -> Option<(f64, f64)> {
        let pricing = self.pricing.as_ref()?;
        let prompt = pricing.prompt.parse::<f64>().ok()?;
        let completion = pricing.completion.parse::<f64>().ok()?;
        Some((prompt * 1_000_000.0, completion * 1_000_000.0))
    }

    /// Whether every one of `terms` appears in the id or the name. `terms` are
    /// expected lowercase; both fields are lowered here.
    ///
    /// Lives here rather than in the picker so the matching rule sits with the
    /// data it matches on. Every term must hit, which is what makes "claude opus"
    /// narrow rather than widen.
    pub fn matches(&self, terms: &[String]) -> bool {
        let id = self.id.to_lowercase();
        let name = self.name.to_lowercase();
        terms
            .iter()
            .all(|term| id.contains(term.as_str()) || name.contains(term.as_str()))
    }
}

/// The catalog response: `{"data": [ … ]}`.
#[derive(Deserialize)]
struct ModelCatalog {
    #[serde(default)]
    data: Vec<ModelInfo>,
}

/// What a completed request gives back to the UI.
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    /// Cache accounting, when the provider reports any.
    ///
    /// This harness resends the whole conversation every turn and appends to it
    /// rather than rewriting it, which is the shape prefix caching is for. It
    /// sends no cache directives of its own, so anything here comes from the
    /// provider caching on its own initiative — which is worth knowing before
    /// deciding whether asking for it explicitly is worth the work.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

impl Usage {
    /// Prompt tokens served from the provider's cache, if it said.
    pub fn cached_tokens(&self) -> u32 {
        self.prompt_tokens_details
            .map_or(0, |details| details.cached_tokens)
    }
}

/// One event from a streaming response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A chunk of assistant content to append.
    Delta(String),
    /// A chunk of the model's reasoning, on models that stream it.
    ///
    /// Its own variant rather than a flag on [`Self::Delta`] so that every
    /// consumer has to say what it does with reasoning. It must never reach the
    /// assistant content: that is what the protocol parses and what goes into
    /// the conversation, and a chain of thought in either would be read as the
    /// model's answer.
    Reasoning(String),
    /// The stream finished. Usage arrives here, in the final chunk.
    Done { usage: Option<Usage> },
}

impl Client {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ai-harness/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            api_key,
            model,
            endpoints: Endpoints::default(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// A copy of this client that asks for `model`.
    ///
    /// The model can change mid-session (`/model`) while the event loop holds
    /// the client behind a shared reference, so each request is built with the
    /// model the app currently has rather than the one the client was made with.
    /// Cloning is cheap: `reqwest::Client` is a handle to a shared connection
    /// pool, not a new one.
    pub fn with_model(&self, model: &str) -> Self {
        Self {
            model: model.to_string(),
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoints.chat = url.into();
        self
    }

    #[cfg(test)]
    fn with_models_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoints.models = url.into();
        self
    }

    /// Bearer token and the attribution headers, shared by every request.
    fn authorised(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", REFERER)
            .header("X-Title", TITLE)
    }

    /// Fetch the model catalog, sorted by id.
    ///
    /// Sorted here rather than at the call site: the API returns its own order,
    /// and a list you narrow by typing reads better with a provider's models
    /// together.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .authorised(self.http.get(&self.endpoints.models))
            .send()
            .await
            .context("requesting the model list from OpenRouter")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("reading the model list from OpenRouter")?;
        if !status.is_success() {
            return Err(anyhow!(
                "OpenRouter returned HTTP {status}: {}",
                error_message(&text)
            ));
        }

        let catalog: ModelCatalog =
            serde_json::from_str(&text).context("parsing the model list from OpenRouter")?;
        let mut models = catalog.data;
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    /// Open a streaming request, yielding token deltas as they arrive.
    ///
    /// The POST and status check happen up front, so a failed request surfaces
    /// here rather than partway through the stream.
    pub async fn open_stream(
        &self,
        messages: &[Message],
    ) -> Result<impl Stream<Item = Result<StreamEvent>>> {
        let body = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let response = self
            .authorised(self.http.post(&self.endpoints.chat))
            .json(&body)
            .send()
            .await
            .context("sending request to OpenRouter")?;

        let status = response.status();
        if !status.is_success() {
            // Errors are a normal JSON body, not a stream. Read it whole.
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenRouter returned HTTP {status}: {}",
                error_message(&text)
            ));
        }

        Ok(sse_events(response.bytes_stream()))
    }

    /// Drain a streaming request to a single completion, for callers that only
    /// want the final text.
    ///
    /// The conversation itself streams (see [`Client::open_stream`]) because the
    /// user is watching it arrive. This is for requests they are not: the
    /// summarising call a compaction makes, whose reply is context rather than
    /// something to read token by token.
    ///
    /// Takes no cancel future, unlike the streaming path — a caller that needs
    /// to abandon one selects over this whole future instead.
    pub async fn complete(&self, messages: &[Message]) -> Result<Completion> {
        let stream = self.open_stream(messages).await?;
        futures_util::pin_mut!(stream);

        let mut content = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::Delta(delta) => content.push_str(&delta),
                // Dropped, not appended. The caller here is the compaction
                // summariser, and a chain of thought folded into the summary
                // would become conversation the model answers from.
                StreamEvent::Reasoning(_) => {}
                // Usage arrives in its own chunk, followed by a `[DONE]` whose
                // usage is None; keep the populated one.
                StreamEvent::Done { usage: Some(u) } => usage = Some(u),
                StreamEvent::Done { usage: None } => {}
            }
        }
        Ok(Completion { content, usage })
    }
}

/// Frame a byte stream of Server-Sent Events into [`StreamEvent`]s.
///
/// SSE events are separated by a blank line; each carries one or more `data:`
/// lines. We buffer bytes until a complete event boundary (`\n\n`) is seen, so a
/// multibyte character or a `data:` line split across network chunks is never
/// decoded halfway.
fn sse_events(
    bytes: impl Stream<Item = reqwest::Result<bytes::Bytes>>,
) -> impl Stream<Item = Result<StreamEvent>> {
    // State carried across chunks: the undecoded byte buffer, and whether the
    // terminating `[DONE]` has been seen (so trailing bytes are ignored).
    struct State<S> {
        bytes: S,
        buffer: Vec<u8>,
        done: bool,
    }

    let init = State {
        bytes: Box::pin(bytes),
        buffer: Vec::new(),
        done: false,
    };

    futures_util::stream::unfold(init, |mut state| async move {
        loop {
            // Emit any complete events already buffered.
            if let Some(boundary) = find_boundary(&state.buffer) {
                let raw = state.buffer.drain(..boundary.end).collect::<Vec<_>>();
                let block = String::from_utf8_lossy(&raw[..boundary.content]);
                match parse_sse_event(&block) {
                    Some(Ok(StreamEvent::Done { usage })) => {
                        state.done = true;
                        return Some((Ok(StreamEvent::Done { usage }), state));
                    }
                    Some(result) => return Some((result, state)),
                    None => continue, // comment or contentless keepalive
                }
            }

            if state.done {
                return None;
            }

            match state.bytes.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    return Some((Err(anyhow!("stream error: {e}")), state));
                }
                None => {
                    // Stream ended without an explicit [DONE]; treat as finished.
                    return None;
                }
            }
        }
    })
}

/// Where an event block ends in the buffer: `content` bytes of text, then
/// `end` bytes consumed including the blank-line separator.
struct Boundary {
    content: usize,
    end: usize,
}

fn find_boundary(buffer: &[u8]) -> Option<Boundary> {
    // Events are separated by "\n\n" (tolerate "\r\n\r\n").
    buffer
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| Boundary {
            content: i,
            end: i + 2,
        })
        .or_else(|| {
            buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| Boundary {
                    content: i,
                    end: i + 4,
                })
        })
}

/// Parse one SSE event block into a [`StreamEvent`].
///
/// Returns `None` for comments and keepalives (nothing to emit), `Some(Ok(..))`
/// for a delta or the terminator, and `Some(Err(..))` for a data line whose JSON
/// we cannot make sense of.
fn parse_sse_event(block: &str) -> Option<Result<StreamEvent>> {
    // Concatenate the payload of every `data:` line in the event.
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim());
        }
        // Lines starting with ':' are comments; other fields (event:, id:) are
        // irrelevant here and ignored.
    }

    if data.is_empty() {
        return None;
    }
    if data == "[DONE]" {
        return Some(Ok(StreamEvent::Done { usage: None }));
    }

    let chunk: StreamChunk = match serde_json::from_str(&data) {
        Ok(chunk) => chunk,
        Err(e) => {
            return Some(Err(anyhow!(
                "could not parse stream chunk ({e}): {}",
                truncate(&data, 300)
            )));
        }
    };

    if let Some(err) = chunk.error {
        return Some(Err(anyhow!("OpenRouter stream error: {}", err.message)));
    }

    let delta = chunk.choices.into_iter().next().map(|c| c.delta);
    let (content, reasoning) = delta.map_or_else(
        || (String::new(), String::new()),
        |d| {
            (
                d.content.unwrap_or_default(),
                d.reasoning.unwrap_or_default(),
            )
        },
    );

    if !content.is_empty() {
        // Content wins when a chunk somehow carries both, and that chunk's
        // reasoning is dropped. One event per chunk is this function's contract
        // and `sse_events`' too, and widening it to a list for a case OpenRouter
        // does not produce — reasoning arrives in its own chunks, ahead of
        // content — would complicate the stream to close a gap nobody can see.
        // Reasoning is a live view that is thrown away at the end of the turn,
        // so the cost of losing a fragment of it is a slightly gappy trace.
        return Some(Ok(StreamEvent::Delta(content)));
    }
    if !reasoning.is_empty() {
        return Some(Ok(StreamEvent::Reasoning(reasoning)));
    }

    // A usage-only final chunk carries no content; report it as Done so the
    // caller records token counts.
    if let Some(usage) = chunk.usage {
        return Some(Ok(StreamEvent::Done { usage: Some(usage) }));
    }
    None
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One streamed chunk: `{"choices":[{"delta":{"content":"…"}}],"usage":…}`.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// The model's reasoning, on models that stream it. Carried separately and
    /// never accumulated into `content` — see [`StreamEvent::Reasoning`].
    #[serde(default)]
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

/// A non-streaming error body: `{"error": {"message": …}}`.
#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<ApiError>,
}

/// What to show for a failed request: the API's own message when the body is
/// the usual error envelope, and the raw body when it is something else.
fn error_message(text: &str) -> String {
    serde_json::from_str::<ErrorEnvelope>(text)
        .ok()
        .and_then(|r| r.error)
        .map(|e| e.message)
        .unwrap_or_else(|| truncate(text, 800))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// What the test server saw.
    struct Captured {
        headers: Vec<String>,
        body: String,
    }

    /// Serve exactly one request, replying with `status` and `response_body`.
    /// Returns the client and a handle yielding what the server received.
    fn serve_once(
        status: &'static str,
        response_body: &'static str,
    ) -> (Client, std::thread::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header");
                let trimmed = line.trim_end().to_string();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.strip_prefix("content-length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                headers.push(trimmed);
            }

            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).expect("read body");
            let body = String::from_utf8_lossy(&buf).to_string();

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let mut stream = reader.into_inner();
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().ok();

            Captured { headers, body }
        });

        // Both endpoints point at the one server: it serves whatever single
        // request arrives, whichever path it asks for.
        let client = Client::new("test-key".into(), "test/model".into())
            .unwrap()
            .with_endpoint(format!("http://{addr}/chat/completions"))
            .with_models_endpoint(format!("http://{addr}/models"));
        (client, handle)
    }

    /// Serve one request with a `text/event-stream` body built from `events`.
    /// Each event becomes a `data: …` block; a terminating `[DONE]` is appended.
    fn serve_sse(events: Vec<String>) -> (Client, std::thread::JoinHandle<Captured>) {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        serve_body("200 OK", "text/event-stream", body)
    }

    /// Serve one request with an arbitrary body, capturing what was received.
    fn serve_body(
        status: &'static str,
        content_type: &'static str,
        body: String,
    ) -> (Client, std::thread::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header");
                let trimmed = line.trim_end().to_string();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.to_lowercase().strip_prefix("content-length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                headers.push(trimmed);
            }
            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).expect("read body");
            let request_body = String::from_utf8_lossy(&buf).to_string();

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = reader.into_inner();
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().ok();

            Captured {
                headers,
                body: request_body,
            }
        });

        let client = Client::new("test-key".into(), "test/model".into())
            .unwrap()
            .with_endpoint(format!("http://{addr}/chat/completions"))
            .with_models_endpoint(format!("http://{addr}/models"));
        (client, handle)
    }

    /// A content-delta chunk in OpenRouter's streaming shape.
    fn delta(text: &str) -> String {
        format!(
            r#"{{"choices":[{{"delta":{{"content":{}}}}}]}}"#,
            json_str(text)
        )
    }

    /// A reasoning-delta chunk, as a reasoning model streams it.
    fn reasoning(text: &str) -> String {
        format!(
            r#"{{"choices":[{{"delta":{{"reasoning":{}}}}}]}}"#,
            json_str(text)
        )
    }

    fn json_str(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    #[tokio::test]
    async fn sends_model_and_messages_with_auth() {
        let (client, server) = serve_sse(vec![delta("hi")]);

        let messages = vec![Message::system("be terse"), Message::user("hello")];
        client
            .complete(&messages)
            .await
            .expect("request should succeed");

        let captured = server.join().unwrap();
        let headers = captured.headers.join("\n").to_lowercase();
        assert!(
            headers.contains("authorization: bearer test-key"),
            "missing bearer auth in:\n{headers}"
        );

        let sent: serde_json::Value =
            serde_json::from_str(&captured.body).expect("valid JSON body");
        assert_eq!(sent["model"], "test/model");
        assert_eq!(sent["stream"], true, "requests must ask for streaming");
        assert_eq!(sent["stream_options"]["include_usage"], true);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][0]["content"], "be terse");
        assert_eq!(sent["messages"][1]["role"], "user");
        assert_eq!(sent["messages"][1]["content"], "hello");
    }

    #[tokio::test]
    async fn with_model_changes_the_model_the_request_asks_for() {
        let (client, server) = serve_sse(vec![delta("hi")]);

        client
            .with_model("other/model")
            .complete(&[Message::user("hello")])
            .await
            .expect("request should succeed");

        let captured = server.join().unwrap();
        let sent: serde_json::Value =
            serde_json::from_str(&captured.body).expect("valid JSON body");
        assert_eq!(sent["model"], "other/model");
        // The original is untouched, so a per-request copy cannot leak back.
        assert_eq!(client.model(), "test/model");
    }

    #[tokio::test]
    async fn lists_models_sorted_by_id() {
        let catalog = r#"{"data":[
            {"id":"z/last","name":"Last","context_length":8000,
             "pricing":{"prompt":"0.000001","completion":"0.000002"}},
            {"id":"a/first","name":"First","context_length":200000,
             "pricing":{"prompt":"0.000005","completion":"0.000025"}}
        ]}"#;
        let (client, server) = serve_body("200 OK", "application/json", catalog.to_string());

        let models = client.list_models().await.expect("catalog should parse");

        let captured = server.join().unwrap();
        let headers = captured.headers.join("\n").to_lowercase();
        assert!(
            headers.contains("authorization: bearer test-key"),
            "missing bearer auth in:\n{headers}"
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a/first", "z/last"],
            "the catalog should come back sorted by id"
        );
        assert_eq!(models[0].name, "First");
        assert_eq!(models[0].context_length, Some(200_000));
        assert_eq!(models[0].price_per_million(), Some((5.0, 25.0)));
    }

    #[tokio::test]
    async fn a_catalog_entry_missing_fields_still_parses() {
        let catalog = r#"{"data":[{"id":"bare/model"}]}"#;
        let (client, server) = serve_body("200 OK", "application/json", catalog.to_string());

        let models = client.list_models().await.expect("catalog should parse");
        server.join().unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "bare/model");
        assert_eq!(models[0].name, "");
        assert_eq!(models[0].context_length, None);
        assert_eq!(models[0].price_per_million(), None);
    }

    #[tokio::test]
    async fn a_failed_catalog_request_surfaces_the_api_message() {
        let body = r#"{"error":{"message":"No auth credentials found"}}"#;
        let (client, server) = serve_body("401 Unauthorized", "application/json", body.to_string());

        let error = client
            .list_models()
            .await
            .expect_err("a 401 should be an error");
        server.join().unwrap();

        let text = format!("{error:#}");
        assert!(text.contains("401"), "{text}");
        assert!(text.contains("No auth credentials found"), "{text}");
    }

    #[test]
    fn price_per_million_scales_the_quoted_per_token_price() {
        let model = ModelInfo {
            id: "a/b".into(),
            name: String::new(),
            context_length: None,
            pricing: Some(Pricing {
                prompt: "0.0000005".into(),
                completion: "0".into(),
            }),
        };
        assert_eq!(model.price_per_million(), Some((0.5, 0.0)));
    }

    #[test]
    fn price_per_million_is_none_when_a_price_is_not_a_number() {
        let model = ModelInfo {
            id: "a/b".into(),
            name: String::new(),
            context_length: None,
            pricing: Some(Pricing {
                prompt: "variable".into(),
                completion: "0".into(),
            }),
        };
        assert_eq!(model.price_per_million(), None);
    }

    #[test]
    fn matching_needs_every_term_and_ignores_case() {
        let model = ModelInfo {
            id: "anthropic/claude-opus-4.5".into(),
            name: "Anthropic: Claude Opus 4.5".into(),
            context_length: None,
            pricing: None,
        };

        assert!(model.matches(&["claude".into(), "opus".into()]));
        // Terms may come from either field.
        assert!(model.matches(&["anthropic".into(), "4.5".into()]));
        assert!(!model.matches(&["claude".into(), "sonnet".into()]));
        // No terms is no filter.
        assert!(model.matches(&[]));
    }

    #[tokio::test]
    async fn assembles_multi_chunk_content_in_order() {
        let (client, server) = serve_sse(vec![delta("2 + 2 "), delta("= "), delta("4")]);
        let completion = client.complete(&[Message::user("2+2")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "2 + 2 = 4");
    }

    #[tokio::test]
    async fn parses_usage_from_the_final_chunk() {
        let usage = r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2}}"#;
        let (client, server) = serve_sse(vec![delta("4"), usage.to_string()]);

        let completion = client.complete(&[Message::user("2+2")]).await.unwrap();
        server.join().unwrap();

        assert_eq!(completion.content, "4");
        let usage = completion.usage.expect("usage should parse");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn response_without_usage_still_succeeds() {
        let (client, server) = serve_sse(vec![delta("ok")]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "ok");
        assert!(completion.usage.is_none());
    }

    /// The invariant the whole feature rests on. `complete` is the compaction
    /// summariser, and a chain of thought folded into the summary would become
    /// conversation the model then answers from.
    #[tokio::test]
    async fn reasoning_never_reaches_the_assembled_content() {
        let (client, server) = serve_sse(vec![
            reasoning("the user probably means"),
            reasoning(" the other thing"),
            delta("Hello."),
        ]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "Hello.");
    }

    #[tokio::test]
    async fn open_stream_separates_reasoning_from_content() {
        let (client, server) = serve_sse(vec![
            reasoning("first I"),
            reasoning(" think"),
            delta("then I "),
            delta("answer"),
        ]);
        let messages = [Message::user("hi")];
        let stream = client.open_stream(&messages).await.unwrap();
        futures_util::pin_mut!(stream);

        let (mut thought, mut said) = (String::new(), String::new());
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::Reasoning(r) => thought.push_str(&r),
                StreamEvent::Delta(d) => said.push_str(&d),
                StreamEvent::Done { .. } => {}
            }
        }
        server.join().unwrap();
        assert_eq!(thought, "first I think");
        assert_eq!(said, "then I answer");
    }

    /// One event per chunk is the contract; a chunk carrying both keeps the
    /// content, since that is the half that cannot be dropped.
    #[test]
    fn a_chunk_with_both_yields_the_content() {
        let event =
            parse_sse_event(r#"data: {"choices":[{"delta":{"content":"hi","reasoning":"hmm"}}]}"#);
        assert_eq!(event.unwrap().unwrap(), StreamEvent::Delta("hi".into()));
    }

    #[test]
    fn parse_sse_event_reads_a_reasoning_delta() {
        let event = parse_sse_event(r#"data: {"choices":[{"delta":{"reasoning":"hmm"}}]}"#);
        assert_eq!(
            event.unwrap().unwrap(),
            StreamEvent::Reasoning("hmm".into())
        );
    }

    #[tokio::test]
    async fn open_stream_yields_deltas_in_order() {
        let (client, server) = serve_sse(vec![delta("a"), delta("b"), delta("c")]);
        let messages = [Message::user("hi")];
        let stream = client.open_stream(&messages).await.unwrap();
        futures_util::pin_mut!(stream);

        let mut deltas = Vec::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::Delta(d) = event.unwrap() {
                deltas.push(d);
            }
        }
        server.join().unwrap();
        assert_eq!(deltas, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn keepalive_comments_are_ignored() {
        // OpenRouter interleaves ": OPENROUTER PROCESSING" comment lines.
        let (first, second) = (delta("hel"), delta("lo"));
        let body = format!(
            ": OPENROUTER PROCESSING\n\n\
             data: {first}\n\n\
             : still working\n\n\
             data: {second}\n\n\
             data: [DONE]\n\n"
        );
        let (client, server) = serve_body("200 OK", "text/event-stream", body);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "hello");
    }

    #[tokio::test]
    async fn a_content_delta_split_across_events_assembles() {
        // Each token is its own SSE event; the assembled string is contiguous.
        let (client, server) = serve_sse(vec![delta("日本"), delta("語")]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "日本語");
    }

    #[tokio::test]
    async fn an_empty_stream_yields_empty_content() {
        // Just [DONE], no deltas. The app layer turns empty content into a
        // protocol error; the client itself simply reports nothing.
        let (client, server) = serve_sse(vec![]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "");
        assert!(completion.usage.is_none());
    }

    #[tokio::test]
    async fn surfaces_the_api_error_message() {
        let (client, server) = serve_once(
            "401 Unauthorized",
            r#"{"error":{"message":"No auth credentials found","code":401}}"#,
        );

        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(
            err.to_string().contains("No auth credentials found"),
            "error should quote the API message, got: {err}"
        );
    }

    #[tokio::test]
    async fn an_error_chunk_mid_stream_is_surfaced() {
        // A streamed error arrives as a data chunk carrying {"error":…}.
        let error = r#"{"error":{"message":"rate limited","code":429}}"#;
        let (client, server) = serve_sse(vec![delta("partial"), error.to_string()]);

        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("rate limited"), "got: {err}");
    }

    #[tokio::test]
    async fn a_malformed_chunk_is_reported_not_dropped() {
        let (client, server) = serve_sse(vec!["{not valid json".to_string()]);
        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("parse stream chunk"), "got: {err}");
    }

    #[tokio::test]
    async fn non_json_body_reports_the_status_and_snippet() {
        let (client, server) = serve_once("502 Bad Gateway", "upstream exploded");
        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        let text = err.to_string();
        assert!(
            text.contains("502"),
            "should mention the status, got: {text}"
        );
        assert!(
            text.contains("upstream exploded"),
            "should include the body, got: {text}"
        );
    }

    #[test]
    fn parse_sse_event_reads_a_content_delta() {
        let event = parse_sse_event(r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#);
        assert_eq!(event.unwrap().unwrap(), StreamEvent::Delta("hi".into()));
    }

    #[test]
    fn parse_sse_event_treats_done_as_terminal() {
        assert_eq!(
            parse_sse_event("data: [DONE]").unwrap().unwrap(),
            StreamEvent::Done { usage: None }
        );
    }

    #[test]
    fn parse_sse_event_ignores_comments_and_blanks() {
        assert!(parse_sse_event(": OPENROUTER PROCESSING").is_none());
        assert!(parse_sse_event("").is_none());
        assert!(parse_sse_event("event: ping").is_none());
    }

    #[test]
    fn parse_sse_event_reports_a_usage_only_chunk_as_done() {
        let event = parse_sse_event(
            r#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        );
        match event.unwrap().unwrap() {
            StreamEvent::Done { usage: Some(u) } => {
                assert_eq!(u.prompt_tokens, 3);
                assert_eq!(u.completion_tokens, 1);
            }
            other => panic!("expected Done with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_event_flags_bad_json() {
        assert!(parse_sse_event("data: {oops").unwrap().is_err());
    }

    #[test]
    fn find_boundary_handles_lf_and_crlf() {
        let b = find_boundary(b"data: x\n\nrest").unwrap();
        assert_eq!(b.content, 7);
        assert_eq!(b.end, 9);

        let b = find_boundary(b"data: x\r\n\r\nrest").unwrap();
        assert_eq!(&b"data: x\r\n\r\nrest"[..b.content], b"data: x");

        assert!(find_boundary(b"data: incomplete").is_none());
    }

    #[test]
    fn roles_serialize_lowercase() {
        let json = serde_json::to_string(&Message::assistant("x")).unwrap();
        assert!(json.contains(r#""role":"assistant""#), "got {json}");
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    /// Hits the real API. Opt-in, since it needs a key and costs money:
    /// `cargo test -- --ignored live_`
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes a real API call"]
    async fn live_round_trip() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());

        let client = Client::new(key, model).unwrap();
        let completion = client
            .complete(&[
                Message::system("Reply with exactly one word."),
                Message::user("What is the capital of France?"),
            ])
            .await
            .expect("live request should succeed");

        println!(
            "model replied: {:?}, usage: {:?}",
            completion.content, completion.usage
        );
        assert!(
            !completion.content.trim().is_empty(),
            "reply should not be empty"
        );
        assert!(
            completion.content.to_lowercase().contains("paris"),
            "unexpected reply: {}",
            completion.content
        );
    }

    /// Confirms the real catalog parses: it is a large third-party payload whose
    /// shape a fixture can only assume.
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes a real API call"]
    async fn live_catalog_parses() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let client = Client::new(key, crate::config::DEFAULT_MODEL.to_string()).unwrap();

        let models = client.list_models().await.expect("catalog should load");

        println!("{} models, first: {:?}", models.len(), models.first());
        assert!(models.len() > 50, "expected a full catalog");
        assert!(
            models.windows(2).all(|w| w[0].id <= w[1].id),
            "the catalog should come back sorted"
        );
        assert!(
            models.iter().any(|m| m.price_per_million().is_some()),
            "at least some models should quote a price"
        );
        assert!(
            models.iter().any(|m| m.context_length.is_some()),
            "at least some models should report a context length"
        );
    }

    /// Confirms a real request actually streams — more than one delta, arriving
    /// incrementally — and reassembles to a coherent reply.
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes a real API call"]
    async fn live_stream_arrives_incrementally() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());
        let client = Client::new(key, model).unwrap();

        let messages = [Message::user("Count from 1 to 10, space separated.")];
        let stream = client.open_stream(&messages).await.unwrap();
        futures_util::pin_mut!(stream);

        let mut deltas = 0usize;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::Delta(d) => {
                    deltas += 1;
                    content.push_str(&d);
                }
                StreamEvent::Reasoning(r) => reasoning.push_str(&r),
                StreamEvent::Done { usage: u } => {
                    if u.is_some() {
                        usage = u;
                    }
                }
            }
        }

        println!(
            "received {deltas} deltas, {} bytes of reasoning, usage: {usage:?}\nassembled: {content:?}",
            reasoning.len()
        );
        assert!(deltas > 1, "a streamed reply should arrive in many chunks");
        assert!(content.contains('5'), "unexpected reply: {content}");
    }
}
