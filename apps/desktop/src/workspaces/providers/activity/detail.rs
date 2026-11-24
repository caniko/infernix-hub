use std::cmp::Reverse;

use super::*;

#[derive(Clone, Copy, PartialEq)]
enum RequestSort {
    Time,
    Model,
    Outcome,
    Latency,
    Tokens,
    CacheHit,
    Cost,
}

const REQUEST_COLUMNS: &[(RequestSort, &str)] = &[
    (RequestSort::Time, "Time"),
    (RequestSort::Model, "Model"),
    (RequestSort::Outcome, "Outcome"),
    (RequestSort::Latency, "Latency (ms)"),
    (RequestSort::Tokens, "Tokens"),
    (RequestSort::CacheHit, "Cache hit %"),
    (RequestSort::Cost, "API-equivalent USD"),
];

#[component]
pub(super) fn Requests(scope: ActivityScope, providers: Vec<ProviderSummary>, on_reset: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut cursors = use_signal(|| vec![None::<String>]);
    let mut selected = use_signal(|| None::<Uuid>);
    let mut sort = use_signal(|| (RequestSort::Time, true));
    let mut refresh = use_signal(|| 0u32);
    let mut live = use_signal(|| false);
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if *live.peek() && cursors.peek().len() == 1 {
                refresh += 1;
            }
        }
    });
    let data = use_resource(move || {
        let (client, mut scope) = (client.clone(), scope.clone());
        let _ = refresh();
        scope.cursor = cursors.read().last().cloned().flatten();
        scope.limit = Some(50);
        async move { timed(client.activity_requests(&scope)).await }
    });
    let result = data.read();
    let (column, desc) = sort();
    rsx! {
        section { class: "glass-card stack",
            h2 { "Requests" }
            p { class: "muted", "Only retained requests are listed; Totals may cover dates whose request details have expired." }
            div { class: "row",
                button { class: "btn ghost", onclick: move |_| refresh += 1, "Refresh requests" }
                Check { label: "Live first page (15s)", value: live(), change: move |v| live.set(v) }
            }
            {read_view(result.as_ref(), |page| {
                let mut days = BTreeMap::<_, Vec<_>>::new();
                for row in &page.items { days.entry(Reverse(row.occurred_at.with_timezone(&Local).date_naive())).or_default().push(row); }
                for rows in days.values_mut() {
                    rows.sort_by(|a, b| {
                        let ordering = match column {
                            RequestSort::Time => a.occurred_at.cmp(&b.occurred_at),
                            RequestSort::Model => a.effective_model.cmp(&b.effective_model),
                            RequestSort::Outcome => a.outcome.cmp(&b.outcome),
                            RequestSort::Latency => a.latency_ms.cmp(&b.latency_ms),
                            RequestSort::Tokens => token_count(a).cmp(&token_count(b)),
                            RequestSort::CacheHit => request_cache_hit_rate(a.input_tokens, a.cache_read_tokens)
                                .partial_cmp(&request_cache_hit_rate(b.input_tokens, b.cache_read_tokens))
                                .unwrap_or(std::cmp::Ordering::Equal),
                            RequestSort::Cost => a.api_equivalent_cost_usd.partial_cmp(&b.api_equivalent_cost_usd).unwrap_or(std::cmp::Ordering::Equal),
                        };
                        (if desc { ordering.reverse() } else { ordering }).then_with(|| a.id.cmp(&b.id))
                    });
                }
                let next = page.next_cursor.clone();
                rsx! {
                    p { "{page.total_matched} matching · page {cursors.read().len()} · sorting within each day on this page" }
                    if days.is_empty() { ActivityEmpty { message: "No matching retained requests.", on_reset } }
                    for (Reverse(day), rows) in days {
                        div { key: "{day}", class: "provider-table", tabindex: "0", role: "region", aria_label: "Requests · {day}",
                            table {
                                caption { "{day} · local time" }
                                thead { tr {
                                    for &(id, label) in REQUEST_COLUMNS {
                                        th { key: "{label}", scope: "col", aria_sort: if id != column { "none" } else if desc { "descending" } else { "ascending" },
                                            button { class: "btn btn-glass btn-xs", onclick: move |_| sort.set((id, id == column && !desc)), "{label}" }
                                        }
                                    }
                                    th { scope: "col", "Source / client" }
                                    th { scope: "col", "Detail" }
                                } }
                                tbody { for row in rows {
                                    {
                                        let id = row.id;
                                        rsx! { tr { key: "{id}",
                                            td { {row.occurred_at.with_timezone(&Local).format("%H:%M:%S").to_string()} }
                                            td { "{row.effective_model}" }
                                            td { "{row.outcome}" }
                                            td { "{maybe_i64(row.latency_ms)}" }
                                            td { "{fmt_signed_count(token_count(row))}" }
                                            td { {fmt_hit_pct(request_cache_hit_rate(row.input_tokens, row.cache_read_tokens))} }
                                            td { "{money(row.api_equivalent_cost_usd)}" }
                                            td { "{activity_source_label(row.source)} · {row.client}" }
                                            td { button { class: "btn btn-glass btn-xs", aria_pressed: selected() == Some(id), onclick: move |_| selected.set(Some(id)), "Inspect" } }
                                        } }
                                    }
                                } }
                            }
                        }
                    }
                    div { class: "row",
                        button { class: "btn btn-glass", disabled: cursors.read().len() <= 1, onclick: move |_| { cursors.write().pop(); selected.set(None); }, "Previous page" }
                        button { class: "btn btn-glass", disabled: next.is_none(), onclick: move |_| if let Some(cursor) = next.clone() { cursors.write().push(Some(cursor)); selected.set(None); }, "Next page" }
                    }
                }
            })}
        }
        if let Some(id) = selected() {
            RequestInspector { key: "{id}", id, providers, operator: panel.operator(), close: move |_| selected.set(None) }
        }
    }
}

