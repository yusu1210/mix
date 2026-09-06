use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mix_core::{AdapterKind, Error, ErrorCode, MixService, ProfileInput};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct ServerState {
    pub service: Arc<MixService>,
    token: Arc<str>,
}

pub struct RunningServer {
    pub address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl RunningServer {
    pub async fn shutdown(mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        let _ = self.task.await;
    }
}

pub fn router(service: Arc<MixService>, token: String, ui_root: Option<PathBuf>) -> Router {
    let state = ServerState {
        service,
        token: token.into(),
    };
    let api = Router::new()
        .route("/health", get(health))
        .route("/state", get(state_snapshot))
        .route("/discovery", get(discovery))
        .route("/diagnostics", get(diagnostics))
        .route("/sessions", get(sessions))
        .route(
            "/workspaces",
            get(workspaces)
                .post(bind_workspace)
                .delete(delete_workspace),
        )
        .route("/switch", post(switch))
        .route("/activity/switch-back", post(switch_back))
        .route("/recovery/interrupted", post(recover))
        .route("/apps", post(register_client))
        .route(
            "/profiles",
            post(add_profile).patch(edit_profile).delete(delete_profile),
        )
        .route("/profiles/import", post(import_profile))
        .route("/accounts/capture", post(capture_account))
        .route("/accounts/enroll", post(enroll_account_request))
        .route("/accounts/enroll/status", get(enroll_status))
        .route("/accounts/repair/active-login", post(repair_account))
        .route("/run", post(run_client))
        .route("/sessions/resume", post(resume_session))
        .route("/native/open", post(open_native))
        .fallback(api_not_found)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(normalize_api_errors))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));

    let mut app = Router::new().nest("/api", api).with_state(state.clone());
    if let Some(root) = ui_root.filter(|root| root.join("index.html").is_file()) {
        app = app.fallback_service(
            ServeDir::new(&root)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(root.join("index.html"))),
        );
    } else {
        app = app.fallback(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Mix visual interface is not installed with this binary",
            )
        });
    }
    app.layer(cors_layer())
    .layer(SetResponseHeaderLayer::if_not_present(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self'",
        ),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    ))
}

fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            origin.to_str().is_ok_and(allowed_origin)
        }))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-mix-token")])
}

async fn api_not_found() -> ApiError {
    ApiError(Error::new(
        ErrorCode::MixNotFound,
        "the requested local API endpoint does not exist",
    ))
}

pub async fn start(
    service: Arc<MixService>,
    host: std::net::IpAddr,
    port: u16,
    token: String,
    ui_root: Option<PathBuf>,
) -> mix_core::Result<RunningServer> {
    if !host.is_loopback() {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            "Mix local HTTP service only accepts a loopback address",
        ));
    }
    if !(32..=512).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(Error::new(
            ErrorCode::MixValidationError,
            "Mix local HTTP service requires a strong header-safe token",
        ));
    }
    let listener = TcpListener::bind((host, port))
        .await
        .map_err(|error| Error::io("cannot bind Mix local service", error))?;
    let address = listener
        .local_addr()
        .map_err(|error| Error::io("cannot inspect Mix local service", error))?;
    let (shutdown, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, router(service, token, ui_root))
            .with_graceful_shutdown(async move {
                let _ = receiver.await;
            })
            .await;
        if let Err(error) = result {
            eprintln!("Mix local service stopped: {error}");
        }
    });
    Ok(RunningServer {
        address,
        task,
        shutdown: Some(shutdown),
    })
}

async fn authorize(State(state): State<ServerState>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get("x-mix-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| constant_time_equal(value.as_bytes(), state.token.as_bytes()));
    if !authorized {
        return ApiError(Error::new(
            ErrorCode::MixAuthRequired,
            "local service authentication failed",
        ))
        .into_response();
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            if !allowed_origin(origin) {
                return ApiError(Error::new(
                    ErrorCode::MixOriginDenied,
                    "request origin is not allowed",
                ))
                .into_response();
            }
        }
    }
    next.run(request).await
}

async fn normalize_api_errors(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if response.status().is_success()
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        return response;
    }
    let status = if response.status() == StatusCode::UNPROCESSABLE_ENTITY {
        StatusCode::BAD_REQUEST
    } else {
        response.status()
    };
    (
        status,
        Json(json!({
            "error": "the local API request is invalid",
            "code": ErrorCode::MixRequestInvalid.as_str(),
            "details": {},
        })),
    )
        .into_response()
}

