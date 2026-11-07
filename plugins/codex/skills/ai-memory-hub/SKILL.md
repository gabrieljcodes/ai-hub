---
name: ai-memory-hub
description: "Use this skill for any ai-hub project management, task tracking, module or sprint planning, team access management, or GitHub repository/issue/PR integration request: listing, creating, or updating tasks (Assignees, State, Priority, Start/Due date, Modules, Cycles, Labels, Subtasks, Wiki links), commenting on tasks, committing the task board to Markdown in Git, managing project team members, or syncing and linking GitHub Issues and PRs. Trigger by semantic intent rather than exact wording."
---
<!-- ai-memory-managed: routing-skill -->

# ai-hub tasks, modules, team, and GitHub

Use this skill to manage Plane-style engineering tasks, modules, cycles/sprints, labels, team access grants, and GitHub repository issues/PRs inside an ai-hub project.

## Tools in this cluster

- `hub_task_list` lists and filters project tasks by `state` (`backlog`, `todo`, `in_progress`, `in_review`, `done`, `cancelled`), `priority` (`urgent`, `high`, `medium`, `low`, `none`), `assignee`, `module_id`, `cycle_id`, `label`, or `search`, returning task metadata, assignees, labels, wiki links, and GitHub issue/PR links.
- `hub_task_get` reads full details for a single task by UUID or sequence key (such as `AIH-1`), including its Markdown description, subtasks, comments, activity log, linked wiki pages, and linked GitHub issues/PRs, or exports the entire project task board to Markdown (`action: "export_markdown"` or `action: "commit_board_to_wiki"`).
- `hub_task_create` creates a new task (or subtask via `parent_task_id`) with `title`, `description`, `state`, `priority`, `assignees` (usernames), `module_id`, `cycle_id`, `start_date` (`YYYY-MM-DD`), `due_date` (`YYYY-MM-DD`), `estimate_points`, `labels`, `wiki_links`, and `created_by_agent`.
- `hub_task_update` updates any fields on an existing task (`title`, `description`, `state`, `priority`, `assignees`, `module_id`, `cycle_id`, `start_date`, `due_date`, `estimate_points`, `labels`, `wiki_links`, `archived`) and records an activity log entry.
- `hub_task_comment` adds a Markdown comment to a task, attributed to the current user and/or AI agent harness.
- `hub_module_manage` lists or creates project modules (`action: "list_modules"` or `"create_module"`), cycles/sprints (`"list_cycles"` or `"create_cycle"`), and project task labels (`"list_labels"` or `"create_label"`).
- `hub_project_team` inspects or updates a project's team members and roles (`owner`, `admin`, `write`, `read`), access mode (`restricted` or `open`), and task key prefix (e.g. `AIH`).
- `hub_github_manage` lists or attaches GitHub repositories (`owner/repo`) on a project, syncs Issues and Pull Requests from GitHub's REST API, lists synced issues/PRs, creates new GitHub issues, comments on GitHub issues/PRs, imports an issue/PR as a project task, or links an existing task to a GitHub issue/PR.

## Workflow guidelines

1. **Check active tasks when starting feature work**: Call `hub_task_list` with `state: "in_progress"` or `state: "todo"` to see what the team is currently building and whether a task already tracks your work.
2. **Keep task state and progress accurate**: When picking up a task, update its `state` to `"in_progress"` with `hub_task_update` and record key implementation milestones or blockers using `hub_task_comment`. When work is complete, transition `state` to `"in_review"` or `"done"`.
3. **Cross-link tasks with Wiki pages and GitHub**: Attach relevant architecture or decision wiki page paths in `wiki_links` on the task, and link corresponding GitHub Issues or PRs via `hub_github_manage` (`action: "link_task"`).
4. **Git-backed Markdown snapshots**: When the user asks to commit or snapshot the task board into the Git wiki repository, call `hub_task_get` with `action: "commit_board_to_wiki"`, which writes `_hub/board.md` into the project's Git-backed Markdown repository.

## Project scope

Choose scope from the MCP client's identity support:

- **Session-aware MCP clients** that forward the real lifecycle-hook session id on every request should use automatic current-project routing. Omit `workspace`, `project`, and `cwd` for the current repository; pass explicit scope only when the user names a different project.
- **Static MCP clients** (including clients with lifecycle hooks but no bridge connecting that hook session id to MCP requests) must pass `workspace` and `project` together on every project-scoped call, including requests about this project, here, or our work. Read the exact names from the nearest `.ai-memory.toml` when it declares both. Without a marker override, derive the project from normalized `upstream`, then `origin`, using the full repository path without its host (`github.com/acme/api` → `acme-api`); use the folder basename only when no valid remote exists. Never rely on the server's last active project.

This rule applies only to project-scoped calls. For cross-project retrieval, `global=true` must omit `workspace`, `project`, and `scopes`. For a standing preference written with `scope: "global"`, omit `workspace` and `project`.