fn token_count(row: &RequestRow) -> i64 {
    row.input_tokens.saturating_add(row.output_tokens)
}
fn maybe_i64(v: Option<i64>) -> String {
    v.map_or_else(|| "—".into(), |n| n.to_string())
}

#[component]
fn RequestInspector(id: Uuid, providers: Vec<ProviderSummary>, operator: bool, close: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let capture_client = client.clone();
    let mut pending = use_signal(|| false);
    let mut capture = use_signal(|| None::<Result<CapturedBodyView, UiError>>);
    let data = use_resource(move || {
        let client = client.clone();
        async move { timed(client.activity_request_detail(id)).await }
    });
    let result = data.read();
    rsx! { section { class: "glass-card stack",
        div { class: "spread",
            h2 { "Request Inspector" }
            button { class: "btn btn-glass", onclick: move |_| close.call(()), "Close" }
        }
        p { class: "muted", "Exchange ID: " code { "{id}" } }
        {read_view(result.as_ref(), |detail| {
            let r = &detail.exchange;
            let hit_label = fmt_hit_pct(request_cache_hit_rate(r.input_tokens, r.cache_read_tokens));
            rsx! {
                p { "{r.client} · {activity_source_label(r.source)} · {activity_purpose_label(r.purpose)} · {text(&r.transport)} · {fmt_dt(r.occurred_at)}" }
                p { "Provider {r.provider_id} · Account {text(&r.account_id)} · Session ID: {text(&r.session_id)} · Moved from {text(&r.moved_from_account_id)}" }
                p { "Requested {r.requested_model} → effective {r.effective_model} · effort {text(&r.effort)} · {r.outcome} · HTTP {maybe_i64(r.status_code.map(i64::from))}" }
                p { "Latency {maybe_i64(r.latency_ms)} ms · TTFT {maybe_i64(r.ttft_ms)} ms · API-equivalent {money(r.api_equivalent_cost_usd)}" }
                p { "Input {fmt_signed_count(r.input_tokens)} · Output {fmt_signed_count(r.output_tokens)} · Cache read {fmt_signed_count(r.cache_read_tokens)} ({hit_label}) · Cache write {fmt_signed_count(r.cache_write_tokens)} · Thinking {fmt_signed_count(r.thinking_tokens)}" }
                if let Some(error) = &r.error_message { p { "{error}" } }
                h3 { "Upstream attempts ({r.attempt_count})" }
                Table { label: "Upstream attempts", headers: &["Attempt", "Provider", "Account", "Moved from", "Requested → effective", "Effort", "Outcome", "HTTP", "Error", "Latency ms", "TTFT ms", "Input", "Output", "Cache read", "Cache hit %", "Cache write", "Thinking", "API-equivalent USD", "Capture", "Started"],
                    rows: detail.attempts.iter().map(|a| vec![a.attempt_index.to_string(), a.provider_id.clone(), text(&a.account_id), text(&a.moved_from_account_id),
                        format!("{} → {}", a.requested_model, a.effective_model), text(&a.effort), a.outcome.clone(), a.status_code.map_or_else(|| "—".into(), |v| v.to_string()),
                        text(&a.error_message), maybe_i64(a.latency_ms), maybe_i64(a.ttft_ms), fmt_signed_count(a.input_tokens), fmt_signed_count(a.output_tokens),
                        fmt_signed_count(a.cache_read_tokens), fmt_hit_pct(request_cache_hit_rate(a.input_tokens, a.cache_read_tokens)),
                        fmt_signed_count(a.cache_write_tokens), fmt_signed_count(a.thinking_tokens), money(a.api_equivalent_cost_usd), capture_state_label(a.capture_state).into(), fmt_dt(a.started_at)
                    ]).collect() }
                if let Some(session_id) = &r.session_id && let Some(provider) = providers.iter().find(|p| p.descriptor.id == r.provider_id) {
                    MoveConversation { provider: provider.clone(), session_id: session_id.clone() }
                }
                p { "Capture: {capture_state_label(r.capture_state)}" if let Some(reason) = &detail.capture_gap_reason { " · {reason}" } }
                if detail.capture_available && operator {
                    button { class: "btn btn-glass", disabled: pending(), onclick: {
                        let client = capture_client.clone();
                        move |_| {
                            let client = client.clone();
                            pending.set(true);
                            spawn(async move {
                                capture.set(Some(timed_for(Duration::from_secs(20), client.activity_request_capture(id)).await));
                                pending.set(false);
                            });
                        }
                    }, "Load Captured Body" }
                }
            }
        })}
        if pending() { p { role: "status", "Loading captured body…" } }
        if operator && let Some(result) = capture.read().as_ref() {
            {read_view(Some(result), |body| rsx! {
                p { "{capture_state_label(body.representation)} · {fmt_count(body.size_bytes)} bytes · " if body.redacted { "Redacted" } else { "Not redacted — sensitive content" } }
                pre { class: "raw-json", "{pretty(&body.payload)}" }
                button { class: "btn btn-glass", onclick: move |_| capture.set(None), "Hide body" }
            })}
        }
    } }
}

