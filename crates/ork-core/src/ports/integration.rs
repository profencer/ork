use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ork_common::error::OrkError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestInfo {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub state: String,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub merged_at: Option<DateTime<Utc>>,
    pub labels: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueInfo {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub state: String,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineInfo {
    pub id: String,
    pub status: String,
    pub branch: String,
    pub commit_sha: String,
    pub url: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RepoQuery {
    pub owner: String,
    pub repo: String,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub branch: Option<String>,
}

#[async_trait]
pub trait SourceControlAdapter: Send + Sync {
    fn provider_name(&self) -> &str;

    async fn list_recent_commits(&self, query: &RepoQuery) -> Result<Vec<CommitInfo>, OrkError>;

    async fn list_pull_requests(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, OrkError>;

    async fn list_issues(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<IssueInfo>, OrkError>;

    async fn list_pipelines(&self, query: &RepoQuery) -> Result<Vec<PipelineInfo>, OrkError>;

    async fn get_merged_prs_between_tags(
        &self,
        owner: &str,
        repo: &str,
        from_tag: &str,
        to_tag: &str,
    ) -> Result<Vec<PullRequestInfo>, OrkError>;
}
