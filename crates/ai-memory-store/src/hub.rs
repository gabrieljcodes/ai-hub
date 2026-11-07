//! AI Hub multi-user collaboration, Plane-style Tasks/Modules/Cycles, and GitHub store layer.

use ai_memory_core::{NewUser, ProjectId, User, UserId, UserRole, WorkspaceId};
use base64::Engine as _;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::{AccessMode, GrantLevel, ReaderPool, WriterHandle};

fn now_us() -> i64 {
    Timestamp::now().as_microsecond()
}

fn format_us_iso(us: i64) -> String {
    Timestamp::from_microsecond(us)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_string()
}

/// Derive a 2-5 character uppercase project prefix from its slug/name (e.g. `ai-hub` -> `AIH`).
#[must_use]
pub fn derive_identifier_prefix(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return "PRJ".to_string();
    }
    if parts.len() >= 2 {
        let mut out = String::new();
        for (i, part) in parts.iter().take(4).enumerate() {
            if i == 0 && parts.len() == 2 && part.len() >= 2 {
                out.extend(part.chars().take(2).map(|c| c.to_ascii_uppercase()));
            } else if let Some(ch) = part.chars().next() {
                out.push(ch.to_ascii_uppercase());
            }
        }
        if out.len() >= 2 {
            return out;
        }
    }
    let clean: String = parts[0]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if clean.len() < 2 {
        format!("{clean}P")
    } else {
        clean
    }
}

/// Instance-level settings for AI Hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSettings {
    /// Display name of the hosted instance.
    pub instance_name: String,
    /// Whether self-service account registration (`/register`) is open.
    pub allow_registration: bool,
    /// Default access mode for newly created projects (`restricted` or `open`).
    pub default_project_access: String,
    /// Last updated timestamp (ISO-8601).
    pub updated_at: String,
}

/// Role of a user within a specific project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubProjectRole {
    /// Creator of the project or instance root.
    Owner,
    /// Project manager who can invite members, edit settings, and write.
    Admin,
    /// Team member who can read and write memory, tasks, and modules.
    Write,
    /// Viewer who can read project memory, tasks, and modules.
    Read,
}

impl HubProjectRole {
    /// Stable string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Write => "write",
            Self::Read => "read",
        }
    }

    /// True if this role permits writing pages, tasks, and modules.
    #[must_use]
    pub const fn can_write(self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Write)
    }

    /// True if this role permits managing project members, settings, and repos.
    #[must_use]
    pub const fn can_manage(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }
}

/// One member of a project team.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubProjectMember {
    /// User UUID.
    pub user_id: UserId,
    /// Username.
    pub username: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Optional email.
    pub email: Option<String>,
    /// Project role (`owner`, `admin`, `write`, `read`).
    pub role: HubProjectRole,
    /// ISO-8601 timestamp when granted/created.
    pub granted_at: String,
}

/// Rich overview of a project for the AI Hub dashboard and project switcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubProjectOverview {
    /// Workspace UUID.
    pub workspace_id: WorkspaceId,
    /// Workspace slug.
    pub workspace_name: String,
    /// Project UUID.
    pub project_id: ProjectId,
    /// Project slug.
    pub project_name: String,
    /// Human description.
    pub description: String,
    /// Task key prefix (e.g. `AIH`).
    pub identifier_prefix: String,
    /// Access mode (`restricted` or `open`).
    pub access_mode: String,
    /// Creator user ID if recorded.
    pub created_by: Option<UserId>,
    /// Creator username if recorded.
    pub created_by_username: Option<String>,
    /// The caller's effective role in this project.
    pub my_role: HubProjectRole,
    /// Number of team members with explicit access (including owner).
    pub member_count: usize,
    /// Count of live wiki pages.
    pub page_count: usize,
    /// Total non-archived tasks.
    pub task_total: usize,
    /// Completed tasks count.
    pub task_done: usize,
    /// In-progress + in-review tasks count.
    pub task_in_progress: usize,
    /// Todo + backlog tasks count.
    pub task_todo: usize,
    /// Count of linked GitHub repositories.
    pub github_repo_count: usize,
    /// Count of open synced GitHub issues.
    pub github_open_issues: usize,
    /// Count of open synced GitHub PRs.
    pub github_open_prs: usize,
    /// ISO-8601 creation time.
    pub created_at: String,
}

/// Project label for categorizing tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubLabel {
    /// Label UUID string.
    pub id: String,
    /// Project UUID.
    pub project_id: ProjectId,
    /// Label name.
    pub name: String,
    /// Hex color (e.g. `#6366f1`).
    pub color: String,
    /// Optional description.
    pub description: String,
}

/// Plane-style Module within a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubModule {
    /// Module UUID string.
    pub id: String,
    /// Workspace UUID.
    pub workspace_id: WorkspaceId,
    /// Project UUID.
    pub project_id: ProjectId,
    /// Module name.
    pub name: String,
    /// Markdown description.
    pub description: String,
    /// Status (`backlog`, `planned`, `in_progress`, `paused`, `completed`, `cancelled`).
    pub status: String,
    /// Lead user ID.
    pub lead_id: Option<UserId>,
    /// Lead username.
    pub lead_username: Option<String>,
    /// Lead display name.
    pub lead_name: Option<String>,
    /// Start date (`YYYY-MM-DD`).
    pub start_date: Option<String>,
    /// Target completion date (`YYYY-MM-DD`).
    pub target_date: Option<String>,
    /// Creator user ID.
    pub created_by: Option<UserId>,
    /// Created timestamp (ISO-8601).
    pub created_at: String,
    /// Updated timestamp (ISO-8601).
    pub updated_at: String,
    /// Total tasks in this module.
    pub task_total: usize,
    /// Completed (`done`) tasks in this module.
    pub task_done: usize,
}

/// Input to create a Module.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewHubModule {
    /// Module name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Status (`backlog`, `planned`, `in_progress`, `paused`, `completed`, `cancelled`).
    pub status: Option<String>,
    /// Lead user ID or username.
    pub lead_username: Option<String>,
    /// Start date (`YYYY-MM-DD`).
    pub start_date: Option<String>,
    /// Target date (`YYYY-MM-DD`).
    pub target_date: Option<String>,
    /// Creator user ID.
    pub created_by: Option<UserId>,
}

/// Input to update a Module.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateHubModule {
    /// Updated name.
    pub name: Option<String>,
    /// Updated description.
    pub description: Option<String>,
    /// Updated status.
    pub status: Option<String>,
    /// Updated lead username (empty string clears).
    pub lead_username: Option<String>,
    /// Updated start date (empty string clears).
    pub start_date: Option<String>,
    /// Updated target date (empty string clears).
    pub target_date: Option<String>,
}

/// Plane-style Cycle / Sprint within a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubCycle {
    /// Cycle UUID string.
    pub id: String,
    /// Workspace UUID.
    pub workspace_id: WorkspaceId,
    /// Project UUID.
    pub project_id: ProjectId,
    /// Cycle name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Status (`upcoming`, `active`, `completed`, `archived`).
    pub status: String,
    /// Start date (`YYYY-MM-DD`).
    pub start_date: Option<String>,
    /// End date (`YYYY-MM-DD`).
    pub end_date: Option<String>,
    /// Creator user ID.
    pub created_by: Option<UserId>,
    /// Created timestamp (ISO-8601).
    pub created_at: String,
    /// Updated timestamp (ISO-8601).
    pub updated_at: String,
    /// Total tasks in this cycle.
    pub task_total: usize,
    /// Completed (`done`) tasks in this cycle.
    pub task_done: usize,
}

/// Input to create a Cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewHubCycle {
    /// Cycle name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Status (`upcoming`, `active`, `completed`, `archived`).
    pub status: Option<String>,
    /// Start date.
    pub start_date: Option<String>,
    /// End date.
    pub end_date: Option<String>,
    /// Creator user ID.
    pub created_by: Option<UserId>,
}

/// Input to update a Cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateHubCycle {
    /// Name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Status.
    pub status: Option<String>,
    /// Start date.
    pub start_date: Option<String>,
    /// End date.
    pub end_date: Option<String>,
}

/// Assignee attached to a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTaskAssignee {
    /// User ID.
    pub user_id: UserId,
    /// Username.
    pub username: String,
    /// Display name.
    pub name: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// ISO-8601 assigned timestamp.
    pub assigned_at: String,
}

/// Link between an AI Hub task and a GitHub Issue or PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTaskGithubLink {
    /// Task ID.
    pub task_id: String,
    /// Cached GitHub item ID if present.
    pub github_item_id: Option<String>,
    /// `owner/repo` string.
    pub repo_full_name: String,
    /// `issue` or `pr`.
    pub kind: String,
    /// Issue or PR number.
    pub number: i64,
    /// Cached title.
    pub title: String,
    /// State (`open`, `closed`, `merged`).
    pub state: String,
    /// HTML URL on GitHub.
    pub html_url: String,
    /// Relation (`tracks`, `fixes`, `closes`, `implements`, `related`).
    pub relation: String,
}

/// Input to link a GitHub Issue/PR to a Task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTaskGithubLink {
    /// `owner/repo`.
    pub repo_full_name: String,
    /// `issue` or `pr`.
    pub kind: String,
    /// Number (`#42`).
    pub number: i64,
    /// Title (optional; auto-filled if item is in cache).
    pub title: Option<String>,
    /// State (optional; defaults to `open`).
    pub state: Option<String>,
    /// HTML URL (optional; auto-derived from `repo_full_name` and `number`).
    pub html_url: Option<String>,
    /// Relation (`tracks`, `fixes`, `closes`, `implements`, `related`).
    pub relation: Option<String>,
}

/// Comment on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTaskComment {
    /// Comment ID.
    pub id: String,
    /// Task ID.
    pub task_id: String,
    /// Author user ID.
    pub author_id: Option<UserId>,
    /// Author username.
    pub author_username: Option<String>,
    /// Author display name.
    pub author_name: Option<String>,
    /// Optional AI agent harness that posted the comment.
    pub author_agent: Option<String>,
    /// Markdown body.
    pub body: String,
    /// ISO-8601 created timestamp.
    pub created_at: String,
    /// ISO-8601 updated timestamp.
    pub updated_at: String,
}

/// Activity log entry on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTaskActivity {
    /// Activity ID.
    pub id: String,
    /// Task ID.
    pub task_id: String,
    /// Actor user ID.
    pub actor_id: Option<UserId>,
    /// Actor username.
    pub actor_username: Option<String>,
    /// Optional AI agent harness.
    pub actor_agent: Option<String>,
    /// Action verb (`created`, `state_changed`, `priority_changed`, etc.).
    pub action: String,
    /// Previous value.
    pub old_value: Option<String>,
    /// New value.
    pub new_value: Option<String>,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Plane-style Task / Work Item summary and card model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTask {
    /// Task UUID string.
    pub id: String,
    /// Workspace UUID.
    pub workspace_id: WorkspaceId,
    /// Workspace slug.
    pub workspace_name: String,
    /// Project UUID.
    pub project_id: ProjectId,
    /// Project slug.
    pub project_name: String,
    /// Project identifier prefix (e.g. `AIH`).
    pub identifier_prefix: String,
    /// Per-project sequence number (`1`, `2`, ...).
    pub sequence_id: i64,
    /// Human-friendly key (`AIH-1`).
    pub key: String,
    /// Task title.
    pub title: String,
    /// Markdown description.
    pub description: String,
    /// State (`backlog`, `todo`, `in_progress`, `in_review`, `done`, `cancelled`).
    pub state: String,
    /// Priority (`urgent`, `high`, `medium`, `low`, `none`).
    pub priority: String,
    /// Optional Module ID.
    pub module_id: Option<String>,
    /// Optional Module name.
    pub module_name: Option<String>,
    /// Optional Cycle ID.
    pub cycle_id: Option<String>,
    /// Optional Cycle name.
    pub cycle_name: Option<String>,
    /// Optional parent task ID.
    pub parent_task_id: Option<String>,
    /// Creator UserId.
    pub created_by: Option<UserId>,
    /// Creator username.
    pub created_by_username: Option<String>,
    /// Creator display name.
    pub created_by_name: Option<String>,
    /// Optional AI agent harness that created the task.
    pub created_by_agent: Option<String>,
    /// Start date (`YYYY-MM-DD`).
    pub start_date: Option<String>,
    /// Due date (`YYYY-MM-DD`).
    pub due_date: Option<String>,
    /// Effort estimate points.
    pub estimate_points: Option<i64>,
    /// Board sort order.
    pub sort_order: f64,
    /// Completed timestamp (ISO-8601).
    pub completed_at: Option<String>,
    /// Archived timestamp (ISO-8601).
    pub archived_at: Option<String>,
    /// Created timestamp (ISO-8601).
    pub created_at: String,
    /// Updated timestamp (ISO-8601).
    pub updated_at: String,
    /// Assigned users.
    pub assignees: Vec<HubTaskAssignee>,
    /// Attached labels.
    pub labels: Vec<HubLabel>,
    /// Linked wiki page paths.
    pub wiki_pages: Vec<String>,
    /// Linked GitHub issues/PRs.
    pub github_links: Vec<HubTaskGithubLink>,
    /// Total comments count.
    pub comments_count: usize,
    /// Total subtasks count.
    pub subtasks_total: usize,
    /// Completed subtasks count.
    pub subtasks_done: usize,
}

/// Full task detail including comments, activity stream, and subtasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubTaskDetail {
    /// The task itself.
    pub task: HubTask,
    /// Chronological comments.
    pub comments: Vec<HubTaskComment>,
    /// Recent activity audit entries.
    pub activities: Vec<HubTaskActivity>,
    /// Direct child subtasks.
    pub subtasks: Vec<HubTask>,
}

/// Filter options for listing tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubTaskFilter {
    /// Filter by state (`backlog`, `todo`, `in_progress`, `in_review`, `done`, `cancelled`).
    pub state: Option<String>,
    /// Filter by priority (`urgent`, `high`, `medium`, `low`, `none`).
    pub priority: Option<String>,
    /// Filter by assignee user ID.
    pub assignee_id: Option<UserId>,
    /// Filter by assignee username.
    pub assignee_username: Option<String>,
    /// Filter by module ID (or name).
    pub module_id: Option<String>,
    /// Filter by cycle ID (or name).
    pub cycle_id: Option<String>,
    /// Filter by label ID or name.
    pub label: Option<String>,
    /// Search query over title, description, or sequence key.
    pub search: Option<String>,
    /// Include archived tasks.
    pub include_archived: bool,
}

