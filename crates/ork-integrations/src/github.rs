use async_trait::async_trait;
use chrono::Utc;
use octocrab::Octocrab;
use ork_common::error::OrkError;
use tracing::debug;

use ork_core::ports::integration::{
    CommitInfo, IssueInfo, PipelineInfo, PullRequestInfo, RepoQuery, SourceControlAdapter,
};

pub struct GitHubAdapter {
    client: Octocrab,
}

impl GitHubAdapter {
    /// Create a new GitHub adapter. For GitHub Enterprise, pass the API base URL
    /// (e.g. `https://github.example.com/api/v3`). Pass `None` for github.com.
    pub fn new(token: &str, base_url: Option<&str>) -> Result<Self, OrkError> {
        let mut builder = Octocrab::builder().personal_token(token.to_string());

        if let Some(url) = base_url {
            builder = builder.base_uri(url).map_err(|e| {
                OrkError::Integration(format!("invalid GitHub base URL '{url}': {e}"))
            })?;
        }

        let client = builder
            .build()
            .map_err(|e| OrkError::Integration(format!("failed to create GitHub client: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl SourceControlAdapter for GitHubAdapter {
    fn provider_name(&self) -> &str {
        "github"
    }

    async fn list_recent_commits(&self, query: &RepoQuery) -> Result<Vec<CommitInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitHub commits");

        let repo_handler = self.client.repos(&query.owner, &query.repo);
        let mut builder = repo_handler.list_commits().per_page(50);

        if let Some(since) = query.since {
            builder = builder.since(since);
        }

        let commits = builder
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub commits fetch failed: {e:?}")))?;

        Ok(commits
            .items
            .into_iter()
            .map(|c| {
                let commit = &c.commit;
                CommitInfo {
                    sha: c.sha,
                    message: commit.message.clone(),
                    author: commit
                        .author
                        .as_ref()
                        .map(|a| a.name.clone())
                        .unwrap_or_default(),
                    timestamp: commit
                        .author
                        .as_ref()
                        .and_then(|a| a.date)
                        .unwrap_or_else(Utc::now),
                    url: c.html_url.to_string(),
                }
            })
            .collect())
    }

    async fn list_pull_requests(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<PullRequestInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitHub PRs");

        let state_param = match state {
            Some("open") => octocrab::params::State::Open,
            Some("closed") => octocrab::params::State::Closed,
            _ => octocrab::params::State::All,
        };

        let prs = self
            .client
            .pulls(&query.owner, &query.repo)
            .list()
            .state(state_param)
            .per_page(50)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub PRs fetch failed: {e:?}")))?;

        Ok(prs
            .items
            .into_iter()
            .filter(|pr| {
                if let Some(since) = query.since {
                    pr.created_at.map(|d| d >= since).unwrap_or(true)
                } else {
                    true
                }
            })
            .map(|pr| PullRequestInfo {
                number: pr.number,
                title: pr.title.unwrap_or_default(),
                author: pr.user.map(|u| u.login).unwrap_or_default(),
                state: pr
                    .state
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_default(),
                url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                created_at: pr.created_at.unwrap_or_else(Utc::now),
                merged_at: pr.merged_at,
                labels: pr
                    .labels
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.name)
                    .collect(),
                description: pr.body,
            })
            .collect())
    }

    async fn list_issues(
        &self,
        query: &RepoQuery,
        state: Option<&str>,
    ) -> Result<Vec<IssueInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitHub issues");

        let state_param = match state {
            Some("open") => octocrab::params::State::Open,
            Some("closed") => octocrab::params::State::Closed,
            _ => octocrab::params::State::All,
        };

        let issues = self
            .client
            .issues(&query.owner, &query.repo)
            .list()
            .state(state_param)
            .per_page(50)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub issues fetch failed: {e:?}")))?;

        Ok(issues
            .items
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .filter(|issue| {
                if let Some(since) = query.since {
                    issue.created_at >= since
                } else {
                    true
                }
            })
            .map(|issue| IssueInfo {
                number: issue.number,
                title: issue.title,
                author: issue.user.login,
                state: format!("{:?}", issue.state).to_lowercase(),
                url: issue.html_url.to_string(),
                created_at: issue.created_at,
                labels: issue.labels.into_iter().map(|l| l.name).collect(),
            })
            .collect())
    }

    async fn list_pipelines(&self, query: &RepoQuery) -> Result<Vec<PipelineInfo>, OrkError> {
        debug!(owner = %query.owner, repo = %query.repo, "fetching GitHub Actions runs");

        let runs = self
            .client
            .workflows(&query.owner, &query.repo)
            .list_all_runs()
            .per_page(20)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub Actions fetch failed: {e:?}")))?;

        Ok(runs
            .items
            .into_iter()
            .map(|r| PipelineInfo {
                id: r.id.to_string(),
                status: r.status.clone(),
                branch: r.head_branch.clone(),
                commit_sha: r.head_sha.clone(),
                url: r.html_url.to_string(),
                started_at: Some(r.created_at),
                finished_at: Some(r.updated_at),
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
            repo, from_tag, to_tag, "fetching merged PRs between tags"
        );

        let comparison = self
            .client
            .commits(owner, repo)
            .compare(from_tag, to_tag)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub compare failed: {e:?}")))?;

        let shas: Vec<String> = comparison.commits.iter().map(|c| c.sha.clone()).collect();

        let all_prs = self
            .client
            .pulls(owner, repo)
            .list()
            .state(octocrab::params::State::Closed)
            .per_page(100)
            .send()
            .await
            .map_err(|e| OrkError::Integration(format!("GitHub PRs fetch failed: {e:?}")))?;

        Ok(all_prs
            .items
            .into_iter()
            .filter(|pr| pr.merged_at.is_some())
            .filter(|pr| {
                pr.merge_commit_sha
                    .as_ref()
                    .map(|sha| shas.contains(sha))
                    .unwrap_or(false)
            })
            .map(|pr| PullRequestInfo {
                number: pr.number,
                title: pr.title.unwrap_or_default(),
                author: pr.user.map(|u| u.login).unwrap_or_default(),
                state: "merged".into(),
                url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                created_at: pr.created_at.unwrap_or_else(Utc::now),
                merged_at: pr.merged_at,
                labels: pr
                    .labels
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.name)
                    .collect(),
                description: pr.body,
            })
            .collect())
    }
}
