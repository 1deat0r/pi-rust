//! Session search — port of `packages/agent/src/search/` (scanning.ts +
//! index.ts): streamed candidates, lazy sources, and substring search across
//! sessions.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{stream, Stream, StreamExt};

use crate::fs::FileSystem;
use crate::session::state::{EntryCursor, EntryOrder, EntryQuery};
use crate::session::types::SessionMetadata;
use crate::session::{Entry, Session};

/// A search hit: owning session, matching entry, timestamp, and the full
/// projected candidate text used for matching (upstream ScanningSessionSearchHit).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    /// Restrict results to canonical entry types (e.g. "message").
    pub entry_types: Option<Vec<String>>,
    /// Maximum number of hits to return.
    pub limit: Option<usize>,
    /// When true the search is aborted (upstream `AbortSignal`).
    pub abort_requested: bool,
    /// Optional live cancellation flag for streamed searches.
    pub abort_signal: Option<Arc<AtomicBool>>,
    /// Optional reason returned when the search is already cancelled.
    pub abort_reason: Option<String>,
    /// Source-specific options produced by the scanning search's
    /// `source_options` hook. This is populated only for the lazy source
    /// callback and is intentionally absent from normal search callers.
    pub source_options: Option<serde_json::Value>,
}

impl SessionSearchOptions {
    fn is_aborted(&self) -> bool {
        self.abort_requested
            || self
                .abort_signal
                .as_ref()
                .is_some_and(|signal| signal.load(Ordering::Acquire))
    }

    fn abort_error(&self) -> String {
        self.abort_reason
            .clone()
            .unwrap_or_else(|| "The operation was aborted".to_string())
    }
}

/// A projected entry yielded by [`scanning_entries`]. This is the Rust
/// counterpart of upstream `SessionSearchCandidate`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchCandidate {
    pub entry_id: String,
    pub seq: u64,
    pub entry_type: String,
    pub timestamp: u64,
    pub text: String,
    pub fields: Option<BTreeMap<String, serde_json::Value>>,
}

/// Optional text projector for a scanning source.
pub type ScanningSearchTextProjector =
    Arc<dyn Fn(&SessionMetadata, &Entry, Option<&str>) -> String + Send + Sync>;

pub type ScanningSearchMatcher =
    Arc<dyn Fn(&str, &SessionSearchCandidate, &SessionMetadata) -> bool + Send + Sync>;

pub type ScanningSearchHitCreator<H> =
    Arc<dyn Fn(&SessionMetadata, &SessionSearchCandidate) -> H + Send + Sync>;

/// The readable portion of the upstream `SessionStorage` contract used by
/// scanning search. Implementors need not be the built-in `Session<F>` facade;
/// adapters can expose any session backend with metadata, entry lookup, and
/// labels.
#[async_trait]
pub trait ScanningReadable: Send + Sync {
    async fn get_metadata(&self) -> SessionMetadata;
    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, String>;
    async fn get_label(&self, entry_id: &str) -> Option<String>;
}

#[async_trait]
impl<F: FileSystem + 'static> ScanningReadable for Session<F> {
    async fn get_metadata(&self) -> SessionMetadata {
        Session::get_metadata(self).await
    }

    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, String> {
        Session::find_entries(self, query)
            .await
            .map_err(|error| error.to_string())
    }

    async fn get_label(&self, entry_id: &str) -> Option<String> {
        Session::get_label(self, entry_id).await
    }
}

/// Project normalized search input/options into source-specific options before
/// a lazy source is constructed. JSON keeps the callback boundary ergonomic
/// for Rust callers while preserving upstream's arbitrary source-options
/// payload semantics across the source callback.
pub type ScanningSourceOptionsFactory =
    Arc<dyn Fn(&str, &SessionSearchOptions) -> Option<serde_json::Value> + Send + Sync>;

pub type TypedScanningSourceOptionsFactory<TSourceOptions> =
    Arc<dyn Fn(&str, &SessionSearchOptions) -> Option<TSourceOptions> + Send + Sync>;

/// Options controlling candidate projection.
#[derive(Clone, Default)]
pub struct ScanningReadableOptions {
    /// `pageSize` in the upstream scanning source.
    pub page_size: Option<usize>,
    /// Optional replacement for the default JSON-plus-label projection.
    pub project_text: Option<ScanningSearchTextProjector>,
}

/// Optional matching and hit-construction hooks for a scanning search.
pub struct ScanningSearchOptions<H = SessionSearchHit> {
    pub readable: ScanningReadableOptions,
    pub matcher: Option<ScanningSearchMatcher>,
    pub create_hit: Option<ScanningSearchHitCreator<H>>,
    pub source_options: Option<ScanningSourceOptionsFactory>,
}

impl<H> Clone for ScanningSearchOptions<H> {
    fn clone(&self) -> Self {
        Self {
            readable: self.readable.clone(),
            matcher: self.matcher.clone(),
            create_hit: self.create_hit.clone(),
            source_options: self.source_options.clone(),
        }
    }
}

fn default_create_hit(
    metadata: &SessionMetadata,
    candidate: &SessionSearchCandidate,
) -> SessionSearchHit {
    SessionSearchHit {
        session_id: metadata.id.clone(),
        entry_id: candidate.entry_id.clone(),
        timestamp: candidate.timestamp,
        snippet: candidate.text.clone(),
    }
}