fn allowed_origin(origin: &str) -> bool {
    if matches!(origin, "tauri://localhost" | "http://tauri.localhost") {
        return true;
    }
    let Ok(uri) = origin.parse::<Uri>() else {
        return false;
    };
    if uri.scheme_str() != Some("http") || uri.port_u16().is_none() {
        return false;
    }
    uri.host().is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

struct ApiError(Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            ErrorCode::MixNotFound
            | ErrorCode::MixCredentialNotFound
            | ErrorCode::MixSessionUnavailable => StatusCode::NOT_FOUND,
            ErrorCode::MixConflict
            | ErrorCode::MixSwitchRecoveryRequired
            | ErrorCode::MixSwitchRolledBack
            | ErrorCode::MixSwitchVerificationFailed
            | ErrorCode::MixActiveAccountUnmanaged
            | ErrorCode::MixAccountReauthRequired
            | ErrorCode::MixAccountLocalRepairUnavailable
            | ErrorCode::MixCodexFileAuthRequired
            | ErrorCode::MixProcessStillRunning => StatusCode::CONFLICT,
            ErrorCode::MixAuthRequired => StatusCode::UNAUTHORIZED,
            ErrorCode::MixOriginDenied => StatusCode::FORBIDDEN,
            ErrorCode::MixLocalFailure
            | ErrorCode::MixInternalError
            | ErrorCode::MixCredentialUnavailable
            | ErrorCode::MixSwitchRecoveryFailed => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        (
            status,
            Json(json!({
                "error": self.0.message,
                "code": self.0.code.as_str(),
                "details": self.0.details,
            })),
        )
            .into_response()
    }
}

impl From<Error> for ApiError {
    fn from(value: Error) -> Self {
        Self(value)
    }
}

async fn health() -> Json<Value> {
    Json(json!({"product":"mix","status":"ready","api_version":1}))
}

async fn state_snapshot(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(serde_json::to_value(state.service.state()?)?))
}

async fn discovery(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(serde_json::to_value(state.service.discovery()?)?))
}

