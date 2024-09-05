//! Automatically rebases and merges dependabot PRs.
//! Requires a personal GitHub token.

use log::{self, error, info};
use octocrab::params::repos::Reference;
use octocrab::{params, Octocrab};
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;

use std::error::Error;
use std::fs;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    github_token: String,
    repos: Vec<String>,
}

#[derive(Debug, Clone)]
struct Repo {
    org: String,
    repo: String,
}

#[derive(Debug, Clone)]
struct DependabotPr {
    url: String,
    number: u64,
    repo: Repo,

    all_checks_pass: bool,
    // PR rebased off a base branch.
    rebased: bool,
    rebase_in_progress: bool,

    new_version: String,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
struct IgnoreResp {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    simple_logger::init_with_level(log::Level::Info)?;

    let cfg_str = fs::read_to_string("config.toml")?;
    let cfg: Config = toml::from_str(&cfg_str)?;

    let octo = Octocrab::builder()
        .personal_token(cfg.github_token)
        .build()?;

    for repo in cfg.repos.iter() {
        if let Err(e) = check_prs(&octo, repo).await {
            error!("[{}] Error: {:?}", repo, e);
        }
    }

    Ok(())
}

async fn check_prs(octo: &Octocrab, repo: &str) -> Result<(), Box<dyn Error>> {
    let prs = dependabot_prs_passing_checks(octo, repo).await?;
    if prs.is_empty() {
        info!("[{}] No dependabot PRs to merge", repo);
        return Ok(());
    }

    if prs.iter().any(|pr| pr.rebase_in_progress) {
        info!(
            "[{}] One of the PRs is being rebased already. Skipping further actions.",
            repo
        );
        return Ok(());
    }

    // Don't merge pre-release versions automatically.
    let prs = prs
        .into_iter()
        .filter(|pr| !pr.new_version.contains('+'))
        .collect::<Vec<_>>();

    let maybe_rebase = if let Some(merged) = maybe_merge_one(octo, &prs).await? {
        prs.iter().find(|pr| pr.url != merged.url && !pr.rebased)
    } else {
        prs.iter().find(|pr| !pr.rebased)
    };

    if let Some(to_rebase) = maybe_rebase {
        info!("Rebasing {:?}", to_rebase.url);
        octo.issues(&to_rebase.repo.org, &to_rebase.repo.repo)
            .create_comment(to_rebase.number, "@dependabot rebase")
            .await?;
    }

    Ok(())
}

async fn maybe_merge_one(
    octo: &Octocrab,
    prs: &[DependabotPr],
) -> Result<Option<DependabotPr>, Box<dyn Error>> {
    if let Some(pr) = prs.iter().find(|pr| pr.all_checks_pass && pr.rebased) {
        info!("Merging {:?}", pr.url);

        // Approve
        let url = format!(
            "/repos/{}/{}/pulls/{}/reviews",
            pr.repo.org, pr.repo.repo, pr.number
        );
        let review_body = serde_json::json!({
            "event": "APPROVE"
        });
        let _resp: IgnoreResp = octo.post(url, Some(&review_body)).await?;

        // Merge
        let url = format!(
            "/repos/{}/{}/pulls/{}/merge",
            pr.repo.org, pr.repo.repo, pr.number
        );
        let res: octocrab::Result<IgnoreResp> = octo.put(url, None::<&()>).await;
        if let Err(e) = res {
            info!("Failed to merge {:?}: {:?}", pr.url, e);
            return Ok(None);
        }

        Ok(Some(pr.clone()))
    } else {
        Ok(None)
    }
}

async fn dependabot_prs_passing_checks(
    octo: &Octocrab,
    repo: &str,
) -> Result<Vec<DependabotPr>, Box<dyn Error>> {
    let mut parts = repo.split('/');
    let org = parts.next().unwrap().to_string();
    let repo = parts.next().unwrap().to_string();

    let prs = octo
        .pulls(&org, &repo)
        .list()
        .state(params::State::Open)
        .send()
        .await?;

    let mut prs_state = Vec::<DependabotPr>::new();

    for pr in prs.into_iter().filter(|pr| {
        pr.user
            .as_ref()
            .map_or(false, |u| u.login == "dependabot[bot]")
    }) {
        // octo.checks() does not return all checks for some reason
        // let checks = octo
        //     .checks(&org, &repo)
        //     .list_check_runs_for_git_ref(pr.head.sha.into())
        //     .send()
        //     .await?;
        let checks_url = format!("/repos/{}/{}/commits/{}/check-runs", org, repo, pr.head.sha);
        let check_runs: octocrab::models::CheckRuns = octo.get(checks_url, None::<&()>).await?;

        let base_branch = octo
            .repos(&org, &repo)
            .get_ref(&Reference::Branch(pr.base.ref_field))
            .await?;
        let base_branch_sha = match base_branch.object {
            octocrab::models::repos::Object::Commit { sha, .. } => sha,
            octocrab::models::repos::Object::Tag { sha, .. } => sha,
            _ => panic!("main branch is not a commit or tag"),
        };

        let all_checks_pass = check_runs
            .check_runs
            .iter()
            .all(|c| c.conclusion != Some("failure".into()));

        let url = format!("/repos/{}/{}/pulls/{}", org, repo, pr.number);
        let pr: octocrab::models::pulls::PullRequest = octo.get(url, None::<&()>).await?;

        prs_state.push(DependabotPr {
            url: pr
                .html_url
                .map(|url| url.to_string())
                .unwrap_or("".to_string()),
            number: pr.number,
            repo: Repo {
                org: org.clone(),
                repo: repo.clone(),
            },
            all_checks_pass,
            rebased: pr.base.sha == base_branch_sha,
            rebase_in_progress: pr
                .body
                .map_or(false, |b| b.contains("Dependabot is rebasing this PR")),
            new_version: pr
                .title
                .and_then(|title| parse_version_from_pr(&title))
                .unwrap_or("".to_string()),
        });
    }

    Ok(prs_state)
}

fn parse_version_from_pr(title: &str) -> Option<String> {
    let re = Regex::new(r"to (\d+\.\d+\.\d+(-[a-zA-Z0-9\.]+)?(a0)?(\+[a-zA-Z0-9\.]+)?)").unwrap();
    re.captures(title)
        .and_then(|captures| captures.get(1).map(|m| m.as_str().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_from_pr() {
        assert_eq!(
            parse_version_from_pr("Bump foo from 1.2.3 to 1.2.4"),
            Some("1.2.4".to_string())
        );
        assert_eq!(
            parse_version_from_pr("Bump foo from 1.2.3 to 1.2.4-alpha"),
            Some("1.2.4-alpha".to_string())
        );
        assert_eq!(
            parse_version_from_pr("Bump foo from 1.2.3 to 1.2.4-alpha.1"),
            Some("1.2.4-alpha.1".to_string())
        );
        assert_eq!(
            parse_version_from_pr("Bump foo from 1.2.3 to 1.2.4-alpha.1+build.1"),
            Some("1.2.4-alpha.1+build.1".to_string())
        );
        assert_eq!(
            parse_version_from_pr("Bump foo from 1.2.3a0+201.fbdbcb12 to 1.2.3a0+210.bafdcd99"),
            Some("1.2.3a0+210.bafdcd99".to_string())
        )
    }
}