impl Default for ScanningSearchOptions<SessionSearchHit> {
    fn default() -> Self {
        Self {
            readable: ScanningReadableOptions::default(),
            matcher: None,
            create_hit: Some(Arc::new(default_create_hit)),
            source_options: None,
        }
    }
}

/// Upstream-shaped scanning-search options with a typed lazy-source payload.
/// The JSON-valued hook on [`ScanningSearchOptions`] remains a compatibility
/// shim for existing Rust callers; new code should prefer this typed contract.
pub struct TypedScanningSearchOptions<H = SessionSearchHit, TSourceOptions = ()> {
    pub readable: ScanningReadableOptions,
    pub matcher: Option<ScanningSearchMatcher>,
    pub create_hit: Option<ScanningSearchHitCreator<H>>,
    pub source_options: Option<TypedScanningSourceOptionsFactory<TSourceOptions>>,
}

impl<H, TSourceOptions> Clone for TypedScanningSearchOptions<H, TSourceOptions> {
    fn clone(&self) -> Self {
        Self {
            readable: self.readable.clone(),
            matcher: self.matcher.clone(),
            create_hit: self.create_hit.clone(),
            source_options: self.source_options.clone(),
        }
    }
}

impl<TSourceOptions> Default for TypedScanningSearchOptions<SessionSearchHit, TSourceOptions> {
    fn default() -> Self {
        Self {
            readable: ScanningReadableOptions::default(),
            matcher: None,
            create_hit: Some(Arc::new(default_create_hit)),
            source_options: None,
        }
    }
}

/// Page size for the entry scan (upstream `pageSize ?? 100`).
const DEFAULT_PAGE_SIZE: usize = 100;

/// Canonical entry type name ("message", "custom", ...).
fn entry_type_name(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message { .. } => "message",
        Entry::ModelChange { .. } => "model_change",
        Entry::ThinkingLevel { .. } => "thinking_level_change",
        Entry::ActiveTools { .. } => "active_tools_change",
        Entry::Compaction { .. } => "compaction",
        Entry::BranchSummary { .. } => "branch_summary",
        Entry::Custom { .. } => "custom",
    }
}

/// Default text projector: JSON.stringify(entry) plus the label when present.
fn default_search_text(entry: &Entry, label: Option<&str>) -> String {
    let json = serde_json::to_string(entry).unwrap_or_default();
    match label {
        Some(label) => format!("{json} {label}"),
        None => json,
    }
}

/// Default matcher: case-insensitive substring on the projected text.
fn default_match(query_text: &str, candidate_text: &str) -> bool {
    candidate_text.to_lowercase().contains(query_text)
}

enum ScanningReadableHandle<'a> {
    Borrowed(&'a dyn ScanningReadable),
    Owned(Arc<dyn ScanningReadable>),
}

impl ScanningReadableHandle<'_> {
    fn readable(&self) -> &dyn ScanningReadable {
        match self {
            Self::Borrowed(readable) => *readable,
            Self::Owned(readable) => readable.as_ref(),
        }
    }
}

struct ScanningStreamState<'a> {
    readable: ScanningReadableHandle<'a>,
    options: ScanningReadableOptions,
    entry_types: Option<Vec<String>>,
    metadata: Option<SessionMetadata>,
    after_seq: u64,
    pending: VecDeque<SessionSearchCandidate>,
    finished: bool,
}

/// Stream projected candidates from one readable session.
///
/// The stream pages entries in oldest-first order and only holds the current
/// page's projected candidates. This preserves upstream's async-iterable
/// behavior while remaining compatible with the existing Rust session facade.
fn scan_readable_entries<'a>(
    readable: ScanningReadableHandle<'a>,
    options: ScanningReadableOptions,
    entry_types: Option<Vec<String>>,
) -> impl Stream<Item = Result<SessionSearchCandidate, String>> + 'a {
    stream::unfold(
        ScanningStreamState {
            readable,
            options,
            entry_types,
            metadata: None,
            after_seq: 0,
            pending: VecDeque::new(),
            finished: false,
        },
        |mut state| async move {
            loop {
                if let Some(candidate) = state.pending.pop_front() {
                    return Some((Ok(candidate), state));
                }
                if state.finished {
                    return None;
                }

                let metadata = match &state.metadata {
                    Some(metadata) => metadata.clone(),
                    None => {
                        let metadata = state.readable.readable().get_metadata().await;
                        state.metadata = Some(metadata.clone());
                        metadata
                    }
                };
                let page_size = state.options.page_size.unwrap_or(DEFAULT_PAGE_SIZE).max(1);
                let query = EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    entry_type: state
                        .entry_types
                        .as_ref()
                        .filter(|types| types.len() == 1)
                        .and_then(|types| types.first().cloned()),
                    limit: Some(page_size),
                    cursor: Some(EntryCursor {
                        after_seq: state.after_seq,
                    }),
                    ..Default::default()
                };
                let entries = match state.readable.readable().find_entries(&query).await {
                    Ok(entries) => entries,
                    Err(error) => {
                        state.finished = true;
                        return Some((Err(format!("find entries: {error}")), state));
                    }
                };
                if entries.is_empty() {
                    return None;
                }

                state.after_seq = entries
                    .last()
                    .map(|entry| entry.seq())
                    .unwrap_or(state.after_seq);
                if entries.len() < page_size {
                    state.finished = true;
                }

                for entry in entries {
                    if let Some(types) = &state.entry_types {
                        if !types.iter().any(|kind| kind == entry_type_name(&entry)) {
                            continue;
                        }
                    }
                    let label = state.readable.readable().get_label(entry.id()).await;
                    let text = match &state.options.project_text {
                        Some(project_text) => project_text(&metadata, &entry, label.as_deref()),
                        None => default_search_text(&entry, label.as_deref()),
                    };
                    let fields = label.map(|label| {
                        let mut fields = BTreeMap::new();
                        fields.insert("label".to_string(), serde_json::Value::String(label));
                        fields
                    });
                    state.pending.push_back(SessionSearchCandidate {
                        entry_id: entry.id().to_string(),
                        seq: entry.seq(),
                        entry_type: entry_type_name(&entry).to_string(),
                        timestamp: entry.timestamp(),
                        text,
                        fields,
                    });
                }
            }
        },
    )
}