async fn diagnostics(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.diagnostics()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionQuery {
    app: Option<String>,
    query: Option<String>,
    adapter: Option<AdapterKind>,
    recovery: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    projects: bool,
}
const fn default_limit() -> usize {
    100
}

async fn sessions(
    State(state): State<ServerState>,
    Query(query): Query<SessionQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.projects {
        if query.app.is_some()
            || query.query.is_some()
            || query.adapter.is_some()
            || query.recovery.is_some()
            || query.offset != 0
            || query.limit != default_limit()
        {
            return Err(mix_core::Error::invalid(
                "project session summaries cannot be combined with session filters",
            )
            .into());
        }
        return Ok(Json(serde_json::to_value(
            state.service.project_sessions()?,
        )?));
    }
    Ok(Json(serde_json::to_value(state.service.sessions(
        query.app.as_deref(),
        query.query.as_deref(),
        query.adapter,
        query.recovery.as_deref(),
        query.limit,
        query.offset,
    )?)?))
}

async fn workspaces(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(serde_json::to_value(
        state.service.state()?.workspaces,
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchBody {
    app: String,
    profile: String,
}

async fn switch(
    State(state): State<ServerState>,
    Json(body): Json<SwitchBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(serde_json::to_value(
        state.service.switch(&body.app, &body.profile)?,
    )?))
}

async fn recover(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.recover_interrupted_switch()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchBackBody {
    activity: String,
}

async fn switch_back(
    State(state): State<ServerState>,
    Json(body): Json<SwitchBackBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.service.switch_back_from_activity(&body.activity)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterBody {
    name: String,
    adapter: AdapterKind,
    live_dir: PathBuf,
}

async fn register_client(
    State(state): State<ServerState>,
    Json(body): Json<RegisterBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .service
                .register_client(&body.name, body.adapter, body.live_dir)?,
        ),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBody {
    app: String,
    name: Option<String>,
    label: Option<String>,
}

async fn capture_account(
    State(state): State<ServerState>,
    Json(body): Json<CaptureBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(state.service.capture_current_account(
            &body.app,
            body.name.as_deref(),
            body.label.as_deref(),
        )?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddProfileBody {
    app: String,
    name: Option<String>,
    label: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, PathBuf>,
    #[serde(default)]
    secret_files: BTreeMap<String, mix_core::SecretRef>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secrets: BTreeMap<String, mix_core::SecretRef>,
    auth_strategy: Option<mix_core::AuthStrategy>,
}

async fn add_profile(
    State(state): State<ServerState>,
    Json(body): Json<AddProfileBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(state.service.add_profile(
            &body.app,
            body.name.as_deref(),
            ProfileInput {
                files: body.files,
                secret_files: body.secret_files,
                env: body.env,
                secrets: body.secrets,
                label: body.label,
                auth_strategy: body.auth_strategy,
            },
        )?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportProfileBody {
    app: String,
    name: Option<String>,
    label: Option<String>,
    files: Vec<String>,
}

async fn import_profile(
    State(state): State<ServerState>,
    Json(body): Json<ImportProfileBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(state.service.capture_profile(
            &body.app,
            body.name.as_deref(),
            &body.files,
            body.label.as_deref(),
        )?),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentBody {
    app: String,
    repair_profile: Option<String>,
}

async fn enroll_account_request(
    State(state): State<ServerState>,
    Json(body): Json<EnrollmentBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.start_account_enrollment(
        &body.app,
        body.repair_profile.as_deref(),
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentQuery {
    id: String,
}

async fn enroll_status(
    State(state): State<ServerState>,
    Query(query): Query<EnrollmentQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.account_enrollment_status(&query.id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairBody {
    app: String,
    profile: String,
}

async fn repair_account(
    State(state): State<ServerState>,
    Json(body): Json<RepairBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state
            .service
            .repair_active_login(&body.app, &body.profile)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBody {
    app: String,
    profile: String,
    cwd: Option<String>,
}

async fn run_client(
    State(state): State<ServerState>,
    Json(body): Json<RunBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.run(
        &body.app,
        &body.profile,
        body.cwd.as_deref(),
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditProfileBody {
    app: String,
    profile: String,
    label: Option<String>,
}

async fn edit_profile(
    State(state): State<ServerState>,
    Json(body): Json<EditProfileBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.edit_profile(
        &body.app,
        &body.profile,
        body.label.as_deref(),
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteProfileBody {
    app: String,
    profile: String,
    #[serde(default)]
    detach_workspaces: bool,
}

async fn delete_profile(
    State(state): State<ServerState>,
    Json(body): Json<DeleteProfileBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.delete_profile(
        &body.app,
        &body.profile,
        body.detach_workspaces,
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBody {
    path: PathBuf,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
    name: Option<String>,
}

async fn bind_workspace(
    State(state): State<ServerState>,
    Json(body): Json<WorkspaceBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .service
                .bind_workspace(body.path, body.bindings, body.name)?,
        ),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDeleteBody {
    workspace: String,
}

async fn delete_workspace(
    State(state): State<ServerState>,
    Json(body): Json<WorkspaceDeleteBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.service.delete_workspace(&body.workspace)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeBody {
    app: String,
    resume_id: String,
}

async fn resume_session(
    State(state): State<ServerState>,
    Json(body): Json<ResumeBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.service.resume_session(&body.app, &body.resume_id)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeOpenBody {
    app: String,
    cwd: Option<String>,
}

async fn open_native(
    State(state): State<ServerState>,
    Json(body): Json<NativeOpenBody>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.service.open_native(&body.app, body.cwd.as_deref())?,
    ))
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        ApiError(Error::from(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    #[test]
    fn token_comparison_requires_exact_length_and_content() {
        assert!(constant_time_equal(b"token", b"token"));
        assert!(!constant_time_equal(b"token", b"tokeN"));
        assert!(!constant_time_equal(b"token", b"token-long"));
    }

    #[test]
    fn mutation_origins_require_an_exact_loopback_host() {
        assert!(allowed_origin("http://127.0.0.1:17666"));
        assert!(allowed_origin("http://[::1]:17666"));
        assert!(allowed_origin("http://localhost:1420"));
        assert!(allowed_origin("tauri://localhost"));
        assert!(!allowed_origin("https://localhost:17666"));
        assert!(!allowed_origin("http://localhost.example.com:17666"));
        assert!(!allowed_origin("http://127.0.0.1.example.com:17666"));
        assert!(!allowed_origin("http://127.0.0.1:17666@evil.example"));
    }

    #[tokio::test]
    async fn every_response_has_local_web_security_headers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-store"))
        );
        assert_eq!(
            response.headers().get(header::REFERRER_POLICY),
            Some(&HeaderValue::from_static("no-referrer"))
        );
        assert_eq!(
            response.headers().get(header::X_CONTENT_TYPE_OPTIONS),
            Some(&HeaderValue::from_static("nosniff"))
        );
        assert_eq!(
            response.headers().get(header::X_FRAME_OPTIONS),
            Some(&HeaderValue::from_static("DENY"))
        );
        assert!(response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some_and(|value| value
                .as_bytes()
                .windows(22)
                .any(|part| part == b"frame-ancestors 'none'")));
    }

    #[tokio::test]
    async fn desktop_webview_preflight_is_allowed_without_exposing_the_token() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/state")
                    .header(header::ORIGIN, "tauri://localhost")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "x-mix-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("tauri://localhost"))
        );
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .is_some_and(|value| value.to_str().is_ok_and(|value| value
                .split(',')
                .any(|header| header.trim().eq_ignore_ascii_case("x-mix-token")))));
    }

    #[tokio::test]
    async fn cross_origin_preflight_rejects_non_loopback_origins() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/state")
                    .header(header::ORIGIN, "https://evil.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "x-mix-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[tokio::test]
    async fn malformed_api_json_uses_the_stable_error_contract() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/switch")
                    .header("x-mix-token", "test-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(value["code"], "MIX_REQUEST_INVALID");
    }

    #[tokio::test]
    async fn api_get_routes_are_authenticated_and_do_not_fall_back_to_the_ui() {
        let directory = tempfile::tempdir().expect("tempdir");
        let ui = directory.path().join("ui");
        std::fs::create_dir(&ui).expect("UI directory");
        std::fs::write(ui.join("index.html"), "visual interface").expect("UI index");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));

        let unauthorized = router(service.clone(), "test-token".into(), Some(ui.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = router(service, "test-token".into(), Some(ui))
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("x-mix-token", "test-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(authorized.status(), StatusCode::OK);
        let body = axum::body::to_bytes(authorized.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&body).expect("JSON health response");
        assert_eq!(value["product"], "mix");
    }

    #[tokio::test]
    async fn bound_server_routes_api_requests_before_the_ui_fallback() {
        let directory = tempfile::tempdir().expect("tempdir");
        let ui = directory.path().join("ui");
        std::fs::create_dir(&ui).expect("UI directory");
        std::fs::write(ui.join("index.html"), "visual interface").expect("UI index");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let server = start(
            service,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            0,
            "test-token-012345678901234567890123".into(),
            Some(ui),
        )
        .await
        .expect("server");
        let mut stream = tokio::net::TcpStream::connect(server.address)
            .await
            .expect("connect");
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        server.shutdown().await;
        let response = String::from_utf8(response).expect("HTTP response");
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(!response.contains("visual interface"), "{response}");
    }

    #[tokio::test]
    async fn unknown_api_fields_are_rejected_instead_of_ignored() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/switch")
                    .header("x-mix-token", "test-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"app":"codex","profile":"work","typo":true}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(value["code"], "MIX_REQUEST_INVALID");
    }

    #[tokio::test]
    async fn unknown_auth_strategies_are_rejected_at_the_api_boundary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), None)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/profiles")
                    .header("x-mix-token", "test-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"app":"claude","auth_strategy":"password"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(value["code"], "MIX_REQUEST_INVALID");
    }

    #[tokio::test]
    async fn unknown_api_routes_never_fall_back_to_the_visual_interface() {
        let directory = tempfile::tempdir().expect("tempdir");
        let ui = directory.path().join("ui");
        std::fs::create_dir(&ui).expect("UI directory");
        std::fs::write(ui.join("index.html"), "visual interface").expect("UI index");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let response = router(service, "test-token".into(), Some(ui))
            .oneshot(
                Request::builder()
                    .uri("/api/does-not-exist")
                    .header("x-mix-token", "test-token")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&body).expect("JSON error");
        assert_eq!(value["code"], "MIX_NOT_FOUND");
    }

    #[tokio::test]
    async fn server_rejects_weak_tokens_before_binding() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service =
            Arc::new(MixService::new(directory.path().join("config.json")).expect("service"));
        let result = start(
            service,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            0,
            "short-token".into(),
            None,
        )
        .await;
        assert!(matches!(
            result,
            Err(error) if error.code == ErrorCode::MixValidationError
        ));
    }
}