/// Input to create a new Task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewHubTask {
    /// Task title.
    pub title: String,
    /// Markdown description.
    #[serde(default, alias = "description_md")]
    pub description: Option<String>,
    /// State (`backlog`, `todo`, `in_progress`, `in_review`, `done`, `cancelled`).
    #[serde(default)]
    pub state: Option<String>,
    /// Priority (`urgent`, `high`, `medium`, `low`, `none`).
    #[serde(default)]
    pub priority: Option<String>,
    /// Module ID or name.
    #[serde(default)]
    pub module_id: Option<String>,
    /// Cycle ID or name.
    #[serde(default)]
    pub cycle_id: Option<String>,
    /// Parent task UUID or key (`AIH-1`).
    #[serde(default, alias = "parent_id")]
    pub parent_task_id: Option<String>,
    /// Creator UserId.
    #[serde(default)]
    pub created_by: Option<UserId>,
    /// Creator AI agent name.
    #[serde(default)]
    pub created_by_agent: Option<String>,
    /// Start date (`YYYY-MM-DD`).
    #[serde(default)]
    pub start_date: Option<String>,
    /// Due date (`YYYY-MM-DD`).
    #[serde(default)]
    pub due_date: Option<String>,
    /// Estimate points.
    #[serde(default)]
    pub estimate_points: Option<i64>,
    /// Usernames or UUIDs of assignees.
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Label names or IDs.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Linked wiki page paths.
    #[serde(default, alias = "wiki_paths")]
    pub wiki_pages: Vec<String>,
    /// Linked GitHub issues/PRs.
    #[serde(default)]
    pub github_links: Vec<NewTaskGithubLink>,
}

/// Input to update an existing Task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateHubTask {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New description.
    #[serde(default, alias = "description_md")]
    pub description: Option<String>,
    /// New state.
    #[serde(default)]
    pub state: Option<String>,
    /// New priority.
    #[serde(default)]
    pub priority: Option<String>,
    /// New module ID/name (`Some("")` clears).
    #[serde(default)]
    pub module_id: Option<String>,
    /// New cycle ID/name (`Some("")` clears).
    #[serde(default)]
    pub cycle_id: Option<String>,
    /// New parent task ref (`Some("")` clears).
    #[serde(default, alias = "parent_id")]
    pub parent_task_id: Option<String>,
    /// Start date (`Some("")` clears).
    #[serde(default)]
    pub start_date: Option<String>,
    /// Due date (`Some("")` clears).
    #[serde(default)]
    pub due_date: Option<String>,
    /// Estimate points (`Some(-1)` clears).
    #[serde(default)]
    pub estimate_points: Option<i64>,
    /// Sort order.
    #[serde(default)]
    pub sort_order: Option<f64>,
    /// Replace assignees list (usernames or UUIDs) when `Some`.
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    /// Replace labels list (names or IDs) when `Some`.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Replace wiki page links when `Some`.
    #[serde(default, alias = "wiki_paths")]
    pub wiki_pages: Option<Vec<String>>,
    /// Replace or append GitHub links when `Some`.
    #[serde(default)]
    pub github_links: Option<Vec<NewTaskGithubLink>>,
    /// Actor user ID performing the update.
    #[serde(default)]
    pub actor_id: Option<UserId>,
    /// Actor AI agent performing the update.
    #[serde(default)]
    pub actor_agent: Option<String>,
}

/// Connected GitHub repository on a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubGithubRepo {
    /// Repo connection UUID.
    pub id: String,
    /// Workspace UUID.
    pub workspace_id: WorkspaceId,
    /// Project UUID.
    pub project_id: ProjectId,
    /// GitHub owner/org.
    pub owner: String,
    /// GitHub repository name.
    pub repo: String,
    /// `owner/repo`.
    pub full_name: String,
    /// Default branch (e.g. `main`).
    pub default_branch: String,
    /// Whether a Personal Access Token is stored for this repo.
    pub has_token: bool,
    /// User who added the repo.
    pub added_by: Option<UserId>,
    /// Username who added the repo.
    pub added_by_username: Option<String>,
    /// Last successful sync timestamp (ISO-8601).
    pub last_synced_at: Option<String>,
    /// Last sync error if any.
    pub last_sync_error: Option<String>,
    /// Open cached issues count.
    pub open_issues_count: usize,
    /// Open cached PRs count.
    pub open_prs_count: usize,
    /// Created timestamp (ISO-8601).
    pub created_at: String,
}

/// Connected GitHub repository together with its optional PAT for sync/API calls.
#[derive(Debug, Clone)]
pub struct HubGithubRepoWithToken {
    /// Public repo metadata.
    pub repo: HubGithubRepo,
    /// Stored Personal Access Token if configured.
    pub access_token: Option<String>,
}

/// Label on a GitHub Issue or PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubGithubLabel {
    /// Label name.
    pub name: String,
    /// Hex color (without `#` or with `#`).
    pub color: String,
}

/// Synced GitHub Issue or Pull Request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubGithubItem {
    /// Internal item ID.
    pub id: String,
    /// Connected repo ID.
    pub repo_id: String,
    /// Project UUID.
    pub project_id: ProjectId,
    /// `owner/repo`.
    pub repo_full_name: String,
    /// `issue` or `pr`.
    pub kind: String,
    /// Issue or PR number.
    pub number: i64,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// State (`open`, `closed`, `merged`).
    pub state: String,
    /// Draft PR flag.
    pub draft: bool,
    /// Author GitHub login.
    pub author_login: String,
    /// Author avatar URL.
    pub author_avatar_url: Option<String>,
    /// Assignee GitHub logins.
    pub assignees: Vec<String>,
    /// GitHub labels.
    pub labels: Vec<HubGithubLabel>,
    /// HTML URL on GitHub.
    pub html_url: String,
    /// PR head branch ref.
    pub head_ref: Option<String>,
    /// PR base branch ref.
    pub base_ref: Option<String>,
    /// Comments count on GitHub.
    pub comments_count: i64,
    /// GitHub created_at ISO string.
    pub gh_created_at: Option<String>,
    /// GitHub updated_at ISO string.
    pub gh_updated_at: Option<String>,
    /// Local sync timestamp (ISO-8601).
    pub synced_at: String,
    /// Keys of linked AI Hub tasks (e.g. `["AIH-1", "AIH-4"]`).
    pub linked_tasks: Vec<String>,
}

/// Payload to upsert a synced GitHub Issue or PR into SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertGithubItem {
    /// `issue` or `pr`.
    pub kind: String,
    /// Issue or PR number.
    pub number: i64,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
    /// State (`open`, `closed`, `merged`).
    pub state: String,
    /// Draft status.
    pub draft: bool,
    /// Author login.
    pub author_login: String,
    /// Author avatar URL.
    pub author_avatar_url: Option<String>,
    /// Assignees logins.
    pub assignees: Vec<String>,
    /// Labels.
    pub labels: Vec<HubGithubLabel>,
    /// HTML URL.
    pub html_url: String,
    /// PR head branch.
    pub head_ref: Option<String>,
    /// PR base branch.
    pub base_ref: Option<String>,
    /// Comments count.
    pub comments_count: i64,
    /// GitHub created_at.
    pub gh_created_at: Option<String>,
    /// GitHub updated_at.
    pub gh_updated_at: Option<String>,
}

pub(crate) fn ensure_auto_token_pepper_sql(conn: &mut Connection) -> StoreResult<String> {
    let existing: Option<Option<String>> = conn
        .query_row(
            "SELECT auto_token_pepper FROM hub_settings WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(Some(pepper)) = existing
        && !pepper.trim().is_empty()
    {
        return Ok(pepper);
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw)
        .map_err(|e| StoreError::InvalidState(format!("OS RNG failed: {e}")))?;
    let generated = format!(
        "aih_pepper_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    );
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_settings (id, instance_name, allow_registration, default_project_access, auto_token_pepper, updated_at) \
         VALUES (1, 'AI Hub', 1, 'restricted', ?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET auto_token_pepper = excluded.auto_token_pepper, updated_at = excluded.updated_at",
        params![&generated, now],
    )?;
    Ok(generated)
}

pub(crate) fn get_hub_settings_sql(conn: &Connection) -> StoreResult<HubSettings> {
    let row = conn
        .query_row(
            "SELECT instance_name, allow_registration, default_project_access, updated_at \
             FROM hub_settings WHERE id = 1",
            [],
            |r| {
                Ok(HubSettings {
                    instance_name: r.get(0)?,
                    allow_registration: r.get::<_, i64>(1)? != 0,
                    default_project_access: r.get(2)?,
                    updated_at: format_us_iso(r.get(3)?),
                })
            },
        )
        .optional()?;
    Ok(row.unwrap_or(HubSettings {
        instance_name: "AI Hub".to_string(),
        allow_registration: true,
        default_project_access: "restricted".to_string(),
        updated_at: format_us_iso(0),
    }))
}

pub(crate) fn update_hub_settings_sql(
    conn: &mut Connection,
    instance_name: Option<String>,
    allow_registration: Option<bool>,
    default_project_access: Option<String>,
) -> StoreResult<HubSettings> {
    let current = get_hub_settings_sql(conn)?;
    let name = instance_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(current.instance_name);
    let allow = allow_registration.unwrap_or(current.allow_registration);
    let access = match default_project_access.as_deref() {
        Some("open") => "open".to_string(),
        Some("restricted") => "restricted".to_string(),
        _ => current.default_project_access,
    };
    let now = now_us();
    conn.execute(
        "UPDATE hub_settings SET instance_name = ?1, allow_registration = ?2, \
         default_project_access = ?3, updated_at = ?4 WHERE id = 1",
        params![name, i64::from(allow), access, now],
    )?;
    get_hub_settings_sql(conn)
}

/// Register a new human account. If no human account with a password exists yet,
/// the account is promoted to `root` (Instance Admin) and marks bootstrap complete.
pub(crate) fn register_human_user_sql(
    conn: &mut Connection,
    new_user: &NewUser,
    password_hash: &str,
) -> StoreResult<(User, bool)> {
    let tx = conn.transaction()?;
    let human_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM users WHERE password_hash IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let is_first = human_count == 0;
    if !is_first {
        let allow_reg: i64 = tx
            .query_row(
                "SELECT allow_registration FROM hub_settings WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(1);
        if allow_reg == 0 {
            return Err(StoreError::Forbidden(
                "self-registration is disabled on this AI Hub instance; ask an administrator for an account",
            ));
        }
    }
    let role = if is_first {
        UserRole::Root
    } else {
        UserRole::User
    };
    let user_id = crate::users::insert_human_user(&tx, new_user, role, Some(password_hash), false)?;
    if is_first {
        tx.execute(
            "UPDATE human_auth_state SET bootstrap_completed = 1 WHERE id = 1",
            [],
        )?;
        tx.execute(
            "UPDATE projects SET created_by = ?1, access_mode = 'restricted' WHERE created_by IS NULL",
            params![user_id.as_bytes()],
        )?;
    }
    let user = crate::users::find_user_by_id(&tx, user_id)?
        .ok_or_else(|| StoreError::NotFound("registered user missing".into()))?;
    tx.commit()?;
    Ok((user, is_first))
}

pub(crate) fn update_user_profile_sql(
    conn: &mut Connection,
    user_id: UserId,
    name: Option<String>,
    email: Option<String>,
) -> StoreResult<User> {
    let name_norm = name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let email_norm = email
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(ref e) = email_norm {
        ai_memory_core::validate_email(e)?;
    }
    conn.execute(
        "UPDATE users SET name = COALESCE(?1, name), email = COALESCE(?2, email) WHERE id = ?3",
        params![name_norm, email_norm, user_id.as_bytes()],
    )?;
    crate::users::find_user_by_id(conn, user_id)?
        .ok_or_else(|| StoreError::NotFound("user not found".into()))
}

pub(crate) fn admin_update_user_sql(
    conn: &mut Connection,
    user_id: UserId,
    role: Option<UserRole>,
    disabled: Option<bool>,
) -> StoreResult<User> {
    let tx = conn.transaction()?;
    if let Some(r) = role {
        tx.execute(
            "UPDATE users SET role = ?1 WHERE id = ?2",
            params![r.as_str(), user_id.as_bytes()],
        )?;
    }
    if let Some(dis) = disabled {
        let ts = if dis { Some(now_us()) } else { None };
        tx.execute(
            "UPDATE users SET disabled_at = ?1 WHERE id = ?2",
            params![ts, user_id.as_bytes()],
        )?;
        if dis {
            crate::web_sessions::revoke_user_sessions(&tx, user_id, None)?;
        }
    }
    let user = crate::users::find_user_by_id(&tx, user_id)?
        .ok_or_else(|| StoreError::NotFound("user not found".into()))?;
    tx.commit()?;
    Ok(user)
}

pub(crate) fn seed_default_labels_sql(conn: &Connection, project_id: ProjectId) -> StoreResult<()> {
    let defaults = [
        ("Feature", "#6366f1", "New feature or capability"),
        ("Bug", "#ef4444", "Defect or regression"),
        ("Architecture", "#3b82f6", "System design & architecture"),
        (
            "AI-Agent",
            "#10b981",
            "Delegated to or authored by AI agent",
        ),
        ("Tech Debt", "#f59e0b", "Refactoring & maintenance"),
        ("Documentation", "#8b5cf6", "Wiki & documentation updates"),
    ];
    let now = now_us();
    for (name, color, desc) in defaults {
        let id = Uuid::now_v7().to_string();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO hub_labels (id, project_id, name, color, description, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, project_id.as_bytes(), name, color, desc, now],
        );
    }
    Ok(())
}

/// Check effective role of a user on a project.
pub(crate) fn user_role_on_project_sql(
    conn: &Connection,
    project_id: ProjectId,
    viewer: Option<UserId>,
    is_root: bool,
) -> StoreResult<Option<HubProjectRole>> {
    let proj_row: Option<(Option<Vec<u8>>, String)> = conn
        .query_row(
            "SELECT created_by, COALESCE(access_mode, 'open') FROM projects WHERE id = ?1",
            params![project_id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((created_by_bytes, _access_mode)) = proj_row else {
        return Ok(None);
    };
    if is_root {
        return Ok(Some(HubProjectRole::Owner));
    }
    let Some(uid) = viewer else {
        return Ok(None);
    };
    let created_by = created_by_bytes.and_then(|b| UserId::from_slice(&b).ok());
    if created_by == Some(uid) {
        return Ok(Some(HubProjectRole::Owner));
    }
    let grant: Option<(String, i64)> = conn
        .query_row(
            "SELECT level, COALESCE(is_manager, 0) FROM project_grants \
             WHERE project_id = ?1 AND user_id = ?2",
            params![project_id.as_bytes(), uid.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((level, is_manager)) = grant {
        if is_manager != 0 {
            return Ok(Some(HubProjectRole::Admin));
        }
        if level == "write" {
            return Ok(Some(HubProjectRole::Write));
        }
        return Ok(Some(HubProjectRole::Read));
    }
    Ok(None)
}

pub(crate) fn list_hub_projects_sql(
    conn: &Connection,
    viewer: Option<UserId>,
    is_root: bool,
) -> StoreResult<Vec<HubProjectOverview>> {
    if !is_root && viewer.is_none() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT p.id, p.workspace_id, w.name, p.name, \
                COALESCE(p.description, ''), COALESCE(p.identifier_prefix, ''), \
                COALESCE(p.access_mode, 'open'), p.created_by, u.username, p.created_at, \
                (SELECT COUNT(*) FROM pages pg WHERE pg.project_id = p.id AND pg.is_latest = 1) AS page_count, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.project_id = p.id AND t.archived_at IS NULL) AS task_total, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.project_id = p.id AND t.archived_at IS NULL AND t.state = 'done') AS task_done, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.project_id = p.id AND t.archived_at IS NULL AND t.state IN ('in_progress', 'in_review')) AS task_ip, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.project_id = p.id AND t.archived_at IS NULL AND t.state IN ('todo', 'backlog')) AS task_todo, \
                (SELECT COUNT(*) FROM hub_github_repos gr WHERE gr.project_id = p.id) AS gh_repos, \
                (SELECT COUNT(*) FROM hub_github_items gi WHERE gi.project_id = p.id AND gi.kind = 'issue' AND gi.state = 'open') AS gh_issues, \
                (SELECT COUNT(*) FROM hub_github_items gi WHERE gi.project_id = p.id AND gi.kind = 'pr' AND gi.state = 'open') AS gh_prs, \
                (SELECT COUNT(*) FROM project_grants g WHERE g.project_id = p.id) AS grant_count \
         FROM projects p \
         JOIN workspaces w ON w.id = p.workspace_id \
         LEFT JOIN users u ON u.id = p.created_by \
         WHERE p.name NOT LIKE '\\_%' ESCAPE '\\' \
         ORDER BY p.created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<Vec<u8>>>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, i64>(9)?,
            r.get::<_, i64>(10)?,
            r.get::<_, i64>(11)?,
            r.get::<_, i64>(12)?,
            r.get::<_, i64>(13)?,
            r.get::<_, i64>(14)?,
            r.get::<_, i64>(15)?,
            r.get::<_, i64>(16)?,
            r.get::<_, i64>(17)?,
            r.get::<_, i64>(18)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            pid_bytes,
            wid_bytes,
            ws_name,
            proj_name,
            desc,
            prefix,
            access_mode,
            created_by_bytes,
            created_by_username,
            created_at,
            page_count,
            task_total,
            task_done,
            task_ip,
            task_todo,
            gh_repos,
            gh_issues,
            gh_prs,
            grant_count,
        ) = row?;
        let Ok(project_id) = ProjectId::from_slice(&pid_bytes) else {
            continue;
        };
        let Ok(workspace_id) = WorkspaceId::from_slice(&wid_bytes) else {
            continue;
        };
        let Some(my_role) = user_role_on_project_sql(conn, project_id, viewer, is_root)? else {
            continue;
        };
        let created_by = created_by_bytes.and_then(|b| UserId::from_slice(&b).ok());
        let identifier_prefix = if prefix.trim().is_empty() {
            derive_identifier_prefix(&proj_name)
        } else {
            prefix
        };
        let member_count = (grant_count as usize).max(usize::from(created_by.is_some()));
        out.push(HubProjectOverview {
            workspace_id,
            workspace_name: ws_name,
            project_id,
            project_name: proj_name,
            description: desc,
            identifier_prefix,
            access_mode,
            created_by,
            created_by_username,
            my_role,
            member_count,
            page_count: page_count as usize,
            task_total: task_total as usize,
            task_done: task_done as usize,
            task_in_progress: task_ip as usize,
            task_todo: task_todo as usize,
            github_repo_count: gh_repos as usize,
            github_open_issues: gh_issues as usize,
            github_open_prs: gh_prs as usize,
            created_at: format_us_iso(created_at),
        });
    }
    Ok(out)
}

