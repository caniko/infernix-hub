use chrono::{DateTime, TimeDelta, Utc};
use contracts::providers::*;
use domain::ports::{ModelProvider, PortError, ProviderDispatch, ProviderIntent, ProviderReconciliation};
use hub_integrations::providers::{
    SpillwayProxyProvider,
    antigravity::{merge_antigravity_windows, parse_antigravity_models_fallback, parse_antigravity_summary},
    claude::{parse_claude_oauth_usage, parse_meridian_quota_all},
    fixture::{FullFixtureModelProvider, ReadOnlyFixtureModelProvider},
    http_client,
    opencode::{parse_chatgpt_wham_usage, parse_opencode_go_usage},
    push::PushModelProvider,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    "2026-11-21T12:00:00Z".parse().unwrap()
}

#[test]
fn claude_windows_keep_nulls_and_converts_minor_units() {
    let windows = parse_claude_oauth_usage(
        &json!({
            "five_hour": {"utilization": 0},
            "seven_day": {"utilization": null, "resets_at": "2026-11-24T00:00:00Z"},
            "seven_day_opus": null,
            "seven_day_sonnet": {"utilization": 60},
            "seven_day_haiku": {"utilization": 100},
            "seven_day_oauth_apps": {"utilization": 12},
            "seven_day_cowork": {"utilization": 18},
        "model_scoped": [{"model_id": "custom-one", "display_name": "Custom One", "utilization": 66}],
            "limits": [{"kind": "weekly_scoped", "percent": 42, "scope": {"model": {"id": "opus-4", "display_name": "Claude Opus 4"}}}],
            "extra_usage": {"is_enabled": true, "used_credits": 570, "monthly_limit": 1000, "utilization": null},
        "spend": {"enabled": true, "used": {"amount_minor": 850, "currency": "EUR", "exponent": 2},
                  "limit": {"amount_minor": 20000, "currency": "EUR", "exponent": 3}}
        }),
        now(),
    );
    let get = |key| windows.iter().find(|w| w.window_key == key).unwrap();
    assert_eq!(get("five_hour").used_percent, Some(0.0));
    assert_eq!(get("seven_day").observation_state, QuotaObservationState::Unavailable);
    assert_eq!(get("seven_day").used_percent, None);
    assert_eq!(get("seven_day_opus").used_percent, None);
    assert_eq!(get("seven_day_sonnet").scope_model.as_deref(), Some("sonnet"));
    assert_eq!(get("seven_day_haiku").scope_model.as_deref(), Some("haiku"));
    assert_eq!(get("seven_day_haiku").kind, QuotaWindowKind::ModelScoped);
    assert_eq!(get("extra_usage").used_amount, Some(5.7));
    assert_eq!(get("extra_usage").used_percent, None);
    assert_eq!(get("spend").used_amount, Some(8.5));
    assert_eq!(get("spend").limit_amount, Some(20.0));
    assert_eq!(get("spend").used_percent, Some(42.5));
    assert_eq!(get("seven_day_custom_one").window_duration_mins, Some(10080));
    assert!(windows.iter().any(|w| w.scope_model.as_deref() == Some("custom-one")));
    assert!(windows.iter().any(|w| w.scope_model.as_deref() == Some("opus-4")));
}

#[test]
fn meridian_preserves_last_good_and_distinguishes_unsupported_and_failed() {
    let accounts = parse_meridian_quota_all(
        &json!({
            "activeProfile": "max",
            "profiles": [
                {"id": "max", "stale": true, "fetchedAt": 1700000000000_i64,
                 "failure": {"reason": "rate_limited", "consecutiveFailures": 3},
                 "windows": [{"type": "five_hour", "utilization": 0.67, "resetsAt": 1700000010000_i64}]},
                {"id": "key", "type": "vertex", "error": "not_oauth", "fetchedAt": null},
                {"id": "missing", "error": "no_token", "windows": []},
                {"id": "unseen", "fetchedAt": null}
            ]
        }),
        "custom-claude",
        now(),
    );
    let [max, key, missing, unseen] = &accounts[..] else { panic!("expected four profiles") };
    assert!(max.is_active);
    assert_eq!(max.windows[0].used_percent, Some(67.0));
    assert_eq!(max.windows[0].observation_state, QuotaObservationState::StaleLastGood);
    assert_eq!(max.last_observed_at, DateTime::from_timestamp_millis(1700000000000));
    assert!(max.status_message.as_deref().unwrap().contains("rate_limited"));
    assert_eq!(key.windows[0].observation_state, QuotaObservationState::Unsupported);
    assert_eq!(missing.windows[0].observation_state, QuotaObservationState::ProbeFailed);
    assert_eq!(unseen.windows[0].observation_state, QuotaObservationState::NeverObserved);
}

