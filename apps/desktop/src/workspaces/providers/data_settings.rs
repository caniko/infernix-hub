use super::{
    dashboard::{HORIZON, LOOKBACK, account_name},
    *,
};

#[component]
pub(super) fn DataExport() -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let ch_client = client.clone();
    let capture = use_resource(move || {
        let client = client.clone();
        let _ = (panel.epoch)();
        async move { timed(client.capture_policy()).await }
    });
    let clickhouse = use_resource(move || {
        let client = ch_client.clone();
        let _ = (panel.epoch)();
        async move { timed(client.clickhouse_status()).await }
    });
    let capture = capture.read();
    rsx! {
        section { class: "glass-card stack",
            div { class: "row",
                h2 { "Capture policy" }
                if let Some(Ok(policy)) = capture.as_ref() {
                    if policy.capture_bodies_enabled {
                        span { class: "chip ok", "Enabled" }
                    } else {
                        span { class: "chip muted", "Disabled" }
                    }
                }
            }
            p { class: "muted", "Off by default. Captured payloads may contain sensitive content; enable only when needed." }
            {read_view(capture.as_ref(), |policy| rsx! { CaptureEditor { key: "{panel.epoch}", policy: policy.clone() } })}
        }
        if panel.operator() { Diagnostics {} }
        section { class: "glass-card stack",
            h2 { "Portable JSONL archive" }
            if panel.operator() { Archive { session: String::new() } } else { p { class: "muted", "Archive export requires an operator session." } }
        }
        section { class: "glass-card stack",
            h2 { "ClickHouse sink" }
            {read_view(clickhouse.read().as_ref(), |status| rsx! { ClickHouseEditor { key: "{panel.epoch}", status: status.clone() } })}
        }
        section { class: "glass-card stack",
            h2 { "Offline encrypted transfer" }
            p { "Use the offline transfer workflow, not the portable JSONL archive, to move retained hub data. Supply the same securely stored HUB_TRANSFER_KEY on export and import; never paste it into this panel." }
            pre { class: "raw-json", "hubctl providers transfer-export --help\nhubctl providers transfer-import --help" }
        }
        if panel.operator() { ClearData {} }
    }
}

#[component]
fn CaptureEditor(policy: CapturePolicy) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut enabled = use_signal(|| policy.capture_bodies_enabled);
    let mut details = use_signal(|| policy.detail_retention_days.to_string());
    let mut bodies = use_signal(|| policy.body_retention_days.to_string());
    let mut budget = use_signal(|| policy.body_budget_mib.to_string());
    let bounded = |s: String, max| s.parse::<u32>().ok().filter(|n| (1..=max).contains(n));
    let updated = match (bounded(details(), 365), bounded(bodies(), 90), bounded(budget(), 1024)) {
        (Some(detail_retention_days), Some(body_retention_days), Some(body_budget_mib)) => Some(CapturePolicy {
            capture_bodies_enabled: enabled(),
            detail_retention_days,
            body_retention_days,
            body_budget_mib,
            health: policy.health.clone(),
        }),
        _ => None,
    };
    rsx! {
        dl { class: "kv-grid content-bounded", aria_label: "Capture counters",
            for (label, value) in [
                ("Bodies", policy.health.stored_bodies),
                ("Bytes", policy.health.stored_bytes),
                ("Gaps", policy.health.gaps),
                ("Dropped", policy.health.dropped),
                ("Rejected", policy.health.rejected),
                ("Evictions", policy.health.over_budget_evictions),
            ] {
                div { key: "{label}", class: "kv-item",
                    dt { class: "kv-label", "{label}" }
                    dd { class: "kv-value", "{fmt_count(value)}" }
                }
            }
        }
        div { class: "form-grid-2 content-bounded", style: "grid-template-columns: repeat(auto-fit, minmax(min(100%, 10rem), 1fr)); max-width: 38rem;",
            Field { label: "Detail retention (1–365 days)", kind: "number", value: details(), change: move |v| details.set(v) }
            Field { label: "Body retention (1–90 days)", kind: "number", value: bodies(), change: move |v| bodies.set(v) }
            Field { label: "Body budget (1–1024 MiB)", kind: "number", value: budget(), change: move |v| budget.set(v) }
        }
        div { class: "row",
            Check { label: "Opt in to body capture", value: enabled(), change: move |v| enabled.set(v) }
            button { class: "btn primary", disabled: updated.is_none() || (panel.busy)() || !panel.operator(), onclick: move |_| {
                let Some(value) = updated.clone() else { return; };
                let client = client.clone();
                let Some(busy) = panel.begin() else { return; };
                spawn(async move {
                    let _busy = busy;
                    let result = timed(client.set_capture_policy(&value)).await;
                    if result.is_ok() { panel.changed(); }
                    panel.report(result.map(|_| "Capture policy saved.".into()));
                });
            }, "Save capture policy" }
        }
    }
}