pub(crate) fn get_hub_project_sql(
    conn: &Connection,
    workspace: &str,
    project: &str,
    viewer: Option<UserId>,
    is_root: bool,
) -> StoreResult<HubProjectOverview> {
    let pid_bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT p.id FROM projects p JOIN workspaces w ON w.id = p.workspace_id \
             WHERE w.name = ?1 AND p.name = ?2",
            params![workspace, project],
            |r| r.get(0),
        )
        .optional()?;
    let Some(pid_bytes) = pid_bytes else {
        return Err(StoreError::NotFound(format!(
            "project {workspace}/{project} not found"
        )));
    };
    let project_id = ProjectId::from_slice(&pid_bytes)?;
    let all = list_hub_projects_sql(conn, viewer, is_root)?;
    all.into_iter()
        .find(|p| p.project_id == project_id)
        .ok_or(StoreError::Forbidden(crate::RESTRICTED_PROJECT_FORBIDDEN))
}

pub(crate) fn create_hub_project_sql(
    conn: &mut Connection,
    workspace: &str,
    name: &str,
    identifier_prefix: Option<&str>,
    description: &str,
    access_mode: AccessMode,
    creator: Option<UserId>,
) -> StoreResult<(WorkspaceId, ProjectId)> {
    let ws_name = if workspace.trim().is_empty() {
        ai_memory_core::DEFAULT_WORKSPACE_NAME
    } else {
        workspace.trim()
    };
    let proj_name = name.trim();
    if proj_name.is_empty() || proj_name.starts_with('_') {
        return Err(StoreError::InvalidState(
            "project name must be non-empty and must not start with '_'".into(),
        ));
    }
    let ws_id = crate::ops::get_or_create_workspace(conn, ws_name)?;
    let (proj_id, _) =
        crate::ops::get_or_create_project_as(conn, &ws_id, proj_name, None, creator, access_mode)?;
    let prefix = identifier_prefix
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(6)
                .map(|c| c.to_ascii_uppercase())
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| derive_identifier_prefix(proj_name));
    conn.execute(
        "UPDATE projects SET description = ?1, identifier_prefix = ?2, access_mode = ?3, \
         created_by = COALESCE(created_by, ?4) WHERE id = ?5",
        params![
            description.trim(),
            prefix,
            access_mode.as_str(),
            creator.map(|u| u.as_bytes().to_vec()),
            proj_id.as_bytes()
        ],
    )?;
    if let Some(uid) = creator {
        let now = now_us();
        conn.execute(
            "INSERT INTO project_grants (workspace_id, project_id, user_id, level, granted_by, granted_at, is_manager) \
             VALUES (?1, ?2, ?3, 'write', ?3, ?4, 1) \
             ON CONFLICT(workspace_id, project_id, user_id) DO UPDATE SET level = 'write', is_manager = 1",
            params![ws_id.as_bytes(), proj_id.as_bytes(), uid.as_bytes(), now],
        )?;
    }
    seed_default_labels_sql(conn, proj_id)?;
    Ok((ws_id, proj_id))
}

pub(crate) fn update_hub_project_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    description: Option<&str>,
    identifier_prefix: Option<&str>,
    access_mode: Option<AccessMode>,
) -> StoreResult<()> {
    if let Some(desc) = description {
        conn.execute(
            "UPDATE projects SET description = ?1 WHERE id = ?2",
            params![desc.trim(), project_id.as_bytes()],
        )?;
    }
    if let Some(pref) = identifier_prefix {
        let clean: String = pref
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(6)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if !clean.is_empty() {
            conn.execute(
                "UPDATE projects SET identifier_prefix = ?1 WHERE id = ?2",
                params![clean, project_id.as_bytes()],
            )?;
        }
    }
    if let Some(mode) = access_mode {
        crate::grants::set_access_mode(conn, project_id, mode)?;
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
pub(crate) fn list_project_members_sql(
    conn: &Connection,
    project_id: ProjectId,
) -> StoreResult<Vec<HubProjectMember>> {
    let mut members = Vec::new();
    let creator: Option<(Vec<u8>, String, Option<String>, Option<String>, i64)> = conn
        .query_row(
            "SELECT u.id, u.username, u.name, u.email, p.created_at \
             FROM projects p JOIN users u ON u.id = p.created_by \
             WHERE p.id = ?1",
            params![project_id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let mut creator_id = None;
    if let Some((uid_b, username, name, email, created_at)) = creator
        && let Ok(uid) = UserId::from_slice(&uid_b)
    {
        creator_id = Some(uid);
        members.push(HubProjectMember {
            user_id: uid,
            username,
            name,
            email,
            role: HubProjectRole::Owner,
            granted_at: format_us_iso(created_at),
        });
    }
    let mut stmt = conn.prepare(
        "SELECT u.id, u.username, u.name, u.email, pg.level, COALESCE(pg.is_manager, 0), pg.granted_at \
         FROM project_grants pg JOIN users u ON u.id = pg.user_id \
         WHERE pg.project_id = ?1 ORDER BY u.username ASC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    for row in rows {
        let (uid_b, username, name, email, level, is_manager, granted_at) = row?;
        let Ok(uid) = UserId::from_slice(&uid_b) else {
            continue;
        };
        if Some(uid) == creator_id {
            continue;
        }
        let role = if is_manager != 0 {
            HubProjectRole::Admin
        } else if level == "write" {
            HubProjectRole::Write
        } else {
            HubProjectRole::Read
        };
        members.push(HubProjectMember {
            user_id: uid,
            username,
            name,
            email,
            role,
            granted_at: format_us_iso(granted_at),
        });
    }
    Ok(members)
}

pub(crate) fn set_project_member_role_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    target_username_or_id: &str,
    role: &str,
    granted_by: Option<UserId>,
) -> StoreResult<HubProjectMember> {
    let user = resolve_user_ref_sql(conn, target_username_or_id)?;
    let (grant_level, is_manager, hub_role) = match role.trim().to_ascii_lowercase().as_str() {
        "admin" | "manager" | "owner" => (GrantLevel::Write, 1i64, HubProjectRole::Admin),
        "write" | "editor" | "member" => (GrantLevel::Write, 0i64, HubProjectRole::Write),
        "read" | "viewer" => (GrantLevel::Read, 0i64, HubProjectRole::Read),
        other => {
            return Err(StoreError::InvalidState(format!(
                "invalid project role {other:?}; expected 'read', 'write', or 'admin'"
            )));
        }
    };
    let now = now_us();
    crate::grants::grant(conn, user.id, project_id, grant_level, granted_by, now)?;
    conn.execute(
        "UPDATE project_grants SET is_manager = ?1 WHERE project_id = ?2 AND user_id = ?3",
        params![is_manager, project_id.as_bytes(), user.id.as_bytes()],
    )?;
    Ok(HubProjectMember {
        user_id: user.id,
        username: user.username,
        name: user.name,
        email: user.email,
        role: hub_role,
        granted_at: format_us_iso(now),
    })
}

pub(crate) fn remove_project_member_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    target_username_or_id: &str,
    revoked_by: Option<UserId>,
) -> StoreResult<bool> {
    let user = resolve_user_ref_sql(conn, target_username_or_id)?;
    let now = now_us();
    crate::grants::revoke(conn, user.id, project_id, revoked_by, now)
}

fn resolve_user_ref_sql(conn: &Connection, raw: &str) -> StoreResult<User> {
    let trimmed = raw.trim();
    if let Ok(uid) = trimmed.parse::<UserId>()
        && let Some(u) = crate::users::find_user_by_id(conn, uid)?
    {
        return Ok(u);
    }
    if let Some(u) = crate::users::find_user_by_username(conn, trimmed)? {
        return Ok(u);
    }
    Err(StoreError::NotFound(format!("user {trimmed:?} not found")))
}

fn resolve_module_ref_sql(
    conn: &Connection,
    project_id: ProjectId,
    raw: &str,
) -> StoreResult<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let found: Option<String> = conn
        .query_row(
            "SELECT id FROM hub_modules WHERE project_id = ?1 AND (id = ?2 OR lower(name) = lower(?2))",
            params![project_id.as_bytes(), trimmed],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found)
}

fn resolve_cycle_ref_sql(
    conn: &Connection,
    project_id: ProjectId,
    raw: &str,
) -> StoreResult<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let found: Option<String> = conn
        .query_row(
            "SELECT id FROM hub_cycles WHERE project_id = ?1 AND (id = ?2 OR lower(name) = lower(?2))",
            params![project_id.as_bytes(), trimmed],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found)
}

/// Resolve a task reference in a project by UUID, `PREFIX-12` key, or raw sequence number `12`.
pub(crate) fn resolve_task_id_sql(
    conn: &Connection,
    project_id: ProjectId,
    task_ref: &str,
) -> StoreResult<String> {
    let trimmed = task_ref.trim();
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM hub_tasks WHERE project_id = ?1 AND id = ?2",
            params![project_id.as_bytes(), trimmed],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    let seq_str = trimmed
        .rsplit_once('-')
        .map(|(_, num)| num)
        .unwrap_or(trimmed)
        .trim_start_matches('#');
    if let Ok(seq) = seq_str.parse::<i64>()
        && let Some(id) = conn
            .query_row(
                "SELECT id FROM hub_tasks WHERE project_id = ?1 AND sequence_id = ?2",
                params![project_id.as_bytes(), seq],
                |r| r.get::<_, String>(0),
            )
            .optional()?
    {
        return Ok(id);
    }
    Err(StoreError::NotFound(format!(
        "task {trimmed:?} not found in project"
    )))
}

fn normalize_task_state(raw: Option<&str>) -> StoreResult<&'static str> {
    match raw
        .map(str::trim)
        .unwrap_or("todo")
        .to_ascii_lowercase()
        .as_str()
    {
        "backlog" => Ok("backlog"),
        "todo" | "open" | "unstarted" => Ok("todo"),
        "in_progress" | "in-progress" | "doing" | "started" => Ok("in_progress"),
        "in_review" | "in-review" | "review" => Ok("in_review"),
        "done" | "completed" | "closed" => Ok("done"),
        "cancelled" | "canceled" => Ok("cancelled"),
        other => Err(StoreError::InvalidState(format!(
            "invalid task state {other:?}; expected backlog, todo, in_progress, in_review, done, or cancelled"
        ))),
    }
}

fn normalize_task_priority(raw: Option<&str>) -> StoreResult<&'static str> {
    match raw
        .map(str::trim)
        .unwrap_or("none")
        .to_ascii_lowercase()
        .as_str()
    {
        "urgent" | "p0" | "critical" => Ok("urgent"),
        "high" | "p1" => Ok("high"),
        "medium" | "med" | "p2" | "normal" => Ok("medium"),
        "low" | "p3" => Ok("low"),
        "none" | "" | "p4" => Ok("none"),
        other => Err(StoreError::InvalidState(format!(
            "invalid task priority {other:?}; expected urgent, high, medium, low, or none"
        ))),
    }
}