/// Stream projected candidates from a session without entry-type filtering.
pub fn scanning_entries<'a, R: ScanningReadable + 'a>(
    readable: &'a R,
    options: ScanningReadableOptions,
) -> impl Stream<Item = Result<SessionSearchCandidate, String>> + 'a {
    scan_readable_entries(ScanningReadableHandle::Borrowed(readable), options, None)
}

fn scanning_owned_entries<R: ScanningReadable + 'static>(
    readable: Arc<R>,
    options: ScanningReadableOptions,
    entry_types: Option<Vec<String>>,
) -> impl Stream<Item = Result<SessionSearchCandidate, String>> + 'static {
    let readable: Arc<dyn ScanningReadable> = readable;
    scan_readable_entries(
        ScanningReadableHandle::Owned(readable),
        options,
        entry_types,
    )
}

/// Search across an owned set of sessions (the array-source form of
/// `createScanningSessionSearch`).
pub struct ScanningSessionSearch<F: FileSystem, H = SessionSearchHit> {
    sessions: Vec<Session<F>>,
    options: ScanningSearchOptions<H>,
}

type CandidateStream<'a> = Pin<Box<dyn Stream<Item = Result<SessionSearchCandidate, String>> + 'a>>;

struct SearchStreamState<'a, F: FileSystem, H> {
    search: &'a ScanningSessionSearch<F, H>,
    options: SessionSearchOptions,
    normalized_text: String,
    session_index: usize,
    metadata: Option<SessionMetadata>,
    candidates: Option<CandidateStream<'a>>,
    seen_session_ids: HashSet<String>,
    hit_count: usize,
    done: bool,
}

impl<F: FileSystem + 'static> ScanningSessionSearch<F, SessionSearchHit> {
    pub fn new(sessions: Vec<Session<F>>) -> Self {
        Self {
            sessions,
            options: ScanningSearchOptions::default(),
        }
    }

    pub async fn search(
        &self,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<SessionSearchHit>, String> {
        self.search_with(text, options).await
    }
}

impl<F: FileSystem + 'static, H> ScanningSessionSearch<F, H> {
    pub fn with_options(sessions: Vec<Session<F>>, options: ScanningSearchOptions<H>) -> Self {
        Self { sessions, options }
    }

    /// Stream search hits as they are discovered, matching upstream's
    /// `SessionSearch.search()` async iterable contract.
    pub fn stream_search<'a>(
        &'a self,
        text: &str,
        options: SessionSearchOptions,
    ) -> impl Stream<Item = Result<H, String>> + 'a {
        let normalized_text = text.trim().to_lowercase();
        let done = normalized_text.is_empty()
            || options.limit == Some(0)
            || options.entry_types.as_ref().is_some_and(Vec::is_empty);
        stream::unfold(
            SearchStreamState {
                search: self,
                options,
                normalized_text,
                session_index: 0,
                metadata: None,
                candidates: None,
                seen_session_ids: HashSet::new(),
                hit_count: 0,
                done,
            },
            |mut state| async move {
                loop {
                    if state.done {
                        return None;
                    }
                    if state.candidates.is_none() {
                        let session = state.search.sessions.get(state.session_index)?;
                        if state.options.is_aborted() {
                            state.done = true;
                            return Some((Err(state.options.abort_error()), state));
                        }
                        let metadata = session.get_metadata().await;
                        if !state.seen_session_ids.insert(metadata.id.clone()) {
                            state.done = true;
                            return Some((
                                Err(format!("Duplicate sessionId: {}", metadata.id)),
                                state,
                            ));
                        }
                        state.metadata = Some(metadata);
                        state.candidates = Some(Box::pin(scan_readable_entries(
                            ScanningReadableHandle::Borrowed(session),
                            state.search.options.readable.clone(),
                            state.options.entry_types.clone(),
                        )));
                    }

                    let candidate = state
                        .candidates
                        .as_mut()
                        .expect("candidate stream initialized")
                        .as_mut()
                        .next()
                        .await;
                    match candidate {
                        Some(Err(error)) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                        Some(Ok(candidate)) => {
                            if state.options.is_aborted() {
                                state.done = true;
                                return Some((Err(state.options.abort_error()), state));
                            }
                            let metadata = state.metadata.as_ref().expect("metadata initialized");
                            let matches = state
                                .search
                                .options
                                .matcher
                                .as_ref()
                                .map(|matcher| {
                                    matcher(&state.normalized_text, &candidate, metadata)
                                })
                                .unwrap_or_else(|| {
                                    default_match(&state.normalized_text, &candidate.text)
                                });
                            if !matches {
                                continue;
                            }
                            let create_hit = match state.search.options.create_hit.as_ref() {
                                Some(create_hit) => create_hit,
                                None => {
                                    state.done = true;
                                    return Some((
                                        Err("Scanning search requires a create_hit callback for this hit type".to_string()),
                                        state,
                                    ));
                                }
                            };
                            let hit = create_hit(metadata, &candidate);
                            state.hit_count += 1;
                            if state
                                .options
                                .limit
                                .is_some_and(|limit| state.hit_count >= limit)
                            {
                                state.done = true;
                            }
                            return Some((Ok(hit), state));
                        }
                        None => {
                            state.candidates = None;
                            state.metadata = None;
                            state.session_index += 1;
                        }
                    }
                }
            },
        )
    }

    /// Search using the configured matcher and hit constructor.
    pub async fn search_with(
        &self,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<H>, String> {
        let mut stream = Box::pin(self.stream_search(text, options.clone()));
        let mut hits = Vec::new();
        while let Some(hit) = stream.next().await {
            hits.push(hit?);
        }
        Ok(hits)
    }
}