#[test]
fn meridian_rate_limit_is_a_quota_error_even_without_a_successful_probe() {
    let accounts = parse_meridian_quota_all(
        &json!({
            "profiles": [
                {"id":"first", "fetchedAt":null, "error":"rate_limited", "failure":{"consecutiveFailures":2}},
                {"id":"cached", "fetchedAt":null, "failure":{"reason":"rate_limited"},
                 "lastGood":{"fetchedAt":1700000000000_i64,"windows":[{"type":"five_hour","utilization":0.4}]}}
            ]
        }),
        "claude",
        now(),
    );
    let [first, cached] = &accounts[..] else { panic!("expected two profiles") };
    assert_eq!(first.enrollment_state, EnrollmentState::Unobserved);
    assert_ne!(first.status, AccountStatus::Error);
    assert_eq!(first.last_observed_at, None);
    assert_eq!(first.windows[0].observation_state, QuotaObservationState::ProbeFailed);
    let error = first.last_error.as_ref().unwrap();
    assert_eq!(
        (error.kind, error.source, error.http_status),
        (AccountErrorKind::QuotaExhausted, AccountErrorSource::QuotaProbe, Some(429))
    );
    assert_eq!(error.consecutive_count, 2);
    assert!(!error.blocks_routing(now()));
    assert_eq!(cached.enrollment_state, EnrollmentState::Verified);
    assert_eq!(cached.last_observed_at, DateTime::from_timestamp_millis(1700000000000));
    assert_eq!(cached.windows[0].observation_state, QuotaObservationState::StaleLastGood);
    assert_eq!(cached.windows[0].used_percent, Some(40.0));
}

#[test]
fn antigravity_merges_summary_and_filters_only_untouched_resets() {
    let reset = now() + TimeDelta::hours(5) + TimeDelta::seconds(60);
    let windows = parse_antigravity_summary(
        &json!({
            "groups": [{"displayName": "Argon preview", "buckets": [
                {"bucketId": "gemini-argon-5h", "window": "5h", "remainingFraction": 1, "resetTime": reset},
                {"bucketId": "gemini-5h", "window": "5h", "remainingFraction": 1, "description": "active", "resetTime": reset},
                {"bucketId": "hide", "disabled": true, "remainingFraction": 0}
            ]}],
            "buckets": [{"bucketId": "3p-weekly", "window": "weekly", "remainingFraction": null}]
        }),
        now(),
    );
    let get = |key| windows.iter().find(|w| w.window_key == key).unwrap();
    assert_eq!(windows.len(), 3);
    assert_eq!(get("gemini-argon-5h").scope_model.as_deref(), Some("Argon preview"));
    assert_eq!(get("gemini-argon-5h").used_percent, Some(0.0));
    assert_eq!(get("gemini-argon-5h").resets_at, None);
    assert_eq!(get("gemini-5h").resets_at, Some(reset));
    assert_eq!(get("3p-weekly").scope_model.as_deref(), Some("Claude & GPT (shared)"));
    assert_eq!(get("3p-weekly").used_percent, None);
}

#[test]
fn antigravity_merges_quota_hosts_by_worst_reading_per_bucket() {
    let resets = now() + TimeDelta::hours(2);
    let summary = |session, weekly| {
        parse_antigravity_summary(
            &json!({"buckets":[
                {"bucketId":"gemini-5h","window":"5h","remainingFraction":session,"resetTime":resets},
                {"bucketId":"gemini-weekly","window":"weekly","remainingFraction":weekly,"resetTime":resets},
            ]}),
            now(),
        )
    };
    let daily = summary(0.0, 0.75);
    let standard = summary(0.5, 0.25);
    let windows = merge_antigravity_windows(daily.into_iter().chain(standard));
    assert_eq!(
        windows.iter().map(|w| (w.window_key.as_str(), w.used_percent)).collect::<Vec<_>>(),
        [("gemini-5h", Some(100.0)), ("gemini-weekly", Some(75.0))]
    );
}

