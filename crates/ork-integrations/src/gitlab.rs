use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use reqwest::Client;
use serde::Deserialize;
use tracing::debug;

use ork_core::ports::integration::{
    CommitInfo, IssueInfo, PipelineInfo, PullRequestInfo, RepoQuery, SourceControlAdapter,
};

pub struct GitLabAdapter {
    client: Client,
    base_url: String,
    token: String,
}

impl GitLabAdapter {
    pub fn new(token: &str, base_url: Option<&str>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.unwrap_or("https://gitlab.com").to_string(),
            token: token.to_string(),
        }
    }

    fn project_path(&self, owner: &str, repo: &str) -> String {
        let encoded = format!("{owner}/{repo}").replace('/', "%2F");
        format!("{}/api/v4/projects/{encoded}", self.base_url)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, OrkError> {
        let resp = self
            .client
            .get(url)
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitLab request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OrkError::Integration(format!(
                "GitLab API error {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| OrkError::Integration(format!("GitLab parse failed: {e}")))
    }
}

#[derive(Deserialize)]
struct GlCommit {
    id: String,
    message: String,
    author_name: String,
    created_at: DateTime<Utc>,
    web_url: String,
}

#[derive(Deserialize)]
struct GlMergeRequest {
    iid: u64,
    title: String,
    author: GlUser,
    state: String,
    web_url: String,
    created_at: DateTime<Utc>,
    merged_at: Option<DateTime<Utc>>,
    labels: Vec<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct GlIssue {
    iid: u64,
    title: String,
    author: GlUser,
    state: String,
    web_url: String,
    created_at: DateTime<Utc>,
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct GlPipeline {
    id: u64,
    status: String,
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
    web_url: String,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct GlUser {
    username: String,
}

#[async_trait]
impl SourceControlAdapter for GitLabAdapter {
    fn provider_name(&self) -> &str {
        "gitlab"
    }

    async fn list_recent_commits(&self, query: &RepoQuery) -> Result<Vec<CommitInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitLab commits");

        let mut url = format!(
            "{}/repository/commits?per_page=50",
            self.project_path(&query.owner, &query.repo)
        );
        if let Some(since) = query.since {
            url.push_str(&format!("&since={}", since.to_rfc3339()));
        }
        if let Some(branch) = &query.branch {
            url.push_str(&format!("&ref_name={branch}"));
        }

        let commits: Vec<GlCommit> = self.get(&url).await?;

        Ok(commits
            .into_iter()
            .map(|c| CommitInfo {
                sha: c.id,
                message: c.message,
                author: c.author_name,
                timestamp: c.created_at,
                url: c.web_url,
            })
            .collect())
    }

    async fn list_pull_requests(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitLab MRs");

        let state_param = state.unwrap_or("all");
        let mut url = format!(
            "{}/merge_requests?state={state_param}&per_page=50",
            self.project_path(&query.owner, &query.repo)
        );
        if let Some(since) = query.since {
            url.push_str(&format!("&created_after={}", since.to_rfc3339()));
        }

        let mrs: Vec<GlMergeRequest> = self.get(&url).await?;

        Ok(mrs
            .into_iter()
            .map(|mr| PullRequestInfo {
                number: mr.iid,
                title: mr.title,
                author: mr.author.username,
                state: mr.state,
                url: mr.web_url,
                created_at: mr.created_at,
                merged_at: mr.merged_at,
                labels: mr.labels,
                description: mr.description,
            })
            .collect())
    }

    async fn list_issues(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<IssueInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitLab issues");

        let state_param = state.unwrap_or("all");
        let mut url = format!(
            "{}/issues?state={state_param}&per_page=50",
            self.project_path(&query.owner, &query.repo)
        );
        if let Some(since) = query.since {
            url.push_str(&format!("&created_after={}", since.to_rfc3339()));
        }

        let issues: Vec<GlIssue> = self.get(&url).await?;

        Ok(issues
            .into_iter()
            .map(|i| IssueInfo {
                number: i.iid,
                title: i.title,
                author: i.author.username,
                state: i.state,
                url: i.web_url,
                created_at: i.created_at,
                labels: i.labels,
            })
            .collect())
    }

    async fn list_pipelines(&self, query: &RepoQuery) -> Result<Vec<PipelineInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitLab pipelines");

        let url = format!(
            "{}/pipelines?per_page=20",
            self.project_path(&query.owner, &query.repo)
        );

        let pipelines: Vec<GlPipeline> = self.get(&url).await?;

        Ok(pipelines
            .into_iter()
            .map(|p| PipelineInfo {
                id: p.id.to_string(),
                status: p.status,
                branch: p.branch,
                commit_sha: p.sha,
                url: p.web_url,
                started_at: p.created_at,
                finished_at: p.updated_at,
            })
            .collect())
    }

    async fn get_merged_prs_between_tags(
        &self,
        owner: &str,
        repo: &str,
        from_tag: &str,
        to_tag: &str,
    ) -> Result<Vec<PullRequestInfo>, OrkError> {
        debug!(
            owner,
            repo, from_tag, to_tag, "fetching merged MRs between tags"
        );

        let url = format!(
            "{}/merge_requests?state=merged&per_page=100",
            self.project_path(owner, repo)
        );

        let mrs: Vec<GlMergeRequest> = self.get(&url).await?;

        Ok(mrs
            .into_iter()
            .map(|mr| PullRequestInfo {
                number: mr.iid,
                title: mr.title,
                author: mr.author.username,
                state: "merged".into(),
                url: mr.web_url,
                created_at: mr.created_at,
                merged_at: mr.merged_at,
                labels: mr.labels,
                description: mr.description,
            })
            .collect())
    }
}