/// Lazy-source form of `createScanningSessionSearch`.
///
/// The source is invoked only after the empty-query, zero-limit, and empty
/// entry-type guards. It receives the normalized query and search options and
/// yields sessions incrementally, matching upstream's async-iterable source.
pub struct LazyScanningSessionSearch<R: ScanningReadable, S, H = SessionSearchHit> {
    source: S,
    options: ScanningSearchOptions<H>,
    _readable: std::marker::PhantomData<fn() -> R>,
}

type ReadableSourceStream<'a, R> = Pin<Box<dyn Stream<Item = Result<R, String>> + 'a>>;

type BoxedReadableSource<R> = Box<
    dyn Fn(&str, &SessionSearchOptions) -> Pin<Box<dyn Stream<Item = Result<R, String>> + 'static>>,
>;

struct LazySearchStreamState<'a, R: ScanningReadable, S, H> {
    search: &'a LazyScanningSessionSearch<R, S, H>,
    options: SessionSearchOptions,
    normalized_text: String,
    source: Option<ReadableSourceStream<'a, R>>,
    source_started: bool,
    candidates: Option<CandidateStream<'a>>,
    metadata: Option<SessionMetadata>,
    seen_session_ids: HashSet<String>,
    hit_count: usize,
    done: bool,
}

impl<R: ScanningReadable + 'static, S, St> LazyScanningSessionSearch<R, S, SessionSearchHit>
where
    S: Fn(&str, &SessionSearchOptions) -> St,
    St: Stream<Item = Result<R, String>> + 'static,
{
    pub fn new(source: S) -> Self {
        Self {
            source,
            options: ScanningSearchOptions::default(),
            _readable: std::marker::PhantomData,
        }
    }
}