#[test]
fn antigravity_model_fallback_keeps_worst_family_reading_without_guessing_duration() {
    let windows = parse_antigravity_models_fallback(
        &json!({"models": {
            "gpt-a": {"quotaInfo": {"remainingFraction": 0.5}},
            "claude-b": {"quotaInfo": {"remainingFraction": 0.25}, "weeklyQuotaInfo": {"remainingFraction": 0.75}},
            "gpt-c": {"quotaInfo": {"remainingFraction": null}},
            "unknown": {"quotaInfo": {"remainingFraction": 0}}
        }}),
        now(),
    );
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].window_key, "3p-quota");
    assert_eq!(windows[1].window_key, "3p-weekly");
    assert!(windows.iter().all(|w| w.scope_model.as_deref() == Some("Claude & GPT (shared)")));
    assert_eq!(windows[0].used_percent, Some(75.0));
    assert_eq!(windows[0].window_duration_mins, None);
    assert_eq!(windows[0].cadence, ResetCadence::Unknown);
    assert_eq!(windows[1].kind, QuotaWindowKind::Weekly);
}

#[test]
fn antigravity_model_fallback_requires_corroboration_for_omitted_zero() {
    let future = now() + TimeDelta::hours(2);
    let mut raw = json!({"models": {
        "gemini-4-neon-high": {"quotaInfo": {"resetTime": future}},
        "gemini-4-neon-low": {"quotaInfo": {"resetTime": future, "remainingFraction": 0.25}},
        "gemini-4-argon-high": {"quotaInfo": {"remainingFraction": 0.995}, "weeklyQuotaInfo": {"resetTime": future}},
        "gemini-4-argon-low": {"quotaInfo": {"resetTime": future, "remainingFraction": null}},
        "gpt-old": {"quotaInfo": {"resetTime": now()}}
    }});
    let windows = parse_antigravity_models_fallback(&raw, now());
    let get = |key| windows.iter().find(|w| w.window_key == key).unwrap();
    let neon = get("gemini-quota");
    assert_eq!(neon.used_percent, Some(100.0));
    assert_eq!(neon.status, QuotaStatus::Rejected);
    assert_eq!(neon.observation_state, QuotaObservationState::Measured);
    assert_eq!(neon.resets_at, Some(future));
    let argon = get("gemini-argon-quota");
    assert_eq!(argon.scope_model.as_deref(), Some("Gemini 4 Argon"));
    assert!((argon.remaining_percent.unwrap() - 99.5).abs() < 1e-9);
    assert_eq!(get("gemini-argon-weekly").status, QuotaStatus::Rejected);
    assert_eq!(get("3p-quota").used_percent, None);

    raw["models"]["gemini-4-neon-low"]["quotaInfo"]["remainingFraction"] = Value::Null;
    raw["models"]["gemini-4-argon-high"]["quotaInfo"]["remainingFraction"] = Value::Null;
    assert!(parse_antigravity_models_fallback(&raw, now()).iter().all(|w| w.used_percent.is_none()));
}

#[test]
fn antigravity_fallback_classifies_explicit_durations() {
    for (minutes, kind) in
        [(300, QuotaWindowKind::Session), (1440, QuotaWindowKind::Daily), (10080, QuotaWindowKind::Weekly), (1234, QuotaWindowKind::Custom)]
    {
        let windows = parse_antigravity_models_fallback(
            &json!({"models":{"gemini-4":{"quotaInfo":{"remainingFraction":0.5,"windowDurationMins":minutes}}}}),
            now(),
        );
        assert_eq!(windows[0].window_duration_mins, Some(minutes));
        assert_eq!(windows[0].kind, kind);
    }
}

#[test]
fn wham_separates_credits_from_reset_grants() {
    let (plan, windows, grant) = parse_chatgpt_wham_usage(
        &json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": null, "limit_window_seconds": 18000, "reset_at": 1700000000},
                "secondary_window": {"used_percent": 0.75, "limit_window_seconds": 604800}
            },
            "credits": {"balance": 750, "unlimited": false},
            "reset_credits": {"available": 2, "grants": [
                {"id": "next", "available": 1, "applicable": 1, "clears": ["primary_window"], "gives_back_weekly": true}
            ]}
        }),
        now(),
    );
    assert_eq!(plan.as_deref(), Some("pro"));
    assert_eq!(windows[0].used_percent, None);
    assert_eq!(windows[0].resets_at, DateTime::from_timestamp(1700000000, 0));
    assert_eq!(windows[1].used_percent, Some(75.0));
    assert_eq!(windows[2].unit, QuotaUnit::Credits);
    assert_eq!(windows[2].used_amount, None);
    assert_eq!(windows[2].label, "Credits · 7.50 remaining");
    let grant = grant.unwrap();
    assert_eq!(grant.grant_id, "next");
    assert!(grant.applicable && grant.weekly_credit_back);
    assert_eq!(grant.cleared_windows, ["primary_window"]);
    assert!(parse_chatgpt_wham_usage(&json!({"credits": {"unlimited": true}}), now()).2.is_none());
}

