//! Panel HTTP surface: UI assets, auth endpoints, panel REST, proxy.
//!
//! Route map:
//! - `GET /` + `/panel.css` + `/panel.js` — embedded UI (client decides
//!   login vs dashboard via `GET /panel/me`)
//! - `GET /healthz` — liveness, no auth
//! - `POST /panel/login` / `POST /panel/logout` / `GET /panel/me`
//! - everything else under `/panel/*` — session-gated panel REST
//!   (admin-only checks per handler)
//! - `/i/{instance}/api/*` — session-gated reverse proxy to the instance

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use axum::{Extension, Json, Router};
use futures::StreamExt;
use serde_json::json;

use crate::auth::{self, CurrentUser};
use crate::db::{self, User};
use crate::{instances, proxy, ui, PanelState};

pub fn router(state: Arc<PanelState>) -> Router {
    // Session-gated panel API (admin checks happen per-handler).
    let panel = Router::new()
        .route("/panel/me", get(me))
        .route("/panel/logout", post(logout))
        .route("/panel/password", post(change_password))
        .route("/panel/notify", post(set_notify_url))
        .route("/panel/audit", get(audit_list))
        .route(
            "/panel/instances",
            get(instances_list).post(instances_create),
        )
        .route("/panel/instances/{id}/start", post(instance_start))
        .route("/panel/instances/{id}/stop", post(instance_stop))
        .route("/panel/instances/{id}/delete", post(instance_delete))
        .route("/panel/instances/{id}/grants", post(instance_grants))
        .route(
            "/panel/instances/{id}/disk_limit",
            post(instance_disk_limit),
        )
        .route("/panel/instances/{id}/logs", get(instance_logs))
        .route("/panel/users", get(users_list).post(users_create))
        .route("/panel/users/{id}/delete", post(users_delete))
        .route("/panel/users/{id}/role", post(users_set_role))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    // Session-gated proxy to instances (access check inside).
    let instances_proxy = Router::new()
        .route("/i/{instance}/api/{*path}", any(proxy::handle))
        .route("/i/{instance}/chat", get(ui::chat_page))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    Router::new()
        .route("/", get(ui::index))
        .route("/panel.css", get(ui::styles))
        .route("/panel.js", get(ui::script))
        .route("/healthz", get(health))
        .route("/panel/login", post(login))
        .merge(panel)
        .merge(instances_proxy)
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "name": "athen-admin",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------- auth --

#[derive(serde::Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(
    State(state): State<Arc<PanelState>>,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    // Brute-force throttle BEFORE any argon2 work: locked-out names and
    // a blown global budget are rejected for free.
    if let Err(retry) = state
        .login_throttle
        .check(&body.username, std::time::Instant::now())
    {
        db::audit(&state.db, &body.username, "login_throttled", "", "").await;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry.as_secs().max(1).to_string())],
            Json(json!({ "error": "too many attempts, retry later" })),
        )
            .into_response();
    }
    let user = match auth::user_by_name(&state.db, &body.username).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            // Burn the same time as a real verify so login timing doesn't
            // reveal which usernames exist.
            let _ = auth::verify_password(body.password, DUMMY_HASH.to_string()).await;
            state
                .login_throttle
                .record_failure(&body.username, std::time::Instant::now());
            db::audit(
                &state.db,
                &body.username,
                "login_failed",
                "",
                "unknown user",
            )
            .await;
            return err(StatusCode::UNAUTHORIZED, "invalid credentials");
        }
        Err(e) => return internal(e),
    };
    if !auth::verify_password(body.password, user.password_hash.clone()).await {
        state
            .login_throttle
            .record_failure(&body.username, std::time::Instant::now());
        db::audit(
            &state.db,
            &body.username,
            "login_failed",
            "",
            "wrong password",
        )
        .await;
        return err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
    state.login_throttle.record_success(&body.username);
    let session = match auth::new_session(&state.db, &user.id).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    db::audit(&state.db, &user.username, "login", "", "").await;
    (
        StatusCode::OK,
        [(header::SET_COOKIE, auth::set_session_cookie(&session))],
        Json(json!({ "username": user.username, "role": user.role })),
    )
        .into_response()
}

async fn logout(
    State(state): State<Arc<PanelState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if let Some(sid) = auth::session_cookie_value(&headers) {
        if let Ok(Some(user)) = auth::user_for_session(&state.db, &sid).await {
            db::audit(&state.db, &user.username, "logout", "", "").await;
        }
        let _ = auth::delete_session(&state.db, &sid).await;
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, auth::clear_session_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn me(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
) -> Json<serde_json::Value> {
    let mut out = json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
        "notify_url": user.notify_url,
    });
    // Operators should see (and fix) a root-equivalent socket; plain
    // users can't act on it, so don't advertise host posture to them.
    if user.is_admin() {
        out["socket_rootless"] = json!(state.socket_rootless);
    }
    Json(out)
}