#[component]
pub(super) fn Traces(scope: ActivityScope, on_reset: EventHandler<()>, pin: EventHandler<EvalSeed>) -> Element {
    let client = use_context::<HubClient>();
    let mut cursors = use_signal(|| vec![None::<String>]);
    let mut chosen = use_signal(|| None::<String>);
    let data = use_resource(move || {
        let (client, mut scope) = (client.clone(), scope.clone());
        scope.limit = Some(35);
        scope.cursor = cursors.read().last().cloned().flatten();
        async move { timed_for(Duration::from_secs(25), client.activity_traces(&scope)).await }
    });
    let result = data.read();
    rsx! {
        section { class: "glass-card stack",
            h2 { "Session traces" }
            {read_view(result.as_ref(), |page| {
                let next = page.next_cursor.clone();
                rsx! {
                    p { "Unassigned exchanges: {page.unassigned_exchanges} · page {cursors.read().len()}" }
                    if page.items.is_empty() { ActivityEmpty { message: "No traces in this scope.", on_reset } }
                    for trace in &page.items {
                        {
                            let id = trace.session_id.clone();
                            rsx! {
                                button { key: "{id}", class: "host-row",
                                    aria_pressed: chosen.read().as_ref() == Some(&id), onclick: move |_| chosen.set(Some(id.clone())),
                                    span { class: "mono", "{trace.session_id}" }
                                    span { "{trace.client} · {trace.primary_model} · {trace.exchange_count} exchanges · {trace.failure_count} failed" }
                                    span { class: "muted", "{fmt_dt(trace.last_activity_at)}" }
                                }
                            }
                        }
                    }
                    div { class: "row",
                        button { class: "btn btn-glass", disabled: cursors.read().len() <= 1, onclick: move |_| { cursors.write().pop(); chosen.set(None); }, "Previous page" }
                        button { class: "btn btn-glass", disabled: next.is_none(), onclick: move |_| if let Some(cursor) = next.clone() { cursors.write().push(Some(cursor)); chosen.set(None); }, "Next page" }
                    }
                }
            })}
        }
        if let Some(session) = chosen() {
            TraceInspector { key: "{session}", session, pin, close: move |_| chosen.set(None) }
        }
    }
}