impl<R: ScanningReadable + 'static, S, St, H> LazyScanningSessionSearch<R, S, H>
where
    S: Fn(&str, &SessionSearchOptions) -> St,
    St: Stream<Item = Result<R, String>> + 'static,
{
    pub fn with_options(source: S, options: ScanningSearchOptions<H>) -> Self {
        Self {
            source,
            options,
            _readable: std::marker::PhantomData,
        }
    }

    /// Stream hits from the lazy session source.
    pub fn stream_search<'a>(
        &'a self,
        text: &str,
        options: SessionSearchOptions,
    ) -> impl Stream<Item = Result<H, String>> + 'a {
        let normalized_text = text.trim().to_lowercase();
        let done = normalized_text.is_empty()
            || options.limit == Some(0)
            || options.entry_types.as_ref().is_some_and(Vec::is_empty);
        stream::unfold(
            LazySearchStreamState {
                search: self,
                options,
                normalized_text,
                source: None,
                source_started: false,
                candidates: None,
                metadata: None,
                seen_session_ids: HashSet::new(),
                hit_count: 0,
                done,
            },
            |mut state| async move {
                loop {
                    if state.done {
                        return None;
                    }
                    if state.candidates.is_none() {
                        if !state.source_started {
                            state.source_started = true;
                            let mut source_search_options = state.options.clone();
                            source_search_options.source_options = state
                                .search
                                .options
                                .source_options
                                .as_ref()
                                .and_then(|factory| {
                                    factory(&state.normalized_text, &state.options)
                                });
                            state.source = Some(Box::pin((state.search.source)(
                                &state.normalized_text,
                                &source_search_options,
                            )));
                        }
                        let next_session = state
                            .source
                            .as_mut()
                            .expect("lazy source initialized")
                            .next()
                            .await;
                        match next_session {
                            Some(Err(error)) => {
                                state.done = true;
                                return Some((Err(error), state));
                            }
                            Some(Ok(session)) => {
                                if state.options.is_aborted() {
                                    state.done = true;
                                    return Some((Err(state.options.abort_error()), state));
                                }
                                let readable = Arc::new(session);
                                let metadata = readable.get_metadata().await;
                                if !state.seen_session_ids.insert(metadata.id.clone()) {
                                    state.done = true;
                                    return Some((
                                        Err(format!("Duplicate sessionId: {}", metadata.id)),
                                        state,
                                    ));
                                }
                                state.metadata = Some(metadata);
                                state.candidates = Some(Box::pin(scanning_owned_entries(
                                    readable,
                                    state.search.options.readable.clone(),
                                    state.options.entry_types.clone(),
                                )));
                            }
                            None => {
                                return None;
                            }
                        }
                    }

                    let candidate = state
                        .candidates
                        .as_mut()
                        .expect("candidate stream initialized")
                        .as_mut()
                        .next()
                        .await;
                    match candidate {
                        Some(Err(error)) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                        Some(Ok(candidate)) => {
                            if state.options.is_aborted() {
                                state.done = true;
                                return Some((Err(state.options.abort_error()), state));
                            }
                            let metadata = state.metadata.as_ref().expect("metadata initialized");
                            let matches = state
                                .search
                                .options
                                .matcher
                                .as_ref()
                                .map(|matcher| {
                                    matcher(&state.normalized_text, &candidate, metadata)
                                })
                                .unwrap_or_else(|| {
                                    default_match(&state.normalized_text, &candidate.text)
                                });
                            if !matches {
                                continue;
                            }
                            let create_hit = match state.search.options.create_hit.as_ref() {
                                Some(create_hit) => create_hit,
                                None => {
                                    state.done = true;
                                    return Some((
                                        Err("Scanning search requires a create_hit callback for this hit type".to_string()),
                                        state,
                                    ));
                                }
                            };
                            let hit = create_hit(metadata, &candidate);
                            state.hit_count += 1;
                            if state
                                .options
                                .limit
                                .is_some_and(|limit| state.hit_count >= limit)
                            {
                                state.done = true;
                            }
                            return Some((Ok(hit), state));
                        }
                        None => {
                            state.candidates = None;
                            state.metadata = None;
                        }
                    }
                }
            },
        )
    }

    pub async fn search_with(
        &self,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<H>, String> {
        let mut stream = Box::pin(self.stream_search(text, options.clone()));
        let mut hits = Vec::new();
        while let Some(hit) = stream.next().await {
            hits.push(hit?);
        }
        Ok(hits)
    }
}

