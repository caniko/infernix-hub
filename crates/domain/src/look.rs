//! Persona-owned look images and generation passes.

use contracts::{
    Asset, CharacterLook, GenerateIn, GenerationRequest, LookPass, LookPassIn, LookSize, LookSourceIn, PinIn, PinOut, Reference, Severity,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, conflict, decode, decode_one, hash, invalid, production};

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

async fn persona(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<()> {
    sqlx::query_scalar::<_, Uuid>("select id from personas where id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await?
        .ok_or(DomainError::NotFound("persona"))?;
    Ok(())
}

pub async fn upload(hub: &Hub, actor: &Actor, persona_id: Uuid, _mime: &str, attested: bool, bytes: &[u8]) -> DResult<Asset> {
    if !attested {
        return Err(invalid("attestation_required", "attestation is required"));
    }
    actor.require(OPERATOR)?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err(invalid("too_large", "image exceeds 32 MiB"));
    }
    let mime = image_mime(bytes).ok_or_else(|| invalid("not_an_image", "expected PNG, JPEG or WebP"))?;
    let mut tx = hub.db.begin().await?;
    persona(&mut tx, persona_id).await?;
    let sha = hash::sha256_hex(bytes);
    let key = hub.assets.put(&sha, bytes).await.map_err(|e| DomainError::Internal(format!("asset store: {e}")))?;
    let source_key = format!("upload:{persona_id}:{sha}");
    sqlx::query("insert into assets (id, persona_id, sha256, storage_key, mime, bytes, state, provenance, source_key) values ($1,$2,$3,$4,$5,$6,'ready',$7,$8) on conflict (source_key) do nothing")
        .bind(Uuid::new_v4()).bind(persona_id).bind(&sha).bind(key).bind(mime).bind(bytes.len() as i64)
        .bind(json!({"source":"operator_upload", "uploaded_by":actor.name(), "attested_not_real_person":true})).bind(&source_key).execute(&mut *tx).await?;
    sqlx::query("insert into image_checks (asset_id, check_key, verdict, checker) select id, 'not_real_person', 'pass', $2 from assets where source_key=$1 on conflict (asset_id, check_key, checker) do update set verdict='pass', detail='', checked_at=now()")
        .bind(&source_key).bind(actor.name()).execute(&mut *tx).await?;
    let row: Value =
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {} from assets where source_key=$1) t", production::ASSET_COLS))
            .bind(&source_key)
            .fetch_one(&mut *tx)
            .await?;
    let asset: Asset = decode_one(row)?;
    audit(&mut tx, actor, "look.uploaded", "persona", persona_id, json!({"asset_id": asset.id})).await?;
    tx.commit().await?;
    Ok(asset)
}

pub async fn add_source(hub: &Hub, actor: &Actor, persona_id: Uuid, input: LookSourceIn) -> DResult<Asset> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    persona(&mut tx, persona_id).await?;
    let source_key = format!("pick:{persona_id}:{}", input.asset_id);
    let valid: bool = sqlx::query_scalar("select exists(select 1 from assets where id=$1 and state='ready' and mime like 'image/%')")
        .bind(input.asset_id)
        .fetch_one(&mut *tx)
        .await?;
    if !valid {
        return Err(invalid("bad_source", "source must be a ready image"));
    }
    sqlx::query("insert into assets (id, persona_id, sha256, storage_key, mime, bytes, state, provenance, source_key) select $1,$2,sha256,storage_key,mime,bytes,'ready',$3,$4 from assets where id=$5 and state='ready' and mime like 'image/%' on conflict (source_key) do nothing")
        .bind(Uuid::new_v4()).bind(persona_id).bind(json!({"source":"library", "from_asset_id":input.asset_id})).bind(&source_key).bind(input.asset_id).execute(&mut *tx).await?;
    let row: Option<Value> =
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {} from assets where source_key=$1) t", production::ASSET_COLS))
            .bind(&source_key)
            .fetch_optional(&mut *tx)
            .await?;
    let asset: Asset = decode_one(row.ok_or_else(|| invalid("bad_source", "source must be a ready image"))?)?;
    audit(&mut tx, actor, "look.source_added", "persona", persona_id, json!({"asset_id":asset.id, "from_asset_id":input.asset_id})).await?;
    tx.commit().await?;
    Ok(asset)
}

