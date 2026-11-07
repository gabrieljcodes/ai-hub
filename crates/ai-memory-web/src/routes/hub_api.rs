//! AI Hub REST API (`/api/v1/hub/...`) for multi-user team collaboration,
//! Plane-style tasks/modules/cycles/labels, interactive Git-backed Wiki
//! editing, and GitHub repository synchronization.

use std::fmt::Write as _;
use std::sync::Arc;

use ai_memory_core::{
    ActorContext, ApiCredential, ApiCredentialId, AuthLevel, AuthorizedViewer, NewUser, PagePath,
    ProjectId, Tier, UserId, UserRole, WorkspaceId, validate_human_password,
};
use ai_memory_store::{
    AccessMode, HubCycle, HubGithubItem, HubGithubLabel, HubGithubRepo, HubLabel, HubModule,
    HubProjectMember, HubProjectOverview, HubProjectRole, HubSettings, HubTask, HubTaskFilter,
    NewHubCycle, NewHubModule, NewHubTask, NewTaskGithubLink, ProjectAccess, ScopeResolutionError,
    StoreError, TokenPepper, UpdateHubCycle, UpdateHubModule, UpdateHubTask, UpsertGithubItem,
    api_key_preview, generate_api_key, lookup_existing_scope_guarded, users::hash_token,
};
use ai_memory_wiki::WritePageRequest;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::WebState;