impl<R: ScanningReadable + 'static, S, St> LazyScanningSessionSearch<R, S, SessionSearchHit>
where
    S: Fn(&str, &SessionSearchOptions) -> St,
    St: Stream<Item = Result<R, String>> + 'static,
{
    pub async fn search(
        &self,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<SessionSearchHit>, String> {
        self.search_with(text, options).await
    }
}

/// Construct the lazy-source form of a scanning session search.
pub fn create_scanning_session_search<R, S, St>(
    source: S,
) -> LazyScanningSessionSearch<R, S, SessionSearchHit>
where
    R: ScanningReadable + 'static,
    S: Fn(&str, &SessionSearchOptions) -> St,
    St: Stream<Item = Result<R, String>> + 'static,
{
    LazyScanningSessionSearch::new(source)
}

/// Construct a lazy scanning search with custom projection, matching, and
/// hit-construction hooks.
pub fn create_scanning_session_search_with_options<R, S, St, H>(
    source: S,
    options: ScanningSearchOptions<H>,
) -> LazyScanningSessionSearch<R, S, H>
where
    R: ScanningReadable + 'static,
    S: Fn(&str, &SessionSearchOptions) -> St,
    St: Stream<Item = Result<R, String>> + 'static,
{
    LazyScanningSessionSearch::with_options(source, options)
}

/// Construct the upstream-shaped lazy scanning search. The source receives
/// the typed value returned by `source_options` (or `None`), while the search
/// itself still receives normalized text and ordinary search options.
pub fn create_typed_scanning_session_search<R, S, St, H, TSourceOptions>(
    source: S,
    options: TypedScanningSearchOptions<H, TSourceOptions>,
) -> LazyScanningSessionSearch<R, BoxedReadableSource<R>, H>
where
    R: ScanningReadable + 'static,
    S: Fn(Option<&TSourceOptions>) -> St + 'static,
    St: Stream<Item = Result<R, String>> + 'static,
    TSourceOptions: 'static,
{
    let TypedScanningSearchOptions {
        readable,
        matcher,
        create_hit,
        source_options,
    } = options;
    let boxed_source: BoxedReadableSource<R> = Box::new(move |query, search_options| {
        let typed_options = source_options
            .as_ref()
            .and_then(|factory| factory(query, search_options));
        Box::pin(source(typed_options.as_ref()))
    });
    LazyScanningSessionSearch::with_options(
        boxed_source,
        ScanningSearchOptions {
            readable,
            matcher,
            create_hit,
            source_options: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MemoryFs;
    use crate::session::jsonl::storage::JsonlSessionStorage;
    use crate::session::types::{EntryNoStats, JsonlV4Header};
    use crate::session::{jsonl_session_directory_name, CreateOptions, JsonlSessionRepo};
    use futures_util::StreamExt;
    use pi_ai::types::{ContentBlock, Message, UserContent};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn user_message(text: &str) -> crate::types::AgentMessage {
        crate::types::AgentMessage::Core(Message::User(UserContent::blocks(
            vec![ContentBlock::text(text)],
            1,
        )))
    }

    fn header(id: &str, cwd: &str) -> JsonlV4Header {
        JsonlV4Header {
            kind: "header".into(),
            version: 4,
            id: id.into(),
            created_at: 1_700_000_000_000,
            cwd: cwd.into(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        }
    }

    /// Memory-backed session with the given id/cwd.
    async fn memory_session(id: &str, cwd: &str) -> Session<MemoryFs> {
        let fs = MemoryFs::new();
        let path = format!("/sessions/{id}.jsonl");
        let storage = JsonlSessionStorage::create(fs, &path, header(id, cwd))
            .await
            .unwrap();
        Session::new(storage)
    }

    #[derive(Clone)]
    struct ReadableAdapter {
        session: Arc<Session<MemoryFs>>,
    }

    #[async_trait]
    impl ScanningReadable for ReadableAdapter {
        async fn get_metadata(&self) -> SessionMetadata {
            self.session.get_metadata().await
        }

        async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, String> {
            self.session
                .find_entries(query)
                .await
                .map_err(|error| error.to_string())
        }

        async fn get_label(&self, entry_id: &str) -> Option<String> {
            self.session.get_label(entry_id).await
        }
    }

    fn notes_entry(text: &str) -> EntryNoStats {
        let msg = user_message(text);
        EntryNoStats::Message {
            id: format!("note-{}", text.len()),
            message: msg,
            terminate: None,
        }
    }

    fn custom_entry(custom_type: &str, data: serde_json::Value) -> EntryNoStats {
        EntryNoStats::Custom {
            id: format!("custom-{custom_type}"),
            custom_type: custom_type.to_string(),
            data: Some(data),
        }
    }

    async fn hit_search(
        sessions: Vec<Session<MemoryFs>>,
        text: &str,
        options: &SessionSearchOptions,
    ) -> Result<Vec<SessionSearchHit>, String> {
        ScanningSessionSearch::new(sessions)
            .search(text, options)
            .await
    }

    fn default_options() -> SessionSearchOptions {
        SessionSearchOptions::default()
    }

    #[tokio::test]
    async fn scanning_entries_yields_projected_candidates() {
        let mut session = memory_session("stream", "/repo").await;
        let entry = session
            .append_entry(notes_entry("stream auth"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("stream label"))
            .await
            .unwrap();

        let options = ScanningReadableOptions {
            page_size: Some(1),
            ..Default::default()
        };
        let mut candidates = Box::pin(scanning_entries(&session, options));
        let candidate = candidates.next().await.unwrap().unwrap();
        assert_eq!(candidate.entry_id, entry.id());
        assert_eq!(candidate.entry_type, "message");
        assert!(candidate.text.contains("stream auth"));
        assert_eq!(candidate.seq, entry.seq());
        assert_eq!(
            candidate
                .fields
                .as_ref()
                .and_then(|fields| fields.get("label")),
            Some(&serde_json::Value::String("stream label".to_string()))
        );
        assert!(candidates.next().await.is_none());
    }

    #[tokio::test]
    async fn session_search_streams_hits_and_honors_live_abort() {
        let mut session = memory_session("stream-search", "/repo").await;
        session
            .append_entry(notes_entry("streamed auth"), "main")
            .await
            .unwrap();
        let search = ScanningSessionSearch::new(vec![session]);
        let mut hits = Box::pin(search.stream_search("auth", default_options()));
        let hit = hits.next().await.unwrap().unwrap();
        assert_eq!(hit.session_id, "stream-search");
        assert!(hits.next().await.is_none());

        let mut session = memory_session("abort-search", "/repo").await;
        session
            .append_entry(notes_entry("abort auth"), "main")
            .await
            .unwrap();
        let abort_signal = Arc::new(AtomicBool::new(false));
        let abort_for_projector = Arc::clone(&abort_signal);
        let search = ScanningSessionSearch::with_options(
            vec![session],
            ScanningSearchOptions {
                readable: ScanningReadableOptions {
                    project_text: Some(Arc::new(move |_, entry, _| {
                        abort_for_projector.store(true, Ordering::Release);
                        serde_json::to_string(entry).unwrap()
                    })),
                    ..Default::default()
                },
                matcher: None,
                create_hit: Some(Arc::new(default_create_hit)),
                source_options: None,
            },
        );
        let options = SessionSearchOptions {
            abort_signal: Some(abort_signal),
            ..Default::default()
        };
        let mut stream = Box::pin(search.stream_search("auth", options));
        assert!(stream.next().await.unwrap().is_err());
    }

    #[tokio::test]
    async fn scanning_search_supports_projector_matcher_and_custom_hit() {
        let mut session = memory_session("custom", "/repo").await;
        let entry = session
            .append_entry(notes_entry("projected auth"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("important"))
            .await
            .unwrap();

        let config = ScanningSearchOptions::<String> {
            readable: ScanningReadableOptions {
                project_text: Some(Arc::new(|metadata, entry, label| {
                    format!(
                        "{}:{}:{}",
                        metadata.id,
                        entry.id(),
                        label.unwrap_or("no-label")
                    )
                })),
                ..Default::default()
            },
            matcher: Some(Arc::new(|query, candidate, metadata| {
                query == "important"
                    && metadata.id == "custom"
                    && candidate.text.contains("important")
            })),
            create_hit: Some(Arc::new(|metadata, candidate| {
                format!("{}:{}", metadata.id, candidate.entry_id)
            })),
            source_options: None,
        };
        let search = ScanningSessionSearch::with_options(vec![session], config);
        let hits = search
            .search_with("important", &default_options())
            .await
            .unwrap();
        assert_eq!(hits, vec![format!("custom:{}", entry.id())]);
    }

    #[tokio::test]
    async fn lazy_scanning_source_is_loaded_on_search() {
        let mut session = memory_session("lazy", "/repo").await;
        session
            .append_entry(notes_entry("lazy auth"), "main")
            .await
            .unwrap();
        let slot = Arc::new(Mutex::new(Some(session)));
        let source_slot = Arc::clone(&slot);
        let source_calls = Arc::new(AtomicUsize::new(0));
        let source_calls_for_source = Arc::clone(&source_calls);
        let search =
            create_scanning_session_search::<Session<MemoryFs>, _, _>(move |_query, _options| {
                source_calls_for_source.fetch_add(1, Ordering::Relaxed);
                stream::iter(source_slot.lock().unwrap().take().into_iter().map(Ok))
            });

        assert!(search
            .search("", &default_options())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(source_calls.load(Ordering::Relaxed), 0);
        let lazy_stream = search.stream_search("auth", default_options());
        assert_eq!(source_calls.load(Ordering::Relaxed), 0);
        drop(lazy_stream);
        let hits = search.search("auth", &default_options()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "lazy");
        assert_eq!(source_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn lazy_source_options_receive_normalized_query_and_search_options() {
        let mut session = memory_session("source-options", "/repo").await;
        session
            .append_entry(notes_entry("source options auth"), "main")
            .await
            .unwrap();
        let source_slot = Arc::new(Mutex::new(Some(session)));
        let source_slot_for_source = Arc::clone(&source_slot);
        let search = create_scanning_session_search_with_options(
            move |query: &str, options: &SessionSearchOptions| {
                assert_eq!(query, "auth");
                assert_eq!(
                    options.source_options.as_ref(),
                    Some(&serde_json::json!({
                        "scope": "project",
                        "limit": 1
                    }))
                );
                stream::iter(
                    source_slot_for_source
                        .lock()
                        .unwrap()
                        .take()
                        .into_iter()
                        .map(Ok),
                )
            },
            ScanningSearchOptions {
                source_options: Some(Arc::new(|query, options| {
                    assert_eq!(query, "auth");
                    assert_eq!(options.limit, Some(1));
                    Some(serde_json::json!({ "scope": "project", "limit": 1 }))
                })),
                ..Default::default()
            },
        );
        let hits = search
            .search(
                "  AUTH  ",
                &SessionSearchOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "source-options");
    }

    #[tokio::test]
    async fn typed_lazy_source_options_match_upstream_source_contract() {
        #[derive(Debug, PartialEq, Eq)]
        struct SourceFilter {
            scope: String,
            limit: usize,
        }

        let mut session = memory_session("typed-source-options", "/repo").await;
        session
            .append_entry(notes_entry("typed source auth"), "main")
            .await
            .unwrap();
        let source_slot = Arc::new(Mutex::new(Some(session)));
        let source_slot_for_source = Arc::clone(&source_slot);
        let search = create_typed_scanning_session_search(
            move |source_options: Option<&SourceFilter>| {
                assert_eq!(
                    source_options,
                    Some(&SourceFilter {
                        scope: "project".to_string(),
                        limit: 1,
                    })
                );
                stream::iter(
                    source_slot_for_source
                        .lock()
                        .unwrap()
                        .take()
                        .into_iter()
                        .map(Ok),
                )
            },
            TypedScanningSearchOptions {
                source_options: Some(Arc::new(|query, options| {
                    assert_eq!(query, "auth");
                    assert_eq!(options.limit, Some(1));
                    Some(SourceFilter {
                        scope: "project".to_string(),
                        limit: 1,
                    })
                })),
                ..Default::default()
            },
        );
        let hits = search
            .search(
                " AUTH ",
                &SessionSearchOptions {
                    limit: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "typed-source-options");
    }

    #[tokio::test]
    async fn lazy_search_accepts_non_session_scanning_readables() {
        let mut session = memory_session("adapter-readable", "/repo").await;
        session
            .append_entry(notes_entry("adapter auth"), "main")
            .await
            .unwrap();
        let readable = ReadableAdapter {
            session: Arc::new(session),
        };
        let search = create_scanning_session_search(move |_query, _options| {
            stream::iter(vec![Ok(readable.clone())])
        });
        let hits = search
            .search("auth", &default_options())
            .await
            .expect("adapter-readable search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "adapter-readable");
    }

    // ---- oracle-ported tests (packages/agent/test/harness/session/search.test.ts) ----

    #[tokio::test]
    async fn scans_in_memory_projected_source() {
        let mut root = memory_session("root", "/repo").await;
        root.append_entry(notes_entry("fix auth flow"), "main")
            .await
            .unwrap();
        let mut other = memory_session("other", "/other").await;
        other
            .append_entry(notes_entry("auth in another workspace"), "main")
            .await
            .unwrap();

        let root_hits = hit_search(vec![root], "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(root_hits.len(), 1);
        assert_eq!(root_hits[0].session_id, "root");

        let other_hits = hit_search(vec![other], "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(other_hits[0].session_id, "other");

        let root = memory_session("root", "/repo").await;
        assert!(hit_search(vec![root], "missing", &default_options())
            .await
            .unwrap()
            .is_empty());
        assert!(hit_search(vec![], "auth", &default_options())
            .await
            .unwrap()
            .is_empty());
        // Trims and is case-insensitive.
        let mut root = memory_session("root", "/repo").await;
        root.append_entry(notes_entry("Fix Auth Flow"), "main")
            .await
            .unwrap();
        let hits = hit_search(vec![root], "  auth  ", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn includes_labels_in_projection() {
        let mut session = memory_session("session", "/repo").await;
        let entry = session
            .append_entry(notes_entry("plain body"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("important label"))
            .await
            .unwrap();

        let hits = hit_search(vec![session], "important", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "session");
        assert_eq!(hits[0].entry_id, entry.id());
        assert!(
            hits[0].snippet.contains("important label"),
            "got: {:?}",
            hits[0].snippet
        );
    }

    #[tokio::test]
    async fn honors_entry_type_filters_and_abort() {
        let mut session = memory_session("session", "/repo").await;
        session
            .append_entry(notes_entry("auth message"), "main")
            .await
            .unwrap();
        session
            .append_entry(
                custom_entry("note", serde_json::json!({ "text": "auth custom" })),
                "main",
            )
            .await
            .unwrap();

        let options = SessionSearchOptions {
            entry_types: Some(vec!["message".to_string()]),
            ..Default::default()
        };
        let hits = hit_search(vec![session], "auth", &options).await.unwrap();
        assert_eq!(hits.len(), 1, "got: {hits:?}");
        assert!(
            hits[0].snippet.contains("auth message"),
            "got: {:?}",
            hits[0].snippet
        );

        // Abort requested -> error surfaced immediately.
        let mut session = memory_session("session", "/repo").await;
        session
            .append_entry(notes_entry("auth message"), "main")
            .await
            .unwrap();
        let abort = SessionSearchOptions {
            abort_requested: true,
            ..Default::default()
        };
        let err = hit_search(vec![session], "auth", &abort).await.unwrap_err();
        assert!(err.to_lowercase().contains("abort"), "got: {err}");
    }

    #[tokio::test]
    async fn duplicate_session_ids_are_rejected() {
        // Two distinct storage backends sharing one session id -> the
        // duplicate guard fires while scanning the second readable.
        let mut a = memory_session("dup", "/a").await;
        a.append_entry(notes_entry("auth x"), "main").await.unwrap();
        let mut b = memory_session("dup", "/b").await;
        b.append_entry(notes_entry("auth y"), "main").await.unwrap();

        let err = hit_search(vec![a, b], "auth", &default_options())
            .await
            .unwrap_err();
        assert!(err.contains("Duplicate sessionId: dup"), "got: {err}");
    }

    #[tokio::test]
    async fn scans_jsonl_sessions_from_disk() {
        let fs = MemoryFs::new();
        let _root = format!("/{}/", jsonl_session_directory_name("/work"));
        let repo = JsonlSessionRepo::new(fs, "/sessions".to_string());
        let mut repo = repo;

        let mut session = repo
            .create(CreateOptions::new("/work").with_id("jsonl"))
            .await
            .unwrap();
        let entry = session
            .append_entry(notes_entry("jsonl backed auth entry"), "main")
            .await
            .unwrap();
        session
            .set_label(entry.id(), Some("disk label"))
            .await
            .unwrap();
        drop(session);

        let mut other = repo
            .create(CreateOptions::new("/other").with_id("other"))
            .await
            .unwrap();
        other
            .append_entry(
                notes_entry("jsonl backed auth entry in another cwd"),
                "main",
            )
            .await
            .unwrap();
        drop(other);

        let metadata_list = repo.list(None).await.unwrap();
        assert_eq!(metadata_list.len(), 2, "repo should discover both sessions");
        let mut sessions = Vec::new();
        for metadata in &metadata_list {
            sessions.push(repo.open(metadata).await.unwrap());
        }

        let hits = hit_search(sessions, "auth", &default_options())
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "got: {hits:?}");
        let ids: Vec<&str> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert!(ids.contains(&"jsonl"), "got: {ids:?}");
        assert!(ids.contains(&"other"), "got: {ids:?}");

        let sessions = {
            let metadata_list = repo.list(None).await.unwrap();
            let mut v = Vec::new();
            for metadata in &metadata_list {
                v.push(repo.open(metadata).await.unwrap());
            }
            v
        };
        let disk_hits = hit_search(sessions, "disk", &default_options())
            .await
            .unwrap();
        assert_eq!(disk_hits.len(), 1, "got: {disk_hits:?}");
        assert_eq!(disk_hits[0].session_id, "jsonl");
    }
}