#[test]
fn wham_rejection_preserves_fractional_precision_and_missing_readings() {
    for flag in [json!({"allowed": false}), json!({"limit_reached": true})] {
        for (used, expected) in [(json!(1.0), Some(100.0)), (Value::Null, None)] {
            let mut raw = flag.clone();
            raw["primary_window"] = json!({"used_percent": used});
            raw["secondary_window"] = json!({"used_percent": 0.999});
            let (_, windows, _) = parse_chatgpt_wham_usage(&json!({"rate_limit": raw}), now());
            assert_eq!(windows[0].used_percent, expected);
            assert_eq!(windows[0].status, QuotaStatus::Rejected);
            assert_eq!(windows[0].observation_state, QuotaObservationState::Measured);
            assert!(windows[1].used_percent.unwrap() < 100.0);
            assert_ne!(windows[1].status, QuotaStatus::Rejected);
        }
    }
    let (_, windows, _) = parse_chatgpt_wham_usage(&json!({"rate_limit":{"primary_window":{"used_percent":1.0}}}), now());
    assert_eq!(windows[0].used_percent, Some(1.0));
}

#[test]
fn wham_scales_the_entire_response_and_enforces_denials_with_expired_windows() {
    let (_, windows, _) = parse_chatgpt_wham_usage(
        &json!({"rate_limit":{"primary_window":{"used_percent":1},"secondary_window":{"used_percent":35}}}),
        now(),
    );
    assert_eq!(windows.iter().map(|w| w.used_percent).collect::<Vec<_>>(), [Some(1.0), Some(35.0)]);

    for flag in [json!({"allowed":false}), json!({"limit_reached":true})] {
        for reset in [now(), now() + TimeDelta::hours(1)] {
            let mut raw = flag.clone();
            raw["primary_window"] = json!({"used_percent":100,"reset_at":now() - TimeDelta::seconds(1)});
            raw["secondary_window"] = json!({"used_percent":35,"reset_at":reset});
            let (_, windows, _) = parse_chatgpt_wham_usage(&json!({"rate_limit":raw}), now());
            assert!(windows.iter().any(|w| w.status == QuotaStatus::Rejected && w.resets_at.is_none_or(|t| t > now())));
            if reset > now() {
                assert!(windows.iter().any(|w| w.status == QuotaStatus::Rejected && w.resets_at == Some(reset)));
            }
        }
    }
}

#[test]
fn opencode_go_keeps_available_and_unavailable_usage_separate() {
    let windows = parse_opencode_go_usage(
        &json!({"usage": {
            "rolling": {"percent": 0, "resetsAt": "2026-11-21T17:00:00Z"},
            "weekly": {"percent": null, "resetsAt": "invalid"},
            "monthly": null
        }}),
        now(),
    );
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].used_percent, Some(0.0));
    assert_eq!(windows[0].window_duration_mins, Some(300));
    assert_eq!(windows[1].used_percent, None);
    assert_eq!(windows[1].resets_at, None);
    assert_eq!(windows[2].observation_state, QuotaObservationState::Unavailable);
}

fn intent(provider_id: &str) -> ProviderIntent {
    ProviderIntent {
        operation_id: Uuid::new_v4(),
        provider_id: provider_id.into(),
        actor: "test".into(),
        idempotency_key: Uuid::new_v4().to_string(),
        target_account_id: None,
        target_host: None,
        target_user: None,
        expected_revision: None,
    }
}

#[tokio::test]
async fn readonly_and_push_advertise_only_real_capabilities() {
    let readonly = ReadOnlyFixtureModelProvider::new("acme-readonly", "Acme", "acme");
    assert_eq!(readonly.descriptor().id, "acme-readonly");
    assert!(readonly.facets().capabilities().is_empty());
    assert!(readonly.facets().quota.is_none() && readonly.facets().accounts.is_none());
    let batch = readonly.collect().await.unwrap().unwrap();
    assert_eq!(batch.accounts.len(), 1);
    assert_eq!(
        batch.accounts[0].windows.iter().map(|w| w.observation_state).collect::<Vec<_>>(),
        [QuotaObservationState::Measured, QuotaObservationState::NeverObserved, QuotaObservationState::Unavailable]
    );

    let push = PushModelProvider::new(
        "session",
        "Session",
        "custom",
        [ProviderCapability::RefreshQuota].into(),
        [EvidenceKind::QuotaWindows].into(),
    );
    assert!(push.collect().await.unwrap().is_none());
    assert_eq!(push.facets().capabilities(), [ProviderCapability::RefreshQuota].into());
    assert!(push.facets().accounts.is_none());
    let quota = push.facets().quota.unwrap();
    assert!(matches!(
        quota.submit(&intent("session"), &QuotaCommand::RefreshGrants { account_id: None }).await,
        Err(PortError::Unsupported(_))
    ));
    assert_eq!(
        quota.submit(&intent("session"), &QuotaCommand::RefreshQuota { account_id: None }).await.unwrap(),
        ProviderDispatch::Accepted { receipt: Some(json!({"queued_for_collector": true})) }
    );
}