fn hydrate_task_sql(conn: &Connection, task_id: &str) -> StoreResult<HubTask> {
    let row = conn
        .query_row(
            "SELECT t.id, t.workspace_id, w.name, t.project_id, p.name, \
                    COALESCE(p.identifier_prefix, ''), t.sequence_id, t.title, t.description, \
                    t.state, t.priority, t.module_id, m.name, t.cycle_id, c.name, \
                    t.parent_task_id, t.created_by, u.username, u.name, t.created_by_agent, \
                    t.start_date, t.due_date, t.estimate_points, t.sort_order, \
                    t.completed_at, t.archived_at, t.created_at, t.updated_at, \
                    (SELECT COUNT(*) FROM hub_task_comments tc WHERE tc.task_id = t.id) AS comments_count, \
                    (SELECT COUNT(*) FROM hub_tasks st WHERE st.parent_task_id = t.id AND st.archived_at IS NULL) AS sub_total, \
                    (SELECT COUNT(*) FROM hub_tasks st WHERE st.parent_task_id = t.id AND st.archived_at IS NULL AND st.state = 'done') AS sub_done \
             FROM hub_tasks t \
             JOIN workspaces w ON w.id = t.workspace_id \
             JOIN projects p ON p.id = t.project_id \
             LEFT JOIN hub_modules m ON m.id = t.module_id \
             LEFT JOIN hub_cycles c ON c.id = t.cycle_id \
             LEFT JOIN users u ON u.id = t.created_by \
             WHERE t.id = ?1",
            params![task_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, Option<String>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, Option<String>>(15)?,
                    r.get::<_, Option<Vec<u8>>>(16)?,
                    r.get::<_, Option<String>>(17)?,
                    r.get::<_, Option<String>>(18)?,
                    r.get::<_, Option<String>>(19)?,
                    r.get::<_, Option<String>>(20)?,
                    r.get::<_, Option<String>>(21)?,
                    r.get::<_, Option<i64>>(22)?,
                    r.get::<_, f64>(23)?,
                    r.get::<_, Option<i64>>(24)?,
                    r.get::<_, Option<i64>>(25)?,
                    r.get::<_, i64>(26)?,
                    r.get::<_, i64>(27)?,
                    r.get::<_, i64>(28)?,
                    r.get::<_, i64>(29)?,
                    r.get::<_, i64>(30)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("task {task_id} not found")))?;

    let (
        id,
        wid_b,
        ws_name,
        pid_b,
        proj_name,
        raw_prefix,
        sequence_id,
        title,
        description,
        state,
        priority,
        module_id,
        module_name,
        cycle_id,
        cycle_name,
        parent_task_id,
        created_by_b,
        created_by_username,
        created_by_name,
        created_by_agent,
        start_date,
        due_date,
        estimate_points,
        sort_order,
        completed_at,
        archived_at,
        created_at,
        updated_at,
        comments_count,
        sub_total,
        sub_done,
    ) = row;

    let workspace_id = WorkspaceId::from_slice(&wid_b)?;
    let project_id = ProjectId::from_slice(&pid_b)?;
    let identifier_prefix = if raw_prefix.trim().is_empty() {
        derive_identifier_prefix(&proj_name)
    } else {
        raw_prefix
    };
    let key = format!("{identifier_prefix}-{sequence_id}");

    // Assignees
    let mut assignees = Vec::new();
    let mut astmt = conn.prepare(
        "SELECT u.id, u.username, u.name, u.email, ta.assigned_at \
         FROM hub_task_assignees ta JOIN users u ON u.id = ta.user_id \
         WHERE ta.task_id = ?1 ORDER BY ta.assigned_at ASC",
    )?;
    let arows = astmt.query_map(params![&id], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for arow in arows {
        let (ub, uname, name, email, at) = arow?;
        if let Ok(uid) = UserId::from_slice(&ub) {
            assignees.push(HubTaskAssignee {
                user_id: uid,
                username: uname,
                name,
                email,
                assigned_at: format_us_iso(at),
            });
        }
    }

    // Labels
    let mut labels = Vec::new();
    let mut lstmt = conn.prepare(
        "SELECT l.id, l.name, l.color, l.description \
         FROM hub_task_labels tl JOIN hub_labels l ON l.id = tl.label_id \
         WHERE tl.task_id = ?1 ORDER BY l.name ASC",
    )?;
    let lrows = lstmt.query_map(params![&id], |r| {
        Ok(HubLabel {
            id: r.get(0)?,
            project_id,
            name: r.get(1)?,
            color: r.get(2)?,
            description: r.get(3)?,
        })
    })?;
    for l in lrows {
        labels.push(l?);
    }

    // Wiki links
    let mut wstmt = conn.prepare(
        "SELECT page_path FROM hub_task_wiki_links WHERE task_id = ?1 ORDER BY page_path ASC",
    )?;
    let wiki_pages = wstmt
        .query_map(params![&id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    // GitHub links
    let mut gstmt = conn.prepare(
        "SELECT task_id, github_item_id, repo_full_name, kind, number, title, state, html_url, relation \
         FROM hub_task_github_links WHERE task_id = ?1 ORDER BY created_at ASC",
    )?;
    let github_links = gstmt
        .query_map(params![&id], |r| {
            Ok(HubTaskGithubLink {
                task_id: r.get(0)?,
                github_item_id: r.get(1)?,
                repo_full_name: r.get(2)?,
                kind: r.get(3)?,
                number: r.get(4)?,
                title: r.get(5)?,
                state: r.get(6)?,
                html_url: r.get(7)?,
                relation: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(HubTask {
        id,
        workspace_id,
        workspace_name: ws_name,
        project_id,
        project_name: proj_name,
        identifier_prefix,
        sequence_id,
        key,
        title,
        description,
        state,
        priority,
        module_id,
        module_name,
        cycle_id,
        cycle_name,
        parent_task_id,
        created_by: created_by_b.and_then(|b| UserId::from_slice(&b).ok()),
        created_by_username,
        created_by_name,
        created_by_agent,
        start_date,
        due_date,
        estimate_points,
        sort_order,
        completed_at: completed_at.map(format_us_iso),
        archived_at: archived_at.map(format_us_iso),
        created_at: format_us_iso(created_at),
        updated_at: format_us_iso(updated_at),
        assignees,
        labels,
        wiki_pages,
        github_links,
        comments_count: comments_count as usize,
        subtasks_total: sub_total as usize,
        subtasks_done: sub_done as usize,
    })
}

pub(crate) fn list_project_tasks_sql(
    conn: &Connection,
    project_id: ProjectId,
    filter: &HubTaskFilter,
) -> StoreResult<Vec<HubTask>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM hub_tasks WHERE project_id = ?1 \
         ORDER BY CASE state \
            WHEN 'in_progress' THEN 1 \
            WHEN 'in_review' THEN 2 \
            WHEN 'todo' THEN 3 \
            WHEN 'backlog' THEN 4 \
            WHEN 'done' THEN 5 \
            ELSE 6 END, \
         CASE priority \
            WHEN 'urgent' THEN 1 \
            WHEN 'high' THEN 2 \
            WHEN 'medium' THEN 3 \
            WHEN 'low' THEN 4 \
            ELSE 5 END, \
         sort_order ASC, sequence_id DESC",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![project_id.as_bytes()], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        let task = hydrate_task_sql(conn, &id)?;
        if !filter.include_archived && task.archived_at.is_some() {
            continue;
        }
        if let Some(ref st) = filter.state
            && !st.trim().is_empty()
            && st != "all"
            && !task.state.eq_ignore_ascii_case(st.trim())
        {
            continue;
        }
        if let Some(ref pr) = filter.priority
            && !pr.trim().is_empty()
            && pr != "all"
            && !task.priority.eq_ignore_ascii_case(pr.trim())
        {
            continue;
        }
        if let Some(uid) = filter.assignee_id
            && !task.assignees.iter().any(|a| a.user_id == uid)
        {
            continue;
        }
        if let Some(ref uname) = filter.assignee_username
            && !uname.trim().is_empty()
            && uname != "all"
            && !task
                .assignees
                .iter()
                .any(|a| a.username.eq_ignore_ascii_case(uname.trim()))
        {
            continue;
        }
        if let Some(ref mid) = filter.module_id
            && !mid.trim().is_empty()
            && mid != "all"
        {
            let m_match = task.module_id.as_deref() == Some(mid.trim())
                || task
                    .module_name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(mid.trim()));
            if !m_match {
                continue;
            }
        }
        if let Some(ref cid) = filter.cycle_id
            && !cid.trim().is_empty()
            && cid != "all"
        {
            let c_match = task.cycle_id.as_deref() == Some(cid.trim())
                || task
                    .cycle_name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(cid.trim()));
            if !c_match {
                continue;
            }
        }
        if let Some(ref lbl) = filter.label
            && !lbl.trim().is_empty()
            && lbl != "all"
            && !task
                .labels
                .iter()
                .any(|l| l.id == lbl.trim() || l.name.eq_ignore_ascii_case(lbl.trim()))
        {
            continue;
        }
        if let Some(ref q) = filter.search
            && !q.trim().is_empty()
        {
            let needle = q.trim().to_ascii_lowercase();
            let hay = format!(
                "{} {} {}",
                task.key.to_ascii_lowercase(),
                task.title.to_ascii_lowercase(),
                task.description.to_ascii_lowercase()
            );
            if !hay.contains(&needle) {
                continue;
            }
        }
        out.push(task);
    }
    Ok(out)
}

pub(crate) fn list_user_tasks_sql(
    conn: &Connection,
    user_id: UserId,
    is_root: bool,
) -> StoreResult<Vec<HubTask>> {
    let projects = list_hub_projects_sql(conn, Some(user_id), is_root)?;
    let mut out = Vec::new();
    for proj in projects {
        let tasks = list_project_tasks_sql(conn, proj.project_id, &HubTaskFilter::default())?;
        for t in tasks {
            let assigned = t.assignees.iter().any(|a| a.user_id == user_id);
            let created = t.created_by == Some(user_id);
            if assigned || created {
                out.push(t);
            }
        }
    }
    Ok(out)
}

pub(crate) fn get_task_detail_sql(
    conn: &Connection,
    project_id: ProjectId,
    task_ref: &str,
) -> StoreResult<HubTaskDetail> {
    let task_id = resolve_task_id_sql(conn, project_id, task_ref)?;
    let task = hydrate_task_sql(conn, &task_id)?;

    let mut cstmt = conn.prepare(
        "SELECT tc.id, tc.task_id, tc.author_id, u.username, u.name, tc.author_agent, \
                tc.body, tc.created_at, tc.updated_at \
         FROM hub_task_comments tc LEFT JOIN users u ON u.id = tc.author_id \
         WHERE tc.task_id = ?1 ORDER BY tc.created_at ASC",
    )?;
    let comments = cstmt
        .query_map(params![&task_id], |r| {
            Ok(HubTaskComment {
                id: r.get(0)?,
                task_id: r.get(1)?,
                author_id: r
                    .get::<_, Option<Vec<u8>>>(2)?
                    .and_then(|b| UserId::from_slice(&b).ok()),
                author_username: r.get(3)?,
                author_name: r.get(4)?,
                author_agent: r.get(5)?,
                body: r.get(6)?,
                created_at: format_us_iso(r.get(7)?),
                updated_at: format_us_iso(r.get(8)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut act_stmt = conn.prepare(
        "SELECT ta.id, ta.task_id, ta.actor_id, u.username, ta.actor_agent, \
                ta.action, ta.old_value, ta.new_value, ta.created_at \
         FROM hub_task_activities ta LEFT JOIN users u ON u.id = ta.actor_id \
         WHERE ta.task_id = ?1 ORDER BY ta.created_at DESC LIMIT 50",
    )?;
    let activities = act_stmt
        .query_map(params![&task_id], |r| {
            Ok(HubTaskActivity {
                id: r.get(0)?,
                task_id: r.get(1)?,
                actor_id: r
                    .get::<_, Option<Vec<u8>>>(2)?
                    .and_then(|b| UserId::from_slice(&b).ok()),
                actor_username: r.get(3)?,
                actor_agent: r.get(4)?,
                action: r.get(5)?,
                old_value: r.get(6)?,
                new_value: r.get(7)?,
                created_at: format_us_iso(r.get(8)?),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut sub_stmt = conn.prepare(
        "SELECT id FROM hub_tasks WHERE parent_task_id = ?1 AND archived_at IS NULL ORDER BY sequence_id ASC",
    )?;
    let sub_ids: Vec<String> = sub_stmt
        .query_map(params![&task_id], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut subtasks = Vec::new();
    for sid in sub_ids {
        subtasks.push(hydrate_task_sql(conn, &sid)?);
    }

    Ok(HubTaskDetail {
        task,
        comments,
        activities,
        subtasks,
    })
}

#[allow(clippy::too_many_arguments)]
fn record_task_activity_sql(
    conn: &Connection,
    task_id: &str,
    actor_id: Option<UserId>,
    actor_agent: Option<&str>,
    action: &str,
    old_val: Option<&str>,
    new_val: Option<&str>,
    now: i64,
) -> StoreResult<()> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO hub_task_activities (id, task_id, actor_id, actor_agent, action, old_value, new_value, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            task_id,
            actor_id.map(|u| u.as_bytes().to_vec()),
            actor_agent,
            action,
            old_val,
            new_val,
            now
        ],
    )?;
    Ok(())
}

pub(crate) fn create_task_sql(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    input: NewHubTask,
) -> StoreResult<HubTaskDetail> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err(StoreError::InvalidState(
            "task title cannot be empty".into(),
        ));
    }
    let state = normalize_task_state(input.state.as_deref())?;
    let priority = normalize_task_priority(input.priority.as_deref())?;

    seed_default_labels_sql(conn, project_id)?;

    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE projects SET task_seq = COALESCE(task_seq, 0) + 1 WHERE id = ?1",
        params![project_id.as_bytes()],
    )?;
    let seq: i64 = tx.query_row(
        "SELECT task_seq FROM projects WHERE id = ?1",
        params![project_id.as_bytes()],
        |r| r.get(0),
    )?;
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    let completed_at = if state == "done" { Some(now) } else { None };
    let module_id = match input.module_id.as_deref() {
        Some(m) => resolve_module_ref_sql(&tx, project_id, m)?,
        None => None,
    };
    let cycle_id = match input.cycle_id.as_deref() {
        Some(c) => resolve_cycle_ref_sql(&tx, project_id, c)?,
        None => None,
    };
    let parent_task_id = match input.parent_task_id.as_deref() {
        Some(p) if !p.trim().is_empty() => Some(resolve_task_id_sql(&tx, project_id, p)?),
        _ => None,
    };
    let start_date = input
        .start_date
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let due_date = input
        .due_date
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    tx.execute(
        "INSERT INTO hub_tasks (id, workspace_id, project_id, sequence_id, title, description, \
         state, priority, module_id, cycle_id, parent_task_id, created_by, created_by_agent, \
         start_date, due_date, estimate_points, sort_order, completed_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?19)",
        params![
            &id,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            seq,
            &title,
            input.description.as_deref().unwrap_or("").trim(),
            state,
            priority,
            module_id,
            cycle_id,
            parent_task_id,
            input.created_by.map(|u| u.as_bytes().to_vec()),
            input.created_by_agent.as_deref(),
            start_date,
            due_date,
            input.estimate_points,
            seq as f64,
            completed_at,
            now,
        ],
    )?;

    set_task_assignees_in_tx(&tx, &id, &input.assignees, input.created_by, now)?;
    set_task_labels_in_tx(&tx, project_id, &id, &input.labels, now)?;
    set_task_wiki_links_in_tx(&tx, &id, &input.wiki_pages, now)?;
    for gh in &input.github_links {
        insert_task_github_link_in_tx(&tx, project_id, &id, gh, now)?;
    }

    record_task_activity_sql(
        &tx,
        &id,
        input.created_by,
        input.created_by_agent.as_deref(),
        "created",
        None,
        Some(&title),
        now,
    )?;

    tx.commit()?;
    get_task_detail_sql(conn, project_id, &id)
}

fn set_task_assignees_in_tx(
    conn: &Connection,
    task_id: &str,
    assignees: &[String],
    assigned_by: Option<UserId>,
    now: i64,
) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM hub_task_assignees WHERE task_id = ?1",
        params![task_id],
    )?;
    for raw in assignees {
        if raw.trim().is_empty() {
            continue;
        }
        if let Ok(u) = resolve_user_ref_sql(conn, raw) {
            conn.execute(
                "INSERT OR IGNORE INTO hub_task_assignees (task_id, user_id, assigned_by, assigned_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    task_id,
                    u.id.as_bytes(),
                    assigned_by.map(|b| b.as_bytes().to_vec()),
                    now
                ],
            )?;
        }
    }
    Ok(())
}

fn set_task_labels_in_tx(
    conn: &Connection,
    project_id: ProjectId,
    task_id: &str,
    labels: &[String],
    now: i64,
) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM hub_task_labels WHERE task_id = ?1",
        params![task_id],
    )?;
    for raw in labels {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM hub_labels WHERE project_id = ?1 AND (id = ?2 OR lower(name) = lower(?2))",
                params![project_id.as_bytes(), trimmed],
                |r| r.get(0),
            )
            .optional()?;
        let label_id = if let Some(lid) = existing_id {
            lid
        } else {
            let new_id = Uuid::now_v7().to_string();
            conn.execute(
                "INSERT INTO hub_labels (id, project_id, name, color, description, created_at) \
                 VALUES (?1, ?2, ?3, '#6366f1', '', ?4)",
                params![&new_id, project_id.as_bytes(), trimmed, now],
            )?;
            new_id
        };
        conn.execute(
            "INSERT OR IGNORE INTO hub_task_labels (task_id, label_id) VALUES (?1, ?2)",
            params![task_id, label_id],
        )?;
    }
    Ok(())
}

fn set_task_wiki_links_in_tx(
    conn: &Connection,
    task_id: &str,
    pages: &[String],
    now: i64,
) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM hub_task_wiki_links WHERE task_id = ?1",
        params![task_id],
    )?;
    for page in pages {
        let p = page.trim();
        if !p.is_empty() {
            conn.execute(
                "INSERT OR IGNORE INTO hub_task_wiki_links (task_id, page_path, created_at) \
                 VALUES (?1, ?2, ?3)",
                params![task_id, p, now],
            )?;
        }
    }
    Ok(())
}

