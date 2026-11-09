use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::client::{
    check_graphql_errors, gh_api, gh_api_graphql, gh_api_paginate, gh_command,
    gh_command_allow_exit_codes, FieldValue,
};
use crate::app::ReviewAction;

define_state_filter!(PrStateFilter);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCheckRollupItem {
    #[serde(rename = "__typename")]
    pub type_name: String,
    pub name: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub context: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    None,
}

impl CiStatus {
    pub fn from_rollup(items: &[StatusCheckRollupItem]) -> Self {
        if items.is_empty() {
            return Self::None;
        }
        let mut has_pending = false;
        for item in items {
            match item.type_name.as_str() {
                "CheckRun" => match item.conclusion.as_deref() {
                    Some("SUCCESS") | Some("NEUTRAL") | Some("SKIPPED") => {}
                    // gh returns conclusion: "" for in-progress/queued checks
                    None | Some("") => {
                        has_pending = true;
                    }
                    Some(_) => return Self::Failure,
                },
                "StatusContext" => match item.state.as_deref() {
                    Some("SUCCESS") => {}
                    // gh returns state: "" for in-progress StatusContext entries
                    Some("PENDING") | Some("EXPECTED") | Some("") => has_pending = true,
                    Some(_) => return Self::Failure,
                    None => {}
                },
                _ => {}
            }
        }
        if has_pending {
            Self::Pending
        } else {
            Self::Success
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckItem {
    pub name: String,
    pub state: String,
    pub bucket: Option<String>,
    pub link: Option<String>,
    #[serde(default)]
    pub workflow: String,
    pub description: Option<String>,
    #[serde(rename = "startedAt")]
    pub started_at: Option<String>,
    #[serde(rename = "completedAt")]
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub author: User,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    pub labels: Vec<Label>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "statusCheckRollup")]
    pub status_check_rollup: Vec<StatusCheckRollupItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u32,
    #[serde(default, rename = "node_id")]
    pub node_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub head: Branch,
    pub base: Branch,
    pub user: User,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub patch: Option<String>,
    #[serde(default)]
    pub viewed: bool,
}

pub async fn fetch_pr(repo: &str, pr_number: u32) -> Result<PullRequest> {
    let endpoint = format!("repos/{}/pulls/{}", repo, pr_number);
    let json = gh_api(&endpoint).await?;
    serde_json::from_value(json).context("Failed to parse PR response")
}

pub async fn fetch_changed_files(repo: &str, pr_number: u32) -> Result<Vec<ChangedFile>> {
    let endpoint = format!("repos/{}/pulls/{}/files?per_page=100", repo, pr_number);
    let json = gh_api_paginate(&endpoint).await?;
    serde_json::from_value(json).context("Failed to parse changed files response")
}

pub async fn hydrate_pr_files(repo: &str, pr: &PullRequest, files: &mut [ChangedFile]) {
    for window in files.chunks_mut(64) {
        let mut tasks = tokio::task::JoinSet::new();
        for (index, batch) in window.chunks(8).enumerate() {
            let repo = repo.to_owned();
            let sha = pr.head.sha.clone();
            let batch = batch.to_vec();
            tasks.spawn(async move {
                let result = hydrate_pr_batch(&repo, &sha, batch).await;
                (index * 8, result)
            });
        }
        while let Some(task) = tasks.join_next().await {
            match task {
                Ok((offset, Ok(patches))) => {
                    for (index, patch) in patches.into_iter().enumerate() {
                        let file = &mut window[offset + index];
                        match patch {
                            Ok(patch) => file.patch = Some(patch),
                            Err(error) => {
                                tracing::warn!(filename = %file.filename, %error, "Full-file PR context unavailable")
                            }
                        }
                    }
                }
                Ok((_, Err(error))) => tracing::warn!(%error, "PR context batch failed"),
                Err(error) => tracing::warn!(%error, "PR context task failed"),
            }
        }
    }
}

async fn hydrate_pr_batch(
    repo: &str,
    sha: &str,
    files: Vec<ChangedFile>,
) -> Result<Vec<Result<String>>> {
    let mut contents = fetch_blob_texts(repo, sha, &files).await;
    tokio::task::spawn_blocking(move || {
        files
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let head = if file.status == "removed" {
                    Ok(String::new())
                } else {
                    match &mut contents {
                        Ok(blobs) => parse_blob_text(blobs[format!("file_{index}")].take()),
                        Err(error) => Err(anyhow::anyhow!(error.to_string())),
                    }
                };
                head.and_then(|head| patch_with_full_context(file, &head))
            })
            .collect()
    })
    .await
    .context("Full-file context task failed")
}