#[tokio::test]
async fn full_fixture_applies_mutations_and_reconciles_without_spending_again() {
    let fixture = FullFixtureModelProvider::new("demo", "Demo", "custom");
    let batch = fixture.collect().await.unwrap().unwrap();
    let initial = &batch.accounts[0];
    let account_id = initial.id.clone();
    let grant_id = initial.reset_grant.as_ref().unwrap().grant_id.clone();
    let ops = fixture.facets();
    let accounts = ops.accounts.unwrap();
    for cmd in [
        AccountCommand::SetPriority { account_id: account_id.clone(), priority: -3 },
        AccountCommand::SetWeight { account_id: account_id.clone(), weight: 4 },
        AccountCommand::SetNote { account_id: account_id.clone(), note: Some("Focused account".into()) },
        AccountCommand::SetEnabled { account_id: account_id.clone(), enabled: false },
    ] {
        accounts.submit(&intent("demo"), &cmd).await.unwrap();
    }
    let op = intent("demo");
    let command = QuotaCommand::ClaimResetGrant { account_id, grant_id };
    let quota = ops.quota.unwrap();
    let receipt = quota.submit(&op, &command).await.unwrap();
    assert_eq!(quota.submit(&op, &command).await.unwrap(), receipt);
    assert!(matches!(quota.reconcile(&op).await.unwrap(), ProviderReconciliation::Applied { .. }));
    let batch = fixture.collect().await.unwrap().unwrap();
    let account = &batch.accounts[0];
    assert_eq!((account.enabled, account.priority, account.weight), (false, -3, 4));
    assert_eq!(account.note.as_deref(), Some("Focused account"));
    assert_eq!(account.reset_grant.as_ref().unwrap().available_count, initial.reset_grant.as_ref().unwrap().available_count - 1);
    let activity = fixture.seed_activity();
    assert!(activity.exchanges.iter().any(|e| e.attempts.iter().any(|a| a.moved_from_account_id.is_some())));
    assert!(activity.exchanges.iter().any(|e| e.parent_session_id.is_some() && e.thinking_tokens > 0));
}