fn insert_task_github_link_in_tx(
    conn: &Connection,
    project_id: ProjectId,
    task_id: &str,
    link: &NewTaskGithubLink,
    now: i64,
) -> StoreResult<()> {
    let repo_full_name = link.repo_full_name.trim();
    if repo_full_name.is_empty() || link.number <= 0 {
        return Ok(());
    }
    let kind = if link.kind.trim().eq_ignore_ascii_case("pr")
        || link.kind.trim().eq_ignore_ascii_case("pull")
    {
        "pr"
    } else {
        "issue"
    };
    let relation = match link
        .relation
        .as_deref()
        .map(str::trim)
        .unwrap_or("tracks")
        .to_ascii_lowercase()
        .as_str()
    {
        "fixes" => "fixes",
        "closes" => "closes",
        "implements" => "implements",
        "related" => "related",
        _ => "tracks",
    };
    let cached: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT id, title, state, html_url FROM hub_github_items \
             WHERE project_id = ?1 AND lower(repo_full_name) = lower(?2) AND number = ?3",
            params![project_id.as_bytes(), repo_full_name, link.number],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (github_item_id, title, state, html_url) = if let Some((gid, t, s, u)) = cached {
        (
            Some(gid),
            link.title.clone().unwrap_or(t),
            link.state.clone().unwrap_or(s),
            link.html_url.clone().unwrap_or(u),
        )
    } else {
        let path_seg = if kind == "pr" { "pull" } else { "issues" };
        let url = link.html_url.clone().unwrap_or_else(|| {
            format!(
                "https://github.com/{repo_full_name}/{path_seg}/{}",
                link.number
            )
        });
        (
            None,
            link.title
                .clone()
                .unwrap_or_else(|| format!("{repo_full_name}#{}", link.number)),
            link.state.clone().unwrap_or_else(|| "open".to_string()),
            url,
        )
    };
    conn.execute(
        "INSERT INTO hub_task_github_links \
         (task_id, github_item_id, repo_full_name, kind, number, title, state, html_url, relation, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(task_id, repo_full_name, number) DO UPDATE SET \
         github_item_id = COALESCE(excluded.github_item_id, hub_task_github_links.github_item_id), \
         title = excluded.title, state = excluded.state, html_url = excluded.html_url, relation = excluded.relation",
        params![
            task_id,
            github_item_id,
            repo_full_name,
            kind,
            link.number,
            title,
            state,
            html_url,
            relation,
            now
        ],
    )?;
    Ok(())
}

pub(crate) fn update_task_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    task_ref: &str,
    input: UpdateHubTask,
) -> StoreResult<HubTaskDetail> {
    let task_id = resolve_task_id_sql(conn, project_id, task_ref)?;
    let before = hydrate_task_sql(conn, &task_id)?;
    let now = now_us();

    let tx = conn.transaction()?;

    if let Some(ref title) = input.title {
        let t = title.trim();
        if !t.is_empty() && t != before.title {
            tx.execute(
                "UPDATE hub_tasks SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![t, now, &task_id],
            )?;
            record_task_activity_sql(
                &tx,
                &task_id,
                input.actor_id,
                input.actor_agent.as_deref(),
                "title_changed",
                Some(&before.title),
                Some(t),
                now,
            )?;
        }
    }

    if let Some(ref desc) = input.description
        && desc != &before.description
    {
        tx.execute(
            "UPDATE hub_tasks SET description = ?1, updated_at = ?2 WHERE id = ?3",
            params![desc, now, &task_id],
        )?;
        record_task_activity_sql(
            &tx,
            &task_id,
            input.actor_id,
            input.actor_agent.as_deref(),
            "description_updated",
            None,
            None,
            now,
        )?;
    }

    if let Some(ref raw_state) = input.state {
        let new_state = normalize_task_state(Some(raw_state))?;
        if new_state != before.state {
            let completed_at = if new_state == "done" { Some(now) } else { None };
            tx.execute(
                "UPDATE hub_tasks SET state = ?1, completed_at = ?2, updated_at = ?3 WHERE id = ?4",
                params![new_state, completed_at, now, &task_id],
            )?;
            record_task_activity_sql(
                &tx,
                &task_id,
                input.actor_id,
                input.actor_agent.as_deref(),
                "state_changed",
                Some(&before.state),
                Some(new_state),
                now,
            )?;
        }
    }

    if let Some(ref raw_prio) = input.priority {
        let new_prio = normalize_task_priority(Some(raw_prio))?;
        if new_prio != before.priority {
            tx.execute(
                "UPDATE hub_tasks SET priority = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_prio, now, &task_id],
            )?;
            record_task_activity_sql(
                &tx,
                &task_id,
                input.actor_id,
                input.actor_agent.as_deref(),
                "priority_changed",
                Some(&before.priority),
                Some(new_prio),
                now,
            )?;
        }
    }

    if let Some(ref m) = input.module_id {
        let resolved = resolve_module_ref_sql(&tx, project_id, m)?;
        tx.execute(
            "UPDATE hub_tasks SET module_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![resolved, now, &task_id],
        )?;
    }

    if let Some(ref c) = input.cycle_id {
        let resolved = resolve_cycle_ref_sql(&tx, project_id, c)?;
        tx.execute(
            "UPDATE hub_tasks SET cycle_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![resolved, now, &task_id],
        )?;
    }

    if let Some(ref p) = input.parent_task_id {
        let resolved = if p.trim().is_empty() {
            None
        } else {
            Some(resolve_task_id_sql(&tx, project_id, p)?)
        };
        if resolved.as_deref() != Some(task_id.as_str()) {
            tx.execute(
                "UPDATE hub_tasks SET parent_task_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![resolved, now, &task_id],
            )?;
        }
    }

    if let Some(ref sd) = input.start_date {
        let val = (!sd.trim().is_empty()).then(|| sd.trim().to_string());
        tx.execute(
            "UPDATE hub_tasks SET start_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &task_id],
        )?;
    }

    if let Some(ref dd) = input.due_date {
        let val = (!dd.trim().is_empty()).then(|| dd.trim().to_string());
        tx.execute(
            "UPDATE hub_tasks SET due_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &task_id],
        )?;
    }

    if let Some(est) = input.estimate_points {
        let val = (est >= 0).then_some(est);
        tx.execute(
            "UPDATE hub_tasks SET estimate_points = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &task_id],
        )?;
    }

    if let Some(so) = input.sort_order {
        tx.execute(
            "UPDATE hub_tasks SET sort_order = ?1, updated_at = ?2 WHERE id = ?3",
            params![so, now, &task_id],
        )?;
    }

    if let Some(ref assignees) = input.assignees {
        set_task_assignees_in_tx(&tx, &task_id, assignees, input.actor_id, now)?;
        record_task_activity_sql(
            &tx,
            &task_id,
            input.actor_id,
            input.actor_agent.as_deref(),
            "assignees_updated",
            None,
            Some(&assignees.join(", ")),
            now,
        )?;
    }

    if let Some(ref labels) = input.labels {
        set_task_labels_in_tx(&tx, project_id, &task_id, labels, now)?;
    }

    if let Some(ref pages) = input.wiki_pages {
        set_task_wiki_links_in_tx(&tx, &task_id, pages, now)?;
    }

    if let Some(ref gh_links) = input.github_links {
        tx.execute(
            "DELETE FROM hub_task_github_links WHERE task_id = ?1",
            params![&task_id],
        )?;
        for gh in gh_links {
            insert_task_github_link_in_tx(&tx, project_id, &task_id, gh, now)?;
        }
    }

    tx.commit()?;
    get_task_detail_sql(conn, project_id, &task_id)
}

pub(crate) fn delete_task_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    task_ref: &str,
) -> StoreResult<bool> {
    let task_id = resolve_task_id_sql(conn, project_id, task_ref)?;
    let rows = conn.execute(
        "DELETE FROM hub_tasks WHERE id = ?1 AND project_id = ?2",
        params![&task_id, project_id.as_bytes()],
    )?;
    Ok(rows > 0)
}

pub(crate) fn add_task_comment_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    task_ref: &str,
    author_id: Option<UserId>,
    author_agent: Option<String>,
    body: String,
) -> StoreResult<HubTaskComment> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidState(
            "comment body cannot be empty".into(),
        ));
    }
    let task_id = resolve_task_id_sql(conn, project_id, task_ref)?;
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_task_comments (id, task_id, author_id, author_agent, body, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            &id,
            &task_id,
            author_id.map(|u| u.as_bytes().to_vec()),
            author_agent.as_deref(),
            trimmed,
            now
        ],
    )?;
    conn.execute(
        "UPDATE hub_tasks SET updated_at = ?1 WHERE id = ?2",
        params![now, &task_id],
    )?;
    record_task_activity_sql(
        conn,
        &task_id,
        author_id,
        author_agent.as_deref(),
        "commented",
        None,
        None,
        now,
    )?;

    let (author_username, author_name) = if let Some(uid) = author_id
        && let Some(u) = crate::users::find_user_by_id(conn, uid)?
    {
        (Some(u.username), u.name)
    } else {
        (None, None)
    };
    Ok(HubTaskComment {
        id,
        task_id,
        author_id,
        author_username,
        author_name,
        author_agent,
        body: trimmed.to_string(),
        created_at: format_us_iso(now),
        updated_at: format_us_iso(now),
    })
}

// ── Modules, Cycles & Labels ─────────────────────────────────────────────

pub(crate) fn list_modules_sql(
    conn: &Connection,
    project_id: ProjectId,
) -> StoreResult<Vec<HubModule>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.workspace_id, m.project_id, m.name, m.description, m.status, \
                m.lead_id, u.username, u.name, m.start_date, m.target_date, m.created_by, \
                m.created_at, m.updated_at, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.module_id = m.id AND t.archived_at IS NULL) AS t_total, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.module_id = m.id AND t.archived_at IS NULL AND t.state = 'done') AS t_done \
         FROM hub_modules m LEFT JOIN users u ON u.id = m.lead_id \
         WHERE m.project_id = ?1 ORDER BY m.created_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        Ok(HubModule {
            id: r.get(0)?,
            workspace_id: WorkspaceId::from_slice(&r.get::<_, Vec<u8>>(1)?)
                .unwrap_or_else(|_| WorkspaceId::new()),
            project_id,
            name: r.get(3)?,
            description: r.get(4)?,
            status: r.get(5)?,
            lead_id: r
                .get::<_, Option<Vec<u8>>>(6)?
                .and_then(|b| UserId::from_slice(&b).ok()),
            lead_username: r.get(7)?,
            lead_name: r.get(8)?,
            start_date: r.get(9)?,
            target_date: r.get(10)?,
            created_by: r
                .get::<_, Option<Vec<u8>>>(11)?
                .and_then(|b| UserId::from_slice(&b).ok()),
            created_at: format_us_iso(r.get(12)?),
            updated_at: format_us_iso(r.get(13)?),
            task_total: r.get::<_, i64>(14)? as usize,
            task_done: r.get::<_, i64>(15)? as usize,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn create_module_sql(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    input: NewHubModule,
) -> StoreResult<HubModule> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(StoreError::InvalidState(
            "module name cannot be empty".into(),
        ));
    }
    let status = match input
        .status
        .as_deref()
        .map(str::trim)
        .unwrap_or("planned")
        .to_ascii_lowercase()
        .as_str()
    {
        "backlog" => "backlog",
        "in_progress" | "in-progress" | "active" => "in_progress",
        "paused" => "paused",
        "completed" | "done" => "completed",
        "cancelled" | "canceled" => "cancelled",
        _ => "planned",
    };
    let lead_id = match input.lead_username.as_deref() {
        Some(l) if !l.trim().is_empty() => resolve_user_ref_sql(conn, l).ok().map(|u| u.id),
        _ => None,
    };
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_modules (id, workspace_id, project_id, name, description, status, \
         lead_id, start_date, target_date, created_by, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
        params![
            &id,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            name,
            input.description.as_deref().unwrap_or("").trim(),
            status,
            lead_id.map(|u| u.as_bytes().to_vec()),
            input
                .start_date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            input
                .target_date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            input.created_by.map(|u| u.as_bytes().to_vec()),
            now,
        ],
    )?;
    list_modules_sql(conn, project_id)?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| StoreError::NotFound("created module missing".into()))
}

