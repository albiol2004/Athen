//! Bundle CRUD Tauri commands (Phase 2 of the Bundles rework — see
//! `docs/BUNDLES.md`). Bundles are the user-facing model-loadout unit:
//! a named map of `ModelProfile → (connection_id, slug)`. Exactly one
//! Bundle is "active" at a time and drives the global LLM router; arc
//! pins still freeze in-flight calls to whatever `(connection, slug)`
//! the pinned arc captured.
//!
//! This module owns the read/write surface; the resolver path
//! (`state::resolve_effective_provider_for_arc_with_config`) was already
//! swapped in Phase 1b. Router rebuild on Bundle switch is done here via
//! [`crate::state::build_router_for_bundle`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::{info, warn};

use athen_core::config::{Bundle, BundleTier, ModelsConfig, ACTIVE_BUNDLE_KEY};
use athen_core::llm::ModelProfile;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Frontend views
// ---------------------------------------------------------------------------

/// A single tier slot in a Bundle, projected for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleTierView {
    pub connection_id: String,
    pub slug: String,
}

/// All four tier slots of a Bundle, explicit per-tier fields so the
/// frontend can render labeled rows without juggling a typed enum map
/// across the Tauri boundary. `Local` is intentionally omitted — it has
/// no UI representation today.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleTiersView {
    pub cheap: Option<BundleTierView>,
    pub fast: Option<BundleTierView>,
    pub code: Option<BundleTierView>,
    pub powerful: Option<BundleTierView>,
}

/// A Bundle projected for the frontend. `id` is the stringified UUID
/// used as both the map key in `models.bundles` and the
/// `assignments["active_bundle"]` value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleView {
    pub id: String,
    pub name: String,
    pub is_active: bool,
    pub tiers: BundleTiersView,
    /// RFC3339 timestamps for the Bundles panel's "last edited" label.
    pub created_at: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_tiers_view(bundle: &Bundle) -> BundleTiersView {
    let pick = |t: ModelProfile| -> Option<BundleTierView> {
        bundle.tiers.get(&t).map(|bt| BundleTierView {
            connection_id: bt.connection_id.clone(),
            slug: bt.slug.clone(),
        })
    };
    BundleTiersView {
        cheap: pick(ModelProfile::Judges),
        fast: pick(ModelProfile::Fast),
        code: pick(ModelProfile::Code),
        powerful: pick(ModelProfile::Powerful),
    }
}

fn from_tiers_view(view: &BundleTiersView) -> HashMap<ModelProfile, BundleTier> {
    let mut out = HashMap::new();
    if let Some(t) = &view.cheap {
        out.insert(
            ModelProfile::Judges,
            BundleTier {
                connection_id: t.connection_id.clone(),
                slug: t.slug.clone(),
            },
        );
    }
    if let Some(t) = &view.fast {
        out.insert(
            ModelProfile::Fast,
            BundleTier {
                connection_id: t.connection_id.clone(),
                slug: t.slug.clone(),
            },
        );
    }
    if let Some(t) = &view.code {
        out.insert(
            ModelProfile::Code,
            BundleTier {
                connection_id: t.connection_id.clone(),
                slug: t.slug.clone(),
            },
        );
    }
    if let Some(t) = &view.powerful {
        out.insert(
            ModelProfile::Powerful,
            BundleTier {
                connection_id: t.connection_id.clone(),
                slug: t.slug.clone(),
            },
        );
    }
    out
}

fn project(bundle: &Bundle, active_id: &str) -> BundleView {
    let id = bundle.id.to_string();
    BundleView {
        is_active: id == active_id,
        id,
        name: bundle.name.clone(),
        tiers: to_tiers_view(bundle),
        created_at: bundle.created_at.to_rfc3339(),
        updated_at: bundle.updated_at.to_rfc3339(),
    }
}

fn active_id_of(models: &ModelsConfig) -> String {
    models
        .assignments
        .get(ACTIVE_BUNDLE_KEY)
        .cloned()
        .unwrap_or_default()
}