const CLEAR_TARGETS: &[(ClearTarget, &str)] = &[
    (ClearTarget::QuotaHistory, "quota history"),
    (ClearTarget::CapturedBodies, "captured bodies"),
    (ClearTarget::ActivityDetails, "activity details"),
    (ClearTarget::ActivityAll, "all activity"),
];

#[component]
fn ClearData() -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut pending = use_signal(|| None::<(ClearTarget, &'static str)>);
    rsx! { section { class: "danger-zone stack",
        h2 { "Delete hub data" }
        p { class: "form-help", "Deletion applies across providers and does not recall rows delivered to an external sink." }
        div { class: "row",
            for &(target, label) in CLEAR_TARGETS {
                button { key: "{label}", class: "btn danger", disabled: (panel.busy)(), onclick: move |_| pending.set(Some((target, label))), "Delete {label}" }
            }
        }
        if let Some((target, label)) = pending() {
            div { class: "gateprompt", role: "group", aria_label: "Confirm deletion",
                span { class: "gate-label", "Permanently delete {label}?"
                    if target == ClearTarget::ActivityDetails { " Saved totals will be kept." }
                }
                button { class: "btn danger", disabled: (panel.busy)(), onclick: move |_| {
                    let client = client.clone();
                    let Some(busy) = panel.begin() else { return; };
                    pending.set(None);
                    spawn(async move {
                        let _busy = busy;
                        let result = timed(client.clear_activity_data(&ClearDataIn { target })).await;
                        if result.is_ok() { panel.changed(); }
                        panel.report(result.map(|out| format!("Deleted {} rows: {label}.", out.rows_deleted)));
                    });
                }, "Confirm deletion" }
                button { class: "btn ghost", onclick: move |_| pending.set(None), "Cancel" }
            }
        }
    } }
}

#[component]
fn Diagnostics() -> Element {
    let client = use_context::<HubClient>();
    let mut ids = use_signal(String::new);
    let mut errors = use_signal(|| false);
    let mut bodies = use_signal(|| false);
    let mut logs = use_signal(|| false);
    let mut pending = use_signal(|| false);
    let mut output = use_signal(|| None::<Result<DiagnosticBundlePreview, UiError>>);
    let raw = ids();
    let parsed: Result<Vec<Uuid>, _> =
        raw.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::parse).collect();
    let request = parsed.map_err(|_| "Enter valid Exchange IDs (UUIDs).").and_then(|mut exchange_ids| {
        if exchange_ids.is_empty() {
            return Err("Enter at least one Exchange ID");
        }
        if exchange_ids.len() > 50 {
            return Err("Enter no more than 50 Exchange IDs.");
        }
        exchange_ids.sort_unstable();
        exchange_ids.dedup();
        Ok(DiagnosticBundlePreviewIn {
            exchange_ids,
            include_error_messages: errors(),
            include_transformed_bodies: bodies(),
            include_proxy_logs: logs(),
        })
    });
    let disabled_reason = request.as_ref().err().copied();
    let result = output.read();
    rsx! { section { class: "glass-card stack",
        h2 { "Diagnostic bundle preview" }
        h3 { "1. Select data" }
        div { class: "form-field content-bounded",
            label { r#for: "diagnostic-exchange-ids", "Exchange IDs (up to 50, space or comma separated)" }
            textarea { id: "diagnostic-exchange-ids", class: "input", rows: "2", style: "min-height: 0;",
                aria_describedby: "diagnostic-id-help", disabled: pending(), value: raw, oninput: move |e| { ids.set(e.value()); output.set(None); } }
            p { id: "diagnostic-id-help", class: "form-help", "Find IDs in Activity → Requests." }
        }
        div { class: "row",
            Check { label: "Include error messages", value: errors(), disabled: pending(), change: move |v| { errors.set(v); output.set(None); } }
            Check { label: "Include transformed bodies", value: bodies(), disabled: pending(), change: move |v| { bodies.set(v); output.set(None); } }
            Check { label: "Include proxy logs", value: logs(), disabled: pending(), change: move |v| { logs.set(v); output.set(None); } }
        }
        div { class: "row",
            button { class: "btn primary", disabled: disabled_reason.is_some() || pending(),
                aria_describedby: disabled_reason.map(|_| "diagnostic-build-reason"), onclick: move |_| {
                let Ok(request) = request.clone() else { return; };
                let client = client.clone();
                pending.set(true);
                output.set(None);
                spawn(async move {
                    output.set(Some(timed_for(Duration::from_secs(30), client.preview_diagnostic_bundle(&request)).await));
                    pending.set(false);
                });
            }, "Build preview" }
            if let Some(reason) = disabled_reason {
                span { id: "diagnostic-build-reason", class: "form-help", "{reason}" }
            }
        }
        div { class: "provider-inset stack",
            h3 { "2. Review redacted preview" }
            if pending() {
                p { role: "status", "Building preview…" }
            } else if let Some(result) = result.as_ref() {
                {read_view(Some(result), |data| rsx! {
                    p { class: "form-help", "Review before sharing. Redaction is not anonymization." }
                    p { "Preview {data.preview_id} · {data.exchange_count} exchanges · {data.estimated_bytes} bytes · {fmt_dt(data.created_at)}" }
                    for warning in &data.redaction_warnings { p { class: "monitoring-error", "{warning}" } }
                    Output { label: "Redacted JSON snapshot", value: pretty(&data.snapshot), filename: "diagnostic-preview.json" }
                })}
            } else {
                p { class: "form-help", "Build a redacted preview above to inspect before copying." }
            }
        }
    } }
}

#[component]
pub(super) fn Archive(session: String) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut session_input = use_signal(|| session.clone());
    let mut raw = use_signal(|| false);
    let mut pending = use_signal(|| false);
    let mut output = use_signal(|| None::<Result<PortableArchiveExport, UiError>>);
    let result = output.read();
    if !panel.operator() {
        return rsx! {};
    }
    rsx! {
        p { class: "muted", "Portable archives are unencrypted. Review before sharing; raw wire may contain sensitive data." }
        if session.is_empty() { p { class: "form-help", "Find session IDs in Activity → Requests." } }
        div { class: "row",
            if session.is_empty() {
                Field { label: "Session ID", value: session_input(), change: move |v| session_input.set(v) }
            }
            Check { label: "Include raw wire if authorized and retained", value: raw(), disabled: pending(), change: move |v| raw.set(v) }
            button { class: "btn btn-glass", disabled: optional(&session_input()).is_none() || pending(), onclick: move |_| {
                let Some(session_id) = optional(&session_input()) else { return; };
                let request = PortableArchiveRequest { session_id, include_raw_wire: raw() };
                let client = client.clone();
                pending.set(true);
                output.set(None);
                spawn(async move {
                    output.set(Some(timed_for(Duration::from_secs(30), client.export_portable_archive(&request)).await));
                    pending.set(false);
                });
            }, "Export Portable JSONL" }
        }
        if pending() { p { role: "status", "Building archive…" } }
        if let Some(result) = result.as_ref() {
            {read_view(Some(result), |data| rsx! {
                details { summary { "Manifest" } pre { class: "raw-json", "{pretty(&data.manifest)}" } }
                for warning in &data.warnings { p { class: "monitoring-error", "{warning}" } }
                Output { label: "Portable JSONL", value: data.jsonl.clone(), filename: "activity-archive.jsonl" }
            })}
        }
    }
}