/// Build the `/api/v1/hub` router.
pub(crate) fn build() -> Router<Arc<WebState>> {
    Router::new()
        .route("/me", get(get_me_handler))
        .route(
            "/me/api-keys",
            get(list_my_api_keys_handler).post(create_my_api_key_handler),
        )
        .route(
            "/me/api-keys/{key_id}",
            delete(revoke_my_api_key_handler).post(revoke_my_api_key_handler),
        )
        .route(
            "/me/api-keys/{key_id}/revoke",
            post(revoke_my_api_key_handler),
        )
        .route(
            "/settings",
            get(get_settings_handler).patch(update_settings_handler),
        )
        .route("/users", get(list_users_handler).post(create_user_handler))
        .route("/users/{user_id}", patch(update_user_handler))
        .route("/my-tasks", get(my_tasks_handler))
        .route(
            "/projects",
            get(list_projects_handler).post(create_project_handler),
        )
        .route(
            "/projects/{workspace}/{project}",
            get(get_project_bundle_handler).patch(update_project_handler),
        )
        .route(
            "/projects/{workspace}/{project}/members",
            get(list_project_members_handler).post(upsert_project_member_handler),
        )
        .route(
            "/projects/{workspace}/{project}/members/{username}",
            delete(remove_project_member_handler),
        )
        .route(
            "/projects/{workspace}/{project}/tasks",
            get(list_tasks_handler).post(create_task_handler),
        )
        .route(
            "/projects/{workspace}/{project}/tasks/{task_ref}",
            get(get_task_handler)
                .patch(update_task_handler)
                .delete(delete_task_handler),
        )
        .route(
            "/projects/{workspace}/{project}/tasks/{task_ref}/comments",
            post(add_task_comment_handler),
        )
        .route(
            "/projects/{workspace}/{project}/tasks/{task_ref}/github-links",
            post(link_task_github_handler),
        )
        .route(
            "/projects/{workspace}/{project}/modules",
            get(list_modules_handler).post(create_module_handler),
        )
        .route(
            "/projects/{workspace}/{project}/modules/{module_id}",
            patch(update_module_handler).delete(delete_module_handler),
        )
        .route(
            "/projects/{workspace}/{project}/cycles",
            get(list_cycles_handler).post(create_cycle_handler),
        )
        .route(
            "/projects/{workspace}/{project}/cycles/{cycle_id}",
            patch(update_cycle_handler).delete(delete_cycle_handler),
        )
        .route(
            "/projects/{workspace}/{project}/labels",
            get(list_labels_handler).post(create_label_handler),
        )
        .route(
            "/projects/{workspace}/{project}/labels/{label_id}",
            delete(delete_label_handler),
        )
        .route(
            "/projects/{workspace}/{project}/pages",
            post(write_wiki_page_handler),
        )
        .route(
            "/projects/{workspace}/{project}/pages/{*path}",
            delete(delete_wiki_page_handler),
        )
        .route(
            "/projects/{workspace}/{project}/tasks-sync-md",
            post(sync_tasks_to_markdown_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/repos",
            get(list_github_repos_handler).post(add_github_repo_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/repos/{repo_id}",
            delete(delete_github_repo_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/sync",
            post(sync_github_repos_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/items",
            get(list_github_items_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/issues",
            post(create_github_issue_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/items/{repo_id}/{number}/comments",
            post(post_github_comment_handler),
        )
        .route(
            "/projects/{workspace}/{project}/github/items/{repo_id}/{number}/import-task",
            post(import_github_item_as_task_handler),
        )
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn store_err(e: StoreError) -> Response {
    match e {
        StoreError::Duplicate(_) => json_err(StatusCode::CONFLICT, e.to_string()),
        StoreError::NotFound(msg) => json_err(StatusCode::NOT_FOUND, msg),
        StoreError::InvalidState(msg) => {
            if msg.contains("not found") || msg.contains("no such") {
                json_err(StatusCode::NOT_FOUND, msg)
            } else if msg.contains("forbidden") || msg.contains("permission") {
                json_err(StatusCode::FORBIDDEN, msg)
            } else {
                json_err(StatusCode::BAD_REQUEST, msg)
            }
        }
        other => {
            tracing::error!(error = %other, "hub store error");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal store error")
        }
    }
}

fn scope_err(err: ScopeResolutionError) -> Response {
    if err.is_bad_request() {
        json_err(StatusCode::BAD_REQUEST, err.to_string())
    } else if err.is_forbidden() {
        json_err(
            StatusCode::FORBIDDEN,
            "you do not have access to this project",
        )
    } else if err.is_not_found() {
        json_err(StatusCode::NOT_FOUND, "project not found")
    } else {
        json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

fn is_root(
    level: Option<Extension<AuthLevel>>,
    _viewer: Option<&Extension<AuthorizedViewer>>,
) -> bool {
    matches!(level.map(|Extension(l)| l), Some(AuthLevel::Root))
}

async fn resolve_project_for_access(
    state: &WebState,
    viewer: Option<&Extension<AuthorizedViewer>>,
    workspace: &str,
    project: &str,
    access: ProjectAccess,
) -> Result<(WorkspaceId, ProjectId), Response> {
    let viewer_id = viewer.map(|Extension(v)| v.user());
    let resolved =
        lookup_existing_scope_guarded(&state.reader, workspace, project, viewer_id, access)
            .await
            .map_err(scope_err)?;
    Ok((resolved.workspace_id, resolved.project_id))
}

async fn require_project_manager(
    state: &WebState,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<&Extension<AuthorizedViewer>>,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), Response> {
    let (ws_id, proj_id) =
        resolve_project_for_access(state, viewer, workspace, project, ProjectAccess::Read).await?;
    let root = is_root(level, viewer);
    let viewer_id = viewer.map(|Extension(v)| v.user());
    let role = state
        .reader
        .hub_user_role_on_project(proj_id, viewer_id, root)
        .await
        .map_err(store_err)?;
    match role {
        Some(r) if r.can_manage() => Ok((ws_id, proj_id)),
        _ => Err(json_err(
            StatusCode::FORBIDDEN,
            "project owner or admin role required",
        )),
    }
}

// ---------------------------------------------------------------------------
// 1. Me, Personal API Keys, Users, Instance Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HubMeResponse {
    authenticated: bool,
    user_id: Option<String>,
    username: Option<String>,
    name: Option<String>,
    email: Option<String>,
    role: String,
    is_root: bool,
    my_tasks_count: usize,
    settings: HubSettings,
    api_keys: Vec<ApiCredential>,
}

async fn get_me_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ActorContext>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
) -> Response {
    let root = is_root(level, viewer.as_ref());
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let actor = actor.map(|Extension(a)| a).unwrap_or_default();
    let settings = state.reader.hub_settings().await.unwrap_or(HubSettings {
        instance_name: "AI Hub".into(),
        allow_registration: true,
        default_project_access: "restricted".into(),
        updated_at: String::new(),
    });
    let mut username = actor.user.clone();
    let mut name = actor.name.clone();
    let mut email = actor.email.clone();
    let mut role_str = if root { "root" } else { "user" }.to_string();
    let mut api_keys = Vec::new();
    let mut my_tasks_count = 0usize;

    if let Some(id) = uid {
        if let Ok(Some(u)) = state.reader.find_user_by_id(id).await {
            username = Some(u.username);
            name = u.name;
            email = u.email;
            role_str = u.role.as_str().to_string();
        }
        if let Ok(keys) = state.reader.list_api_credentials_for_user(id).await {
            api_keys = keys;
        }
        if let Ok(tasks) = state.reader.list_user_tasks(id, root).await {
            my_tasks_count = tasks.len();
        }
    }

    Json(HubMeResponse {
        authenticated: uid.is_some() || username.is_some() || root,
        user_id: uid.map(|u| u.to_string()),
        username,
        name,
        email,
        role: role_str,
        is_root: root,
        my_tasks_count,
        settings,
        api_keys,
    })
    .into_response()
}

async fn list_my_api_keys_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
) -> Response {
    let Some(uid) = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()))
    else {
        return Json(Vec::<ApiCredential>::new()).into_response();
    };
    match state.reader.list_api_credentials_for_user(uid).await {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateApiKeyBody {
    label: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreatedApiKeyResponse {
    id: String,
    token: String,
    key: String,
    preview: String,
    label: String,
    created_at: String,
}

async fn create_my_api_key_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Json(body): Json<CreateApiKeyBody>,
) -> Response {
    let Some(uid) = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()))
    else {
        return json_err(
            StatusCode::UNAUTHORIZED,
            "sign in with a personal user account to create API keys",
        );
    };
    let pepper_str = match state.wiki.writer().ensure_auto_token_pepper().await {
        Ok(p) => p,
        Err(e) => return store_err(e),
    };
    let pepper = TokenPepper::new(pepper_str);
    let raw_key = match generate_api_key() {
        Ok(k) => k,
        Err(e) => return store_err(e),
    };
    let preview = api_key_preview(&raw_key);
    let hash = hash_token(&raw_key, &pepper);
    let label = body
        .label
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Personal Key".to_string());
    let cred_id = ApiCredentialId::new();
    match state
        .wiki
        .writer()
        .create_api_credential(cred_id, uid, label.clone(), hash, Some(preview.clone()))
        .await
    {
        Ok(id) => (
            StatusCode::CREATED,
            Json(CreatedApiKeyResponse {
                id: id.to_string(),
                token: raw_key.clone(),
                key: raw_key,
                preview,
                label,
                created_at: jiff::Timestamp::now().to_string(),
            }),
        )
            .into_response(),
        Err(e) => store_err(e),
    }
}

async fn revoke_my_api_key_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path(key_id): Path<String>,
) -> Response {
    let Some(uid) = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()))
    else {
        return json_err(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let Ok(cred_id) = key_id.parse::<ApiCredentialId>() else {
        return json_err(StatusCode::BAD_REQUEST, "invalid key id");
    };
    let owned = state
        .reader
        .list_api_credentials_for_user(uid)
        .await
        .map(|list| list.iter().any(|c| c.id == cred_id))
        .unwrap_or(false);
    if !owned {
        return json_err(StatusCode::NOT_FOUND, "api key not found");
    }
    match state.wiki.writer().revoke_api_credential(cred_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => store_err(e),
    }
}

async fn get_settings_handler(State(state): State<Arc<WebState>>) -> Response {
    match state.reader.hub_settings().await {
        Ok(s) => Json(s).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateSettingsBody {
    allow_registration: Option<bool>,
    default_project_access: Option<String>,
    instance_name: Option<String>,
}

async fn update_settings_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Json(body): Json<UpdateSettingsBody>,
) -> Response {
    if !is_root(level, viewer.as_ref()) {
        return json_err(StatusCode::FORBIDDEN, "instance admin role required");
    }
    match state
        .wiki
        .writer()
        .update_hub_settings(
            body.instance_name,
            body.allow_registration,
            body.default_project_access,
        )
        .await
    {
        Ok(s) => Json(s).into_response(),
        Err(e) => store_err(e),
    }
}

async fn list_users_handler(State(state): State<Arc<WebState>>) -> Response {
    match state.reader.list_users().await {
        Ok(users) => Json(users).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateUserBody {
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    password: String,
    #[serde(default)]
    role: Option<String>,
}

async fn create_user_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Json(body): Json<CreateUserBody>,
) -> Response {
    if !is_root(level, viewer.as_ref()) {
        return json_err(StatusCode::FORBIDDEN, "instance admin role required");
    }
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "username is required");
    }
    if let Err(e) = validate_human_password(&body.password, Some(&username), &[]) {
        return json_err(StatusCode::BAD_REQUEST, e.to_string());
    }
    let role = match body.role.as_deref() {
        Some("root") | Some("admin") => UserRole::Root,
        None | Some("user") => UserRole::User,
        Some(_) => return json_err(StatusCode::BAD_REQUEST, "role must be 'root' or 'user'"),
    };
    let phc = match ai_memory_store::password::hash_password(body.password).await {
        Ok(h) => h,
        Err(e) => return store_err(e),
    };
    let new_user = NewUser {
        username,
        name: body
            .name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        email: body
            .email
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    let (mut user, _) = match state.wiki.writer().register_human_user(new_user, phc).await {
        Ok(res) => res,
        Err(e) => return store_err(e),
    };
    if role == UserRole::Root
        && user.role != UserRole::Root
        && let Ok(updated) = state
            .wiki
            .writer()
            .admin_update_user(user.id, Some(UserRole::Root), None)
            .await
    {
        user = updated;
    }
    (StatusCode::CREATED, Json(user)).into_response()
}

#[derive(Debug, Deserialize)]
struct UpdateUserBody {
    role: Option<String>,
    disabled: Option<bool>,
}

async fn update_user_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path(user_id): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> Response {
    if !is_root(level, viewer.as_ref()) {
        return json_err(StatusCode::FORBIDDEN, "instance admin role required");
    }
    let Ok(uid) = user_id.parse::<UserId>() else {
        return json_err(StatusCode::BAD_REQUEST, "invalid user_id");
    };
    let role = match body.role.as_deref() {
        Some("root") | Some("admin") => Some(UserRole::Root),
        Some("user") => Some(UserRole::User),
        None => None,
        Some(_) => return json_err(StatusCode::BAD_REQUEST, "role must be 'root' or 'user'"),
    };
    match state
        .wiki
        .writer()
        .admin_update_user(uid, role, body.disabled)
        .await
    {
        Ok(u) => Json(u).into_response(),
        Err(e) => store_err(e),
    }
}

// ---------------------------------------------------------------------------
// 2. Projects & Team Access Control
// ---------------------------------------------------------------------------

async fn list_projects_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
) -> Response {
    let root = is_root(level, viewer.as_ref());
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    if !root && uid.is_none() {
        return Json(Vec::<HubProjectOverview>::new()).into_response();
    }
    match state.reader.list_hub_projects(uid, root).await {
        Ok(projects) => Json(projects).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateProjectBody {
    #[serde(default)]
    workspace: String,
    #[serde(alias = "name")]
    project: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    identifier_prefix: Option<String>,
    #[serde(default)]
    access_mode: Option<String>,
}

async fn create_project_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Json(body): Json<CreateProjectBody>,
) -> Response {
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let workspace = if body.workspace.trim().is_empty() {
        "default".to_string()
    } else {
        body.workspace.trim().to_string()
    };
    let project_name = body.project.trim().to_string();
    if project_name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "project name is required");
    }
    let access_mode = match body.access_mode.as_deref() {
        Some("open") => AccessMode::Open,
        Some("restricted") | None => AccessMode::Restricted,
        Some(_) => {
            return json_err(
                StatusCode::BAD_REQUEST,
                "access_mode must be 'open' or 'restricted'",
            );
        }
    };
    match state
        .wiki
        .writer()
        .create_hub_project(
            workspace.clone(),
            project_name.clone(),
            body.identifier_prefix,
            body.description.unwrap_or_default(),
            access_mode,
            uid,
        )
        .await
    {
        Ok((ws_id, proj_id)) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "workspace_id": ws_id.to_string(),
                "project_id": proj_id.to_string(),
                "workspace": workspace,
                "project": project_name,
            })),
        )
            .into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Serialize)]