fn patch_with_full_context(file: &ChangedFile, head: &str) -> Result<String> {
    let mut patch = file.patch.clone().unwrap_or_default();
    if patch.is_empty() && file.status == "added" && !head.contains('\0') {
        let count = head.lines().count();
        if count != file.additions as usize {
            anyhow::bail!("Added-file contents do not match PR patch statistics");
        }
        patch = format!("@@ -0,0 +{},{count} @@", usize::from(count > 0));
        for line in head.lines() {
            patch.push_str("\n+");
            patch.push_str(line);
        }
    }
    let mut additions = 0;
    let mut deletions = 0;
    let mut classifier = crate::diff::PatchClassifier::new();
    for line in patch.lines() {
        match classifier.classify(line).0 {
            crate::diff::LineType::Added => additions += 1,
            crate::diff::LineType::Removed => deletions += 1,
            _ => {}
        }
    }
    if additions != file.additions || deletions != file.deletions {
        anyhow::bail!("PR patch is incomplete; preserving the original patch");
    }
    crate::diff::expand_full_context(&patch, head)
        .context("PR patch does not match the exact head contents")
}

fn blob_batch_query(sha: &str, files: &[ChangedFile]) -> (String, Vec<(String, String)>) {
    let mut variables = String::from("$owner: String!, $name: String!");
    let mut selections = String::new();
    let mut expressions = Vec::new();
    for (index, file) in files.iter().enumerate() {
        if file.status == "removed" {
            continue;
        }
        let key = format!("file_{index}");
        variables.push_str(&format!(", ${key}: String!"));
        selections.push_str(&format!(
            "{key}: object(expression: ${key}) {{ ... on Blob {{ text isBinary byteSize }} }} "
        ));
        expressions.push((key, format!("{sha}:{}", file.filename)));
    }
    (
        format!(
            "query({variables}) {{ repository(owner: $owner, name: $name) {{ {selections} }} }}"
        ),
        expressions,
    )
}

async fn fetch_blob_texts(
    repo: &str,
    sha: &str,
    files: &[ChangedFile],
) -> Result<serde_json::Value> {
    let (query, expressions) = blob_batch_query(sha, files);
    if expressions.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let (owner, name) = repo.split_once('/').context("Invalid repository name")?;
    let mut fields = vec![
        ("owner", FieldValue::String(owner)),
        ("name", FieldValue::String(name)),
    ];
    fields.extend(
        expressions
            .iter()
            .map(|(key, expression)| (key.as_str(), FieldValue::String(expression))),
    );
    let mut response = gh_api_graphql(&query, &fields).await?;
    check_graphql_errors(&response)?;
    let blobs = response["data"]["repository"].take();
    if !blobs.is_object() {
        anyhow::bail!("Repository file contents unavailable");
    }
    Ok(blobs)
}

fn parse_blob_text(blob: serde_json::Value) -> Result<String> {
    if blob["isBinary"].as_bool() != Some(false) {
        anyhow::bail!("File is binary or no text blob is available");
    }
    let text = blob["text"].as_str().context("File contents unavailable")?;
    if blob["byteSize"].as_u64() != Some(text.len() as u64) {
        anyhow::bail!("File contents were truncated");
    }
    Ok(text.to_owned())
}

pub async fn submit_review(
    repo: &str,
    pr_number: u32,
    action: ReviewAction,
    body: &str,
) -> Result<()> {
    let action_flag = match action {
        ReviewAction::Approve => "--approve",
        ReviewAction::RequestChanges => "--request-changes",
        ReviewAction::Comment => "--comment",
    };

    gh_command(&[
        "pr",
        "review",
        &pr_number.to_string(),
        action_flag,
        "-b",
        body,
        "-R",
        repo,
    ])
    .await?;

    Ok(())
}