pub(crate) fn update_module_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    module_ref: &str,
    input: UpdateHubModule,
) -> StoreResult<HubModule> {
    let module_id = resolve_module_ref_sql(conn, project_id, module_ref)?
        .ok_or_else(|| StoreError::NotFound(format!("module {module_ref:?} not found")))?;
    let now = now_us();
    if let Some(ref name) = input.name
        && !name.trim().is_empty()
    {
        conn.execute(
            "UPDATE hub_modules SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name.trim(), now, &module_id],
        )?;
    }
    if let Some(ref desc) = input.description {
        conn.execute(
            "UPDATE hub_modules SET description = ?1, updated_at = ?2 WHERE id = ?3",
            params![desc.trim(), now, &module_id],
        )?;
    }
    if let Some(ref st) = input.status {
        let status = match st.trim().to_ascii_lowercase().as_str() {
            "backlog" => "backlog",
            "in_progress" | "in-progress" | "active" => "in_progress",
            "paused" => "paused",
            "completed" | "done" => "completed",
            "cancelled" | "canceled" => "cancelled",
            _ => "planned",
        };
        conn.execute(
            "UPDATE hub_modules SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now, &module_id],
        )?;
    }
    if let Some(ref lead) = input.lead_username {
        let lead_id = if lead.trim().is_empty() {
            None
        } else {
            resolve_user_ref_sql(conn, lead).ok().map(|u| u.id)
        };
        conn.execute(
            "UPDATE hub_modules SET lead_id = ?1, updated_at = ?2 WHERE id = ?3",
            params![lead_id.map(|u| u.as_bytes().to_vec()), now, &module_id],
        )?;
    }
    if let Some(ref sd) = input.start_date {
        let val = (!sd.trim().is_empty()).then(|| sd.trim().to_string());
        conn.execute(
            "UPDATE hub_modules SET start_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &module_id],
        )?;
    }
    if let Some(ref td) = input.target_date {
        let val = (!td.trim().is_empty()).then(|| td.trim().to_string());
        conn.execute(
            "UPDATE hub_modules SET target_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &module_id],
        )?;
    }
    list_modules_sql(conn, project_id)?
        .into_iter()
        .find(|m| m.id == module_id)
        .ok_or_else(|| StoreError::NotFound("module not found".into()))
}

pub(crate) fn delete_module_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    module_ref: &str,
) -> StoreResult<bool> {
    let Some(module_id) = resolve_module_ref_sql(conn, project_id, module_ref)? else {
        return Ok(false);
    };
    let rows = conn.execute(
        "DELETE FROM hub_modules WHERE id = ?1 AND project_id = ?2",
        params![module_id, project_id.as_bytes()],
    )?;
    Ok(rows > 0)
}

pub(crate) fn list_cycles_sql(
    conn: &Connection,
    project_id: ProjectId,
) -> StoreResult<Vec<HubCycle>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.workspace_id, c.project_id, c.name, c.description, c.status, \
                c.start_date, c.end_date, c.created_by, c.created_at, c.updated_at, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.cycle_id = c.id AND t.archived_at IS NULL) AS t_total, \
                (SELECT COUNT(*) FROM hub_tasks t WHERE t.cycle_id = c.id AND t.archived_at IS NULL AND t.state = 'done') AS t_done \
         FROM hub_cycles c WHERE c.project_id = ?1 ORDER BY c.created_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        Ok(HubCycle {
            id: r.get(0)?,
            workspace_id: WorkspaceId::from_slice(&r.get::<_, Vec<u8>>(1)?)
                .unwrap_or_else(|_| WorkspaceId::new()),
            project_id,
            name: r.get(3)?,
            description: r.get(4)?,
            status: r.get(5)?,
            start_date: r.get(6)?,
            end_date: r.get(7)?,
            created_by: r
                .get::<_, Option<Vec<u8>>>(8)?
                .and_then(|b| UserId::from_slice(&b).ok()),
            created_at: format_us_iso(r.get(9)?),
            updated_at: format_us_iso(r.get(10)?),
            task_total: r.get::<_, i64>(11)? as usize,
            task_done: r.get::<_, i64>(12)? as usize,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn create_cycle_sql(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    input: NewHubCycle,
) -> StoreResult<HubCycle> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(StoreError::InvalidState(
            "cycle name cannot be empty".into(),
        ));
    }
    let status = match input
        .status
        .as_deref()
        .map(str::trim)
        .unwrap_or("active")
        .to_ascii_lowercase()
        .as_str()
    {
        "upcoming" => "upcoming",
        "completed" | "done" => "completed",
        "archived" => "archived",
        _ => "active",
    };
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_cycles (id, workspace_id, project_id, name, description, status, \
         start_date, end_date, created_by, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            &id,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            name,
            input.description.as_deref().unwrap_or("").trim(),
            status,
            input
                .start_date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            input
                .end_date
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            input.created_by.map(|u| u.as_bytes().to_vec()),
            now,
        ],
    )?;
    list_cycles_sql(conn, project_id)?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| StoreError::NotFound("created cycle missing".into()))
}

pub(crate) fn update_cycle_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    cycle_ref: &str,
    input: UpdateHubCycle,
) -> StoreResult<HubCycle> {
    let cycle_id = resolve_cycle_ref_sql(conn, project_id, cycle_ref)?
        .ok_or_else(|| StoreError::NotFound(format!("cycle {cycle_ref:?} not found")))?;
    let now = now_us();
    if let Some(ref name) = input.name
        && !name.trim().is_empty()
    {
        conn.execute(
            "UPDATE hub_cycles SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name.trim(), now, &cycle_id],
        )?;
    }
    if let Some(ref desc) = input.description {
        conn.execute(
            "UPDATE hub_cycles SET description = ?1, updated_at = ?2 WHERE id = ?3",
            params![desc.trim(), now, &cycle_id],
        )?;
    }
    if let Some(ref st) = input.status {
        let status = match st.trim().to_ascii_lowercase().as_str() {
            "upcoming" => "upcoming",
            "completed" | "done" => "completed",
            "archived" => "archived",
            _ => "active",
        };
        conn.execute(
            "UPDATE hub_cycles SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now, &cycle_id],
        )?;
    }
    if let Some(ref sd) = input.start_date {
        let val = (!sd.trim().is_empty()).then(|| sd.trim().to_string());
        conn.execute(
            "UPDATE hub_cycles SET start_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &cycle_id],
        )?;
    }
    if let Some(ref ed) = input.end_date {
        let val = (!ed.trim().is_empty()).then(|| ed.trim().to_string());
        conn.execute(
            "UPDATE hub_cycles SET end_date = ?1, updated_at = ?2 WHERE id = ?3",
            params![val, now, &cycle_id],
        )?;
    }
    list_cycles_sql(conn, project_id)?
        .into_iter()
        .find(|c| c.id == cycle_id)
        .ok_or_else(|| StoreError::NotFound("cycle not found".into()))
}

pub(crate) fn delete_cycle_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    cycle_ref: &str,
) -> StoreResult<bool> {
    let Some(cycle_id) = resolve_cycle_ref_sql(conn, project_id, cycle_ref)? else {
        return Ok(false);
    };
    let rows = conn.execute(
        "DELETE FROM hub_cycles WHERE id = ?1 AND project_id = ?2",
        params![cycle_id, project_id.as_bytes()],
    )?;
    Ok(rows > 0)
}