#[tokio::test]
async fn fixture_routes_discovers_and_records_sanitized_probes() {
    let fixture = FullFixtureModelProvider::new("demo", "Demo", "claude");
    let early = fixture.collect().await.unwrap().unwrap();
    let id = early.accounts[1].id.clone();
    let model_id = early.models[0].id.clone();
    let facets = fixture.facets();
    let op = || intent("demo");
    facets
        .accounts
        .unwrap()
        .submit(&op(), &AccountCommand::SelectLocalAccount { account_id: id.clone(), host: Some("demo-host".into()), unix_user: None })
        .await
        .unwrap();
    let routing = facets.routing.unwrap();
    routing
        .submit(
            &op(),
            &RoutingCommand::SetRoutePolicy {
                policy: RoutingPolicy { strategy: RouteStrategy::WeightedRoundRobin, ..RoutingPolicy::default() },
            },
        )
        .await
        .unwrap();
    routing.submit(&op(), &RoutingCommand::SetPoolOverride { account_id: Some(id.clone()) }).await.unwrap();
    routing.submit(&op(), &RoutingCommand::ControlProxy { action: ProxyAction::Stop }).await.unwrap();
    let group_id = "demo:new".to_string();
    let groups = facets.key_groups.unwrap();
    groups
        .submit(
            &op(),
            &KeyGroupCommand::UpsertGroup {
                group_id: Some(group_id.clone()),
                family: "claude".into(),
                name: "Additional group".into(),
                base_url: None,
                enabled: false,
                request_identity: RequestIdentityMode::Api,
                configured_models: vec!["extra-model".into()],
                aliases: Vec::new(),
            },
        )
        .await
        .unwrap();
    const SECRET: &str = "secret-test-token";
    let key_receipt = groups
        .submit(&op(), &KeyGroupCommand::AddKey { group_id: group_id.clone(), label: "Extra key".into(), key_ref: SECRET.into() })
        .await
        .unwrap();
    assert!(!serde_json::to_string(&key_receipt).unwrap().contains(SECRET));
    facets.models.unwrap().submit(&op(), &ModelCommand::DiscoverModels { group_id: Some(group_id.clone()) }).await.unwrap();
    let probes = facets.probes.unwrap();
    for (probe, field) in [
        (ProbeCommand::RunCanary { model_id: model_id.clone(), effort: None, account_id: Some(id.clone()) }, "prompts_passed"),
        (
            ProbeCommand::RunThinkingTest {
                model_id,
                effort: "high".into(),
                compare_model_id: None,
                compare_effort: Some("low".into()),
                sample_count: 3,
            },
            "mean_thinking_tokens",
        ),
        (
            ProbeCommand::ReplayTextEval {
                case_id: Uuid::new_v4(),
                account_id: Some(id.clone()),
                model_override: None,
                effort_override: None,
            },
            "outcome",
        ),
    ] {
        let op = op();
        let ProviderDispatch::Completed { receipt } = probes.submit(&op, &probe).await.unwrap() else { panic!("expected completed probe") };
        assert_eq!(receipt["fixture"], true);
        assert!(!receipt[field].is_null());
        assert_eq!(probes.reconcile(&op).await.unwrap(), ProviderReconciliation::Applied { receipt });
    }
    let later = fixture.collect().await.unwrap().unwrap();
    assert!(later.accounts[1].is_active && later.accounts[1].is_preferred);
    assert_eq!(later.accounts[1].host.as_deref(), Some("demo-host"));
    assert_eq!(later.accounts[1].success_count, early.accounts[1].success_count + 2);
    let view = later.routing_status.unwrap();
    assert_eq!(view.revision, 3);
    assert_eq!(view.policy.strategy, RouteStrategy::WeightedRoundRobin);
    assert_eq!(view.policy.preferred_account_id, Some(id));
    assert!(!view.runtime.unwrap().running);
    let group = later.key_groups.iter().find(|g| g.id == group_id).unwrap();
    assert!(!group.enabled);
    assert!(group.discovered_models.iter().any(|model| model.id == "extra-model"));
    assert!(!serde_json::to_string(group).unwrap().contains(SECRET));
    assert!(view.recent_logs.iter().any(|entry| entry.message == "Fixture thinking test completed"));
}

async fn serve(responses: impl Fn(&str, Value) -> (u16, String, Value) + Send + Sync + 'static) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut first = String::new();
            stream.read_line(&mut first).await.unwrap();
            let mut headers = String::new();
            let content_length = loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line.is_empty() {
                    break headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or_default();
                }
                headers.push_str(&line);
            };
            let mut body = vec![0; content_length];
            stream.read_exact(&mut body).await.unwrap();
            let body = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() };
            let (status, extra, reply) = responses(&format!("{first}{headers}"), body);
            let reply = reply.to_string();
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{reply}",
                reply.len()
            );
            stream.get_mut().write_all(response.as_bytes()).await.unwrap();
            stream.get_mut().shutdown().await.unwrap();
        }
    });
    (base, task)
}

#[tokio::test]
async fn proxy_inventory_only_preserves_tier_multipliers() {
    let path = std::env::temp_dir().join(format!("hub-provider-test-{}", Uuid::new_v4()));
    tokio::fs::write(&path, "fake-management-token-for-loopback").await.unwrap();
    let (base, task) = serve(|request, _| match request.lines().next().unwrap() {
        "GET /v0/management/auth-files HTTP/1.1" | "GET /v8/management/credentials HTTP/1.1" => (
            200,
            String::new(),
            json!({
                "files": [
                    {"id": "nested.json", "provider": "claude", "plan": {"multiplier": 20.0}},
                    {"id": "flat.json", "provider": "claude", "plan": {"name": null}, "tier_multiplier": 5.0}
                ]
            }),
        ),
        _ => (404, String::new(), Value::Null),
    })
    .await;
    let mut batches = Vec::new();
    for version in ["v0", "v8"] {
        let provider =
            SpillwayProxyProvider::new("my-proxy", "Spillway", "claude", "router-1", &format!("{base}/{version}/management"), &path)
                .unwrap();
        batches.push(provider.collect().await);
    }
    task.abort();
    tokio::fs::remove_file(path).await.unwrap();
    for batch in batches {
        let batch = batch.unwrap().unwrap();
        assert_eq!(batch.accounts.len(), 2);
        for (id, multiplier) in [("my-proxy:nested.json", 20.0), ("my-proxy:flat.json", 5.0)] {
            let account = batch.accounts.iter().find(|account| account.id == id).unwrap();
            assert_eq!(account.plan, None);
            assert_eq!(account.tier_multiplier, Some(multiplier));
        }
    }
}

