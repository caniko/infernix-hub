use super::*;
use crate::hash::{canonical_json, sha256_hex};
use serde::Serialize;

#[derive(Serialize, Deserialize)]
struct Delivery {
    delivery_id: Uuid,
    receipts: Vec<IngestReceipt>,
}

impl Delivery {
    async fn response(self, conn: &mut PgConnection, hub: &Hub, viewer: &Viewer<'_>) -> DResult<SignalBatchOut> {
        let Self { delivery_id, receipts } = self;
        if viewer.can_review() {
            let mut ids: Vec<_> = receipts.iter().map(|receipt| receipt.id).collect();
            ids.sort_unstable();
            ids.dedup();
            let unique_count = ids.len();
            let mut sql = visible_sql(hub, viewer, true);
            let visible = sql.push(" and s.id = any(").push_bind(ids).push(")").build_query_scalar::<Uuid>().fetch_all(conn).await?;
            if visible.len() == unique_count {
                return Ok(SignalBatchOut::Receipts { delivery_id, receipts });
            }
        }
        Ok(SignalBatchOut::Accepted { delivery_id, accepted_count: receipts.len() })
    }
}

pub async fn ingest_batch(hub: &Hub, actor: &Actor, input: SignalBatchIn, key: Option<&str>) -> DResult<SignalBatchOut> {
    ingest_delivery(hub, actor, input, key, None).await
}

const PROMOTION_EXTERNAL_ID_PREFIX: &str = "redacted-mail:";

pub async fn promote_from_mail(hub: &Hub, actor: &Actor, mut input: PromoteFromMailIn) -> DResult<SignalBatchOut> {
    actor.require(&[Role::Operator, Role::Member])?;
    let hyphenated = input.mail_message_id.to_string();
    let compact = input.mail_message_id.simple().to_string();
    let mut values = vec![&input.facts, &input.evidence];
    let contains_id = |text: &str| {
        let text = text.chars().filter(|c| !c.is_control()).collect::<String>().to_ascii_lowercase();
        text.contains(&hyphenated) || text.contains(&compact)
    };
    const RAW_MAIL_KEYS: &[&str] = &[
        "body_text",
        "body_html",
        "raw_path",
        "message_id_header",
        "headers",
        "raw_body",
        "raw_headers",
        "eml_path",
        "mail_message_id",
        "raw_eml",
        "rfc822",
        "attachments",
    ];
    let forbidden = || invalid("invalid_promotion_payload", "promotions must not contain raw mail fields or the private message ID");
    while let Some(value) = values.pop() {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if RAW_MAIL_KEYS.iter().any(|raw| key.eq_ignore_ascii_case(raw)) || contains_id(key) {
                        return Err(forbidden());
                    }
                    values.push(value);
                }
            }
            Value::Array(items) => values.extend(items),
            Value::String(text) if contains_id(text) => return Err(forbidden()),
            _ => (),
        }
    }
    if contains_id(&input.title) || contains_id(&input.summary) {
        return Err(forbidden());
    }
    let evidence =
        serde_json::from_value(input.evidence.clone()).map_err(|_| invalid("invalid_promotion_payload", "invalid promotion evidence"))?;
    // Ignore legacy caller-supplied IDs, including when hashing an intent for replay.
    input.external_id = None;
    let now = Utc::now();
    let run_id = format!(
        "{PROMOTION_EXTERNAL_ID_PREFIX}{}",
        sha256_hex(canonical_json(&json!([actor.principal.id, input.idempotency_key])).as_bytes())
    );
    let batch = SignalBatchIn {
        source: input.source_id.clone(),
        run: SourceRunIn {
            run_id: run_id.clone(),
            status: IntelRunStatus::Ok,
            finished_at: now,
            watermark: json!({}),
            error_code: None,
            error_detail: None,
        },
        signals: vec![SignalIn {
            external_id: run_id.clone(),
            kind: input.kind,
            severity: input.severity,
            title: input.title.clone(),
            summary: input.summary.clone(),
            occurred_at: input.occurred_at.unwrap_or(now),
            expires_at: None,
            audience: None,
            confidence: 1.0,
            evidence,
            facts: input.facts.clone(),
            subjects: Vec::new(),
            links: input.links.clone(),
            persona_id: None,
            project_id: input.project_id,
            backfill: false,
            provenance: SignalProvenance {
                producer: "redacted_mail".into(),
                run_id,
                git_rev: None,
                model: None,
                method: ProvenanceMethod::Observed,
                promoted_from: Some("redacted_mail".into()),
                promoted_by: Some(actor.name()),
            },
        }],
    };
    ingest_delivery(hub, actor, batch, Some(&input.idempotency_key), Some(&input)).await
}

