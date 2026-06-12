//! Panel user auth: argon2 password hashing + opaque server-side
//! session cookies.
//!
//! Sessions are random 64-hex ids stored in SQLite — nothing is signed or
//! decoded client-side, so there is no key to manage and revocation is a
//! row delete. Cookie is `HttpOnly; SameSite=Strict; Path=/`, which is
//! also the CSRF story: no cross-site request carries it. `Secure` is
//! intentionally not set — TLS is terminated by whatever fronts the panel
//! (Caddy/cloudflared), and the cookie must work on plain-HTTP localhost
//! and LAN/VPN deployments.

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use std::sync::Arc;

use crate::db::{random_token, Db, User};
use crate::PanelState;

pub const SESSION_COOKIE: &str = "athen_admin_session";
const SESSION_DAYS: i64 = 30;

/// Hash on the blocking pool — argon2 takes ~100ms by design.
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        // 16 bytes of OS randomness for the salt (uuid v4 = getrandom
        // under the hood; avoids wiring rand_core features).
        let salt = SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes())
            .map_err(|e| anyhow::anyhow!("argon2 salt: {e}"))?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("argon2 hash: {e}"))
    })
    .await?
}

pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

/// First boot: if no users exist, create `admin`. Password comes from
/// `ATHEN_ADMIN_PASSWORD`, else is generated and printed ONCE to stdout
/// (the operator is watching the first start; it never hits the log file
/// because it goes to stdout directly, not through tracing).
pub async fn bootstrap_admin(db: &Db) -> anyhow::Result<()> {
    let count: i64 = db
        .call(|c| c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)))
        .await?;
    if count > 0 {
        return Ok(());
    }
    let (password, generated) = match std::env::var("ATHEN_ADMIN_PASSWORD") {
        Ok(p) if !p.is_empty() => (p, false),
        _ => (random_token()[..20].to_string(), true),
    };
    create_user(db, "admin", &password, "admin").await?;
    if generated {
        println!("\n==============================================================");
        println!("  First start: created panel user 'admin'");
        println!("  Password: {password}");
        println!("  (shown once — change it or set ATHEN_ADMIN_PASSWORD)");
        println!("==============================================================\n");
    } else {
        tracing::info!("First start: created panel user 'admin' from ATHEN_ADMIN_PASSWORD");
    }
    Ok(())
}

pub async fn create_user(
    db: &Db,
    username: &str,
    password: &str,
    role: &str,
) -> anyhow::Result<User> {
    let user = User {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_string(),
        password_hash: hash_password(password.to_string()).await?,
        role: role.to_string(),
        created_at: Utc::now().to_rfc3339(),
        notify_url: String::new(),
    };
    let u = user.clone();
    db.call(move |c| {
        c.execute(
            "INSERT INTO users (id, username, password_hash, role, created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![u.id, u.username, u.password_hash, u.role, u.created_at],
        )
    })
    .await?;
    Ok(user)
}

pub async fn user_by_name(db: &Db, username: &str) -> anyhow::Result<Option<User>> {
    let name = username.to_string();
    db.call(move |c| {
        c.query_row(
            "SELECT * FROM users WHERE username = ?1",
            [name],
            User::from_row,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
    })
    .await
}

/// Create a session row and return the cookie value to set.
pub async fn new_session(db: &Db, user_id: &str) -> anyhow::Result<String> {
    let id = random_token();
    let uid = user_id.to_string();
    let sid = id.clone();
    db.call(move |c| {
        c.execute(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![
                sid,
                uid,
                Utc::now().to_rfc3339(),
                (Utc::now() + Duration::days(SESSION_DAYS)).to_rfc3339()
            ],
        )
    })
    .await?;
    Ok(id)
}

pub async fn delete_session(db: &Db, session_id: &str) -> anyhow::Result<()> {
    let sid = session_id.to_string();
    db.call(move |c| c.execute("DELETE FROM sessions WHERE id = ?1", [sid]))
        .await?;
    Ok(())
}

/// Resolve a session cookie value to its (unexpired) user.
pub async fn user_for_session(db: &Db, session_id: &str) -> anyhow::Result<Option<User>> {
    let sid = session_id.to_string();
    db.call(move |c| {
        c.query_row(
            "SELECT u.* FROM users u JOIN sessions s ON s.user_id = u.id \
             WHERE s.id = ?1 AND s.expires_at > ?2",
            rusqlite::params![sid, Utc::now().to_rfc3339()],
            User::from_row,
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })
    })
    .await
}

pub fn session_cookie_value(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == SESSION_COOKIE).then(|| v.to_string())
    })
}

pub fn set_session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_DAYS * 24 * 3600
    )
}

pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// Authenticated user, attached as a request extension by the middleware.
#[derive(Clone)]
pub struct CurrentUser(pub User);

/// Like [`require_session`], but a browser without a session is sent to
/// the panel login page instead of getting a bare 401. For the web-UI
/// passthrough routes (`/i/{instance}/`), where the client is a human in
/// a browser, not a fetch call that can render an error.
pub async fn require_session_or_login(
    State(state): State<Arc<PanelState>>,
    req: Request,
    next: Next,
) -> Result<Response, Response> {
    let to_login = req.method() == axum::http::Method::GET;
    // Carry the requested path through the login round-trip so deep links
    // (`/i/{id}/…` shared with a user) land back where they pointed. The
    // panel JS validates `next` as a same-origin `/i/…` path before using it.
    let wanted = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_default();
    match require_session(State(state), req, next).await {
        Ok(resp) => Ok(resp),
        Err(StatusCode::UNAUTHORIZED) if to_login => {
            let to = format!("/?next={}", percent_encode_query(&wanted));
            Ok(axum::response::Redirect::temporary(&to).into_response())
        }
        Err(code) => Err(code.into_response()),
    }
}

/// Minimal percent-encoding for a query-param value: keeps unreserved
/// chars and `/`, encodes everything else (incl. `&`, `=`, `?`, `#`).
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Middleware: every route behind this requires a live session.
pub async fn require_session(
    State(state): State<Arc<PanelState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(sid) = session_cookie_value(req.headers()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    match user_for_session(&state.db, &sid).await {
        Ok(Some(user)) => {
            // Per-user request budget — stops runaway clients/scripts.
            if !state.buckets.allow(&user.id, std::time::Instant::now()) {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            req.extensions_mut().insert(CurrentUser(user));
            Ok(next.run(req).await)
        }
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(e) => {
            tracing::error!(error = %e, "session lookup failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Can `user` reach `instance_id`? Admins reach everything; users need a
/// `user_instances` grant.
pub async fn user_can_access(db: &Db, user: &User, instance_id: &str) -> anyhow::Result<bool> {
    if user.is_admin() {
        return Ok(true);
    }
    let (uid, iid) = (user.id.clone(), instance_id.to_string());
    let n: i64 = db
        .call(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM user_instances WHERE user_id = ?1 AND instance_id = ?2",
                [uid, iid],
                |r| r.get(0),
            )
        })
        .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::percent_encode_query;

    #[test]
    fn percent_encode_keeps_paths_and_escapes_delimiters() {
        assert_eq!(percent_encode_query("/i/abc-123/"), "/i/abc-123/");
        assert_eq!(
            percent_encode_query("/i/x/?a=1&b=2#f"),
            "/i/x/%3Fa%3D1%26b%3D2%23f"
        );
        assert_eq!(percent_encode_query("a b"), "a%20b");
    }
}
