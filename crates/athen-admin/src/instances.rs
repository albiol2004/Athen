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
use crate::seed::{self, LlmSeed};
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
    /// Mutually exclusive with `llm_seed` for `models_toml` — if both are
    /// set, `create` returns an error (ambiguous).
    pub config_toml: Option<String>,
    pub models_toml: Option<String>,
    /// Structured LLM provider seed. When present, the panel generates a
    /// `models.toml` automatically and injects the API key into the
    /// container env. Mutually exclusive with a raw `models_toml` string.
    pub llm_seed: Option<LlmSeed>,
    /// Users granted access right away.
    pub user_ids: Vec<String>,
    /// Hard memory limit (cgroup, swap disabled); `None` = unlimited.
    pub memory_mb: Option<u64>,
    /// Hard CPU limit in fractional cores; `None` = unlimited.
    pub cpus: Option<f64>,
    /// Disk quota on the data volume (sweep-enforced: warn, then stop);
    /// `None` = no threshold.
    pub disk_limit_mb: Option<u64>,
}

/// Full provisioning flow. On Docker failure after the row insert, the row
/// is removed again so a retry doesn't hit UNIQUE constraints.
///
/// Validation rules applied before any Docker interaction:
/// - Raw `models_toml` and `llm_seed` are mutually exclusive.
/// - When `llm_seed` is present, it must pass `LlmSeed::validate()`.
pub async fn create(state: &PanelState, spec: CreateSpec) -> anyhow::Result<Instance> {
    // ── Reject ambiguous seed ──────────────────────────────────────────────
    if spec.models_toml.is_some() && spec.llm_seed.is_some() {
        anyhow::bail!(
            "models_toml and llm_seed are mutually exclusive — \
             set one or the other, not both"
        );
    }

    // ── Validate structured seed (before any side effects) ────────────────
    if let Some(ref s) = spec.llm_seed {
        s.validate()
            .map_err(|e| anyhow::anyhow!("invalid llm_seed: {e}"))?;
    }

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
        memory_mb: spec.memory_mb,
        cpus: spec.cpus,
        disk_limit_mb: spec.disk_limit_mb,
    };

    let row = instance.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO instances (id, name, container_name, volume_name, http_token, internal_url, created_at, memory_mb, cpus, disk_limit_mb) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![
                    row.id,
                    row.name,
                    row.container_name,
                    row.volume_name,
                    row.http_token,
                    row.internal_url,
                    row.created_at,
                    row.memory_mb,
                    row.cpus,
                    row.disk_limit_mb
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

    // Inject the API key from llm_seed into the container env (never the file).
    let generated_models_toml: Option<String> = if let Some(ref s) = spec.llm_seed {
        if !s.api_key.is_empty() {
            let var_name = seed::provider_env_var(&s.provider_id);
            tracing::info!(
                provider = %s.provider_id,
                key_len = s.api_key.len(),
                var = %var_name,
                "injecting provider api_key into instance env"
            );
            env.push(format!("{var_name}={}", s.api_key));
        }
        Some(seed::generate_models_toml(s, Utc::now()))
    } else {
        None
    };

    let effective_models_toml = generated_models_toml
        .as_deref()
        .or(spec.models_toml.as_deref());

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
                spec.memory_mb,
                spec.cpus,
            )
            .await?;
        let mut file_seeds: Vec<(String, String)> = Vec::new();
        if let Some(c) = &spec.config_toml {
            file_seeds.push(("config.toml".into(), c.clone()));
        }
        if let Some(m) = effective_models_toml {
            file_seeds.push(("models.toml".into(), m.to_string()));
        }
        if !file_seeds.is_empty() {
            state
                .docker
                .upload_files(&instance.container_name, "/data", &file_seeds)
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

/// Change the disk quota after create (`None` clears it). DB-only — the
/// sweep picks it up next pass. This is the way out of an enforced
/// quota stop without deleting the instance.
pub async fn set_disk_limit(
    db: &Db,
    instance_id: &str,
    disk_limit_mb: Option<u64>,
) -> anyhow::Result<usize> {
    let iid = instance_id.to_string();
    db.call(move |c| {
        c.execute(
            "UPDATE instances SET disk_limit_mb = ?1 WHERE id = ?2",
            rusqlite::params![disk_limit_mb, iid],
        )
    })
    .await
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