async fn ingest_delivery(
    hub: &Hub,
    actor: &Actor,
    mut input: SignalBatchIn,
    key: Option<&str>,
    promotion: Option<&PromoteFromMailIn>,
) -> DResult<SignalBatchOut> {
    let mut tx = hub.db.begin().await?;
    if let Some(promotion) = promotion {
        crate::mail::lock_mail_message_owner(&mut tx, &hub.resolve_owner(actor)?, promotion.mail_message_id, true).await?;
        if promotion.links.iter().any(|link| link.target_type == LinkTargetType::MailMessage) {
            return Err(invalid("invalid_promotion_link", "a redacted promotion cannot link to a mail message"));
        }
    }
    let config = &hub.config.intel;
    let viewer = Viewer::new(hub, actor)?;
    if matches!(&viewer, Viewer::Granted(g, _) if !g.write_sources.contains(&input.source)) {
        return Err(DomainError::Denied { code: "intel_write_denied", message: "caller may not write to this source".into() });
    }
    let policy = config.sources.get(&input.source).ok_or(DomainError::NotFound("intel source"))?;
    let company = &policy.company_id;
    if !viewer.in_company(company) {
        return Err(DomainError::Denied { code: "intel_write_denied", message: "caller may not write to this source".into() });
    }
    if key.is_some_and(|key| key.is_empty() || key.len() > 128 || !key.bytes().all(|c| c.is_ascii_graphic())) {
        return Err(invalid("invalid_idempotency_key", "idempotency key must be 1 to 128 visible ASCII characters"));
    }
    let operation = if promotion.is_some() { "promotion" } else { "ingest" };
    let promotion_key = promotion.map(|p| format!("promo:{}:{}", actor.principal.id, p.idempotency_key));
    let key = promotion_key.as_deref().or(key);
    if input.signals.len() > 100 {
        return Err(invalid("invalid_batch", "an intel batch may contain at most 100 signals"));
    }
    if promotion.is_none() {
        // Promotion provenance is assigned by the server, never by a producer.
        for item in &mut input.signals {
            item.provenance.promoted_from = None;
            item.provenance.promoted_by = None;
        }
    }
    let request_hash = key.map(|_| {
        let payload = match promotion {
            Some(p) => json!({"v":"intel-mail-promotion-v1", "actor":actor.principal.id, "promotion":p}),
            None => json!({"v":"intel-delivery-v1", "batch":input}),
        };
        sha256_hex(canonical_json(&payload).as_bytes())
    });

    // Serialize all deliveries for a source, including its first delivery,
    // without writing anything before authorization and the replay lookup.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('hub.intel.source:' || $1, 0))")
        .bind(&input.source)
        .execute(&mut *tx)
        .await?;
    let bound: Option<String> =
        sqlx::query_scalar("select company_id from intel_sources where id = $1").bind(&input.source).fetch_optional(&mut *tx).await?;
    if bound.as_ref().is_some_and(|bound| bound != company) {
        return Err(conflict("source_company_mismatch", "source is bound to a different company"));
    }
    let mut checked = Vec::with_capacity(input.signals.len());
    for mut item in input.signals {
        clean_item(&mut item, policy)?;
        let item = check_item(&mut tx, hub, actor, &viewer, company, item, promotion.is_some()).await?;
        if promotion.is_some() && item.owner.is_some() {
            return Err(invalid("invalid_promotion_link", "a redacted promotion cannot link to an owner-scoped target"));
        }
        checked.push(item);
    }
    if let Some((key, hash)) = key.zip(request_hash.as_ref()) {
        let previous: Option<(String, Value)> = sqlx::query_as(
            "select request_sha256,receipt_json from intel_deliveries
              where company_id = $1 and source_id = $2 and idempotency_key = $3 and operation = $4",
        )
        .bind(company)
        .bind(&input.source)
        .bind(key)
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((stored_hash, receipt)) = previous {
            if stored_hash != *hash {
                return Err(conflict("idempotency_conflict", "idempotency key was used for a different batch"));
            }
            let response = decode_one::<Delivery>(receipt)?.response(&mut tx, hub, &viewer).await?;
            tx.commit().await?;
            return Ok(response);
        }
    }

    // Count accepted input rows, including identical sends without a key, but
    // not idempotent replays. The audit and quota ledger are the same write.
    let used: i64 = sqlx::query_scalar(
        "select coalesce(sum((detail->>'accepted_count')::int),0) from audit_log
          where subject_type = 'intel_source' and subject_id = $1 and action = 'intel.ingest'
             and at >= date_trunc('day', now(), 'UTC') and at < date_trunc('day', now(), 'UTC') + interval '24 hours'",
    )
    .bind(&input.source)
    .fetch_one(&mut *tx)
    .await?;
    if used + checked.len() as i64 > i64::from(policy.max_signals_per_day) {
        return Err(DomainError::RateLimited { code: "source_budget_exceeded", message: "daily intel source quota exceeded".into() });
    }

    let run = &mut input.run;
    run.run_id = bounded(&run.run_id, 256, false, "invalid_run")?;
    if run
        .error_code
        .as_ref()
        .is_some_and(|code| code.is_empty() || code.len() > 128 || !code.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)))
    {
        return Err(invalid("invalid_run", "intel error code must be a short identifier"));
    }
    run.error_detail = run.error_detail.as_deref().map(|s| bounded(s, 4000, true, "invalid_run")).transpose()?;
    clean_json(&mut run.watermark, "invalid_watermark")?;
    if bound.is_none() {
        sqlx::query("insert into intel_sources (id,company_id,policy_version) values ($1,$2,$3)")
            .bind(&input.source)
            .bind(company)
            .bind(policy.policy_version)
            .execute(&mut *tx)
            .await?;
    }
    let mut receipts = Vec::with_capacity(checked.len());
    let mut last_event = None;
    for checked in checked {
        last_event = last_event.max(Some(checked.item.occurred_at));
        receipts.push(upsert_signal(&mut tx, actor, &input.source, policy, checked).await?);
    }
    if promotion.is_none() {
        sqlx::query(
            "update intel_sources set policy_version = $2, last_ingest_at = now(), last_event_at = greatest(last_event_at,$3),
           last_status = $4, watermark = $5, last_error_code = $6, last_error_detail = $7, updated_at = now() where id = $1",
        )
        .bind(&input.source)
        .bind(policy.policy_version)
        .bind(last_event)
        .bind(run.status.as_str())
        .bind(&run.watermark)
        .bind(&run.error_code)
        .bind(&run.error_detail)
        .execute(&mut *tx)
        .await?;
    }
    let delivery_id = Uuid::new_v4();
    audit(
        &mut tx,
        actor,
        "intel.ingest",
        "intel_source",
        &input.source,
        json!({
            "delivery_id": delivery_id, "accepted_count": receipts.len(), "run_id": run.run_id,
            "status": run.status, "finished_at": run.finished_at,
        }),
    )
    .await?;
    let delivery = Delivery { delivery_id, receipts };
    if let Some((key, hash)) = key.zip(request_hash) {
        sqlx::query(
            "insert into intel_deliveries (company_id,source_id,idempotency_key,request_sha256,receipt_json,operation)
             values ($1,$2,$3,$4,$5,$6)",
        )
        .bind(company)
        .bind(input.source)
        .bind(key)
        .bind(hash)
        .bind(json!(delivery))
        .bind(operation)
        .execute(&mut *tx)
        .await?;
    }
    let response = delivery.response(&mut tx, hub, &viewer).await?;
    tx.commit().await?;
    Ok(response)
}

