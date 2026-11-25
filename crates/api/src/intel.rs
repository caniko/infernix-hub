use super::*;
use axum::{extract::DefaultBodyLimit, routing::MethodRouter};
use domain::intel;

pub(super) fn signals() -> MethodRouter<AppState> {
    get(list).post(ingest).layer(DefaultBodyLimit::max(16 * 1024 * 1024))
}

async fn ingest(State(s): State<AppState>, Caller(a): Caller, headers: HeaderMap, Json(body): Json<SignalBatchIn>) -> R<SignalBatchOut> {
    let values = headers.get_all(IDEMPOTENCY_HEADER);
    let mut keys = values.iter();
    let invalid_key = || err(&a)(domain::invalid("invalid_idempotency_key", "expected at most one ASCII idempotency key"));
    let key = keys.next().map(HeaderValue::to_str).transpose().map_err(|_| invalid_key())?;
    if keys.next().is_some() {
        return Err(invalid_key());
    }
    call!(a, intel::ingest_batch(&s.hub, &a, body, key))
}

async fn list(State(s): State<AppState>, Caller(a): Caller, Query(query): Query<IntelSignalQuery>) -> R<IntelSignalPage> {
    call!(a, intel::list_signals(&s.hub, &a, query))
}

pub(super) async fn detail(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<IntelSignalDetail> {
    call!(a, intel::get_signal(&s.hub, &a, id))
}

pub(super) async fn decide(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SignalDecisionIn>,
) -> R<IntelSignal> {
    call!(a, intel::decide_signal(&s.hub, &a, id, body))
}

pub(super) async fn clear(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<SignalClearanceIn>,
) -> R<IntelSignal> {
    call!(a, intel::clear_signal(&s.hub, &a, id, body))
}

pub(super) async fn sources(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<IntelSourceStatus>> {
    call!(a, intel::list_sources(&s.hub, &a))
}