pub async fn request_pass(hub: &Hub, actor: &Actor, persona_id: Uuid, input: LookPassIn) -> DResult<LookPass> {
    actor.require(OPERATOR)?;
    if input.prompt.trim().is_empty() {
        return Err(invalid("empty_prompt", "prompt is required"));
    }
    if !(1..=4).contains(&input.variants) {
        return Err(invalid("invalid_variants", "variants must be 1..=4"));
    }
    if input.source_asset_id.is_some() && !input.strength.is_some_and(|s| (0.15..=0.85).contains(&s)) {
        return Err(invalid("invalid_strength", "strength must be 0.15..=0.85"));
    }
    if input.source_asset_id.is_none() && input.size == LookSize::Final {
        return Err(invalid("final_needs_source", "final needs a source"));
    }
    let source: Option<(String, String)> = if let Some(id) = input.source_asset_id {
        Some(
            sqlx::query_as("select sha256, mime from assets where id=$1 and persona_id=$2 and state='ready' and mime like 'image/%'")
                .bind(id)
                .bind(persona_id)
                .fetch_optional(&hub.db)
                .await?
                .ok_or_else(|| invalid("bad_source", "source must be a ready image owned by this persona"))?,
        )
    } else {
        None
    };
    if let Some(seed) = input.reuse_seeds_from {
        let exists: bool = sqlx::query_scalar("select exists(select 1 from generation_requests where id=$1 and persona_id=$2)")
            .bind(seed)
            .bind(persona_id)
            .fetch_one(&hub.db)
            .await?;
        if !exists {
            return Err(invalid("bad_seed_source", "seed source must belong to this persona"));
        }
    }
    let (name, inputs) = match source {
        None => ("moss-look-txt2img", json!({"prompt":input.prompt})),
        Some((sha, mime)) => (
            if input.size == LookSize::Final { "moss-look-img2img-final" } else { "moss-look-img2img" },
            json!({"prompt":input.prompt, "image":production::source_filename(&sha, &mime), "denoise":input.strength}),
        ),
    };
    let workflow_id: Uuid = sqlx::query_scalar("select id from workflows where name=$1 order by version desc limit 1")
        .bind(name)
        .fetch_optional(&hub.db)
        .await?
        .ok_or(DomainError::NotFound("workflow"))?;
    let generation = production::start(
        hub,
        actor,
        production::Owner::Persona(persona_id),
        GenerateIn { workflow_id, inputs, variants: input.variants },
        input.source_asset_id,
        input.reuse_seeds_from,
    )
    .await?;
    Ok(LookPass { generation, assets: vec![] })
}

const REFERENCES: &str = "select to_jsonb(t) from (select r.id, r.persona_id, r.asset_id, a.sha256, r.pinned_by, r.pinned_at from persona_references r join assets a on a.id=r.asset_id where r.persona_id=$1 order by r.pinned_at desc, r.id desc) t";

