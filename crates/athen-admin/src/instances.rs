//! Instance store + provisioning orchestration.
//!
//! A row in `instances` is the panel's source of truth; the container and
//! volume are derived state. Provisioning: insert row → create volume +
//! container (panel-generated `ATHEN_HTTP_TOKEN` injected as env) → seed
//! optional config files into `/data` → start. The instance id never
//! reaches Docker except as a label; container/volume names use a short
//! prefix so `docker ps` stays readable.

use std::collections::HashMap;

use chrono::Utc;

use crate::db::{random_token, Db, Instance};
use crate::PanelState;

/// HTTP port instances listen on inside the network (never published).
pub const INSTANCE_PORT: u16 = 8787;

pub async fn list_all(db: &Db) -> anyhow::Result<Vec<Instance>> {
    db.call(|c| {
        let mut stmt = c.prepare("SELECT * FROM instances ORDER BY created_at")?;
        let rows = stmt.query_map([], Instance::from_row)?;
        rows.collect()
    })
    .await
}

pub async fn list_for_user(db: &Db, user_id: &str) -> anyhow::Result<Vec<Instance>> {
    let uid = user_id.to_string();
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT i.* FROM instances i JOIN user_instances ui ON ui.instance_id = i.id \
             WHERE ui.user_id = ?1 ORDER BY i.created_at",
        )?;
        let rows = stmt.query_map([uid], Instance::from_row)?;
        rows.collect()
    })
    .await
}

pub async fn get(db: &Db, instance_id: &str) -> anyhow::Result<Option<Instance>> {
    let iid = instance_id.to_string();
    db.call(move |c| {
        c.query_row(
            "SELECT * FROM instances WHERE id = ?1",
            [iid],
            Instance::from_row,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
    })
    .await
}

pub struct CreateSpec {
    pub name: String,
    /// Extra container env (`ATHEN_PROVIDER_*_API_KEY`,
    /// `ATHEN_TELEGRAM_BOT_TOKEN`, …). Operator-provided.
    pub env: HashMap<String, String>,
    /// Optional file contents seeded into `/data` before first start.
    pub config_toml: Option<String>,
    pub models_toml: Option<String>,
    /// Users granted access right away.
    pub user_ids: Vec<String>,
}

/// Full provisioning flow. On Docker failure after the row insert, the row
/// is removed again so a retry doesn't hit UNIQUE constraints.
pub async fn create(state: &PanelState, spec: CreateSpec) -> anyhow::Result<Instance> {
    let id = uuid::Uuid::new_v4().to_string();
    let short = &id[..8];
    let instance = Instance {
        id: id.clone(),
        name: spec.name.clone(),
        container_name: format!("athen-{short}"),
        volume_name: format!("athen-{short}-data"),
        http_token: random_token(),
        internal_url: format!("http://athen-{short}:{INSTANCE_PORT}"),
        created_at: Utc::now().to_rfc3339(),
    };

    let row = instance.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO instances (id, name, container_name, volume_name, http_token, internal_url, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    row.id,
                    row.name,
                    row.container_name,
                    row.volume_name,
                    row.http_token,
                    row.internal_url,
                    row.created_at
                ],
            )
        })
        .await?;

    let mut env: Vec<String> = vec![
        format!("ATHEN_HTTP_ADDR=0.0.0.0:{INSTANCE_PORT}"),
        format!("ATHEN_HTTP_TOKEN={}", instance.http_token),
    ];
    for (k, v) in &spec.env {
        if k.contains('=') || k.is_empty() {
            anyhow::bail!("invalid env var name: {k:?}");
        }
        env.push(format!("{k}={v}"));
    }

    let provisioned: anyhow::Result<()> = async {
        state
            .docker
            .create_instance(
                &instance.id,
                &instance.container_name,
                &instance.volume_name,
                &state.cfg.instance_image,
                &state.cfg.network,
                env,
            )
            .await?;
        let mut seed: Vec<(String, String)> = Vec::new();
        if let Some(c) = &spec.config_toml {
            seed.push(("config.toml".into(), c.clone()));
        }
        if let Some(m) = &spec.models_toml {
            seed.push(("models.toml".into(), m.clone()));
        }
        if !seed.is_empty() {
            state
                .docker
                .upload_files(&instance.container_name, "/data", &seed)
                .await?;
        }
        state.docker.start(&instance.container_name).await?;
        Ok(())
    }
    .await;

    if let Err(e) = provisioned {
        // Roll back: best-effort container/volume cleanup + row delete.
        let _ = state
            .docker
            .remove(&instance.container_name, &instance.volume_name, true)
            .await;
        let iid = instance.id.clone();
        let _ = state
            .db
            .call(move |c| c.execute("DELETE FROM instances WHERE id = ?1", [iid]))
            .await;
        return Err(e);
    }

    set_grants(&state.db, &instance.id, &spec.user_ids).await?;
    Ok(instance)
}

/// Delete an instance: container always; volume (the user's data!) only
/// when `delete_data`. Keeping the volume allows re-provisioning later.
pub async fn delete(
    state: &PanelState,
    instance: &Instance,
    delete_data: bool,
) -> anyhow::Result<()> {
    state
        .docker
        .remove(&instance.container_name, &instance.volume_name, delete_data)
        .await?;
    let iid = instance.id.clone();
    state
        .db
        .call(move |c| c.execute("DELETE FROM instances WHERE id = ?1", [iid]))
        .await?;
    Ok(())
}

/// Replace the set of users who may reach `instance_id`.
pub async fn set_grants(db: &Db, instance_id: &str, user_ids: &[String]) -> anyhow::Result<()> {
    let iid = instance_id.to_string();
    let uids = user_ids.to_vec();
    db.call(move |c| {
        c.execute("DELETE FROM user_instances WHERE instance_id = ?1", [&iid])?;
        for uid in &uids {
            c.execute(
                "INSERT OR IGNORE INTO user_instances (user_id, instance_id) VALUES (?1, ?2)",
                [uid, &iid],
            )?;
        }
        Ok(())
    })
    .await
}

/// instance_id → granted user ids (for the admin users/instances views).
pub async fn all_grants(db: &Db) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = db
        .call(|c| {
            let mut stmt = c.prepare("SELECT instance_id, user_id FROM user_instances")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
        .await?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (iid, uid) in rows {
        map.entry(iid).or_default().push(uid);
    }
    Ok(map)
}
