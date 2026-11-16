//! Projects, their members and accounts.
use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, conflict, decode, decode_one, invalid};
use contracts::{Project, ProjectAccountsIn, ProjectIn, ProjectKind, ProjectMemberIn};
use serde_json::{Value, json};
use uuid::Uuid;

const SELECT: &str = "select to_jsonb(t) from (
    select i.id, i.company_id, i.name, i.kind, i.premise, i.dynamics, i.created_at,
      coalesce((select jsonb_agg(to_jsonb(m) order by m.added_at, m.persona_id) from (
        select im.persona_id, p.name, im.role, im.added_at,
          (select asset_id from persona_references where persona_id=p.id order by pinned_at desc, id desc limit 1) as reference_asset_id,
          exists(select 1 from canon_revisions c where c.persona_id = p.id and c.state = 'accepted') as has_accepted_canon
        from project_members im join personas p on p.id = im.persona_id where im.project_id = i.id
      ) m), '[]'::jsonb) as members,
      coalesce((select jsonb_agg(a.id order by a.id) from accounts a where a.project_id = i.id), '[]'::jsonb) as account_ids
    from projects i";

async fn get_conn(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<Project> {
    let row: Option<Value> = sqlx::query_scalar(&format!("{SELECT} where i.id = $1) t")).bind(id).fetch_optional(conn).await?;
    decode_one(row.ok_or(DomainError::NotFound("project"))?)
}

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<Project>> {
    actor.require(crate::ANY_READER)?;
    let rows = sqlx::query_scalar(&format!("{SELECT} order by i.name, i.id) t")).fetch_all(&hub.db).await?;
    decode(rows)
}

pub async fn get(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<Project> {
    actor.require(crate::ANY_READER)?;
    let mut conn = hub.db.acquire().await?;
    get_conn(&mut conn, id).await
}

fn name_error(e: sqlx::Error) -> DomainError {
    if e.as_database_error().and_then(|e| e.constraint()) == Some("projects_company_id_name_key") {
        conflict("project_name_taken", "project name already exists")
    } else {
        e.into()
    }
}

pub async fn create(hub: &Hub, actor: &Actor, input: ProjectIn) -> DResult<Project> {
    actor.require(OPERATOR)?;
    if input.name.trim().is_empty() {
        return Err(invalid("invalid_project", "name is required"));
    }
    let mut tx = hub.db.begin().await?;
    let id = Uuid::new_v4();
    sqlx::query("insert into projects (id, company_id, name, kind, premise, dynamics) values ($1,$2,$3,$4,$5,$6)")
        .bind(id)
        .bind(&hub.config.company_id)
        .bind(&input.name)
        .bind(input.kind.as_str())
        .bind(&input.premise)
        .bind(&input.dynamics)
        .execute(&mut *tx)
        .await
        .map_err(name_error)?;
    audit(&mut tx, actor, "project.created", "project", id, json!({"name": input.name})).await?;
    let result = get_conn(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn update(hub: &Hub, actor: &Actor, id: Uuid, input: ProjectIn) -> DResult<Project> {
    actor.require(OPERATOR)?;
    if input.name.trim().is_empty() {
        return Err(invalid("invalid_project", "name is required"));
    }
    let mut tx = hub.db.begin().await?;
    sqlx::query("select id from projects where id = $1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("project"))?;
    let count: i64 = sqlx::query_scalar("select count(*) from project_members where project_id = $1").bind(id).fetch_one(&mut *tx).await?;
    if input.kind == ProjectKind::Solo && count > 1 {
        return Err(conflict("solo_has_one_member", "solo project has more than one member"));
    }
    sqlx::query("update projects set name=$2, kind=$3, premise=$4, dynamics=$5, updated_at=now() where id=$1")
        .bind(id)
        .bind(&input.name)
        .bind(input.kind.as_str())
        .bind(&input.premise)
        .bind(&input.dynamics)
        .execute(&mut *tx)
        .await
        .map_err(name_error)?;
    audit(&mut tx, actor, "project.updated", "project", id, json!({"name": input.name})).await?;
    let result = get_conn(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn put_member(hub: &Hub, actor: &Actor, id: Uuid, persona_id: Uuid, input: ProjectMemberIn) -> DResult<Project> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let (company, kind): (String, String) = sqlx::query_as("select company_id, kind from projects where id=$1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("project"))?;
    let persona_company: String = sqlx::query_scalar("select company_id from personas where id=$1")
        .bind(persona_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("persona"))?;
    if company != persona_company {
        return Err(invalid("other_company", "persona belongs to another company"));
    }
    let other: bool = sqlx::query_scalar("select exists(select 1 from project_members where project_id=$1 and persona_id <> $2)")
        .bind(id)
        .bind(persona_id)
        .fetch_one(&mut *tx)
        .await?;
    if kind == "solo" && other {
        return Err(conflict("solo_has_one_member", "solo project already has a member"));
    }
    sqlx::query("insert into project_members (project_id, persona_id, role) values ($1,$2,$3) on conflict (project_id, persona_id) do update set role=excluded.role")
        .bind(id).bind(persona_id).bind(&input.role).execute(&mut *tx).await?;
    audit(&mut tx, actor, "project.member_added", "project", id, json!({"persona_id": persona_id, "role": input.role})).await?;
    let result = get_conn(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn remove_member(hub: &Hub, actor: &Actor, id: Uuid, persona_id: Uuid) -> DResult<Project> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    sqlx::query("delete from project_members where project_id=$1 and persona_id=$2").bind(id).bind(persona_id).execute(&mut *tx).await?;
    let result = get_conn(&mut tx, id).await?;
    audit(&mut tx, actor, "project.member_removed", "project", id, json!({"persona_id": persona_id})).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn set_accounts(hub: &Hub, actor: &Actor, id: Uuid, input: ProjectAccountsIn) -> DResult<Project> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    sqlx::query("select id from projects where id=$1 for update")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("project"))?;
    sqlx::query("update accounts set project_id=null where project_id=$1").bind(id).execute(&mut *tx).await?;
    for account in &input.account_ids {
        if sqlx::query("update accounts set project_id=$1 where id=$2").bind(id).bind(account).execute(&mut *tx).await?.rows_affected() == 0
        {
            return Err(DomainError::NotFound("account"));
        }
    }
    audit(&mut tx, actor, "project.accounts_set", "project", id, json!({"account_ids": input.account_ids})).await?;
    let result = get_conn(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn for_persona(conn: &mut sqlx::PgConnection, persona_id: Uuid) -> DResult<Vec<(String, String, String, String)>> {
    Ok(sqlx::query_as("select i.name, i.premise, i.dynamics, m.role from project_members m join projects i on i.id=m.project_id where m.persona_id=$1 order by m.added_at, i.id").bind(persona_id).fetch_all(conn).await?)
}