struct ProjectBundleResponse {
    project: HubProjectOverview,
    caller_role: String,
    members: Vec<HubProjectMember>,
    modules: Vec<HubModule>,
    cycles: Vec<HubCycle>,
    labels: Vec<HubLabel>,
    github_repos: Vec<HubGithubRepo>,
}

async fn get_project_bundle_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let root = is_root(level, viewer.as_ref());
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));

    let proj_overview = match state
        .reader
        .get_hub_project(workspace, project, uid, root)
        .await
    {
        Ok(p) => p,
        Err(e) => return store_err(e),
    };
    let caller_role = proj_overview.my_role.as_str().to_string();
    let members = state
        .reader
        .list_project_members(proj_id)
        .await
        .unwrap_or_default();
    let modules = state.reader.list_modules(proj_id).await.unwrap_or_default();
    let cycles = state.reader.list_cycles(proj_id).await.unwrap_or_default();
    let labels = state.reader.list_labels(proj_id).await.unwrap_or_default();
    let github_repos = state
        .reader
        .list_github_repos(proj_id)
        .await
        .unwrap_or_default();

    Json(ProjectBundleResponse {
        project: proj_overview,
        caller_role,
        members,
        modules,
        cycles,
        labels,
        github_repos,
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct UpdateProjectBody {
    description: Option<String>,
    identifier_prefix: Option<String>,
    access_mode: Option<String>,
}

async fn update_project_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<UpdateProjectBody>,
) -> Response {
    let (_ws_id, proj_id) =
        match require_project_manager(&state, level, viewer.as_ref(), &workspace, &project).await {
            Ok(ids) => ids,
            Err(resp) => return resp,
        };
    let access_mode = match body.access_mode.as_deref() {
        Some("open") => Some(AccessMode::Open),
        Some("restricted") => Some(AccessMode::Restricted),
        None => None,
        Some(_) => {
            return json_err(
                StatusCode::BAD_REQUEST,
                "access_mode must be 'open' or 'restricted'",
            );
        }
    };
    match state
        .wiki
        .writer()
        .update_hub_project(
            proj_id,
            body.description,
            body.identifier_prefix,
            access_mode,
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => store_err(e),
    }
}

async fn list_project_members_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.list_project_members(proj_id).await {
        Ok(members) => Json(members).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct UpsertProjectMemberBody {
    username: String,
    role: String,
}

async fn upsert_project_member_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<UpsertProjectMemberBody>,
) -> Response {
    let actor_uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.as_ref().map(|Extension(v)| v.user()));
    let (_ws_id, proj_id) =
        match require_project_manager(&state, level, viewer.as_ref(), &workspace, &project).await {
            Ok(ids) => ids,
            Err(resp) => return resp,
        };
    let role = match body.role.trim().to_ascii_lowercase().as_str() {
        "owner" => HubProjectRole::Owner,
        "admin" => HubProjectRole::Admin,
        "write" | "member" => HubProjectRole::Write,
        "read" | "viewer" => HubProjectRole::Read,
        _ => {
            return json_err(
                StatusCode::BAD_REQUEST,
                "role must be 'owner', 'admin', 'write', or 'read'",
            );
        }
    };
    match state
        .wiki
        .writer()
        .set_project_member_role(
            proj_id,
            body.username.trim().to_string(),
            role.as_str().to_string(),
            actor_uid,
        )
        .await
    {
        Ok(member) => Json(member).into_response(),
        Err(e) => store_err(e),
    }
}