/// Pick the connection the user-facing "active provider" snapshot should
/// point at. We prefer Fast (the everyday-loop tier) then Cheap then the
/// first present tier. Returned to the caller of `set_active_bundle` so
/// it can keep `state.active_provider_id` coherent with the bundle —
/// legacy callers (vision-check, router rebuild fallbacks) still read
/// it.
pub(crate) fn derive_primary_connection(bundle: &Bundle) -> Option<(String, String)> {
    for tier in [
        ModelProfile::Fast,
        ModelProfile::Judges,
        ModelProfile::Code,
        ModelProfile::Powerful,
        ModelProfile::Local,
    ] {
        if let Some(bt) = bundle.tiers.get(&tier) {
            return Some((bt.connection_id.clone(), bt.slug.clone()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// List every Bundle. The currently-active one is flagged via
/// `is_active`. Order is stable but unspecified — the FE sorts by name.
#[tauri::command]
pub async fn list_bundles() -> std::result::Result<Vec<BundleView>, String> {
    let models = crate::settings::load_models_config();
    let active = active_id_of(&models);
    let mut out: Vec<BundleView> = models
        .bundles
        .values()
        .map(|b| project(b, &active))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Create a new empty Bundle. Does NOT make it active — the FE must
/// follow up with `set_active_bundle` if the user clicks "switch to
/// this." Name must be non-empty and not already in use.
#[tauri::command]
pub async fn create_bundle(name: String) -> std::result::Result<BundleView, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Bundle name cannot be empty".into());
    }
    let mut models = crate::settings::load_models_config();
    if models.bundles.values().any(|b| b.name == trimmed) {
        return Err(format!("A Bundle named '{trimmed}' already exists"));
    }
    let now = chrono::Utc::now();
    let bundle = Bundle {
        id: uuid::Uuid::new_v4(),
        name: trimmed.clone(),
        created_at: now,
        updated_at: now,
        tiers: HashMap::new(),
    };
    let id = bundle.id.to_string();
    models.bundles.insert(id.clone(), bundle.clone());
    crate::settings::save_models_config(&models)?;
    info!(bundle_id = %id, bundle_name = %trimmed, "Created Bundle");
    let active = active_id_of(&models);
    Ok(project(&bundle, &active))
}

/// Patch an existing Bundle. `name` and/or `tiers` may be `None` to
/// leave that field alone. Renaming to an in-use name (other than the
/// bundle's own current name) errors. `tiers` is a *full replacement*
/// when present — to clear a tier, send `null` for that field.
#[tauri::command]
pub async fn update_bundle(
    id: String,
    name: Option<String>,
    tiers: Option<BundleTiersView>,
    state: State<'_, AppState>,
) -> std::result::Result<BundleView, String> {
    update_bundle_core(id, name, tiers, &state).await
}

pub(crate) async fn update_bundle_core(
    id: String,
    name: Option<String>,
    tiers: Option<BundleTiersView>,
    state: &AppState,
) -> std::result::Result<BundleView, String> {
    let mut models = crate::settings::load_models_config();
    let active = active_id_of(&models);
    let Some(bundle) = models.bundles.get(&id).cloned() else {
        return Err(format!("Bundle '{id}' not found"));
    };

    let mut updated = bundle;
    if let Some(new_name) = name {
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return Err("Bundle name cannot be empty".into());
        }
        if models
            .bundles
            .values()
            .any(|b| b.id.to_string() != id && b.name == trimmed)
        {
            return Err(format!("A Bundle named '{trimmed}' already exists"));
        }
        updated.name = trimmed;
    }
    if let Some(new_tiers) = tiers {
        updated.tiers = from_tiers_view(&new_tiers);
    }
    updated.updated_at = chrono::Utc::now();

    models.bundles.insert(id.clone(), updated.clone());
    crate::settings::save_models_config(&models)?;

    // If the active Bundle was edited, rebuild the live router so the
    // change is reflected immediately. Hydrate first so the rebuilt
    // router has real credentials.
    if id == active {
        let hydrated = crate::settings::load_models_config_hydrated(state.vault.as_ref()).await;
        let new_router = crate::state::build_router_for_bundle(&updated, &hydrated.providers);
        *state.router.write().await = new_router;
        if let Some((cid, slug)) = derive_primary_connection(&updated) {
            *state.active_provider_id.lock().await = cid;
            *state.model_name.lock().await = slug;
        }
        info!(bundle_id = %id, "Active Bundle edited; router rebuilt");
    }

    Ok(project(&updated, &active))
}

/// Delete a Bundle. Refuses to delete the currently-active Bundle — the
/// FE must point `set_active_bundle` at a different Bundle first.
#[tauri::command]
pub async fn delete_bundle(id: String) -> std::result::Result<(), String> {
    let mut models = crate::settings::load_models_config();
    let active = active_id_of(&models);
    if id == active {
        return Err("Cannot delete the active Bundle. Switch to a different Bundle first.".into());
    }
    if models.bundles.remove(&id).is_none() {
        return Err(format!("Bundle '{id}' not found"));
    }
    crate::settings::save_models_config(&models)?;
    info!(bundle_id = %id, "Deleted Bundle");
    Ok(())
}

/// Activate a Bundle. Rebuilds the global LLM router from the Bundle's
/// per-tier `(connection, slug)` picks (cross-vendor by construction)
/// and updates `state.active_provider_id` / `state.model_name` to the
/// Bundle's primary tier so legacy readers (vision-check etc.) stay
/// coherent. In-flight arcs are unaffected — their per-arc pin still
/// wins (see `docs/PROVIDER_PINNING.md`).
#[tauri::command]
pub async fn set_active_bundle(
    id: String,
    state: State<'_, AppState>,
) -> std::result::Result<String, String> {
    set_active_bundle_core(id, &state).await
}

pub(crate) async fn set_active_bundle_core(
    id: String,
    state: &AppState,
) -> std::result::Result<String, String> {
    let mut models = crate::settings::load_models_config();
    let Some(bundle) = models.bundles.get(&id).cloned() else {
        return Err(format!("Bundle '{id}' not found"));
    };

    // Persist the new active assignment.
    models
        .assignments
        .insert(ACTIVE_BUNDLE_KEY.to_string(), id.clone());
    crate::settings::save_models_config(&models)?;

    // Rebuild the live router with hydrated credentials.
    let hydrated = crate::settings::load_models_config_hydrated(state.vault.as_ref()).await;
    let new_router = crate::state::build_router_for_bundle(&bundle, &hydrated.providers);
    *state.router.write().await = new_router;

    // Update the derived "primary" snapshot so legacy code that reads
    // `state.active_provider_id` stays consistent (vision-check, etc.).
    if let Some((cid, slug)) = derive_primary_connection(&bundle) {
        *state.active_provider_id.lock().await = cid;
        *state.model_name.lock().await = slug;
    } else {
        warn!(
            bundle_id = %id,
            bundle_name = %bundle.name,
            "Activated Bundle has no tier slots filled; router will reject every request"
        );
    }

    info!(bundle_id = %id, bundle_name = %bundle.name, "Switched active Bundle");
    Ok(format!("Switched to Bundle '{}'", bundle.name))
}

/// Duplicate a Bundle under a new name. Useful for "fork the Default
/// Bundle to experiment without losing the original."
#[tauri::command]
pub async fn duplicate_bundle(
    id: String,
    new_name: String,
) -> std::result::Result<BundleView, String> {
    let trimmed = new_name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Bundle name cannot be empty".into());
    }
    let mut models = crate::settings::load_models_config();
    if models.bundles.values().any(|b| b.name == trimmed) {
        return Err(format!("A Bundle named '{trimmed}' already exists"));
    }
    let Some(source) = models.bundles.get(&id).cloned() else {
        return Err(format!("Bundle '{id}' not found"));
    };
    let now = chrono::Utc::now();
    let copy = Bundle {
        id: uuid::Uuid::new_v4(),
        name: trimmed.clone(),
        created_at: now,
        updated_at: now,
        tiers: source.tiers.clone(),
    };
    let new_id = copy.id.to_string();
    models.bundles.insert(new_id.clone(), copy.clone());
    crate::settings::save_models_config(&models)?;
    info!(
        source_id = %id,
        new_id = %new_id,
        new_name = %trimmed,
        "Duplicated Bundle"
    );
    let active = active_id_of(&models);
    Ok(project(&copy, &active))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_bundle(name: &str, tiers: &[(ModelProfile, &str, &str)]) -> Bundle {
        let mut map: HashMap<ModelProfile, BundleTier> = HashMap::new();
        for (t, cid, slug) in tiers {
            map.insert(
                *t,
                BundleTier {
                    connection_id: (*cid).to_string(),
                    slug: (*slug).to_string(),
                },
            );
        }
        let now = chrono::Utc::now();
        Bundle {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            created_at: now,
            updated_at: now,
            tiers: map,
        }
    }

    #[test]
    fn primary_connection_prefers_fast() {
        let b = mk_bundle(
            "x",
            &[
                (ModelProfile::Judges, "deepseek", "v4-flash"),
                (ModelProfile::Fast, "anthropic", "claude-sonnet-4-6"),
            ],
        );
        let (cid, slug) = derive_primary_connection(&b).unwrap();
        assert_eq!(cid, "anthropic");
        assert_eq!(slug, "claude-sonnet-4-6");
    }

    #[test]
    fn primary_connection_falls_back_to_cheap_when_no_fast() {
        let b = mk_bundle(
            "x",
            &[
                (ModelProfile::Powerful, "openai", "gpt-5"),
                (ModelProfile::Judges, "deepseek", "v4-flash"),
            ],
        );
        let (cid, _) = derive_primary_connection(&b).unwrap();
        assert_eq!(cid, "deepseek");
    }

    #[test]
    fn primary_connection_empty_bundle_returns_none() {
        let b = mk_bundle("x", &[]);
        assert!(derive_primary_connection(&b).is_none());
    }

    #[test]
    fn tiers_view_roundtrip() {
        let b = mk_bundle(
            "Default",
            &[
                (ModelProfile::Judges, "deepseek", "v4-flash"),
                (ModelProfile::Code, "anthropic", "claude-opus-4-7"),
            ],
        );
        let view = to_tiers_view(&b);
        assert_eq!(view.cheap.as_ref().unwrap().slug, "v4-flash");
        assert!(view.fast.is_none());
        assert_eq!(view.code.as_ref().unwrap().connection_id, "anthropic");
        assert!(view.powerful.is_none());

        // Round-trip back.
        let map = from_tiers_view(&view);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&ModelProfile::Judges).unwrap().slug, "v4-flash");
        assert_eq!(
            map.get(&ModelProfile::Code).unwrap().connection_id,
            "anthropic"
        );
    }
}