fn clean_json(value: &mut Value, code: &'static str) -> DResult<()> {
    if value.to_string().len() > 16 * 1024 {
        return Err(invalid(code, "intel JSON fields must be no larger than 16 KiB"));
    }
    fn strip(value: &mut Value, code: &'static str, depth: u8) -> DResult<()> {
        if depth > 32 {
            return Err(invalid(code, "intel JSON exceeds nesting limit"));
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key.chars().any(char::is_control) {
                        return Err(invalid(code, "intel JSON keys cannot contain control characters"));
                    }
                    strip(value, code, depth + 1)?;
                }
            }
            Value::Array(items) => {
                for item in items {
                    strip(item, code, depth + 1)?;
                }
            }
            Value::String(s) => *s = bounded(s, 16 * 1024, true, code)?,
            _ => (),
        }
        Ok(())
    }
    strip(value, code, 0)
}

fn clean_item(item: &mut SignalIn, policy: &IntelSourcePolicy) -> DResult<()> {
    if !policy.kinds.contains(&item.kind) {
        return Err(invalid("kind_not_allowed", "signal kind is not permitted by the source policy"));
    }
    if item.audience.is_some_and(|audience| audience.rank() < policy.default_audience.rank()) {
        return Err(invalid("audience_exceeds_ceiling", "a producer may only narrow the source audience"));
    }
    if !(0.0..=1.0).contains(&item.confidence)
        || item.evidence.is_empty()
        || item.evidence.len() > 16
        || item.subjects.len() > 32
        || item.links.len() > 32
    {
        return Err(invalid("invalid_signal", "intel signal confidence, evidence, subjects or links are out of bounds"));
    }
    // The public read goes through JSONB, which does not preserve negative zero.
    if item.confidence == 0.0 {
        item.confidence = 0.0;
    }
    item.external_id = bounded(&item.external_id, 256, false, "invalid_external_id")?;
    item.title = bounded(&item.title, 200, false, "invalid_title")?;
    item.summary = bounded(&item.summary, 4000, true, "invalid_summary")?;
    for e in &mut item.evidence {
        e.url = bounded(&e.url, 2048, false, "invalid_evidence")?;
        e.quote = bounded(&e.quote, 1000, true, "invalid_evidence")?;
    }
    for subject in &mut item.subjects {
        subject.value = bounded(&subject.value, 2048, false, "invalid_subject")?;
    }
    clean_json(&mut item.facts, "invalid_facts")?;
    let p = &mut item.provenance;
    p.producer = bounded(&p.producer, 256, false, "invalid_provenance")?;
    p.run_id = bounded(&p.run_id, 256, false, "invalid_provenance")?;
    p.git_rev = p.git_rev.as_deref().map(|s| bounded(s, 256, false, "invalid_provenance")).transpose()?;
    p.model = p.model.as_deref().map(|s| bounded(s, 256, false, "invalid_provenance")).transpose()?;
    if (matches!(policy.trust_tier, IntelTrustTier::Llm | IntelTrustTier::UntrustedWeb) || p.method == ProvenanceMethod::Llm)
        && item.severity == Severity::Critical
    {
        item.severity = Severity::Warning;
    }
    // Postgres stores microseconds; hash the same instants that readers see.
    item.occurred_at = DateTime::from_timestamp_micros(item.occurred_at.timestamp_micros())
        .ok_or_else(|| invalid("invalid_timestamp", "invalid occurred_at"))?;
    item.expires_at = item
        .expires_at
        .map(|t| DateTime::from_timestamp_micros(t.timestamp_micros()).ok_or_else(|| invalid("invalid_timestamp", "invalid expires_at")))
        .transpose()?;
    Ok(())
}

