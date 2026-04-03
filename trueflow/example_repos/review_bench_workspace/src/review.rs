use crate::indexer::IndexSummary;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve { path: String },
    Comment { path: String, note: String },
    Escalate { path: String, owner: String },
}

#[derive(Debug, Clone)]
pub struct ReviewSession {
    summary: IndexSummary,
}

impl ReviewSession {
    pub fn from_summary(summary: &IndexSummary) -> Self {
        Self {
            summary: summary.clone(),
        }
    }

    pub fn default_decisions(&self) -> Vec<ReviewDecision> {
        self.summary
            .largest_files
            .iter()
            .map(|file| {
                if file.blocks >= 10 {
                    ReviewDecision::Comment {
                        path: file.path.clone(),
                        note: "needs deeper walkthrough".to_string(),
                    }
                } else if file.reviewable {
                    ReviewDecision::Approve {
                        path: file.path.clone(),
                    }
                } else {
                    ReviewDecision::Escalate {
                        path: file.path.clone(),
                        owner: "ops".to_string(),
                    }
                }
            })
            .collect()
    }
}
