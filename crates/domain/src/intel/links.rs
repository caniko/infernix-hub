use super::*;

/// `None` means the endpoint is invisible or missing; a visible endpoint can
/// be ownerless or carry an owner that must also scope the origin signal.
pub(super) async fn check(
    conn: &mut PgConnection,
    hub: &Hub,
    actor: &Actor,
    viewer: &Viewer<'_>,
    company: &str,
    link: &SignalLinkIn,
) -> DResult<Option<Option<String>>> {
    let Ok(id) = Uuid::parse_str(&link.target_id) else {
        return Ok(None);
    };
    if link.target_type == LinkTargetType::MailMessage {
        let Viewer::Operator(Some(owner)) = viewer else { return Ok(None) };
        let found = sqlx::query_scalar("select owner from mail_messages where id = $1 and owner = $2")
            .bind(id)
            .bind(owner)
            .fetch_optional(conn)
            .await?;
        return Ok(found.map(Some));
    }
    if link.target_type == LinkTargetType::Signal {
        let Some(target) = visible_signal(conn, hub, viewer, id).await? else { return Ok(None) };
        if target.company_id != company && target.cleared_audience != Some(Audience::PublicCleared) {
            return Ok(None);
        }
        let owner: Option<String> = sqlx::query_scalar("select owner from intel_signals where id = $1").bind(id).fetch_one(conn).await?;
        return Ok(Some(owner));
    }
    if matches!(viewer, Viewer::Granted(g) if g.company_id != company) {
        return Ok(None);
    }
    // News is public and global, including for authorized ingest-only writers.
    if link.target_type == LinkTargetType::News {
        let found: bool = sqlx::query_scalar("select exists(select 1 from news_items where id = $1)").bind(id).fetch_one(conn).await?;
        return Ok(found.then_some(None));
    }
    if !crate::ANY_READER.contains(&actor.principal.role) {
        return Ok(None);
    }
    // Legacy readers are role-scoped; explicitly constrain their endpoints by
    // the signal tenant as well.
    let sql = match link.target_type {
        LinkTargetType::Opportunity => "select exists(select 1 from opportunities where id = $1 and company_id = $2)",
        LinkTargetType::Relationship => {
            "select exists(select 1 from relationships r join accounts a on a.id = r.account_id
             where r.id = $1 and a.company_id = $2)"
        }
        LinkTargetType::Content => {
            "select exists(select 1 from content_items c join personas p on p.id = c.persona_id
             where c.id = $1 and p.company_id = $2)"
        }
        LinkTargetType::Project => "select exists(select 1 from projects where id = $1 and company_id = $2)",
        LinkTargetType::Persona => "select exists(select 1 from personas where id = $1 and company_id = $2)",
        LinkTargetType::News | LinkTargetType::MailMessage | LinkTargetType::Signal => unreachable!(),
    };
    let found: bool = sqlx::query_scalar(sql).bind(id).bind(company).fetch_one(conn).await?;
    Ok(found.then_some(None))
}

pub(super) async fn insert(
    conn: &mut PgConnection,
    actor: &Actor,
    id: Uuid,
    link: SignalLinkIn,
    target_owner: Option<String>,
) -> DResult<()> {
    // The NULLS NOT DISTINCT key reserves a unique slot per logical key.
    // Phase 1 links have no promotion operation: use the canonical edge key.
    let key = json!(["edge", link.target_type, link.target_id, link.relation]).to_string();
    sqlx::query(
        "insert into intel_signal_links (signal_id,target_type,target_id,target_owner,relation,logical_promotion_key,created_by)
         values ($1,$2,$3,$4,$5,$6,$7) on conflict (signal_id,target_type,target_id,relation) do nothing",
    )
    .bind(id)
    .bind(link.target_type.as_str())
    .bind(link.target_id)
    .bind(target_owner)
    .bind(link.relation.as_str())
    .bind(key)
    .bind(actor.name())
    .execute(conn)
    .await?;
    Ok(())
}

pub(super) async fn read(
    conn: &mut PgConnection,
    hub: &Hub,
    actor: &Actor,
    viewer: &Viewer<'_>,
    signal: &IntelSignal,
) -> DResult<Vec<SignalLink>> {
    let rows: Vec<(Value, Option<String>)> = sqlx::query_as(
        "select to_jsonb(l) - array['signal_id','target_owner','logical_promotion_key'], target_owner
          from intel_signal_links l where signal_id = $1 order by created_at, target_type, target_id, relation",
    )
    .bind(signal.id)
    .fetch_all(&mut *conn)
    .await?;
    let mut visible = Vec::new();
    for (row, owner) in rows {
        let link: SignalLink = decode_one(row)?;
        let target = SignalLinkIn { target_type: link.target_type, target_id: link.target_id.clone(), relation: link.relation };
        if check(conn, hub, actor, viewer, &signal.company_id, &target).await?.is_some_and(|actual| actual == owner) {
            visible.push(link);
        }
    }
    Ok(visible)
}