fn projection(item: &SignalIn) -> DResult<(Value, String)> {
    // Serialize the wire form so the digest uses the same rounding for f32 as the response.
    let mut payload = serde_json::to_vec(item)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes))
        .map_err(|error| DomainError::Internal(format!("serialize intel projection: {error}")))?;
    payload.retain(|field, _| PUBLIC_PROJECTION_FIELDS.contains(&field.as_str()));
    let payload = Value::Object(payload);
    let hash = sha256_hex(canonical_json(&json!({"v": "intel-projection-v2", "projection": payload})).as_bytes());
    Ok((payload, hash))
}

struct CheckedSignal {
    item: SignalIn,
    links: Vec<(SignalLinkIn, Option<String>)>,
    owner: Option<String>,
}

async fn check_item(
    conn: &mut PgConnection,
    hub: &Hub,
    actor: &Actor,
    viewer: &Viewer<'_>,
    company: &str,
    mut item: SignalIn,
    is_promotion: bool,
) -> DResult<CheckedSignal> {
    if !is_promotion
        && ["promo:", PROMOTION_EXTERNAL_ID_PREFIX]
            .iter()
            .any(|prefix| item.external_id.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix)))
    {
        return Err(invalid("reserved_intel_external_id", "signal external ID uses a reserved promotion prefix"));
    }
    // Hash the JSONB form that readers will see, including normalized numbers.
    let (associations, facts): (bool, Value) = sqlx::query_as(
        "select ($2::uuid is null or exists(select 1 from personas where company_id = $1 and id = $2))
             and ($3::uuid is null or exists(select 1 from projects where company_id = $1 and id = $3)), $4::jsonb",
    )
    .bind(company)
    .bind(item.persona_id)
    .bind(item.project_id)
    .bind(&item.facts)
    .fetch_one(&mut *conn)
    .await?;
    if !associations {
        return Err(invalid("invalid_association", "signal persona or project is not in the source company"));
    }
    item.facts = facts;
    let mut checked_links = Vec::with_capacity(item.links.len());
    let mut owner = None;
    for mut link in std::mem::take(&mut item.links) {
        let denied = || DomainError::NotFound("intel link target");
        link.target_id = Uuid::parse_str(&link.target_id).map_err(|_| denied())?.to_string();
        let target_owner = links::check(conn, hub, actor, viewer, company, &link).await?.ok_or_else(denied)?;
        if let Some(ref target) = target_owner {
            if owner.as_ref().is_some_and(|owner| owner != target) {
                return Err(DomainError::NotFound("intel link target"));
            }
            owner = Some(target.clone());
        }
        checked_links.push((link, target_owner));
    }
    Ok(CheckedSignal { item, links: checked_links, owner })
}