async fn remove_project_member_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, username)): Path<(String, String, String)>,
) -> Response {
    let actor_uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.as_ref().map(|Extension(v)| v.user()));
    let (_ws_id, proj_id) =
        match require_project_manager(&state, level, viewer.as_ref(), &workspace, &project).await {
            Ok(ids) => ids,
            Err(resp) => return resp,
        };
    match state
        .wiki
        .writer()
        .remove_project_member(proj_id, username, actor_uid)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "member not found"),
        Err(e) => store_err(e),
    }
}

// ---------------------------------------------------------------------------
// 3. Tasks, Subtasks, Comments & Activity Log
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct TaskQueryParams {
    state: Option<String>,
    priority: Option<String>,
    #[serde(alias = "assignee")]
    assignee_username: Option<String>,
    assignee_id: Option<String>,
    module_id: Option<String>,
    cycle_id: Option<String>,
    label: Option<String>,
    #[serde(alias = "q")]
    search: Option<String>,
    #[serde(default)]
    include_archived: bool,
}

async fn my_tasks_handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
) -> Response {
    let root = is_root(level, viewer.as_ref());
    let Some(uid) = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()))
    else {
        return Json(Vec::<HubTask>::new()).into_response();
    };
    match state.reader.list_user_tasks(uid, root).await {
        Ok(tasks) => Json(tasks).into_response(),
        Err(e) => store_err(e),
    }
}

async fn list_tasks_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(q): Query<TaskQueryParams>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let filter = HubTaskFilter {
        state: q.state.filter(|s| !s.is_empty()),
        priority: q.priority.filter(|s| !s.is_empty()),
        assignee_id: q.assignee_id.and_then(|s| s.parse().ok()),
        assignee_username: q.assignee_username.filter(|s| !s.is_empty()),
        module_id: q.module_id.filter(|s| !s.is_empty()),
        cycle_id: q.cycle_id.filter(|s| !s.is_empty()),
        label: q.label.filter(|s| !s.is_empty()),
        search: q.search.filter(|s| !s.is_empty()),
        include_archived: q.include_archived,
    };
    match state.reader.list_project_tasks(proj_id, filter).await {
        Ok(tasks) => Json(tasks).into_response(),
        Err(e) => store_err(e),
    }
}

async fn create_task_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(mut input): Json<NewHubTask>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    if input.created_by.is_none() {
        input.created_by = user_id
            .map(|Extension(u)| u)
            .or_else(|| viewer.map(|Extension(v)| v.user()));
    }
    match state.wiki.writer().create_task(ws_id, proj_id, input).await {
        Ok(detail) => (StatusCode::CREATED, Json(detail)).into_response(),
        Err(e) => store_err(e),
    }
}

async fn get_task_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, task_ref)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.get_task_detail(proj_id, task_ref).await {
        Ok(detail) => Json(detail).into_response(),
        Err(e) => store_err(e),
    }
}

async fn update_task_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, task_ref)): Path<(String, String, String)>,
    Json(mut input): Json<UpdateHubTask>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    if input.actor_id.is_none() {
        input.actor_id = user_id
            .map(|Extension(u)| u)
            .or_else(|| viewer.map(|Extension(v)| v.user()));
    }
    match state
        .wiki
        .writer()
        .update_task(proj_id, task_ref, input)
        .await
    {
        Ok(detail) => Json(detail).into_response(),
        Err(e) => store_err(e),
    }
}

async fn delete_task_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, task_ref)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.wiki.writer().delete_task(proj_id, task_ref).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "task not found"),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct AddCommentBody {
    #[serde(alias = "body_md")]
    body: String,
    #[serde(default)]
    author_agent: Option<String>,
}