pub async fn pin(hub: &Hub, actor: &Actor, persona_id: Uuid, input: PinIn) -> DResult<PinOut> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    sqlx::query_scalar::<_, Uuid>("select id from personas where id=$1 for update")
        .bind(persona_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("persona"))?;
    let sha: String =
        sqlx::query_scalar("select sha256 from assets where id=$1 and persona_id=$2 and state='ready' and selected for update")
            .bind(input.asset_id)
            .bind(persona_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| invalid("pin_needs_a_kept_image", "pin needs a kept image of this character"))?;
    let history: Vec<Reference> = decode(sqlx::query_scalar(REFERENCES).bind(persona_id).fetch_all(&mut *tx).await?)?;
    let previous = history.first();
    let names: Vec<String> = sqlx::query_scalar(
        "select i.name from influencers i join influencer_members m on m.influencer_id=i.id where m.persona_id=$1 order by i.name, i.id",
    )
    .bind(persona_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(current) = previous {
        if current.asset_id == input.asset_id {
            return Ok(PinOut { reference: current.clone(), affected_influencers: names, stale_stages: vec![] });
        }
        if !input.confirm_repin {
            return Err(conflict(
                "repin_needs_confirmation",
                format!("Re-pinning affects: {}. 3D and rig built from the old reference need redoing.", names.join(", ")),
            ));
        }
    }
    // ponytail: 3D and rig have no hub data yet, so "needs redo" is the audit entry and the response field; when those stages land they store the reference id they were built from and compare.
    let stale_stages: Vec<String> = if previous.is_some() { vec!["3d".into(), "rig".into()] } else { vec![] };
    sqlx::query("insert into persona_references (id, persona_id, asset_id, pinned_by, pinned_at) values ($1,$2,$3,$4,clock_timestamp())")
        .bind(Uuid::new_v4())
        .bind(persona_id)
        .bind(input.asset_id)
        .bind(actor.name())
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        actor,
        "look.pinned",
        "persona",
        persona_id,
        json!({"asset_id":input.asset_id, "sha256":sha, "previous_asset_id":previous.map(|r| r.asset_id), "stale_stages":stale_stages}),
    )
    .await?;
    let reference: Reference = decode_one(sqlx::query_scalar(REFERENCES).bind(persona_id).fetch_one(&mut *tx).await?)?;
    tx.commit().await?;
    Ok(PinOut { reference, affected_influencers: names, stale_stages })
}

pub async fn get(hub: &Hub, actor: &Actor, persona_id: Uuid) -> DResult<CharacterLook> {
    actor.require(crate::ANY_READER)?;
    let mut conn = hub.db.acquire().await?;
    persona(&mut conn, persona_id).await?;
    let assets: Vec<Asset> = decode(
        sqlx::query_scalar(&format!(
            "select to_jsonb(t) from (select {} from assets where persona_id=$1 order by created_at, id) t",
            production::ASSET_COLS
        ))
        .bind(persona_id)
        .fetch_all(&mut *conn)
        .await?,
    )?;
    let generations: Vec<GenerationRequest> = decode(
        sqlx::query_scalar(&format!("{} where g.persona_id=$1 order by g.created_at, g.id) t", production::GEN_SELECT))
            .bind(persona_id)
            .fetch_all(&mut *conn)
            .await?,
    )?;
    let uploads = assets.iter().filter(|a| a.generation_request_id.is_none()).cloned().collect();
    let passes = generations
        .into_iter()
        .map(|generation| {
            let assets = assets.iter().filter(|a| a.generation_request_id == Some(generation.id)).cloned().collect();
            LookPass { generation, assets }
        })
        .collect();
    let reference_history: Vec<Reference> = decode(sqlx::query_scalar(REFERENCES).bind(persona_id).fetch_all(&mut *conn).await?)?;
    let reference = reference_history.first().cloned();
    Ok(CharacterLook { uploads, passes, reference, reference_history })
}

pub async fn notify_finished(hub: &Hub) -> DResult<()> {
    // ponytail: 24 h window scan; add a notified_at column if passes become frequent
    let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as("select g.id, p.id, p.name, g.state from generation_requests g join personas p on p.id=g.persona_id where g.state in ('succeeded','failed','cancelled','outcome_unknown') and g.updated_at > now() - interval '24 hours'").fetch_all(&hub.db).await?;
    for (id, persona_id, name, state) in rows {
        crate::notifications::raise(
            hub,
            &crate::notifications::Raise {
                source: "look".into(),
                dedupe_key: format!("look:pass:{id}"),
                severity: if state == "succeeded" { Severity::Info } else { Severity::Warning },
                title: format!("{name}: look pass {state}"),
                body: format!("Look pass {id} finished."),
                subject_type: Some("persona".into()),
                subject_id: Some(persona_id.to_string()),
                url: None,
            },
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::image_mime;
    #[test]
    fn detects_image_magic() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_mime(b"\xff\xd8\xff"), Some("image/jpeg"));
        assert_eq!(image_mime(b"RIFF1234WEBP"), Some("image/webp"));
        assert_eq!(image_mime(b"RIFFWEBP"), None);
    }
}
