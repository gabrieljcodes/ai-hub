-- AI Hub multi-user team collaboration, Plane-style Tasks/Modules/Cycles,
-- and GitHub Repositories / Issues / PRs integration.

-- 1. Instance-level settings and automatic pepper persistence
CREATE TABLE IF NOT EXISTS hub_settings (
    id                      INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    instance_name           TEXT NOT NULL DEFAULT 'AI Hub',
    allow_registration      INTEGER NOT NULL DEFAULT 1 CHECK (allow_registration IN (0, 1)),
    default_project_access  TEXT NOT NULL DEFAULT 'restricted' CHECK (default_project_access IN ('open', 'restricted')),
    auto_token_pepper       TEXT,
    updated_at              INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO hub_settings (id, instance_name, allow_registration, default_project_access, updated_at)
VALUES (1, 'AI Hub', 1, 'restricted', 0);

-- 2. Project extensions for Tasks & Team collaboration
ALTER TABLE projects ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN identifier_prefix TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN task_seq INTEGER NOT NULL DEFAULT 0;

-- 3. Project-level manager/admin flag on project_grants
ALTER TABLE project_grants ADD COLUMN is_manager INTEGER NOT NULL DEFAULT 0
    CHECK (is_manager IN (0, 1));

-- 4. Plane-style Modules
CREATE TABLE IF NOT EXISTS hub_modules (
    id              TEXT NOT NULL PRIMARY KEY,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'planned'
                    CHECK (status IN ('backlog', 'planned', 'in_progress', 'paused', 'completed', 'cancelled')),
    lead_id         BLOB REFERENCES users(id) ON DELETE SET NULL,
    start_date      TEXT,
    target_date     TEXT,
    created_by      BLOB REFERENCES users(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE INDEX IF NOT EXISTS idx_hub_modules_project ON hub_modules(project_id, created_at DESC);

-- 5. Plane-style Cycles / Sprints
CREATE TABLE IF NOT EXISTS hub_cycles (
    id              TEXT NOT NULL PRIMARY KEY,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('upcoming', 'active', 'completed', 'archived')),
    start_date      TEXT,
    end_date        TEXT,
    created_by      BLOB REFERENCES users(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE INDEX IF NOT EXISTS idx_hub_cycles_project ON hub_cycles(project_id, created_at DESC);

-- 6. Project Labels
CREATE TABLE IF NOT EXISTS hub_labels (
    id              TEXT NOT NULL PRIMARY KEY,
    project_id      BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    color           TEXT NOT NULL DEFAULT '#6366f1',
    description     TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    UNIQUE (project_id, name)
);

CREATE INDEX IF NOT EXISTS idx_hub_labels_project ON hub_labels(project_id, name);

-- 7. Plane-style Tasks / Work Items
CREATE TABLE IF NOT EXISTS hub_tasks (
    id                  TEXT NOT NULL PRIMARY KEY,
    workspace_id        BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id          BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sequence_id         INTEGER NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    state               TEXT NOT NULL DEFAULT 'todo'
                        CHECK (state IN ('backlog', 'todo', 'in_progress', 'in_review', 'done', 'cancelled')),
    priority            TEXT NOT NULL DEFAULT 'none'
                        CHECK (priority IN ('urgent', 'high', 'medium', 'low', 'none')),
    module_id           TEXT REFERENCES hub_modules(id) ON DELETE SET NULL,
    cycle_id            TEXT REFERENCES hub_cycles(id) ON DELETE SET NULL,
    parent_task_id      TEXT REFERENCES hub_tasks(id) ON DELETE SET NULL,
    created_by          BLOB REFERENCES users(id) ON DELETE SET NULL,
    created_by_agent    TEXT,
    start_date          TEXT,
    due_date            TEXT,
    estimate_points     INTEGER,
    sort_order          REAL NOT NULL DEFAULT 0,
    completed_at        INTEGER,
    archived_at         INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (project_id, sequence_id)
);

CREATE INDEX IF NOT EXISTS idx_hub_tasks_project_state ON hub_tasks(project_id, state, sort_order, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_hub_tasks_module ON hub_tasks(module_id);
CREATE INDEX IF NOT EXISTS idx_hub_tasks_cycle ON hub_tasks(cycle_id);
CREATE INDEX IF NOT EXISTS idx_hub_tasks_parent ON hub_tasks(parent_task_id);
CREATE INDEX IF NOT EXISTS idx_hub_tasks_creator ON hub_tasks(created_by);

-- 8. Task Assignees (Multiple users per task)
CREATE TABLE IF NOT EXISTS hub_task_assignees (
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    user_id         BLOB NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    assigned_by     BLOB REFERENCES users(id) ON DELETE SET NULL,
    assigned_at     INTEGER NOT NULL,
    PRIMARY KEY (task_id, user_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_hub_task_assignees_user ON hub_task_assignees(user_id, task_id);

-- 9. Task Labels
CREATE TABLE IF NOT EXISTS hub_task_labels (
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    label_id        TEXT NOT NULL REFERENCES hub_labels(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, label_id)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_hub_task_labels_label ON hub_task_labels(label_id, task_id);

-- 10. Task Comments
CREATE TABLE IF NOT EXISTS hub_task_comments (
    id              TEXT NOT NULL PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    author_id       BLOB REFERENCES users(id) ON DELETE SET NULL,
    author_agent    TEXT,
    body            TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_hub_task_comments_task ON hub_task_comments(task_id, created_at ASC);

-- 11. Task Activity Log
CREATE TABLE IF NOT EXISTS hub_task_activities (
    id              TEXT NOT NULL PRIMARY KEY,
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    actor_id        BLOB REFERENCES users(id) ON DELETE SET NULL,
    actor_agent     TEXT,
    action          TEXT NOT NULL,
    old_value       TEXT,
    new_value       TEXT,
    created_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_hub_task_activities_task ON hub_task_activities(task_id, created_at DESC);

-- 12. Task <-> Wiki Memory Page Links
CREATE TABLE IF NOT EXISTS hub_task_wiki_links (
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    page_path       TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (task_id, page_path)
) WITHOUT ROWID;

-- 13. GitHub Repositories per Project
CREATE TABLE IF NOT EXISTS hub_github_repos (
    id              TEXT NOT NULL PRIMARY KEY,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    owner           TEXT NOT NULL,
    repo            TEXT NOT NULL,
    full_name       TEXT NOT NULL,
    default_branch  TEXT NOT NULL DEFAULT 'main',
    access_token    TEXT,
    added_by        BLOB REFERENCES users(id) ON DELETE SET NULL,
    last_synced_at  INTEGER,
    last_sync_error TEXT,
    created_at      INTEGER NOT NULL,
    UNIQUE (project_id, full_name)
);

CREATE INDEX IF NOT EXISTS idx_hub_github_repos_project ON hub_github_repos(project_id);

-- 14. Cached / Synced GitHub Issues & Pull Requests
CREATE TABLE IF NOT EXISTS hub_github_items (
    id                  TEXT NOT NULL PRIMARY KEY,
    repo_id             TEXT NOT NULL REFERENCES hub_github_repos(id) ON DELETE CASCADE,
    project_id          BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repo_full_name      TEXT NOT NULL,
    kind                TEXT NOT NULL CHECK (kind IN ('issue', 'pr')),
    number              INTEGER NOT NULL,
    title               TEXT NOT NULL,
    body                TEXT NOT NULL DEFAULT '',
    state               TEXT NOT NULL CHECK (state IN ('open', 'closed', 'merged')),
    draft               INTEGER NOT NULL DEFAULT 0 CHECK (draft IN (0, 1)),
    author_login        TEXT NOT NULL DEFAULT '',
    author_avatar_url   TEXT,
    assignees_json      TEXT NOT NULL DEFAULT '[]',
    labels_json         TEXT NOT NULL DEFAULT '[]',
    html_url            TEXT NOT NULL,
    head_ref            TEXT,
    base_ref            TEXT,
    comments_count      INTEGER NOT NULL DEFAULT 0,
    gh_created_at       TEXT,
    gh_updated_at       TEXT,
    synced_at           INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);

CREATE INDEX IF NOT EXISTS idx_hub_github_items_project ON hub_github_items(project_id, kind, state, number DESC);

-- 15. Task <-> GitHub Issue/PR Links
CREATE TABLE IF NOT EXISTS hub_task_github_links (
    task_id         TEXT NOT NULL REFERENCES hub_tasks(id) ON DELETE CASCADE,
    github_item_id  TEXT REFERENCES hub_github_items(id) ON DELETE SET NULL,
    repo_full_name  TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('issue', 'pr')),
    number          INTEGER NOT NULL,
    title           TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL DEFAULT 'open',
    html_url        TEXT NOT NULL,
    relation        TEXT NOT NULL DEFAULT 'tracks'
                    CHECK (relation IN ('tracks', 'fixes', 'closes', 'implements', 'related')),
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (task_id, repo_full_name, number)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_hub_task_github_links_repo ON hub_task_github_links(repo_full_name, number);