async fn add_task_comment_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, task_ref)): Path<(String, String, String)>,
    Json(body): Json<AddCommentBody>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    match state
        .wiki
        .writer()
        .add_task_comment(proj_id, task_ref, uid, body.author_agent, body.body)
        .await
    {
        Ok(comment) => (StatusCode::CREATED, Json(comment)).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct LinkTaskGithubBody {
    #[serde(default)]
    repo_id: Option<String>,
    #[serde(default)]
    repo_full_name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    number: i64,
    #[serde(default)]
    relation: Option<String>,
}

async fn link_task_github_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, task_ref)): Path<(String, String, String)>,
    Json(body): Json<LinkTaskGithubBody>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let repo_full_name = if let Some(full) = body.repo_full_name.filter(|s| !s.trim().is_empty()) {
        full
    } else if let Some(rid) = body.repo_id.filter(|s| !s.trim().is_empty()) {
        match state.reader.get_github_repo_with_token(proj_id, rid).await {
            Ok(r) => r.repo.full_name,
            Err(e) => return store_err(e),
        }
    } else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "repo_id or repo_full_name is required",
        );
    };
    let link = NewTaskGithubLink {
        repo_full_name,
        kind: body.kind.unwrap_or_else(|| "issue".into()),
        number: body.number,
        title: None,
        state: None,
        html_url: None,
        relation: body.relation,
    };
    match state
        .wiki
        .writer()
        .link_task_github(proj_id, task_ref, link, uid, None)
        .await
    {
        Ok(detail) => Json(detail).into_response(),
        Err(e) => store_err(e),
    }
}

// ---------------------------------------------------------------------------
// 4. Modules, Cycles & Labels
// ---------------------------------------------------------------------------

async fn list_modules_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.list_modules(proj_id).await {
        Ok(modules) => Json(modules).into_response(),
        Err(e) => store_err(e),
    }
}

async fn create_module_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(mut input): Json<NewHubModule>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    if input.created_by.is_none() {
        input.created_by = user_id
            .map(|Extension(u)| u)
            .or_else(|| viewer.map(|Extension(v)| v.user()));
    }
    match state
        .wiki
        .writer()
        .create_module(ws_id, proj_id, input)
        .await
    {
        Ok(m) => (StatusCode::CREATED, Json(m)).into_response(),
        Err(e) => store_err(e),
    }
}

async fn update_module_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, module_id)): Path<(String, String, String)>,
    Json(input): Json<UpdateHubModule>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state
        .wiki
        .writer()
        .update_module(proj_id, module_id, input)
        .await
    {
        Ok(m) => Json(m).into_response(),
        Err(e) => store_err(e),
    }
}

async fn delete_module_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, module_id)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.wiki.writer().delete_module(proj_id, module_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "module not found"),
        Err(e) => store_err(e),
    }
}

async fn list_cycles_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.list_cycles(proj_id).await {
        Ok(cycles) => Json(cycles).into_response(),
        Err(e) => store_err(e),
    }
}

async fn create_cycle_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(mut input): Json<NewHubCycle>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    if input.created_by.is_none() {
        input.created_by = user_id
            .map(|Extension(u)| u)
            .or_else(|| viewer.map(|Extension(v)| v.user()));
    }
    match state
        .wiki
        .writer()
        .create_cycle(ws_id, proj_id, input)
        .await
    {
        Ok(c) => (StatusCode::CREATED, Json(c)).into_response(),
        Err(e) => store_err(e),
    }
}

async fn update_cycle_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, cycle_id)): Path<(String, String, String)>,
    Json(input): Json<UpdateHubCycle>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state
        .wiki
        .writer()
        .update_cycle(proj_id, cycle_id, input)
        .await
    {
        Ok(c) => Json(c).into_response(),
        Err(e) => store_err(e),
    }
}

async fn delete_cycle_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, cycle_id)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.wiki.writer().delete_cycle(proj_id, cycle_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "cycle not found"),
        Err(e) => store_err(e),
    }
}

async fn list_labels_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.list_labels(proj_id).await {
        Ok(labels) => Json(labels).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct LabelBody {
    name: String,
    #[serde(default = "default_label_color")]
    color: String,
    #[serde(default)]
    description: String,
}

fn default_label_color() -> String {
    "#6366f1".into()
}

async fn create_label_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<LabelBody>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state
        .wiki
        .writer()
        .create_label(proj_id, body.name, body.color, body.description)
        .await
    {
        Ok(l) => (StatusCode::CREATED, Json(l)).into_response(),
        Err(e) => store_err(e),
    }
}

async fn delete_label_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, label_id)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.wiki.writer().delete_label(proj_id, label_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "label not found"),
        Err(e) => store_err(e),
    }
}

// ---------------------------------------------------------------------------
// 5. Git-Backed Wiki Markdown Live Editor & Task Board Markdown Commit
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WriteWikiPageBody {
    path: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    body: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    commit_message: Option<String>,
}

async fn write_wiki_page_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<Extension<ActorContext>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<WriteWikiPageBody>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let actor = actor.map(|Extension(a)| a).unwrap_or_default();

    let clean_path = if body.path.trim().ends_with(".md") {
        body.path.trim().to_string()
    } else {
        format!("{}.md", body.path.trim())
    };
    let page_path = match PagePath::new(clean_path.clone()) {
        Ok(p) => p,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let tier = body
        .tier
        .as_deref()
        .and_then(|t| t.parse::<Tier>().ok())
        .unwrap_or(Tier::Semantic);
    let mut fm = serde_json::Map::new();
    if let Some(t) = body
        .title
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        fm.insert("title".into(), serde_json::Value::String(t.to_string()));
    }
    if let Some(k) = body
        .kind
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        fm.insert("kind".into(), serde_json::Value::String(k.to_string()));
    }
    let req = WritePageRequest {
        workspace_id: ws_id,
        project_id: proj_id,
        path: page_path,
        frontmatter: serde_json::Value::Object(fm),
        body: body.body,
        tier,
        pinned: body.pinned,
        title: body.title,
        admission_ctx: None,
        author_id: uid,
        evidence: Vec::new(),
        actor: actor.clone(),
    };
    let page_id = match state.wiki.write_page(req).await {
        Ok(id) => id,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let commit_msg = body.commit_message.unwrap_or_else(|| {
        let who = actor.user.as_deref().unwrap_or("web");
        format!("wiki({workspace}/{project}): update {clean_path} by {who}")
    });
    let commit_oid = state
        .wiki
        .commit_all(&commit_msg)
        .ok()
        .flatten()
        .map(|oid| oid.to_string());
    Json(serde_json::json!({
        "page_id": page_id.to_string(),
        "path": clean_path,
        "commit_oid": commit_oid,
    }))
    .into_response()
}