#[component]
fn TraceInspector(session: String, pin: EventHandler<EvalSeed>, close: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let encoded = segment(&session);
    let data = use_resource(move || {
        let (client, id) = (client.clone(), encoded.clone());
        async move { timed_for(Duration::from_secs(25), client.activity_trace_detail(&path_id(id)?)).await }
    });
    let result = data.read();
    rsx! { section { class: "glass-card stack",
        div { class: "spread",
            h2 { "Trace Tree" }
            button { class: "btn btn-glass", onclick: move |_| close.call(()), "Close" }
        }
        p { class: "mono", "{session}" }
        {read_view(result.as_ref(), |trace| {
            let s = &trace.summary;
            rsx! {
                p { "Own: {usd(s.own_cost_usd)} · {fmt_count(s.own_tokens)} tokens. Known subtotal: {usd(s.subtotal_cost_usd)} · {fmt_count(s.subtotal_tokens)} tokens (includes own usage)." }
                if s.has_unobserved_parent || s.has_parent_conflict || s.has_cycle {
                    p { class: "monitoring-error", role: "status",
                        if s.has_unobserved_parent { "Unobserved parent. " }
                        if s.has_parent_conflict { "Parent conflict. " }
                        if s.has_cycle { "Cycle detected." }
                    }
                }
                ol { class: "provider-trace",
                    for node in &trace.nodes {
                        li { key: "{node.session_id}", style: "margin-inline-start: {node.depth.min(8)}rem",
                            div { class: "row", strong { class: "mono", "{node.session_id}" }
                                if let Some(kind) = node.edge_kind {
                                    span { class: "chip", {match kind { TraceEdgeKind::Subagent => "subagent", TraceEdgeKind::Fork => "fork", TraceEdgeKind::Compaction => "compaction" }} }
                                }
                                for edge in trace.edges.iter().filter(|e| e.child_session_id == node.session_id) {
                                    if !edge.observed_parent { span { class: "chip warn", "Unobserved parent" } }
                                    if edge.conflict { span { class: "chip warn", "Parent conflict" } }
                                    if edge.cycle { span { class: "chip bad", "Cycle" } }
                                }
                            }
                            p { "Depth {node.depth} · own {fmt_count(node.own_requests)} requests ({fmt_count(node.own_failures)} failed), {fmt_count(node.own_tokens)} tokens and {usd(node.own_cost_usd)} · known subtotal {usd(node.known_subtotal_cost_usd)} · {fmt_count(node.unpriced_requests)} unpriced" }
                            if let Some(parent) = &node.parent_session_id { p { class: "muted", "Parent: {parent}" } }
                            for (from, to) in &node.moves { p { class: "muted", "Moved {from} → {to}" } }
                            if !node.exchanges.is_empty() {
                                details { summary { "Retained exchanges ({node.exchanges.len()})" }
                                    for exchange in &node.exchanges {
                                        {
                                            let value = EvalSeed { provider: exchange.provider_id.clone(), model: exchange.effective_model.clone(),
                                                effort: exchange.effort.clone(), exchange: exchange.source_exchange_id.clone() };
                                            rsx! { div { key: "{exchange.id}", class: "row quota-window",
                                                span { "{fmt_dt(exchange.occurred_at)} · {exchange.effective_model} · {exchange.outcome} · {exchange.attempt_count} attempts · {capture_state_label(exchange.capture_state)}" }
                                                if panel.operator() {
                                                    button { class: "btn btn-glass", onclick: move |_| pin.call(value.clone()), "Pin as Eval" }
                                                }
                                            } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Archive { session: session.clone() }
            }
        })}
    } }
}