#[tokio::test]
async fn proxy_collects_typed_observations_and_dispatches_against_auth_index() {
    const MANAGEMENT_TOKEN: &str = "fake-management-token-for-loopback";
    const API_KEY: &str = "fake-private-upstream-api-key-1234";
    let path = std::env::temp_dir().join(format!("hub-provider-test-{}", Uuid::new_v4()));
    tokio::fs::write(&path, MANAGEMENT_TOKEN).await.unwrap();
    let (base, task) = serve(|request, payload| {
        assert!(request.contains(&format!("authorization: Bearer {MANAGEMENT_TOKEN}")));
        let first = request.lines().next().unwrap();
        let route = first.strip_suffix(" HTTP/1.1").unwrap();
        let body = match route {
            "GET /v8/management/credentials" => json!({"files": [
                {"id": "a.json", "name": "name with space.json", "provider": "claude"},
                {"id": "b.json", "name": "b.json", "provider": "codex"}
            ]}),
            "GET /v8/management/usage/windows" => json!({"credentials": [
                {"id": "a.json", "auth_index": "proxy-index", "provider": "claude", "label": "Work", "preferred": true,
                 "plan": {"multiplier": 20.0},
                 "observed_at": "2026-11-21T12:00:00Z", "windows": [
                    {"key": "five_hour", "used_percent": 0, "duration_seconds": 18000, "reset_cadence": "on_use"},
                    {"key": "weekly", "used_percent": null, "duration_seconds": 604800}
                 ],
                 "reset_credits": {"available": 1, "grants": [{"id": "grant-one", "available": 1, "applicable": 1, "clears": ["five_hour"]}]}},
                {"id": "c.json", "provider": "claude", "tier_multiplier": 5.0},
            ]}),
            "GET /v8/management/config/routing/strategy" => return (200, "X-CPA-Version: v8.2.3\r\n".into(), json!("capacity_balanced")),
            "GET /v8/management/config" => return (200, "ETag: \"revision-1\"\r\n".into(), json!({
                "routing": {"session-affinity": false, "prefer-auth-kind": "api-key"},
                "api-keys": {"claude": [{"name": "Work keys", "keys": [{"api-key": API_KEY}], "models": [{"name": "claude-sonnet", "alias": "sonnet"}]}]}
            })),
            "GET /v8/management/observability/logs?limit=100" => json!({"lines": [format!("[2026-11-21 12:00:00] [request] [warn] Authorization: Bearer {API_KEY}")]}),
            "PUT /v8/management/routing/preferred-auth" => {
                assert_eq!(payload, json!({"provider": "claude", "auth_index": "proxy-index"}));
                json!({"preferred": {"claude": "proxy-index"}})
            }
            "POST /v8/management/routing/move-session" => {
                assert_eq!(payload, json!({"session_id": "claude:thread/1", "auth_index": "proxy-index"}));
                json!({"bound": 2})
            }
            "POST /v8/management/usage/reset-credit" => {
                assert_eq!(payload, json!({"auth_index": "proxy-index", "grant_id": "grant-one"}));
                json!({"code": "reset", "windows_reset": 1, "refresh_error": API_KEY})
            }
            "PATCH /v8/management/credentials/status" => {
                assert_eq!(payload, json!({"name": "name with space.json", "disabled": true}));
                return (403, String::new(), json!({"error": format!("{MANAGEMENT_TOKEN} {API_KEY}")}));
            }
            "PATCH /v8/management/credentials/fields" => {
                assert_eq!(payload, json!({"name":"name with space.json","plan":"Max","tier_multiplier":20.0}));
                json!({"status": "ok"})
            }
            "PATCH /v8/management/config/routing" => {
                assert!(request.contains("if-match: \"revision-1\""));
                assert_eq!(payload["strategy"], "capacity-balanced");
                json!({"status": "ok"})
            }
            "PUT /v8/management/config/api-keys/claude" => {
                assert!(request.contains("if-match: \"revision-1\""));
                assert_eq!(payload[0]["keys"][0]["api-key"], API_KEY);
                assert_eq!(payload[0]["keys"][0]["disabled"], true);
                json!({"status": "ok"})
            }
            route => panic!("unexpected route {route}"),
        };
        (200, String::new(), body)
    }).await;
    let provider = SpillwayProxyProvider::new("my-proxy", "Spillway", "claude", "router-1", &base, &path).unwrap();
    let result = async {
        let batch = provider.collect().await.unwrap().unwrap();
        assert_eq!(batch.accounts.len(), 2);
        let account = batch.accounts.iter().find(|a| a.id == "my-proxy:a.json").unwrap();
        assert_eq!(account.plan, None);
        assert_eq!(account.tier_multiplier, Some(20.0));
        let fallback = batch.accounts.iter().find(|a| a.id == "my-proxy:c.json").unwrap();
        assert_eq!(fallback.plan, None);
        assert_eq!(fallback.tier_multiplier, Some(5.0));
        assert_eq!(account.windows[0].used_percent, Some(0.0));
        assert_eq!(account.windows[1].used_percent, None);
        assert_eq!(batch.reset_grants[0].1.grant_id, "grant-one");
        let view = batch.routing_status.as_ref().unwrap();
        assert_eq!(view.policy.strategy, RouteStrategy::CapacityBalanced);
        assert_eq!(view.runtime.as_ref().unwrap().build_version.as_deref(), Some("v8.2.3"));
        assert_eq!(view.recent_logs[0].level, "warn");
        let text = serde_json::to_string(&batch).unwrap();
        assert!(!text.contains(API_KEY) && !text.contains(MANAGEMENT_TOKEN));
        let facets = provider.facets();
        let routing = facets.routing.unwrap();
        let op = intent("my-proxy");
        routing.submit(&op, &RoutingCommand::SetRoutePolicy { policy: RoutingPolicy::default() }).await.unwrap();
        assert!(account.capabilities.contains(&ProviderCapability::SetCapacity));
        facets
            .accounts
            .as_ref()
            .unwrap()
            .submit(
                &op,
                &AccountCommand::SetCapacity { account_id: account.id.clone(), plan: Some("Max".into()), tier_multiplier: Some(20.0) },
            )
            .await
            .unwrap();
        routing.submit(&op, &RoutingCommand::SetPoolOverride { account_id: Some(account.id.clone()) }).await.unwrap();
        routing
            .submit(
                &op,
                &RoutingCommand::MoveConversation {
                    session_id: "claude:thread/1".into(),
                    target_account_id: account.id.clone(),
                    include_subagents: true,
                },
            )
            .await
            .unwrap();
        let grant = facets
            .quota
            .unwrap()
            .submit(&op, &QuotaCommand::ClaimResetGrant { account_id: account.id.clone(), grant_id: "grant-one".into() })
            .await
            .unwrap();
        assert_eq!(
            grant,
            ProviderDispatch::Completed { receipt: json!({"reset_applied": true, "windows_reset": 1, "refresh_failed": true}) }
        );
        let group = &batch.key_groups[0];
        facets
            .key_groups
            .unwrap()
            .submit(&op, &KeyGroupCommand::ToggleKey { group_id: group.id.clone(), key_id: group.keys[0].id.clone(), enabled: false })
            .await
            .unwrap();
        facets
            .accounts
            .unwrap()
            .submit(&op, &AccountCommand::SetEnabled { account_id: account.id.clone(), enabled: false })
            .await
            .unwrap_err()
    }
    .await;
    task.abort();
    tokio::fs::remove_file(path).await.unwrap();
    assert!(matches!(&result, PortError::Rejected { status: 403, .. }));
    assert!(!result.to_string().contains(MANAGEMENT_TOKEN) && !result.to_string().contains(API_KEY));
}

#[tokio::test]
async fn meridian_probe_does_not_forward_credentials_on_redirect() {
    let (base, task) = serve(|request, _| {
        assert!(request.starts_with("GET /v1/usage/quota/all "));
        assert!(request.contains("x-api-key: fake-meridian-key"));
        (307, "Location: /unexpected\r\n".into(), json!({"error": "fake-meridian-key"}))
    })
    .await;
    let result = hub_integrations::providers::claude::probe_meridian_quota(&http_client().unwrap(), &base, Some("fake-meridian-key"))
        .await
        .unwrap_err();
    assert!(result.contains("307"));
    assert!(!result.contains("fake-meridian-key"));
    task.abort();
}