#[component]
fn ClickHouseEditor(status: ClickHouseStatus) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut draft = use_signal(|| status.config.clone());
    let config = draft();
    let current = config.clone();
    let valid = !config.enabled || config.endpoint.as_deref().is_some_and(|s| s.starts_with("https://") || s.starts_with("http://"));
    let connection = if status.connected { "Connected" } else { "Not connected" };
    let schema = status.schema_version.map_or_else(|| "—".to_string(), |v| v.to_string());
    rsx! {
        p { "Connection: {connection} · Outbox: {status.queued_outbox_rows} queued · Schema: {schema} · Last delivery: {date(status.last_delivery_at)}" }
        if let Some(error) = &status.last_error { p { role: "status", "{error}" } }
        p { class: "muted", "Changing local retention does not recall exported rows. Do not include credentials in the endpoint URL." }
        div { class: "row",
            Check { label: "Enabled", value: current.enabled, change: move |v| draft.write().enabled = v }
            Field { label: "Endpoint", kind: "url", value: current.endpoint.unwrap_or_default(), change: move |v: String| draft.write().endpoint = optional(&v) }
            Field { label: "Database", value: current.database.unwrap_or_default(), change: move |v: String| draft.write().database = optional(&v) }
            Field { label: "Source label", value: current.source_label.unwrap_or_default(), change: move |v: String| draft.write().source_label = optional(&v) }
            Check { label: "Export content", value: current.export_content, change: move |v| draft.write().export_content = v }
            Check { label: "Export raw wire", value: current.export_raw_wire, change: move |v| draft.write().export_raw_wire = v }
            Check { label: "Read through", value: current.read_through_enabled, change: move |v| draft.write().read_through_enabled = v }
            button { class: "btn btn-primary", disabled: !valid || (panel.busy)() || !panel.operator(), onclick: move |_| {
                let client = client.clone();
                let config = config.clone();
                let Some(busy) = panel.begin() else { return; };
                spawn(async move {
                    let _busy = busy;
                    let result = timed(client.set_clickhouse_config(&config)).await;
                    if result.is_ok() { panel.changed(); }
                    panel.report(result.map(|_| "ClickHouse configuration saved.".into()));
                });
            }, "Save ClickHouse configuration" }
        }
    }
}