pub(crate) fn list_labels_sql(
    conn: &Connection,
    project_id: ProjectId,
) -> StoreResult<Vec<HubLabel>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, color, description FROM hub_labels WHERE project_id = ?1 ORDER BY name ASC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        Ok(HubLabel {
            id: r.get(0)?,
            project_id,
            name: r.get(1)?,
            color: r.get(2)?,
            description: r.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn create_label_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    name: &str,
    color: &str,
    description: &str,
) -> StoreResult<HubLabel> {
    let n = name.trim();
    if n.is_empty() {
        return Err(StoreError::InvalidState(
            "label name cannot be empty".into(),
        ));
    }
    let c = if color.trim().is_empty() {
        "#6366f1"
    } else {
        color.trim()
    };
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_labels (id, project_id, name, color, description, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(project_id, name) DO UPDATE SET color = excluded.color, description = excluded.description",
        params![&id, project_id.as_bytes(), n, c, description.trim(), now],
    )?;
    list_labels_sql(conn, project_id)?
        .into_iter()
        .find(|l| l.name.eq_ignore_ascii_case(n))
        .ok_or_else(|| StoreError::NotFound("label not found".into()))
}

// ── GitHub Repositories & Items ──────────────────────────────────────────

pub(crate) fn list_github_repos_sql(
    conn: &Connection,
    project_id: ProjectId,
) -> StoreResult<Vec<HubGithubRepo>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.workspace_id, r.owner, r.repo, r.full_name, r.default_branch, \
                r.access_token IS NOT NULL AND length(r.access_token) > 0, \
                r.added_by, u.username, r.last_synced_at, r.last_sync_error, r.created_at, \
                (SELECT COUNT(*) FROM hub_github_items gi WHERE gi.repo_id = r.id AND gi.kind = 'issue' AND gi.state = 'open') AS open_issues, \
                (SELECT COUNT(*) FROM hub_github_items gi WHERE gi.repo_id = r.id AND gi.kind = 'pr' AND gi.state = 'open') AS open_prs \
         FROM hub_github_repos r LEFT JOIN users u ON u.id = r.added_by \
         WHERE r.project_id = ?1 ORDER BY r.created_at ASC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        Ok(HubGithubRepo {
            id: r.get(0)?,
            workspace_id: WorkspaceId::from_slice(&r.get::<_, Vec<u8>>(1)?)
                .unwrap_or_else(|_| WorkspaceId::new()),
            project_id,
            owner: r.get(2)?,
            repo: r.get(3)?,
            full_name: r.get(4)?,
            default_branch: r.get(5)?,
            has_token: r.get(6)?,
            added_by: r
                .get::<_, Option<Vec<u8>>>(7)?
                .and_then(|b| UserId::from_slice(&b).ok()),
            added_by_username: r.get(8)?,
            last_synced_at: r.get::<_, Option<i64>>(9)?.map(format_us_iso),
            last_sync_error: r.get(10)?,
            open_issues_count: r.get::<_, i64>(12)? as usize,
            open_prs_count: r.get::<_, i64>(13)? as usize,
            created_at: format_us_iso(r.get(11)?),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(crate) fn get_github_repo_with_token_sql(
    conn: &Connection,
    project_id: ProjectId,
    repo_id_or_name: &str,
) -> StoreResult<HubGithubRepoWithToken> {
    let trimmed = repo_id_or_name.trim();
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT id, access_token FROM hub_github_repos \
             WHERE project_id = ?1 AND (id = ?2 OR lower(full_name) = lower(?2))",
            params![project_id.as_bytes(), trimmed],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((id, access_token)) = row else {
        return Err(StoreError::NotFound(format!(
            "GitHub repo {trimmed:?} not found in project"
        )));
    };
    let repo = list_github_repos_sql(conn, project_id)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| StoreError::NotFound("GitHub repo not found".into()))?;
    Ok(HubGithubRepoWithToken { repo, access_token })
}

pub(crate) fn add_github_repo_sql(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    full_name: &str,
    default_branch: Option<&str>,
    access_token: Option<String>,
    added_by: Option<UserId>,
) -> StoreResult<HubGithubRepo> {
    let cleaned = full_name
        .trim()
        .trim_start_matches("https://github.com/")
        .trim_end_matches(".git")
        .trim_matches('/');
    let Some((owner, repo)) = cleaned.split_once('/') else {
        return Err(StoreError::InvalidState(
            "repository must be in 'owner/repo' format".into(),
        ));
    };
    let owner = owner.trim();
    let repo = repo.trim();
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err(StoreError::InvalidState(
            "repository must be in 'owner/repo' format".into(),
        ));
    }
    let normalized = format!("{owner}/{repo}");
    let branch = default_branch
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("main");
    let token = access_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let id = Uuid::now_v7().to_string();
    let now = now_us();
    conn.execute(
        "INSERT INTO hub_github_repos (id, workspace_id, project_id, owner, repo, full_name, \
         default_branch, access_token, added_by, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
         ON CONFLICT(project_id, full_name) DO UPDATE SET \
         default_branch = excluded.default_branch, \
         access_token = COALESCE(excluded.access_token, hub_github_repos.access_token)",
        params![
            &id,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            owner,
            repo,
            &normalized,
            branch,
            token,
            added_by.map(|u| u.as_bytes().to_vec()),
            now,
        ],
    )?;
    list_github_repos_sql(conn, project_id)?
        .into_iter()
        .find(|r| r.full_name.eq_ignore_ascii_case(&normalized))
        .ok_or_else(|| StoreError::NotFound("added repo missing".into()))
}

pub(crate) fn remove_github_repo_sql(
    conn: &mut Connection,
    project_id: ProjectId,
    repo_id_or_name: &str,
) -> StoreResult<bool> {
    let trimmed = repo_id_or_name.trim();
    let rows = conn.execute(
        "DELETE FROM hub_github_repos WHERE project_id = ?1 AND (id = ?2 OR lower(full_name) = lower(?2))",
        params![project_id.as_bytes(), trimmed],
    )?;
    Ok(rows > 0)
}

pub(crate) fn record_github_sync_result_sql(
    conn: &mut Connection,
    repo_id: &str,
    project_id: ProjectId,
    repo_full_name: &str,
    items: &[UpsertGithubItem],
    sync_error: Option<&str>,
) -> StoreResult<usize> {
    let now = now_us();
    let tx = conn.transaction()?;
    if let Some(err) = sync_error {
        tx.execute(
            "UPDATE hub_github_repos SET last_sync_error = ?1 WHERE id = ?2",
            params![err, repo_id],
        )?;
        tx.commit()?;
        return Ok(0);
    }
    let mut count = 0usize;
    for item in items {
        let item_id = format!("{repo_id}:{}:{}", item.kind, item.number);
        let assignees_json =
            serde_json::to_string(&item.assignees).unwrap_or_else(|_| "[]".to_string());
        let labels_json = serde_json::to_string(&item.labels).unwrap_or_else(|_| "[]".to_string());
        tx.execute(
            "INSERT INTO hub_github_items \
             (id, repo_id, project_id, repo_full_name, kind, number, title, body, state, draft, \
              author_login, author_avatar_url, assignees_json, labels_json, html_url, head_ref, base_ref, \
              comments_count, gh_created_at, gh_updated_at, synced_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21) \
             ON CONFLICT(repo_id, number) DO UPDATE SET \
             kind = excluded.kind, title = excluded.title, body = excluded.body, \
             state = excluded.state, draft = excluded.draft, author_login = excluded.author_login, \
             author_avatar_url = excluded.author_avatar_url, assignees_json = excluded.assignees_json, \
             labels_json = excluded.labels_json, html_url = excluded.html_url, \
             head_ref = excluded.head_ref, base_ref = excluded.base_ref, \
             comments_count = excluded.comments_count, gh_updated_at = excluded.gh_updated_at, \
             synced_at = excluded.synced_at",
            params![
                &item_id,
                repo_id,
                project_id.as_bytes(),
                repo_full_name,
                &item.kind,
                item.number,
                &item.title,
                &item.body,
                &item.state,
                i64::from(item.draft),
                &item.author_login,
                item.author_avatar_url.as_deref(),
                assignees_json,
                labels_json,
                &item.html_url,
                item.head_ref.as_deref(),
                item.base_ref.as_deref(),
                item.comments_count,
                item.gh_created_at.as_deref(),
                item.gh_updated_at.as_deref(),
                now,
            ],
        )?;
        // Also refresh title/state on any linked tasks!
        tx.execute(
            "UPDATE hub_task_github_links SET github_item_id = ?1, title = ?2, state = ?3, html_url = ?4 \
             WHERE lower(repo_full_name) = lower(?5) AND number = ?6",
            params![&item_id, &item.title, &item.state, &item.html_url, repo_full_name, item.number],
        )?;
        count += 1;
    }
    tx.execute(
        "UPDATE hub_github_repos SET last_synced_at = ?1, last_sync_error = NULL WHERE id = ?2",
        params![now, repo_id],
    )?;
    tx.commit()?;
    Ok(count)
}

pub(crate) fn list_github_items_sql(
    conn: &Connection,
    project_id: ProjectId,
    repo_id: Option<&str>,
    kind: Option<&str>,
    state: Option<&str>,
    search: Option<&str>,
) -> StoreResult<Vec<HubGithubItem>> {
    let prefix: String = conn
        .query_row(
            "SELECT COALESCE(NULLIF(identifier_prefix, ''), name) FROM projects WHERE id = ?1",
            params![project_id.as_bytes()],
            |r| r.get(0),
        )
        .optional()?
        .map(|s: String| {
            if s.len() <= 6
                && s.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                s
            } else {
                derive_identifier_prefix(&s)
            }
        })
        .unwrap_or_else(|| "PRJ".to_string());

    let mut stmt = conn.prepare(
        "SELECT id, repo_id, repo_full_name, kind, number, title, body, state, draft, \
                author_login, author_avatar_url, assignees_json, labels_json, html_url, \
                head_ref, base_ref, comments_count, gh_created_at, gh_updated_at, synced_at \
         FROM hub_github_items WHERE project_id = ?1 ORDER BY number DESC",
    )?;
    let rows = stmt.query_map(params![project_id.as_bytes()], |r| {
        let assignees_s: String = r.get(11)?;
        let labels_s: String = r.get(12)?;
        let assignees: Vec<String> = serde_json::from_str(&assignees_s).unwrap_or_default();
        let labels: Vec<HubGithubLabel> = serde_json::from_str(&labels_s).unwrap_or_default();
        Ok(HubGithubItem {
            id: r.get(0)?,
            repo_id: r.get(1)?,
            project_id,
            repo_full_name: r.get(2)?,
            kind: r.get(3)?,
            number: r.get(4)?,
            title: r.get(5)?,
            body: r.get(6)?,
            state: r.get(7)?,
            draft: r.get::<_, i64>(8)? != 0,
            author_login: r.get(9)?,
            author_avatar_url: r.get(10)?,
            assignees,
            labels,
            html_url: r.get(13)?,
            head_ref: r.get(14)?,
            base_ref: r.get(15)?,
            comments_count: r.get(16)?,
            gh_created_at: r.get(17)?,
            gh_updated_at: r.get(18)?,
            synced_at: format_us_iso(r.get(19)?),
            linked_tasks: Vec::new(),
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let mut item = row?;
        if let Some(rid) = repo_id
            && !rid.trim().is_empty()
            && rid != "all"
            && item.repo_id != rid.trim()
            && !item.repo_full_name.eq_ignore_ascii_case(rid.trim())
        {
            continue;
        }
        if let Some(k) = kind
            && !k.trim().is_empty()
            && k != "all"
            && !item.kind.eq_ignore_ascii_case(k.trim())
        {
            continue;
        }
        if let Some(st) = state
            && !st.trim().is_empty()
            && st != "all"
            && !item.state.eq_ignore_ascii_case(st.trim())
        {
            continue;
        }
        if let Some(q) = search
            && !q.trim().is_empty()
        {
            let needle = q.trim().to_ascii_lowercase();
            let hay = format!(
                "#{} {} {} {}",
                item.number,
                item.title.to_ascii_lowercase(),
                item.body.to_ascii_lowercase(),
                item.author_login.to_ascii_lowercase()
            );
            if !hay.contains(&needle) {
                continue;
            }
        }
        let mut lstmt = conn.prepare(
            "SELECT t.sequence_id FROM hub_task_github_links gl \
             JOIN hub_tasks t ON t.id = gl.task_id \
             WHERE t.project_id = ?1 AND lower(gl.repo_full_name) = lower(?2) AND gl.number = ?3 \
             ORDER BY t.sequence_id ASC",
        )?;
        let seqs: Vec<i64> = lstmt
            .query_map(
                params![project_id.as_bytes(), &item.repo_full_name, item.number],
                |r| r.get(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        item.linked_tasks = seqs.into_iter().map(|s| format!("{prefix}-{s}")).collect();
        out.push(item);
    }
    Ok(out)
}

// ── ReaderPool & WriterHandle Async Facades ──────────────────────────────

impl ReaderPool {
    /// Read auto token pepper from `hub_settings` if present.
    pub async fn get_auto_token_pepper(&self) -> StoreResult<Option<String>> {
        self.with_conn(|conn| {
            let res: Option<Option<String>> = conn
                .query_row(
                    "SELECT auto_token_pepper FROM hub_settings WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(res.flatten().filter(|s| !s.trim().is_empty()))
        })
        .await
    }

    /// Read instance-level AI Hub settings.
    pub async fn hub_settings(&self) -> StoreResult<HubSettings> {
        self.with_conn(get_hub_settings_sql).await
    }

    /// Read the user's effective role on a project (`None` if denied).
    pub async fn hub_user_role_on_project(
        &self,
        project_id: ProjectId,
        viewer: Option<UserId>,
        is_root: bool,
    ) -> StoreResult<Option<HubProjectRole>> {
        self.with_conn(move |conn| user_role_on_project_sql(conn, project_id, viewer, is_root))
            .await
    }

    /// List all projects visible to `viewer` with AI Hub stats.
    pub async fn list_hub_projects(
        &self,
        viewer: Option<UserId>,
        is_root: bool,
    ) -> StoreResult<Vec<HubProjectOverview>> {
        self.with_conn(move |conn| list_hub_projects_sql(conn, viewer, is_root))
            .await
    }

    /// Get a single project's AI Hub overview.
    pub async fn get_hub_project(
        &self,
        workspace: String,
        project: String,
        viewer: Option<UserId>,
        is_root: bool,
    ) -> StoreResult<HubProjectOverview> {
        self.with_conn(move |conn| get_hub_project_sql(conn, &workspace, &project, viewer, is_root))
            .await
    }

    /// List members of a project.
    pub async fn list_project_members(
        &self,
        project_id: ProjectId,
    ) -> StoreResult<Vec<HubProjectMember>> {
        self.with_conn(move |conn| list_project_members_sql(conn, project_id))
            .await
    }

    /// List tasks in a project matching `filter`.
    pub async fn list_project_tasks(
        &self,
        project_id: ProjectId,
        filter: HubTaskFilter,
    ) -> StoreResult<Vec<HubTask>> {
        self.with_conn(move |conn| list_project_tasks_sql(conn, project_id, &filter))
            .await
    }

    /// List tasks assigned to or created by `user_id` across all accessible projects.
    pub async fn list_user_tasks(
        &self,
        user_id: UserId,
        is_root: bool,
    ) -> StoreResult<Vec<HubTask>> {
        self.with_conn(move |conn| list_user_tasks_sql(conn, user_id, is_root))
            .await
    }

    /// Get full details of one task by UUID, `PREFIX-12` key, or sequence number.
    pub async fn get_task_detail(
        &self,
        project_id: ProjectId,
        task_ref: String,
    ) -> StoreResult<HubTaskDetail> {
        self.with_conn(move |conn| get_task_detail_sql(conn, project_id, &task_ref))
            .await
    }

    /// List modules in a project.
    pub async fn list_modules(&self, project_id: ProjectId) -> StoreResult<Vec<HubModule>> {
        self.with_conn(move |conn| list_modules_sql(conn, project_id))
            .await
    }

    /// List cycles in a project.
    pub async fn list_cycles(&self, project_id: ProjectId) -> StoreResult<Vec<HubCycle>> {
        self.with_conn(move |conn| list_cycles_sql(conn, project_id))
            .await
    }

    /// List labels in a project.
    pub async fn list_labels(&self, project_id: ProjectId) -> StoreResult<Vec<HubLabel>> {
        self.with_conn(move |conn| list_labels_sql(conn, project_id))
            .await
    }

    /// List connected GitHub repositories in a project.
    pub async fn list_github_repos(
        &self,
        project_id: ProjectId,
    ) -> StoreResult<Vec<HubGithubRepo>> {
        self.with_conn(move |conn| list_github_repos_sql(conn, project_id))
            .await
    }

    /// Get a connected GitHub repository and its optional PAT for syncing.
    pub async fn get_github_repo_with_token(
        &self,
        project_id: ProjectId,
        repo_id_or_name: String,
    ) -> StoreResult<HubGithubRepoWithToken> {
        self.with_conn(move |conn| {
            get_github_repo_with_token_sql(conn, project_id, &repo_id_or_name)
        })
        .await
    }

    /// List synced GitHub Issues and PRs for a project.
    pub async fn list_github_items(
        &self,
        project_id: ProjectId,
        repo_id: Option<String>,
        kind: Option<String>,
        state: Option<String>,
        search: Option<String>,
    ) -> StoreResult<Vec<HubGithubItem>> {
        self.with_conn(move |conn| {
            list_github_items_sql(
                conn,
                project_id,
                repo_id.as_deref(),
                kind.as_deref(),
                state.as_deref(),
                search.as_deref(),
            )
        })
        .await
    }
}

impl WriterHandle {
    /// Ensure a persistent token pepper exists in `hub_settings` and return it.
    pub async fn ensure_auto_token_pepper(&self) -> StoreResult<String> {
        self.with_conn(ensure_auto_token_pepper_sql).await
    }

    /// Update instance-level AI Hub settings.
    pub async fn update_hub_settings(
        &self,
        instance_name: Option<String>,
        allow_registration: Option<bool>,
        default_project_access: Option<String>,
    ) -> StoreResult<HubSettings> {
        self.with_conn(move |conn| {
            update_hub_settings_sql(
                conn,
                instance_name,
                allow_registration,
                default_project_access,
            )
        })
        .await
    }

    /// Register a human user account (promoting the first user on the server to `root`).
    pub async fn register_human_user(
        &self,
        new_user: NewUser,
        password_hash: String,
    ) -> StoreResult<(User, bool)> {
        self.with_conn(move |conn| register_human_user_sql(conn, &new_user, &password_hash))
            .await
    }

    /// Update a user's display name and email.
    pub async fn update_user_profile(
        &self,
        user_id: UserId,
        name: Option<String>,
        email: Option<String>,
    ) -> StoreResult<User> {
        self.with_conn(move |conn| update_user_profile_sql(conn, user_id, name, email))
            .await
    }

    /// Update a user's role or disabled state (Admin operation).
    pub async fn admin_update_user(
        &self,
        user_id: UserId,
        role: Option<UserRole>,
        disabled: Option<bool>,
    ) -> StoreResult<User> {
        self.with_conn(move |conn| admin_update_user_sql(conn, user_id, role, disabled))
            .await
    }

    /// Create a new AI Hub project with prefix, description, access mode, and default labels.
    pub async fn create_hub_project(
        &self,
        workspace: String,
        name: String,
        identifier_prefix: Option<String>,
        description: String,
        access_mode: AccessMode,
        creator: Option<UserId>,
    ) -> StoreResult<(WorkspaceId, ProjectId)> {
        self.with_conn(move |conn| {
            create_hub_project_sql(
                conn,
                &workspace,
                &name,
                identifier_prefix.as_deref(),
                &description,
                access_mode,
                creator,
            )
        })
        .await
    }

    /// Update project metadata and access mode.
    pub async fn update_hub_project(
        &self,
        project_id: ProjectId,
        description: Option<String>,
        identifier_prefix: Option<String>,
        access_mode: Option<AccessMode>,
    ) -> StoreResult<()> {
        self.with_conn(move |conn| {
            update_hub_project_sql(
                conn,
                project_id,
                description.as_deref(),
                identifier_prefix.as_deref(),
                access_mode,
            )
        })
        .await
    }

    /// Grant or update a user's role (`read`, `write`, `admin`) on a project.
    pub async fn set_project_member_role(
        &self,
        project_id: ProjectId,
        target_username_or_id: String,
        role: String,
        granted_by: Option<UserId>,
    ) -> StoreResult<HubProjectMember> {
        self.with_conn(move |conn| {
            set_project_member_role_sql(conn, project_id, &target_username_or_id, &role, granted_by)
        })
        .await
    }

    /// Remove a user's grant from a project.
    pub async fn remove_project_member(
        &self,
        project_id: ProjectId,
        target_username_or_id: String,
        revoked_by: Option<UserId>,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| {
            remove_project_member_sql(conn, project_id, &target_username_or_id, revoked_by)
        })
        .await
    }

    /// Create a new task in a project.
    pub async fn create_task(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        input: NewHubTask,
    ) -> StoreResult<HubTaskDetail> {
        self.with_conn(move |conn| create_task_sql(conn, workspace_id, project_id, input))
            .await
    }

    /// Update an existing task in a project.
    pub async fn update_task(
        &self,
        project_id: ProjectId,
        task_ref: String,
        input: UpdateHubTask,
    ) -> StoreResult<HubTaskDetail> {
        self.with_conn(move |conn| update_task_sql(conn, project_id, &task_ref, input))
            .await
    }

    /// Delete a task from a project.
    pub async fn delete_task(&self, project_id: ProjectId, task_ref: String) -> StoreResult<bool> {
        self.with_conn(move |conn| delete_task_sql(conn, project_id, &task_ref))
            .await
    }

    /// Add a comment to a task.
    pub async fn add_task_comment(
        &self,
        project_id: ProjectId,
        task_ref: String,
        author_id: Option<UserId>,
        author_agent: Option<String>,
        body: String,
    ) -> StoreResult<HubTaskComment> {
        self.with_conn(move |conn| {
            add_task_comment_sql(conn, project_id, &task_ref, author_id, author_agent, body)
        })
        .await
    }

    /// Link a GitHub Issue or PR to a task.
    pub async fn link_task_github(
        &self,
        project_id: ProjectId,
        task_ref: String,
        link: NewTaskGithubLink,
        actor_id: Option<UserId>,
        actor_agent: Option<String>,
    ) -> StoreResult<HubTaskDetail> {
        self.with_conn(move |conn| {
            let task_id = resolve_task_id_sql(conn, project_id, &task_ref)?;
            let now = now_us();
            insert_task_github_link_in_tx(conn, project_id, &task_id, &link, now)?;
            record_task_activity_sql(
                conn,
                &task_id,
                actor_id,
                actor_agent.as_deref(),
                "linked_github",
                None,
                Some(&format!("{}#{}", link.repo_full_name, link.number)),
                now,
            )?;
            get_task_detail_sql(conn, project_id, &task_id)
        })
        .await
    }

    /// Create a Plane-style Module in a project.
    pub async fn create_module(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        input: NewHubModule,
    ) -> StoreResult<HubModule> {
        self.with_conn(move |conn| create_module_sql(conn, workspace_id, project_id, input))
            .await
    }

    /// Update a Module in a project.
    pub async fn update_module(
        &self,
        project_id: ProjectId,
        module_ref: String,
        input: UpdateHubModule,
    ) -> StoreResult<HubModule> {
        self.with_conn(move |conn| update_module_sql(conn, project_id, &module_ref, input))
            .await
    }

    /// Delete a Module from a project.
    pub async fn delete_module(
        &self,
        project_id: ProjectId,
        module_ref: String,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| delete_module_sql(conn, project_id, &module_ref))
            .await
    }

    /// Create a Cycle/Sprint in a project.
    pub async fn create_cycle(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        input: NewHubCycle,
    ) -> StoreResult<HubCycle> {
        self.with_conn(move |conn| create_cycle_sql(conn, workspace_id, project_id, input))
            .await
    }

    /// Update a Cycle/Sprint in a project.
    pub async fn update_cycle(
        &self,
        project_id: ProjectId,
        cycle_ref: String,
        input: UpdateHubCycle,
    ) -> StoreResult<HubCycle> {
        self.with_conn(move |conn| update_cycle_sql(conn, project_id, &cycle_ref, input))
            .await
    }

    /// Delete a Cycle/Sprint from a project.
    pub async fn delete_cycle(
        &self,
        project_id: ProjectId,
        cycle_ref: String,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| delete_cycle_sql(conn, project_id, &cycle_ref))
            .await
    }

    /// Create or update a label in a project.
    pub async fn create_label(
        &self,
        project_id: ProjectId,
        name: String,
        color: String,
        description: String,
    ) -> StoreResult<HubLabel> {
        self.with_conn(move |conn| create_label_sql(conn, project_id, &name, &color, &description))
            .await
    }

    /// Delete a label from a project by ID or name.
    pub async fn delete_label(
        &self,
        project_id: ProjectId,
        label_ref: String,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| {
            let trimmed = label_ref.trim();
            let n = conn.execute(
                "DELETE FROM hub_labels WHERE project_id = ?1 AND (id = ?2 OR lower(name) = lower(?2))",
                params![project_id.as_bytes(), trimmed],
            )?;
            Ok(n > 0)
        })
        .await
    }

    /// Add or update a GitHub repository on a project.
    pub async fn add_github_repo(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        full_name: String,
        default_branch: Option<String>,
        access_token: Option<String>,
        added_by: Option<UserId>,
    ) -> StoreResult<HubGithubRepo> {
        self.with_conn(move |conn| {
            add_github_repo_sql(
                conn,
                workspace_id,
                project_id,
                &full_name,
                default_branch.as_deref(),
                access_token,
                added_by,
            )
        })
        .await
    }

    /// Remove a GitHub repository from a project.
    pub async fn remove_github_repo(
        &self,
        project_id: ProjectId,
        repo_id_or_name: String,
    ) -> StoreResult<bool> {
        self.with_conn(move |conn| remove_github_repo_sql(conn, project_id, &repo_id_or_name))
            .await
    }

    /// Record synced GitHub Issues and PRs for a repository.
    pub async fn record_github_sync_result(
        &self,
        repo_id: String,
        project_id: ProjectId,
        repo_full_name: String,
        items: Vec<UpsertGithubItem>,
        sync_error: Option<String>,
    ) -> StoreResult<usize> {
        self.with_conn(move |conn| {
            record_github_sync_result_sql(
                conn,
                &repo_id,
                project_id,
                &repo_full_name,
                &items,
                sync_error.as_deref(),
            )
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use ai_memory_core::{NewUser, UserRole};

    #[tokio::test]
    async fn hub_end_to_end_multi_user_tasks_modules_github_and_team_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        let writer = store.writer.clone();
        let reader = store.reader.clone();

        // 1. Auto-pepper generation works out-of-the-box.
        let pepper1 = writer.ensure_auto_token_pepper().await.expect("pepper 1");
        let pepper2 = writer.ensure_auto_token_pepper().await.expect("pepper 2");
        assert_eq!(pepper1, pepper2);
        assert!(pepper1.len() >= 32);

        // 2. First registered user becomes Root; second user becomes User.
        let (alice, alice_promoted) = writer
            .register_human_user(
                NewUser {
                    username: "alice".to_string(),
                    name: Some("Alice Admin".to_string()),
                    email: Some("alice@acme.dev".to_string()),
                },
                "$argon2id$v=19$m=19456,t=2,p=1$dummy$dummy".to_string(),
            )
            .await
            .expect("register alice");
        assert!(alice_promoted);
        assert_eq!(alice.role, UserRole::Root);

        let (bob, bob_promoted) = writer
            .register_human_user(
                NewUser {
                    username: "bob".to_string(),
                    name: Some("Bob Engineer".to_string()),
                    email: Some("bob@acme.dev".to_string()),
                },
                "$argon2id$v=19$m=19456,t=2,p=1$dummy$dummy".to_string(),
            )
            .await
            .expect("register bob");
        assert!(!bob_promoted);
        assert_eq!(bob.role, UserRole::User);

        // 3. Create restricted team project & invite Bob with Write access.
        let (ws_id, proj_id) = writer
            .create_hub_project(
                "acme".to_string(),
                "ai-hub".to_string(),
                Some("AIH".to_string()),
                "Team engineering hub".to_string(),
                AccessMode::Restricted,
                Some(alice.id),
            )
            .await
            .expect("create hub project");

        let bob_member = writer
            .set_project_member_role(
                proj_id,
                "bob".to_string(),
                "write".to_string(),
                Some(alice.id),
            )
            .await
            .expect("grant bob write");
        assert_eq!(bob_member.role, HubProjectRole::Write);

        let team = reader
            .list_project_members(proj_id)
            .await
            .expect("list project members");
        assert_eq!(team.len(), 2);

        let bob_role = reader
            .hub_user_role_on_project(proj_id, Some(bob.id), false)
            .await
            .expect("bob role");
        assert_eq!(bob_role, Some(HubProjectRole::Write));

        let (charlie, _) = writer
            .register_human_user(
                NewUser {
                    username: "charlie".to_string(),
                    name: Some("Charlie Outsider".to_string()),
                    email: Some("charlie@acme.dev".to_string()),
                },
                "$argon2id$v=19$m=19456,t=2,p=1$dummy$dummy".to_string(),
            )
            .await
            .expect("register charlie");
        let charlie_role = reader
            .hub_user_role_on_project(proj_id, Some(charlie.id), false)
            .await
            .expect("charlie role");
        assert_eq!(charlie_role, None, "uninvited user must have no role");

        let charlie_projects = reader
            .list_hub_projects(Some(charlie.id), false)
            .await
            .expect("charlie projects");
        assert_eq!(
            charlie_projects.len(),
            0,
            "uninvited user must see 0 projects"
        );

        let guest_projects = reader
            .list_hub_projects(None, false)
            .await
            .expect("guest projects");
        assert_eq!(guest_projects.len(), 0, "guest must see 0 projects");

        let bob_projects = reader
            .list_hub_projects(Some(bob.id), false)
            .await
            .expect("bob projects");
        assert_eq!(bob_projects.len(), 1, "invited user must see project");

        let alice_projects = reader
            .list_hub_projects(Some(alice.id), true)
            .await
            .expect("alice root projects");
        assert_eq!(alice_projects.len(), 1, "root must see project");

        // 4. Create Module, Cycle, and Labels.
        let module = writer
            .create_module(
                ws_id,
                proj_id,
                NewHubModule {
                    name: "Core Platform".to_string(),
                    description: Some("Multi-user auth, tasks, and GitHub sync".to_string()),
                    status: Some("in_progress".to_string()),
                    lead_username: Some("alice".to_string()),
                    start_date: Some("2026-11-01".to_string()),
                    target_date: Some("2026-12-01".to_string()),
                    created_by: Some(alice.id),
                },
            )
            .await
            .expect("create module");
        assert_eq!(module.name, "Core Platform");
        assert_eq!(module.lead_username.as_deref(), Some("alice"));

        let cycle = writer
            .create_cycle(
                ws_id,
                proj_id,
                NewHubCycle {
                    name: "Sprint 1".to_string(),
                    description: Some("Initial launch sprint".to_string()),
                    status: Some("active".to_string()),
                    start_date: Some("2026-11-01".to_string()),
                    end_date: Some("2026-11-15".to_string()),
                    created_by: Some(alice.id),
                },
            )
            .await
            .expect("create cycle");
        assert_eq!(cycle.name, "Sprint 1");

        // 5. Create Task with assignees, module, cycle, dates, labels, wiki links, and GitHub links.
        let created = writer
            .create_task(
                ws_id,
                proj_id,
                NewHubTask {
                    title: "Implement Plane-style Kanban board".to_string(),
                    description: Some(
                        "Support drag & drop, priorities, modules, and GitHub links.".to_string(),
                    ),
                    state: Some("in_progress".to_string()),
                    priority: Some("urgent".to_string()),
                    module_id: Some(module.id.clone()),
                    cycle_id: Some(cycle.id.clone()),
                    parent_task_id: None,
                    start_date: Some("2026-11-02".to_string()),
                    due_date: Some("2026-11-10".to_string()),
                    estimate_points: Some(8),
                    created_by: Some(alice.id),
                    created_by_agent: Some("claude-code".to_string()),
                    assignees: vec!["alice".to_string(), "bob".to_string()],
                    labels: vec!["frontend".to_string(), "feature".to_string()],
                    wiki_pages: vec!["architecture/overview.md".to_string()],
                    github_links: vec![NewTaskGithubLink {
                        repo_full_name: "acme/ai-hub".to_string(),
                        kind: "pr".to_string(),
                        number: 42,
                        title: Some("feat: add Kanban board".to_string()),
                        state: Some("open".to_string()),
                        html_url: None,
                        relation: Some("implements".to_string()),
                    }],
                },
            )
            .await
            .expect("create task");

        assert_eq!(created.task.sequence_id, 1);
        assert_eq!(created.task.key, "AIH-1");
        assert_eq!(created.task.assignees.len(), 2);
        assert_eq!(created.task.wiki_pages.len(), 1);
        assert_eq!(created.task.github_links.len(), 1);

        // 6. Add comment & update task state to done.
        let comment = writer
            .add_task_comment(
                proj_id,
                created.task.key.clone(),
                Some(bob.id),
                Some("codex".to_string()),
                "Kanban board and slide-over drawer verified!".to_string(),
            )
            .await
            .expect("add comment");
        assert_eq!(comment.author_username.as_deref(), Some("bob"));

        let updated = writer
            .update_task(
                proj_id,
                created.task.key.clone(),
                UpdateHubTask {
                    state: Some("done".to_string()),
                    actor_id: Some(bob.id),
                    actor_agent: Some("codex".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("update task");
        assert_eq!(updated.task.state, "done");
        assert!(updated.task.completed_at.is_some());
        assert_eq!(updated.comments.len(), 1);
        assert!(updated.activities.len() >= 2);

        // 7. Attach GitHub repo, sync issue, and verify linked_tasks resolution.
        let repo = writer
            .add_github_repo(
                ws_id,
                proj_id,
                "acme/ai-hub".to_string(),
                Some("main".to_string()),
                None,
                Some(alice.id),
            )
            .await
            .expect("add github repo");
        assert_eq!(repo.full_name, "acme/ai-hub");

        let synced = writer
            .record_github_sync_result(
                repo.id.clone(),
                proj_id,
                repo.full_name.clone(),
                vec![UpsertGithubItem {
                    kind: "pr".to_string(),
                    number: 42,
                    title: "feat: add Kanban board".to_string(),
                    body: "Implements AIH-1".to_string(),
                    state: "open".to_string(),
                    draft: false,
                    author_login: "alice".to_string(),
                    author_avatar_url: None,
                    assignees: vec!["bob".to_string()],
                    labels: vec![HubGithubLabel {
                        name: "frontend".to_string(),
                        color: "6366f1".to_string(),
                    }],
                    html_url: "https://github.com/acme/ai-hub/pull/42".to_string(),
                    head_ref: Some("feat/kanban".to_string()),
                    base_ref: Some("main".to_string()),
                    comments_count: 2,
                    gh_created_at: Some("2026-11-03T10:00:00Z".to_string()),
                    gh_updated_at: Some("2026-11-03T12:00:00Z".to_string()),
                }],
                None,
            )
            .await
            .expect("sync github item");
        assert_eq!(synced, 1);

        let gh_items = reader
            .list_github_items(proj_id, None, None, None, None)
            .await
            .expect("list github items");
        assert_eq!(gh_items.len(), 1);
        assert_eq!(gh_items[0].linked_tasks, vec!["AIH-1".to_string()]);
    }
}