async fn delete_wiki_page_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<Extension<ActorContext>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let actor = actor.map(|Extension(a)| a).unwrap_or_default();
    let page_path = match PagePath::new(path.clone()) {
        Ok(p) => p,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match state
        .wiki
        .delete_page(ws_id, proj_id, &page_path, None, uid)
        .await
    {
        Ok(()) => {
            let who = actor.user.as_deref().unwrap_or("web");
            let _ = state.wiki.commit_all(&format!(
                "wiki({workspace}/{project}): delete {path} by {who}"
            ));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => json_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

/// Compile the project's tasks, modules, and cycles into `tasks/board.md`
/// inside the Git-backed Wiki and create a Git commit.
async fn sync_tasks_to_markdown_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<Extension<ActorContext>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let actor = actor.map(|Extension(a)| a).unwrap_or_default();

    let tasks = state
        .reader
        .list_project_tasks(proj_id, HubTaskFilter::default())
        .await
        .unwrap_or_default();
    let modules = state.reader.list_modules(proj_id).await.unwrap_or_default();
    let cycles = state.reader.list_cycles(proj_id).await.unwrap_or_default();

    let mut md = String::new();
    let _ = writeln!(md, "# {project} — Task Board & Engineering Roadmap\n");
    let _ = writeln!(
        md,
        "> Automatically synchronized from AI Hub Tasks into the Git-backed Wiki.\n"
    );

    if !modules.is_empty() {
        let _ = writeln!(md, "## Modules\n");
        let _ = writeln!(md, "| Module | Status | Lead | Target Date | Progress |");
        let _ = writeln!(md, "| :--- | :--- | :--- | :--- | :--- |");
        for m in &modules {
            let lead = m.lead_username.as_deref().unwrap_or("—");
            let target = m.target_date.as_deref().unwrap_or("—");
            let _ = writeln!(
                md,
                "| **{}** | `{}` | {} | {} | {}/{} done |",
                m.name, m.status, lead, target, m.task_done, m.task_total
            );
        }
        let _ = writeln!(md);
    }

    if !cycles.is_empty() {
        let _ = writeln!(md, "## Cycles / Sprints\n");
        for c in &cycles {
            let dates = match (&c.start_date, &c.end_date) {
                (Some(s), Some(e)) => format!("{s} → {e}"),
                (Some(s), None) => format!("from {s}"),
                _ => "unscheduled".into(),
            };
            let _ = writeln!(
                md,
                "- **{}** (`{}` · {}) — {}/{} tasks completed",
                c.name, c.status, dates, c.task_done, c.task_total
            );
        }
        let _ = writeln!(md);
    }

    let states = [
        ("in_progress", "In Progress"),
        ("in_review", "In Review"),
        ("todo", "Todo"),
        ("backlog", "Backlog"),
        ("done", "Done"),
        ("cancelled", "Cancelled"),
    ];
    for (st, heading) in states {
        let group: Vec<&HubTask> = tasks.iter().filter(|t| t.state == st).collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(md, "## {heading} ({})\n", group.len());
        for t in group {
            let assignees = if t.assignees.is_empty() {
                "unassigned".to_string()
            } else {
                t.assignees
                    .iter()
                    .map(|a| format!("@{}", a.username))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let due = t
                .due_date
                .as_deref()
                .map(|d| format!(" · due {d}"))
                .unwrap_or_default();
            let module = t
                .module_name
                .as_deref()
                .map(|m| format!(" · module: {m}"))
                .unwrap_or_default();
            let _ = writeln!(
                md,
                "- **[{}]** {} (`{}` · {}{}{})",
                t.key, t.title, t.priority, assignees, due, module
            );
        }
        let _ = writeln!(md);
    }

    let Ok(page_path) = PagePath::new("tasks/board.md".to_string()) else {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "invalid board path");
    };
    let req = WritePageRequest {
        workspace_id: ws_id,
        project_id: proj_id,
        path: page_path,
        frontmatter: serde_json::json!({
            "title": format!("{project} Task Board"),
            "tags": ["tasks", "roadmap", "board"]
        }),
        body: md,
        tier: Tier::Working,
        pinned: true,
        title: Some(format!("{project} Task Board")),
        admission_ctx: None,
        author_id: uid,
        evidence: Vec::new(),
        actor: actor.clone(),
    };
    let page_id = match state.wiki.write_page(req).await {
        Ok(id) => id,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let who = actor.user.as_deref().unwrap_or("hub");
    let commit_oid = state
        .wiki
        .commit_all(&format!(
            "tasks({workspace}/{project}): snapshot task board ({} tasks) by {who}",
            tasks.len()
        ))
        .ok()
        .flatten()
        .map(|oid| oid.to_string());

    Json(serde_json::json!({
        "page_id": page_id.to_string(),
        "path": "tasks/board.md",
        "task_count": tasks.len(),
        "commit_oid": commit_oid,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// 6. GitHub Repository Integration (Read + Write + Task Linking)
// ---------------------------------------------------------------------------

async fn list_github_repos_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state.reader.list_github_repos(proj_id).await {
        Ok(repos) => Json(repos).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct AddGithubRepoBody {
    #[serde(alias = "repo")]
    full_name: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default, alias = "token")]
    access_token: Option<String>,
}

async fn add_github_repo_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<AddGithubRepoBody>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    match state
        .wiki
        .writer()
        .add_github_repo(
            ws_id,
            proj_id,
            body.full_name,
            body.default_branch,
            body.access_token,
            uid,
        )
        .await
    {
        Ok(view) => (StatusCode::CREATED, Json(view)).into_response(),
        Err(e) => store_err(e),
    }
}

async fn delete_github_repo_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, repo_id)): Path<(String, String, String)>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state
        .wiki
        .writer()
        .remove_github_repo(proj_id, repo_id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_err(StatusCode::NOT_FOUND, "github repo not found"),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct SyncGithubBody {
    #[serde(default)]
    repo_id: Option<String>,
}

/// Sync Issues and PRs from GitHub REST API (`https://api.github.com/repos/{owner}/{repo}/issues`).
pub(crate) async fn sync_project_github_repos(
    state: &WebState,
    proj_id: ProjectId,
    only_repo_id: Option<&str>,
) -> Result<(usize, Vec<String>), String> {
    let repos = state
        .reader
        .list_github_repos(proj_id)
        .await
        .map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .user_agent("ai-hub-github-sync/1.0")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let mut total_synced = 0usize;
    let mut errors = Vec::new();

    for repo in repos {
        if let Some(target) = only_repo_id
            && repo.id != target
            && repo.full_name != target
        {
            continue;
        }
        let creds = state
            .reader
            .get_github_repo_with_token(proj_id, repo.id.clone())
            .await
            .ok();
        let token = creds.and_then(|c| c.access_token);

        let url = format!(
            "https://api.github.com/repos/{}/{}/issues?state=all&per_page=100&sort=updated&direction=desc",
            repo.owner, repo.repo
        );
        let mut req = client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token.as_deref().filter(|s| !s.is_empty()) {
            req = req.bearer_auth(t);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{}: network error: {e}", repo.full_name);
                let _ = state
                    .wiki
                    .writer()
                    .record_github_sync_result(
                        repo.id.clone(),
                        proj_id,
                        repo.full_name.clone(),
                        Vec::new(),
                        Some(msg.clone()),
                    )
                    .await;
                errors.push(msg);
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("{}: GitHub API returned {status}: {text}", repo.full_name);
            let _ = state
                .wiki
                .writer()
                .record_github_sync_result(
                    repo.id.clone(),
                    proj_id,
                    repo.full_name.clone(),
                    Vec::new(),
                    Some(msg.clone()),
                )
                .await;
            errors.push(msg);
            continue;
        }
        let raw_items: Vec<serde_json::Value> = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("{}: invalid JSON response: {e}", repo.full_name);
                errors.push(msg);
                continue;
            }
        };

        let mut parsed = Vec::with_capacity(raw_items.len());
        for item in raw_items {
            let Some(number) = item.get("number").and_then(serde_json::Value::as_i64) else {
                continue;
            };
            let is_pr = item.get("pull_request").is_some();
            let kind = if is_pr { "pr" } else { "issue" }.to_string();
            let raw_state = item
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("open");
            let merged_at = item
                .get("pull_request")
                .and_then(|pr| pr.get("merged_at"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let state_str = if merged_at.is_some() {
                "merged".to_string()
            } else {
                raw_state.to_string()
            };
            let title = item
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let body = item
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let author_login = item
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("ghost")
                .to_string();
            let author_avatar_url = item
                .get("user")
                .and_then(|u| u.get("avatar_url"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let html_url = item
                .get("html_url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let labels: Vec<HubGithubLabel> = item
                .get("labels")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| {
                            let name = l.get("name")?.as_str()?.to_string();
                            let color = l
                                .get("color")
                                .and_then(serde_json::Value::as_str)
                                .map(|c| format!("#{}", c.trim_start_matches('#')))
                                .unwrap_or_else(|| "#6366f1".into());
                            Some(HubGithubLabel { name, color })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let assignees: Vec<String> = item
                .get("assignees")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|u| u.get("login").and_then(serde_json::Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let comments_count = item
                .get("comments")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let gh_created_at = item
                .get("created_at")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let gh_updated_at = item
                .get("updated_at")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);

            parsed.push(UpsertGithubItem {
                kind,
                number,
                state: state_str,
                title,
                body,
                author_login,
                author_avatar_url: if author_avatar_url.is_empty() {
                    None
                } else {
                    Some(author_avatar_url)
                },
                html_url,
                labels,
                assignees,
                comments_count,
                head_ref: None,
                base_ref: None,
                draft: item
                    .get("draft")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                gh_created_at,
                gh_updated_at,
            });
        }

        match state
            .wiki
            .writer()
            .record_github_sync_result(
                repo.id.clone(),
                proj_id,
                repo.full_name.clone(),
                parsed,
                None,
            )
            .await
        {
            Ok(count) => total_synced += count,
            Err(e) => errors.push(format!("{}: store error: {e}", repo.full_name)),
        }
    }

    Ok((total_synced, errors))
}

async fn sync_github_repos_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    body: Option<Json<SyncGithubBody>>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let only_repo = body.and_then(|Json(b)| b.repo_id);
    match sync_project_github_repos(&state, proj_id, only_repo.as_deref()).await {
        Ok((synced, errors)) => Json(serde_json::json!({
            "synced_items": synced,
            "errors": errors,
        }))
        .into_response(),
        Err(e) => json_err(StatusCode::BAD_GATEWAY, e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct GithubItemsQuery {
    repo_id: Option<String>,
    kind: Option<String>,
    state: Option<String>,
    search: Option<String>,
}

async fn list_github_items_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(q): Query<GithubItemsQuery>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Read,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    match state
        .reader
        .list_github_items(
            proj_id,
            q.repo_id.filter(|s| !s.is_empty()),
            q.kind.filter(|s| !s.is_empty()),
            q.state.filter(|s| !s.is_empty()),
            q.search.filter(|s| !s.is_empty()),
        )
        .await
    {
        Ok(items) => Json(items).into_response(),
        Err(e) => store_err(e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateGithubIssueBody {
    repo_id: String,
    title: String,
    #[serde(default, alias = "body_md")]
    body: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    task_ref: Option<String>,
}

async fn create_github_issue_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
    Json(body): Json<CreateGithubIssueBody>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let repo_with_tok = match state
        .reader
        .get_github_repo_with_token(proj_id, body.repo_id.clone())
        .await
    {
        Ok(r) => r,
        Err(e) => return store_err(e),
    };
    let Some(token) = repo_with_tok.access_token.filter(|s| !s.trim().is_empty()) else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "this GitHub repository has no personal access token configured for write operations",
        );
    };
    let client = match reqwest::Client::builder()
        .user_agent("ai-hub-github-sync/1.0")
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let url = format!(
        "https://api.github.com/repos/{}/{}/issues",
        repo_with_tok.repo.owner, repo_with_tok.repo.repo
    );
    let resp = match client
        .post(&url)
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&serde_json::json!({
            "title": body.title,
            "body": body.body,
            "labels": body.labels,
        }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return json_err(
                StatusCode::BAD_GATEWAY,
                format!("GitHub request failed: {e}"),
            );
        }
    };
    if !resp.status().is_success() {
        let st = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return json_err(
            StatusCode::BAD_GATEWAY,
            format!("GitHub API returned {st}: {text}"),
        );
    }
    let item: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let number = item
        .get("number")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let html_url = item
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let author_login = item
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("github")
        .to_string();
    let now_iso = jiff::Timestamp::now().to_string();
    let upsert = UpsertGithubItem {
        kind: "issue".into(),
        number,
        state: "open".into(),
        title: body.title.clone(),
        body: body.body,
        author_login,
        author_avatar_url: None,
        html_url: html_url.clone(),
        labels: body
            .labels
            .into_iter()
            .map(|name| HubGithubLabel {
                name,
                color: "#6366f1".into(),
            })
            .collect(),
        assignees: Vec::new(),
        comments_count: 0,
        head_ref: None,
        base_ref: None,
        draft: false,
        gh_created_at: Some(now_iso.clone()),
        gh_updated_at: Some(now_iso),
    };
    let _ = state
        .wiki
        .writer()
        .record_github_sync_result(
            repo_with_tok.repo.id.clone(),
            proj_id,
            repo_with_tok.repo.full_name.clone(),
            vec![upsert],
            None,
        )
        .await;

    if let Some(task_ref) = body.task_ref.filter(|s| !s.trim().is_empty()) {
        let link = NewTaskGithubLink {
            repo_full_name: repo_with_tok.repo.full_name,
            kind: "issue".into(),
            number,
            title: Some(body.title),
            state: Some("open".into()),
            html_url: Some(html_url.clone()),
            relation: Some("closes".into()),
        };
        let _ = state
            .wiki
            .writer()
            .link_task_github(proj_id, task_ref, link, uid, None)
            .await;
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "number": number,
            "html_url": html_url,
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct GithubCommentBody {
    #[serde(alias = "body_md")]
    body: String,
}

async fn post_github_comment_handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, repo_id, number)): Path<(String, String, String, i64)>,
    Json(body): Json<GithubCommentBody>,
) -> Response {
    let (_ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let repo_with_tok = match state
        .reader
        .get_github_repo_with_token(proj_id, repo_id)
        .await
    {
        Ok(r) => r,
        Err(e) => return store_err(e),
    };
    let Some(token) = repo_with_tok.access_token.filter(|s| !s.trim().is_empty()) else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "this GitHub repository has no personal access token configured for write operations",
        );
    };
    let client = match reqwest::Client::builder()
        .user_agent("ai-hub-github-sync/1.0")
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let url = format!(
        "https://api.github.com/repos/{}/{}/issues/{number}/comments",
        repo_with_tok.repo.owner, repo_with_tok.repo.repo
    );
    let resp = match client
        .post(&url)
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&serde_json::json!({ "body": body.body }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return json_err(
                StatusCode::BAD_GATEWAY,
                format!("GitHub request failed: {e}"),
            );
        }
    };
    if !resp.status().is_success() {
        let st = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return json_err(
            StatusCode::BAD_GATEWAY,
            format!("GitHub API returned {st}: {text}"),
        );
    }
    let val: serde_json::Value = resp.json().await.unwrap_or_default();
    (StatusCode::CREATED, Json(val)).into_response()
}

async fn import_github_item_as_task_handler(
    State(state): State<Arc<WebState>>,
    user_id: Option<Extension<UserId>>,
    viewer: Option<Extension<AuthorizedViewer>>,
    Path((workspace, project, repo_id, number)): Path<(String, String, String, i64)>,
) -> Response {
    let (ws_id, proj_id) = match resolve_project_for_access(
        &state,
        viewer.as_ref(),
        &workspace,
        &project,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let uid = user_id
        .map(|Extension(u)| u)
        .or_else(|| viewer.map(|Extension(v)| v.user()));
    let items: Vec<HubGithubItem> = match state
        .reader
        .list_github_items(proj_id, Some(repo_id), None, None, None)
        .await
    {
        Ok(list) => list,
        Err(e) => return store_err(e),
    };
    let Some(item) = items.into_iter().find(|i| i.number == number) else {
        return json_err(StatusCode::NOT_FOUND, "github item not found in cache");
    };
    let task_state = if item.state == "closed" || item.state == "merged" {
        "done"
    } else {
        "todo"
    };
    let desc = format!(
        "Imported from GitHub {} [{}#{}]({})\n\n{}",
        item.kind.to_uppercase(),
        item.repo_full_name,
        item.number,
        item.html_url,
        item.body
    );
    let create = NewHubTask {
        title: format!("[#{}] {}", item.number, item.title),
        description: Some(desc),
        state: Some(task_state.into()),
        priority: Some("medium".into()),
        module_id: None,
        cycle_id: None,
        parent_task_id: None,
        created_by: uid,
        created_by_agent: None,
        start_date: None,
        due_date: None,
        estimate_points: None,
        assignees: Vec::new(),
        labels: Vec::new(),
        wiki_pages: Vec::new(),
        github_links: vec![NewTaskGithubLink {
            repo_full_name: item.repo_full_name,
            kind: item.kind,
            number: item.number,
            title: Some(item.title),
            state: Some(item.state),
            html_url: Some(item.html_url),
            relation: Some("tracks".into()),
        }],
    };
    match state
        .wiki
        .writer()
        .create_task(ws_id, proj_id, create)
        .await
    {
        Ok(detail) => (StatusCode::CREATED, Json(detail)).into_response(),
        Err(e) => store_err(e),
    }
}