/// Fetch the raw diff for a PR using `gh pr diff`
pub async fn fetch_pr_diff(repo: &str, pr_number: u32) -> Result<String> {
    gh_command(&["pr", "diff", &pr_number.to_string(), "-R", repo]).await
}

#[derive(Debug, Deserialize)]
struct GraphqlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrFileNode {
    path: String,
    #[serde(rename = "viewerViewedState")]
    viewer_viewed_state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrFilesConnection {
    nodes: Vec<GraphqlPrFileNode>,
    #[serde(rename = "pageInfo")]
    page_info: GraphqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct GraphqlPrNode {
    files: GraphqlPrFilesConnection,
}

#[derive(Debug, Deserialize)]
struct GraphqlFilesViewedStateData {
    node: Option<GraphqlPrNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlFilesViewedStateResponse {
    data: Option<GraphqlFilesViewedStateData>,
}

pub async fn fetch_files_viewed_state(
    _repo: &str,
    pr_node_id: &str,
) -> Result<HashMap<String, bool>> {
    let query = r#"
query($pullRequestId: ID!, $after: String) {
  node(id: $pullRequestId) {
    ... on PullRequest {
      files(first: 100, after: $after) {
        nodes {
          path
          viewerViewedState
        }
        pageInfo {
          hasNextPage
          endCursor
        }
      }
    }
  }
}
"#;

    let mut viewed_state = HashMap::new();
    let mut after: Option<String> = None;

    loop {
        let mut fields = vec![("pullRequestId", FieldValue::String(pr_node_id))];
        if let Some(cursor) = after.as_deref() {
            fields.push(("after", FieldValue::String(cursor)));
        }

        let response = gh_api_graphql(query, &fields).await?;

        check_graphql_errors(&response)?;

        let parsed: GraphqlFilesViewedStateResponse = serde_json::from_value(response)
            .context("Failed to parse files viewed-state GraphQL response")?;
        let Some(data) = parsed.data else {
            anyhow::bail!("GitHub GraphQL response missing data");
        };
        let Some(node) = data.node else {
            anyhow::bail!("Pull request node not found for viewed-state query");
        };

        for file in node.files.nodes {
            viewed_state.insert(
                file.path,
                matches!(file.viewer_viewed_state.as_deref(), Some("VIEWED")),
            );
        }

        if node.files.page_info.has_next_page {
            let Some(next_cursor) = node.files.page_info.end_cursor else {
                anyhow::bail!("GitHub GraphQL pageInfo missing endCursor");
            };
            after = Some(next_cursor);
        } else {
            break;
        }
    }

    Ok(viewed_state)
}

const MARK_VIEWED_QUERY: &str = r#"
mutation($pullRequestId: ID!, $path: String!) {
  markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

const UNMARK_VIEWED_QUERY: &str = r#"
mutation($pullRequestId: ID!, $path: String!) {
  unmarkFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
    clientMutationId
  }
}
"#;

pub async fn set_file_viewed(
    _repo: &str,
    pr_node_id: &str,
    path: &str,
    viewed: bool,
) -> Result<()> {
    let query = if viewed {
        MARK_VIEWED_QUERY
    } else {
        UNMARK_VIEWED_QUERY
    };

    let response = gh_api_graphql(
        query,
        &[
            ("pullRequestId", FieldValue::String(pr_node_id)),
            ("path", FieldValue::String(path)),
        ],
    )
    .await?;

    check_graphql_errors(&response)?;

    Ok(())
}

/// ページネーション結果
pub struct PrListPage {
    pub items: Vec<PullRequestSummary>,
    pub has_more: bool,
}

pub async fn fetch_pr_list(repo: &str, state: PrStateFilter, limit: u32) -> Result<PrListPage> {
    fetch_pr_list_with_offset(repo, state, 0, limit).await
}

/// PR一覧取得（オフセット付き、追加ロード用）
pub async fn fetch_pr_list_with_offset(
    repo: &str,
    state: PrStateFilter,
    offset: u32,
    limit: u32,
) -> Result<PrListPage> {
    // gh pr list doesn't support offset directly, so we fetch offset+limit+1 and skip
    let fetch_count = offset + limit + 1;
    let output = gh_command(&[
        "pr",
        "list",
        "-R",
        repo,
        "-s",
        state.as_gh_arg(),
        "--json",
        "number,title,state,author,isDraft,labels,updatedAt,statusCheckRollup",
        "--limit",
        &fetch_count.to_string(),
    ])
    .await?;

    let all_items: Vec<PullRequestSummary> =
        serde_json::from_str(&output).context("Failed to parse PR list response")?;

    // Check if there are more items beyond what we're returning
    let has_more = all_items.len() > (offset + limit) as usize;

    // Skip the offset items and take limit items
    let items: Vec<PullRequestSummary> = all_items
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    Ok(PrListPage { items, has_more })
}

pub async fn fetch_pr_checks(repo: &str, pr_number: u32) -> Result<Vec<CheckItem>> {
    let output = gh_command_allow_exit_codes(
        &[
            "pr",
            "checks",
            &pr_number.to_string(),
            "-R",
            repo,
            "--json",
            "name,state,bucket,link,workflow,description,startedAt,completedAt",
        ],
        &[8], // exit code 8 = checks pending
    )
    .await?;
    serde_json::from_str(&output).context("Failed to parse PR checks response")
}

#[cfg(test)]
mod tests {
    #[test]
    fn pr_blob_batch_uses_exact_revision_variables_and_skips_removed_paths() {
        let file = |filename: &str, status: &str| super::ChangedFile {
            filename: filename.into(),
            status: status.into(),
            additions: 0,
            deletions: 0,
            patch: None,
            viewed: false,
        };
        let (query, expressions) = super::blob_batch_query(
            "exact-head",
            &[
                file("renamed.rs", "renamed"),
                file("deleted.rs", "removed"),
                file("quote\"終.rs", "added"),
            ],
        );
        assert_eq!(
            expressions,
            vec![
                ("file_0".into(), "exact-head:renamed.rs".into()),
                ("file_2".into(), "exact-head:quote\"終.rs".into())
            ]
        );
        assert!(query.contains("file_0: object(expression: $file_0)"));
        assert!(query.contains("file_2: object(expression: $file_2)"));
        assert!(!query.contains("file_1"));
        assert!(
            !query.contains("renamed.rs"),
            "paths belong in GraphQL variables"
        );
    }

    #[test]
    fn exact_blob_response_rejects_binary_and_truncated_text() {
        use super::parse_blob_text;
        let response = |text: serde_json::Value, binary: bool, size: u64| {
            serde_json::json!({
                "text": text, "isBinary": binary, "byteSize": size
            })
        };
        assert_eq!(
            parse_blob_text(response("終\n".into(), false, 4)).unwrap(),
            "終\n"
        );
        assert!(parse_blob_text(response(serde_json::Value::Null, true, 4)).is_err());
        assert!(parse_blob_text(response("short".into(), false, 50)).is_err());
    }

    #[test]
    fn full_context_preserves_patch_counts_and_handles_missing_added_file_patch() {
        use super::{patch_with_full_context, ChangedFile};
        let mut file = ChangedFile {
            filename: "new.rs".into(),
            status: "added".into(),
            additions: 2,
            deletions: 0,
            patch: None,
            viewed: false,
        };
        insta::assert_snapshot!(patch_with_full_context(&file, "one\ntwo\n").unwrap(), @r"
        @@ -0,0 +1,2 @@
        +one
        +two
        ");
        file.status = "modified".into();
        assert!(patch_with_full_context(&file, "one\ntwo\n").is_err());
        file.patch = Some("@@ -1 +1 @@\n-old\n+one".into());
        assert!(patch_with_full_context(&file, "one\ntwo\n").is_err());
        file.additions = 1;
        file.deletions = 1;
        assert!(patch_with_full_context(&file, "one\ntwo\n")
            .unwrap()
            .ends_with(" two"));
    }

    use super::*;

    #[test]
    fn test_pr_state_filter_as_gh_arg() {
        assert_eq!(PrStateFilter::Open.as_gh_arg(), "open");
        assert_eq!(PrStateFilter::Closed.as_gh_arg(), "closed");
        assert_eq!(PrStateFilter::All.as_gh_arg(), "all");
    }

    #[test]
    fn test_pr_state_filter_display_name() {
        assert_eq!(PrStateFilter::Open.display_name(), "open");
        assert_eq!(PrStateFilter::Closed.display_name(), "closed");
        assert_eq!(PrStateFilter::All.display_name(), "all");
    }

    #[test]
    fn test_pr_state_filter_next_cycle() {
        assert_eq!(PrStateFilter::Open.next(), PrStateFilter::Closed);
        assert_eq!(PrStateFilter::Closed.next(), PrStateFilter::All);
        assert_eq!(PrStateFilter::All.next(), PrStateFilter::Open);
    }

    #[test]
    fn test_ci_status_from_rollup_empty() {
        assert_eq!(CiStatus::from_rollup(&[]), CiStatus::None);
    }

    #[test]
    fn test_ci_status_from_rollup_all_success() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("lint".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("NEUTRAL".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Success);
    }

    #[test]
    fn test_ci_status_from_rollup_failure() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("test".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("FAILURE".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Failure);
    }

    #[test]
    fn test_ci_status_from_rollup_pending() {
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("deploy".to_string()),
                status: Some("IN_PROGRESS".to_string()),
                conclusion: None,
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_status_context() {
        let items = vec![StatusCheckRollupItem {
            type_name: "StatusContext".to_string(),
            name: None,
            status: None,
            conclusion: None,
            context: Some("ci/test".to_string()),
            state: Some("PENDING".to_string()),
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_skipped() {
        let items = vec![StatusCheckRollupItem {
            type_name: "CheckRun".to_string(),
            name: Some("skip-check".to_string()),
            status: Some("COMPLETED".to_string()),
            conclusion: Some("SKIPPED".to_string()),
            context: None,
            state: None,
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Success);
    }

    #[test]
    fn test_ci_status_from_rollup_empty_conclusion_is_pending() {
        // gh returns conclusion: "" (not null) for in-progress/queued checks
        let items = vec![
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("build".to_string()),
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                context: None,
                state: None,
            },
            StatusCheckRollupItem {
                type_name: "CheckRun".to_string(),
                name: Some("test".to_string()),
                status: Some("IN_PROGRESS".to_string()),
                conclusion: Some("".to_string()),
                context: None,
                state: None,
            },
        ];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_ci_status_from_rollup_empty_state_is_pending() {
        // gh may return state: "" for in-progress StatusContext entries,
        // analogous to conclusion: "" for CheckRun.
        let items = vec![StatusCheckRollupItem {
            type_name: "StatusContext".to_string(),
            name: None,
            status: None,
            conclusion: None,
            context: Some("ci/in-progress".to_string()),
            state: Some("".to_string()),
        }];
        assert_eq!(CiStatus::from_rollup(&items), CiStatus::Pending);
    }

    #[test]
    fn test_check_item_deserialize() {
        let json = r#"{
            "name": "build",
            "state": "SUCCESS",
            "bucket": "pass",
            "link": "https://example.com/run/1",
            "workflow": "CI",
            "description": "Build succeeded",
            "startedAt": "2024-01-01T00:00:00Z",
            "completedAt": "2024-01-01T00:05:00Z"
        }"#;
        let item: CheckItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.name, "build");
        assert_eq!(item.bucket.as_deref(), Some("pass"));
        assert_eq!(item.link.as_deref(), Some("https://example.com/run/1"));
    }

    #[test]
    fn test_check_item_deserialize_minimal() {
        let json = r#"{
            "name": "test",
            "state": "PENDING",
            "workflow": ""
        }"#;
        let item: CheckItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.name, "test");
        assert!(item.bucket.is_none());
        assert!(item.link.is_none());
    }

    #[test]
    fn test_status_check_rollup_item_deserialize() {
        let json = r#"{
            "__typename": "CheckRun",
            "name": "build",
            "status": "COMPLETED",
            "conclusion": "SUCCESS"
        }"#;
        let item: StatusCheckRollupItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.type_name, "CheckRun");
        assert_eq!(item.name.as_deref(), Some("build"));
        assert_eq!(item.conclusion.as_deref(), Some("SUCCESS"));
    }
}