#[derive(serde::Deserialize)]
struct NotifyBody {
    /// Webhook URL (ntfy topic or any plain-POST sink); empty disables.
    url: String,
}

/// Set the caller's own push webhook.
async fn set_notify_url(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Json(body): Json<NotifyBody>,
) -> impl IntoResponse {
    let url = body.url.trim().to_string();
    if !(url.is_empty() || url.starts_with("http://") || url.starts_with("https://")) {
        return err(StatusCode::BAD_REQUEST, "url must be http(s) or empty");
    }
    let (uid, u) = (user.id.clone(), url.clone());
    match state
        .db
        .call(move |c| c.execute("UPDATE users SET notify_url = ?1 WHERE id = ?2", [u, uid]))
        .await
    {
        Ok(_) => {
            let detail = if url.is_empty() { "cleared" } else { "set" };
            db::audit(&state.db, &user.username, "notify_url", "", detail).await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: u32,
}

fn default_audit_limit() -> u32 {
    200
}

/// Recent audit-trail rows, newest first (admin only).
async fn audit_list(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    match db::audit_recent(&state.db, q.limit.min(1000)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Json(body): Json<PasswordBody>,
) -> impl IntoResponse {
    if body.new.len() < 8 {
        return err(StatusCode::BAD_REQUEST, "new password too short (min 8)");
    }
    if !auth::verify_password(body.current, user.password_hash.clone()).await {
        return err(StatusCode::UNAUTHORIZED, "current password is wrong");
    }
    let hash = match auth::hash_password(body.new).await {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let uid = user.id.clone();
    match state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                [hash, uid],
            )
        })
        .await
    {
        Ok(_) => {
            db::audit(&state.db, &user.username, "password_changed", "", "").await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ----------------------------------------------------------- instances --

/// Admins see every instance; users see their grants. Status comes from
/// one `docker ps` sweep. Tokens never leave the panel.
async fn instances_list(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
) -> impl IntoResponse {
    let list = if user.is_admin() {
        instances::list_all(&state.db).await
    } else {
        instances::list_for_user(&state.db, &user.id).await
    };
    let list = match list {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let status = state.docker.status_by_container().await.unwrap_or_default();
    let grants = if user.is_admin() {
        instances::all_grants(&state.db).await.unwrap_or_default()
    } else {
        HashMap::new()
    };
    let disk = state
        .disk_usage
        .lock()
        .expect("disk usage mutex poisoned")
        .clone();
    let out: Vec<_> = list
        .into_iter()
        .map(|i| {
            let (s, detail) = status
                .get(&i.container_name)
                .cloned()
                .unwrap_or_else(|| ("missing".into(), "container not found".into()));
            json!({
                "id": i.id,
                "name": i.name,
                "container_name": i.container_name,
                "created_at": i.created_at,
                "state": s,
                "status": detail,
                "memory_mb": i.memory_mb,
                "cpus": i.cpus,
                "disk_limit_mb": i.disk_limit_mb,
                // From the periodic sweep; absent until the first sweep
                // after panel start (or when the volume driver can't
                // report sizes).
                "disk_used_mb": disk.get(&i.id).map(|b| b / (1024 * 1024)),
                "user_ids": grants.get(&i.id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Json(out).into_response()
}

#[derive(serde::Deserialize)]
struct CreateInstanceBody {
    name: String,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    config_toml: Option<String>,
    #[serde(default)]
    models_toml: Option<String>,
    #[serde(default)]
    user_ids: Vec<String>,
    #[serde(default)]
    memory_mb: Option<u64>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    disk_limit_mb: Option<u64>,
}

async fn instances_create(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Json(body): Json<CreateInstanceBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    if body.name.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "name is required");
    }
    if body.cpus.is_some_and(|c| !c.is_finite() || c <= 0.0)
        || body.memory_mb.is_some_and(|m| m < 64)
        || body.disk_limit_mb.is_some_and(|m| m < 64)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "cpus must be > 0; memory_mb and disk_limit_mb must be >= 64",
        );
    }
    match instances::create(
        &state,
        instances::CreateSpec {
            name: body.name.trim().to_string(),
            env: body.env,
            config_toml: body.config_toml,
            models_toml: body.models_toml,
            user_ids: body.user_ids,
            memory_mb: body.memory_mb,
            cpus: body.cpus,
            disk_limit_mb: body.disk_limit_mb,
        },
    )
    .await
    {
        Ok(i) => {
            db::audit(&state.db, &user.username, "instance_create", &i.name, "").await;
            Json(json!({ "id": i.id, "container_name": i.container_name })).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "instance provisioning failed");
            err(
                StatusCode::BAD_GATEWAY,
                &format!("provisioning failed: {e:#}"),
            )
        }
    }
}

async fn instance_start(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    instance_action(&state, &user, &id, "instance_start", |st, i| async move {
        st.docker.start(&i.container_name).await
    })
    .await
}

async fn instance_stop(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    instance_action(&state, &user, &id, "instance_stop", |st, i| async move {
        st.docker.stop(&i.container_name).await
    })
    .await
}

#[derive(serde::Deserialize)]
struct DeleteInstanceBody {
    #[serde(default)]
    delete_data: bool,
}

async fn instance_delete(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<DeleteInstanceBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    let instance = match must_instance(&state, &id).await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    match instances::delete(&state, &instance, body.delete_data).await {
        Ok(()) => {
            let detail = if body.delete_data {
                "with data volume"
            } else {
                "data volume kept"
            };
            db::audit(
                &state.db,
                &user.username,
                "instance_delete",
                &instance.name,
                detail,
            )
            .await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct GrantsBody {
    user_ids: Vec<String>,
}

async fn instance_grants(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<GrantsBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    let instance = match must_instance(&state, &id).await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    match instances::set_grants(&state.db, &id, &body.user_ids).await {
        Ok(()) => {
            db::audit(
                &state.db,
                &user.username,
                "grants_set",
                &instance.name,
                &format!("{} user(s)", body.user_ids.len()),
            )
            .await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct DiskLimitBody {
    /// New soft quota in MB; `null` clears it (and un-stops nothing —
    /// the operator starts the instance back up explicitly).
    disk_limit_mb: Option<u64>,
}

/// Adjust an instance's disk quota after create. The enforcement sweep
/// reads the row fresh each pass, so raising the limit is how an
/// over-quota stopped instance becomes startable again.
async fn instance_disk_limit(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<DiskLimitBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    if body.disk_limit_mb.is_some_and(|m| m < 64) {
        return err(StatusCode::BAD_REQUEST, "disk_limit_mb must be >= 64");
    }
    let instance = match must_instance(&state, &id).await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    match instances::set_disk_limit(&state.db, &id, body.disk_limit_mb).await {
        Ok(_) => {
            let detail = match body.disk_limit_mb {
                Some(mb) => format!("{mb} MB"),
                None => "cleared".into(),
            };
            db::audit(
                &state.db,
                &user.username,
                "disk_limit_set",
                &instance.name,
                &detail,
            )
            .await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct LogsQuery {
    #[serde(default = "default_tail")]
    tail: u32,
    #[serde(default)]
    follow: bool,
}

fn default_tail() -> u32 {
    200
}

/// Container logs as SSE (`event: log`, one event per docker log frame).
async fn instance_logs(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> axum::response::Response {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    let instance = match must_instance(&state, &id).await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let stream = state
        .docker
        .logs(&instance.container_name, q.tail, q.follow)
        .map(|line| Ok::<_, std::convert::Infallible>(Event::default().event("log").data(line)));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

// --------------------------------------------------------------- users --

async fn users_list(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    let users: anyhow::Result<Vec<User>> = state
        .db
        .call(|c| {
            let mut stmt = c.prepare("SELECT * FROM users ORDER BY created_at")?;
            let rows = stmt.query_map([], User::from_row)?;
            rows.collect()
        })
        .await;
    match users {
        Ok(u) => Json(u).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct CreateUserBody {
    username: String,
    password: String,
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    instance_ids: Vec<String>,
}

fn default_role() -> String {
    "user".into()
}

async fn users_create(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Json(body): Json<CreateUserBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    let username = body.username.trim();
    if username.is_empty() || body.password.len() < 8 {
        return err(
            StatusCode::BAD_REQUEST,
            "username required; password min 8 chars",
        );
    }
    if !matches!(body.role.as_str(), "admin" | "user") {
        return err(StatusCode::BAD_REQUEST, "role must be admin or user");
    }
    let created = match auth::create_user(&state.db, username, &body.password, &body.role).await {
        Ok(u) => u,
        Err(e) => {
            return if e.to_string().contains("UNIQUE") {
                err(StatusCode::CONFLICT, "username already exists")
            } else {
                internal(e)
            }
        }
    };
    for iid in &body.instance_ids {
        let (uid, iid) = (created.id.clone(), iid.clone());
        let _ = state
            .db
            .call(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO user_instances (user_id, instance_id) VALUES (?1, ?2)",
                    [uid, iid],
                )
            })
            .await;
    }
    db::audit(
        &state.db,
        &user.username,
        "user_create",
        &created.username,
        &format!("role {}", created.role),
    )
    .await;
    Json(json!({ "id": created.id, "username": created.username })).into_response()
}

#[derive(serde::Deserialize)]
struct RoleBody {
    role: String,
}

/// Promote / demote a user (multi-admin support). Two invariants keep
/// the panel administrable: the last admin can never be demoted, and a
/// no-op change is rejected so the audit trail only records real
/// transitions. Self-demotion is allowed when another admin exists —
/// the role is re-read from the DB on every request, so it takes effect
/// on the demoted admin's very next call.
async fn users_set_role(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(body): Json<RoleBody>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    if !matches!(body.role.as_str(), "admin" | "user") {
        return err(StatusCode::BAD_REQUEST, "role must be admin or user");
    }
    let target_id = id.clone();
    let target: anyhow::Result<Option<User>> = state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT * FROM users WHERE id = ?1",
                [target_id],
                User::from_row,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .await;
    let target = match target {
        Ok(Some(t)) => t,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => return internal(e),
    };
    if target.role == body.role {
        return err(StatusCode::BAD_REQUEST, "user already has that role");
    }
    if target.is_admin() {
        let admins: anyhow::Result<i64> = state
            .db
            .call(|c| {
                c.query_row("SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| {
                    r.get(0)
                })
            })
            .await;
        match admins {
            Ok(n) if n <= 1 => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "cannot demote the last admin — promote someone else first",
                )
            }
            Ok(_) => {}
            Err(e) => return internal(e),
        }
    }
    let (uid, role) = (id, body.role.clone());
    match state
        .db
        .call(move |c| c.execute("UPDATE users SET role = ?1 WHERE id = ?2", [role, uid]))
        .await
    {
        Ok(_) => {
            db::audit(
                &state.db,
                &user.username,
                "user_role",
                &target.username,
                &format!("{} -> {}", target.role, body.role),
            )
            .await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn users_delete(
    State(state): State<Arc<PanelState>>,
    Extension(CurrentUser(user)): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = require_admin(&user) {
        return resp;
    }
    if id == user.id {
        return err(StatusCode::BAD_REQUEST, "refusing to delete yourself");
    }
    let target = id.clone();
    match state
        .db
        .call(move |c| c.execute("DELETE FROM users WHERE id = ?1", [id]))
        .await
    {
        Ok(n) if n > 0 => {
            db::audit(&state.db, &user.username, "user_delete", &target, "").await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(_) => err(StatusCode::NOT_FOUND, "no such user"),
        Err(e) => internal(e),
    }
}

// ------------------------------------------------------------- helpers --

/// Shared shape for start/stop: admin-only, resolve instance, run action,
/// audit on success.
async fn instance_action<F, Fut>(
    state: &Arc<PanelState>,
    user: &User,
    id: &str,
    audit_as: &str,
    action: F,
) -> axum::response::Response
where
    F: FnOnce(Arc<PanelState>, crate::db::Instance) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    if let Some(resp) = require_admin(user) {
        return resp;
    }
    let instance = match must_instance(state, id).await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let name = instance.name.clone();
    match action(state.clone(), instance).await {
        Ok(()) => {
            db::audit(&state.db, &user.username, audit_as, &name, "").await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("{e:#}")),
    }
}

fn require_admin(user: &User) -> Option<axum::response::Response> {
    (!user.is_admin()).then(|| err(StatusCode::FORBIDDEN, "admin only"))
}

async fn must_instance(
    state: &PanelState,
    id: &str,
) -> Result<crate::db::Instance, axum::response::Response> {
    match instances::get(&state.db, id).await {
        Ok(Some(i)) => Ok(i),
        Ok(None) => Err(err(StatusCode::NOT_FOUND, "no such instance")),
        Err(e) => Err(internal(e)),
    }
}

/// A valid argon2 PHC string for a throwaway password; only used to
/// equalize login timing for unknown usernames.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$bm90LWEtcmVhbC1zYWx0$2x1zd8nB1WrLWLLpDLZ48qBLgzuTzzNS3ZpdEEsBuRI";

fn err(code: StatusCode, msg: &str) -> axum::response::Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn internal(e: anyhow::Error) -> axum::response::Response {
    tracing::error!(error = %e, "panel internal error");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}