#[component]
pub(super) fn SettingsHelp(providers: Vec<ProviderSummary>, ready: bool) -> Element {
    rsx! {
        if ready { Settings { providers } }
        section { class: "glass-card stack",
            h2 { "Help" }
            for (title, hint, body) in [
                ("Steer new conversations", "Choose how newly started sessions are assigned.", "In Accounts, set account priorities or weights, then choose the matching strategy in Routing & Proxy. Existing sessions may stay pinned by session affinity."),
                ("Steer all traffic", "Temporarily prefer one account across the pool.", "In Routing & Proxy, set a preferred pool account to override ordinary scheduling. Choose Automatic to return to policy."),
                ("Move one conversation", "Reassign an existing session without changing pool policy.", "Open Activity → Requests, inspect a request with a session ID, and select Move Conversation. Choose whether to include subagents; this can lose warm cache."),
                ("Stop using an account", "Disable routing without removing its credentials.", "In Accounts, open Manage and choose Disable to stop routing new requests to that account. Remove login is a separate credential operation."),
                ("Unlock an outside tool", "Clear a lockout for an external client.", "Check Outside-tool lockouts in Routing & Proxy and unlock the matching entry. Correct the outside tool’s management configuration before retrying."),
            ] {
                details { key: "{title}", class: "content-bounded",
                    summary {
                        "{title}"
                        span { class: "form-help", style: "display: block; padding-inline-start: 1em;", "{hint}" }
                    }
                    p { "{body}" }
                }
            }
            p { class: "muted", "Actions depend on the provider’s advertised capabilities and hub authorization. An accepted operation is not proof it was executed." }
        }
    }
}

