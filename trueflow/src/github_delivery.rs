use crate::github::{PostedPullRequestReview, PullRequestReviewState, ResolvedPullRequestRef};
use crate::store::CommitId;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

const GITHUB_DELIVERY_LEDGER_VERSION: u32 = 1;
pub const GITHUB_DELIVERY_LEDGER_FILE: &str = "github_delivery.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubDeliveryLedger {
    version: u32,
    #[serde(default)]
    pull_requests: Vec<PullRequestDeliveryState>,
}

impl Default for GitHubDeliveryLedger {
    fn default() -> Self {
        Self {
            version: GITHUB_DELIVERY_LEDGER_VERSION,
            pull_requests: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequestDeliveryState {
    pub pr: ResolvedPullRequestRef,
    #[serde(default)]
    pub delivered_record_ids: Vec<String>,
    #[serde(default)]
    pub pending_reviews: Vec<PendingReviewState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingReviewState {
    pub review_id: u64,
    pub html_url: String,
    pub head_sha: CommitId,
    #[serde(default)]
    pub staged_record_ids: Vec<String>,
}

impl GitHubDeliveryLedger {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, format!("{}\n", serde_json::to_string_pretty(self)?))?;
        Ok(())
    }

    pub fn excluded_record_ids(&self, pr: &ResolvedPullRequestRef) -> HashSet<String> {
        let Some(state) = self.pull_request_state(pr) else {
            return HashSet::new();
        };
        state
            .delivered_record_ids
            .iter()
            .cloned()
            .chain(
                state
                    .pending_reviews
                    .iter()
                    .flat_map(|review| review.staged_record_ids.iter().cloned()),
            )
            .collect()
    }

    pub fn sync_pending_reviews<F>(
        &mut self,
        pr: &ResolvedPullRequestRef,
        mut lookup: F,
    ) -> Result<()>
    where
        F: FnMut(u64) -> Result<Option<PostedPullRequestReview>>,
    {
        let Some(state) = self.pull_request_state_mut(pr) else {
            return Ok(());
        };

        let mut delivered_ids = state
            .delivered_record_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut pending = Vec::new();

        for review in state.pending_reviews.drain(..) {
            match lookup(review.review_id)? {
                Some(current) if current.state == PullRequestReviewState::Pending => {
                    pending.push(PendingReviewState {
                        review_id: current.id,
                        html_url: current.html_url,
                        ..review
                    });
                }
                Some(_) => {
                    delivered_ids.extend(review.staged_record_ids);
                }
                None => {}
            }
        }

        state.delivered_record_ids = delivered_ids.into_iter().collect();
        state.delivered_record_ids.sort();
        state.pending_reviews = pending;
        Ok(())
    }

    pub fn record_pending_review(
        &mut self,
        pr: &ResolvedPullRequestRef,
        review: PostedPullRequestReview,
        head_sha: &CommitId,
        staged_record_ids: Vec<String>,
    ) {
        let state = self.ensure_pull_request_state(pr);
        if let Some(existing) = state
            .pending_reviews
            .iter_mut()
            .find(|pending| pending.review_id == review.id)
        {
            existing.html_url = review.html_url;
            existing.head_sha = head_sha.clone();
            existing.staged_record_ids.extend(staged_record_ids);
            existing.staged_record_ids.sort();
            existing.staged_record_ids.dedup();
            return;
        }

        let mut staged_record_ids = staged_record_ids;
        staged_record_ids.sort();
        staged_record_ids.dedup();
        state.pending_reviews.push(PendingReviewState {
            review_id: review.id,
            html_url: review.html_url,
            head_sha: head_sha.clone(),
            staged_record_ids,
        });
    }

    pub fn pending_reviews(&self, pr: &ResolvedPullRequestRef) -> Vec<PendingReviewState> {
        self.pull_request_state(pr)
            .map(|state| state.pending_reviews.clone())
            .unwrap_or_default()
    }

    pub fn record_submitted_review(&mut self, pr: &ResolvedPullRequestRef, review_id: u64) {
        let Some(state) = self.pull_request_state_mut(pr) else {
            return;
        };
        let mut delivered_ids = state
            .delivered_record_ids
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut pending = Vec::new();
        for review in state.pending_reviews.drain(..) {
            if review.review_id == review_id {
                delivered_ids.extend(review.staged_record_ids);
            } else {
                pending.push(review);
            }
        }
        state.delivered_record_ids = delivered_ids.into_iter().collect();
        state.delivered_record_ids.sort();
        state.pending_reviews = pending;
    }

    fn pull_request_state(&self, pr: &ResolvedPullRequestRef) -> Option<&PullRequestDeliveryState> {
        self.pull_requests.iter().find(|state| state.pr == *pr)
    }

    fn pull_request_state_mut(
        &mut self,
        pr: &ResolvedPullRequestRef,
    ) -> Option<&mut PullRequestDeliveryState> {
        self.pull_requests.iter_mut().find(|state| state.pr == *pr)
    }

    fn ensure_pull_request_state(
        &mut self,
        pr: &ResolvedPullRequestRef,
    ) -> &mut PullRequestDeliveryState {
        if let Some(index) = self.pull_requests.iter().position(|state| state.pr == *pr) {
            return &mut self.pull_requests[index];
        }
        self.pull_requests.push(PullRequestDeliveryState {
            pr: pr.clone(),
            delivered_record_ids: Vec::new(),
            pending_reviews: Vec::new(),
        });
        let index = self.pull_requests.len().saturating_sub(1);
        &mut self.pull_requests[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PullRequestReviewState;

    fn pr() -> ResolvedPullRequestRef {
        ResolvedPullRequestRef {
            host: "github.com".to_string(),
            owner: "jmqd".to_string(),
            repo: "trueflow".to_string(),
            number: 11,
        }
    }

    #[test]
    fn excluded_record_ids_include_pending_and_delivered_ids() {
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &pr(),
            PostedPullRequestReview {
                id: 1,
                html_url: "https://example.test/review/1".to_string(),
                state: PullRequestReviewState::Pending,
                body: "<!-- trueflow:pending-review -->".to_string(),
                node_id: Some("R_1".to_string()),
            },
            &CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            vec!["staged".to_string()],
        );
        ledger.ensure_pull_request_state(&pr()).delivered_record_ids = vec!["done".to_string()];

        let excluded = ledger.excluded_record_ids(&pr());
        assert!(excluded.contains("staged"));
        assert!(excluded.contains("done"));
    }

    #[test]
    fn record_pending_review_merges_staged_ids_for_existing_review() {
        let mut ledger = GitHubDeliveryLedger::default();
        let head_sha = CommitId::new("1111111111111111111111111111111111111111").unwrap();
        ledger.record_pending_review(
            &pr(),
            PostedPullRequestReview {
                id: 1,
                html_url: "https://example.test/review/1".to_string(),
                state: PullRequestReviewState::Pending,
                body: "<!-- trueflow:pending-review -->".to_string(),
                node_id: Some("R_1".to_string()),
            },
            &head_sha,
            vec!["first".to_string()],
        );
        ledger.record_pending_review(
            &pr(),
            PostedPullRequestReview {
                id: 1,
                html_url: "https://example.test/review/1-updated".to_string(),
                state: PullRequestReviewState::Pending,
                body: "<!-- trueflow:pending-review -->".to_string(),
                node_id: Some("R_1".to_string()),
            },
            &head_sha,
            vec!["first".to_string(), "second".to_string()],
        );

        let pending = ledger.pending_reviews(&pr());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].html_url, "https://example.test/review/1-updated");
        assert_eq!(
            pending[0].staged_record_ids,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn record_submitted_review_moves_staged_ids_to_delivered() {
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &pr(),
            PostedPullRequestReview {
                id: 1,
                html_url: "https://example.test/review/1".to_string(),
                state: PullRequestReviewState::Pending,
                body: "<!-- trueflow:pending-review -->".to_string(),
                node_id: Some("R_1".to_string()),
            },
            &CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            vec!["staged".to_string()],
        );

        ledger.record_submitted_review(&pr(), 1);

        let state = ledger.pull_request_state(&pr()).unwrap();
        assert!(state.pending_reviews.is_empty());
        assert_eq!(state.delivered_record_ids, vec!["staged".to_string()]);
    }

    #[test]
    fn sync_pending_reviews_moves_submitted_ids_to_delivered() {
        let mut ledger = GitHubDeliveryLedger::default();
        ledger.record_pending_review(
            &pr(),
            PostedPullRequestReview {
                id: 1,
                html_url: "https://example.test/review/1".to_string(),
                state: PullRequestReviewState::Pending,
                body: "<!-- trueflow:pending-review -->".to_string(),
                node_id: Some("R_1".to_string()),
            },
            &CommitId::new("1111111111111111111111111111111111111111").unwrap(),
            vec!["staged".to_string()],
        );

        ledger
            .sync_pending_reviews(&pr(), |_| {
                Ok(Some(PostedPullRequestReview {
                    id: 1,
                    html_url: "https://example.test/review/1".to_string(),
                    state: PullRequestReviewState::Commented,
                    body: "<!-- trueflow:pending-review -->".to_string(),
                    node_id: Some("R_1".to_string()),
                }))
            })
            .unwrap();

        let state = ledger.pull_request_state(&pr()).unwrap();
        assert!(state.pending_reviews.is_empty());
        assert_eq!(state.delivered_record_ids, vec!["staged".to_string()]);
    }
}