async fn upsert_signal(
    conn: &mut PgConnection,
    actor: &Actor,
    source: &str,
    policy: &IntelSourcePolicy,
    checked: CheckedSignal,
) -> DResult<IngestReceipt> {
    let CheckedSignal { item, links, owner } = checked;
    let company = &policy.company_id;
    let namespace = owner.as_deref().unwrap_or_default();
    let audience = if owner.is_some() { Audience::OperatorOnly } else { item.audience.unwrap_or(policy.default_audience) };
    let (mut payload, hash) = projection(&item)?;
    let old = sqlx::query(
        "select id,revision,projection_sha256,sticky_min_audience,dismissed_severity,state,persona_id,project_id
           from intel_signals where company_id = $1 and source_id = $2 and owner_namespace = $3 and external_id = $4 for update",
    )
    .bind(company)
    .bind(source)
    .bind(namespace)
    .bind(&item.external_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (id, revision, changed, sticky, reopened) = if let Some(ref old) = old {
        use sqlx::Row;
        let revision: i32 = old.try_get("revision")?;
        let sticky =
            Audience::parse(old.try_get("sticky_min_audience")?).ok_or(DomainError::Internal("invalid stored intel audience".into()))?;
        let changed = old.try_get::<String, _>("projection_sha256")? != hash
            || old.try_get::<Option<Uuid>, _>("persona_id")? != item.persona_id
            || old.try_get::<Option<Uuid>, _>("project_id")? != item.project_id
            || audience.rank() > sticky.rank();
        let dismissed = old.try_get::<Option<&str>, _>("dismissed_severity")?.and_then(Severity::parse);
        let reopened =
            old.try_get::<&str, _>("state")? == "dismissed" && dismissed.is_some_and(|s| severity_rank(item.severity) > severity_rank(s));
        (old.try_get("id")?, revision + i32::from(changed), changed, std::cmp::max_by_key(audience, sticky, Audience::rank), reopened)
    } else {
        (Uuid::new_v4(), 1, true, audience, false)
    };
    sqlx::query(
        "insert into intel_signals
           (id,company_id,source_id,owner_namespace,owner,external_id,kind,sticky_min_audience,severity,title,summary,facts,subjects,evidence,
             confidence,provenance,occurred_at,expires_at,revision,projection_sha256,persona_id,project_id)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22)
         on conflict (id) do update set
           kind=excluded.kind, sticky_min_audience=excluded.sticky_min_audience, severity=excluded.severity,
           title=excluded.title, summary=excluded.summary, facts=excluded.facts, subjects=excluded.subjects, evidence=excluded.evidence,
           confidence=excluded.confidence, provenance=excluded.provenance, occurred_at=excluded.occurred_at, expires_at=excluded.expires_at,
           revision=excluded.revision, projection_sha256=excluded.projection_sha256, persona_id=excluded.persona_id, project_id=excluded.project_id,
            cleared_audience=case when not $23 then intel_signals.cleared_audience end,
            cleared_base_audience=case when not $23 then intel_signals.cleared_base_audience end,
            cleared_source_default_audience=case when not $23 then intel_signals.cleared_source_default_audience end,
           cleared_revision=case when not $23 then intel_signals.cleared_revision end,
           cleared_policy_version=case when not $23 then intel_signals.cleared_policy_version end,
           cleared_sha256=case when not $23 then intel_signals.cleared_sha256 end,
           state=case when $24 then 'new' else intel_signals.state end,
           dismissed_severity=case when not $24 then intel_signals.dismissed_severity end,
           snoozed_until=case when not $24 then intel_signals.snoozed_until end, updated_at=now()",
    ).bind(id).bind(company).bind(source).bind(namespace).bind(&owner).bind(item.external_id).bind(item.kind.as_str())
        .bind(sticky.as_str()).bind(item.severity.as_str()).bind(item.title).bind(item.summary).bind(item.facts)
        .bind(json!(item.subjects)).bind(json!(item.evidence)).bind(item.confidence).bind(json!(item.provenance))
        .bind(item.occurred_at).bind(item.expires_at).bind(revision).bind(&hash).bind(item.persona_id).bind(item.project_id)
         .bind(changed).bind(reopened).execute(&mut *conn).await?;
    if changed {
        payload["persona_id"] = json!(item.persona_id);
        payload["project_id"] = json!(item.project_id);
        payload["sticky_min_audience"] = json!(sticky);
        sqlx::query("insert into intel_signal_revisions (signal_id,revision,projection_sha256,payload) values ($1,$2,$3,$4)")
            .bind(id)
            .bind(revision)
            .bind(hash)
            .bind(payload)
            .execute(&mut *conn)
            .await?;
    }
    if reopened {
        audit(conn, actor, "intel.escalation_reopened", "signal", id, json!({"severity": item.severity})).await?;
    }
    for (link, target_owner) in links {
        links::insert(conn, actor, id, link, target_owner).await?;
    }
    Ok(IngestReceipt { id, created: old.is_none(), revision })
}