#[component]
fn Settings(providers: Vec<ProviderSummary>) -> Element {
    let client = use_context::<HubClient>();
    let mut panel = use_context::<PanelState>();
    let mut draft = use_signal(|| panel.settings.cloned());
    let settings = draft();
    let mut cadence = use_signal(|| settings.refresh_interval_secs.to_string());
    let mut saving = use_signal(|| false);
    let operator = panel.operator();
    let parsed = cadence().parse::<u32>().ok().filter(|n| (5..=3600).contains(n));
    let valid_slots = settings.tray_slots.iter().all(|slot| {
        !slot.provider_id.is_empty()
            && !slot.window_key.is_empty()
            && (0.0..=100.0).contains(&slot.warn_threshold_pct)
            && (0.0..=100.0).contains(&slot.critical_threshold_pct)
            && if slot.show_remaining {
                slot.critical_threshold_pct <= slot.warn_threshold_pct
            } else {
                slot.critical_threshold_pct >= slot.warn_threshold_pct
            }
    });
    let disabled_reason = if !operator {
        Some("Read-only: saving requires operator permissions.")
    } else if saving() || (panel.toolbar_saving)() {
        Some("Saving preferences…")
    } else if (panel.busy)() {
        Some("Wait for the current hub operation to finish.")
    } else if parsed.is_none() {
        Some("Enter a reload interval from 5 to 3600 seconds.")
    } else if !valid_slots {
        Some("Choose a provider and window for each slot, and check the threshold values and order.")
    } else {
        None
    };
    rsx! {
        section { class: "glass-card stack",
            h2 { "Saved default preferences" }
            p { class: "form-help", "Changes here take effect when you save and are separate from the current provider and activity filters." }
            fieldset { class: "form-stack", disabled: !operator || saving(), style: "border: 0; padding: 0; margin: 0; min-width: 0;",
                div { class: "form-grid-2 content-bounded", style: "align-items: start; max-width: 36rem;",
                    div { class: "form-field",
                        label { class: "form-help", r#for: "provider-reload-interval", "Provider reload interval" }
                        input { id: "provider-reload-interval", class: "input", r#type: "number", min: "5", max: "3600",
                            aria_describedby: "provider-reload-help", value: cadence(), oninput: move |e| cadence.set(e.value()) }
                        p { id: "provider-reload-help", class: "form-help", "5–3,600 seconds between automatic reloads." }
                    }
                    Choice { label: "Account sort", value: sort_key(settings.account_sort), options: pairs(SORTS),
                        change: move |v: String| draft.write().account_sort = sort_order(&v) }
                    Choice { label: "Drain lookback", value: settings.drain_lookback_hours.to_string(), options: pairs(LOOKBACK),
                        change: move |v: String| if let Ok(n) = v.parse() { draft.write().drain_lookback_hours = n; } }
                    Choice { label: "Forecast horizon", value: settings.forecast_horizon_hours.to_string(), options: pairs(HORIZON),
                        change: move |v: String| if let Ok(n) = v.parse() { draft.write().forecast_horizon_hours = n; } }
                }
                Check { label: "Mask emails", value: settings.mask_emails, change: move |v| draft.write().mask_emails = v }
                h3 { "Notifications" }
                div { class: "form-grid-2 content-bounded", style: "max-width: 28rem; row-gap: 4px;", role: "group", aria_label: "Notifications",
                    Check { label: "Limit hit", value: settings.notify_limit_hit, change: move |v| draft.write().notify_limit_hit = v }
                    Check { label: "Limit reset", value: settings.notify_limit_reset, change: move |v| draft.write().notify_limit_reset = v }
                    Check { label: "Account switch", value: settings.notify_account_switch, change: move |v| draft.write().notify_account_switch = v }
                    Check { label: "Proxy crash", value: settings.notify_proxy_crash, change: move |v| draft.write().notify_proxy_crash = v }
                }
                h3 { "Tray metric slots" }
                p { class: "form-help", "Each slot measures one window. Thresholds follow the selected direction (used or remaining)." }
                for (index, slot) in settings.tray_slots.iter().enumerate() {
                    TraySlot { key: "{slot.id}", providers: providers.clone(), index, slot: slot.clone(), draft }
                }
                div { class: "row",
                    button { class: "btn secondary", onclick: move |_| draft.write().tray_slots.push(TrayMetricSlot {
                        id: Uuid::new_v4().to_string(), provider_id: String::new(), account_id: None, window_key: String::new(),
                        show_remaining: true, warn_threshold_pct: 25.0, critical_threshold_pct: 10.0, shape: "circle".into(), accent_color: None,
                    }), "Add metric slot" }
                    if settings.tray_slots.is_empty() { span { class: "form-help", "No tray metric slots configured yet" } }
                }
            }
            div { class: "row",
                button { class: "btn primary", disabled: disabled_reason.is_some(), aria_describedby: disabled_reason.map(|_| "settings-save-reason"), onclick: move |_| {
                    let Some(refresh_interval_secs) = parsed else { return; };
                    if !valid_slots || *panel.toolbar_saving.peek() {
                        return;
                    }
                    let value = ProviderPanelSettings { refresh_interval_secs, ..draft.cloned() };
                    let client = client.clone();
                    let Some(busy) = panel.begin() else { return; };
                    panel.toolbar_save_pending.set(false);
                    saving.set(true);
                    spawn(async move {
                        let _busy = busy;
                        let result = timed(client.set_provider_settings(&value)).await;
                        if let Ok(saved) = &result {
                            cadence.set(saved.refresh_interval_secs.to_string());
                            draft.set(saved.clone());
                            panel.settings.set(saved.clone());
                            panel.saved_settings.set(Some(saved.clone()));
                            panel.changed();
                        }
                        saving.set(false);
                        panel.report(result.map(|_| "Panel settings saved.".into()));
                    });
                }, "Save settings" }
                if let Some(reason) = disabled_reason {
                    span { class: "form-help", id: "settings-save-reason", role: "status", "{reason}" }
                }
            }
        }
    }
}

#[component]
fn TraySlot(providers: Vec<ProviderSummary>, index: usize, slot: TrayMetricSlot, mut draft: Signal<ProviderPanelSettings>) -> Element {
    let mask = draft.read().mask_emails;
    let provider = providers.iter().find(|p| p.descriptor.id == slot.provider_id);
    let mut provider_options = vec![(String::new(), "Select provider".into())];
    provider_options.extend(providers.iter().map(|p| (p.descriptor.id.clone(), p.descriptor.label.clone())));
    let mut account_options = vec![(String::new(), "Pool".into())];
    let mut windows = std::collections::BTreeMap::new();
    if let Some(provider) = provider {
        account_options.extend(provider.accounts.iter().map(|a| (a.id.clone(), account_name(a, mask))));
        if let Some(account) = provider.accounts.iter().find(|a| Some(&a.id) == slot.account_id.as_ref()) {
            windows.extend(account.windows.iter().map(|w| (w.window_key.clone(), w.label.clone())));
        } else {
            windows.extend(provider.pools.iter().map(|w| (w.window_key.clone(), w.label.clone())));
        }
    }
    let mut window_options = vec![(String::new(), "Select window".into())];
    window_options.extend(windows);
    let mut warn = use_signal(|| slot.warn_threshold_pct.to_string());
    let mut critical = use_signal(|| slot.critical_threshold_pct.to_string());
    let set_threshold = move |text: String, critical_value: bool| {
        let value = text.parse::<f64>().unwrap_or(-1.0);
        if let Some(slot) = draft.write().tray_slots.get_mut(index) {
            if critical_value {
                slot.critical_threshold_pct = value;
            } else {
                slot.warn_threshold_pct = value;
            }
        }
    };
    let mut set_warn = set_threshold;
    let mut set_critical = set_threshold;
    rsx! {
        div { class: "provider-inset row",
            Choice { label: "Provider", value: slot.provider_id, options: provider_options, change: move |v| {
                if let Some(s) = draft.write().tray_slots.get_mut(index) {
                    s.provider_id = v; s.account_id = None; s.window_key.clear();
                }
            } }
            Choice { label: "Account", value: slot.account_id.unwrap_or_default(), options: account_options, change: move |v: String| {
                if let Some(s) = draft.write().tray_slots.get_mut(index) { s.account_id = optional(&v); s.window_key.clear(); }
            } }
            Choice { label: "Window", value: slot.window_key, options: window_options,
                change: move |v| if let Some(s) = draft.write().tray_slots.get_mut(index) { s.window_key = v; } }
            Check { label: "Show remaining (off: used)", value: slot.show_remaining,
                change: move |v| if let Some(s) = draft.write().tray_slots.get_mut(index) { s.show_remaining = v; } }
            Field { label: "Warn %", kind: "number", value: warn(), change: move |v: String| { warn.set(v.clone()); set_warn(v, false); } }
            Field { label: "Critical %", kind: "number", value: critical(), change: move |v: String| { critical.set(v.clone()); set_critical(v, true); } }
            Choice { label: "Shape", value: slot.shape, options: pairs(&[("circle", "Circle"), ("rounded", "Rounded"), ("square", "Square"), ("text", "Text")]),
                change: move |v| if let Some(s) = draft.write().tray_slots.get_mut(index) { s.shape = v; } }
            Field { label: "Accent color (optional)", value: slot.accent_color.unwrap_or_default(),
                change: move |v: String| if let Some(s) = draft.write().tray_slots.get_mut(index) { s.accent_color = optional(&v); } }
            button { class: "btn btn-glass", disabled: index == 0, onclick: move |_| {
                let mut settings = draft.write();
                if index > 0 && index < settings.tray_slots.len() { settings.tray_slots.swap(index - 1, index); }
            }, "Move up" }
            button { class: "btn btn-glass danger", onclick: move |_| {
                let mut settings = draft.write();
                if index < settings.tray_slots.len() { settings.tray_slots.remove(index); }
            }, "Remove slot" }
        }
    }
}
