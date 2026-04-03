#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReviewMetrics {
    pub files_seen: usize,
    pub blocks_seen: usize,
    pub reviewed_blocks: usize,
}

impl ReviewMetrics {
    pub fn coverage(self) -> f64 {
        if self.blocks_seen == 0 {
            return 1.0;
        }
        self.reviewed_blocks as f64 / self.blocks_seen as f64
    }

    pub fn merge(self, other: ReviewMetrics) -> ReviewMetrics {
        ReviewMetrics {
            files_seen: self.files_seen + other.files_seen,
            blocks_seen: self.blocks_seen + other.blocks_seen,
            reviewed_blocks: self.reviewed_blocks + other.reviewed_blocks,
        }
    }
}

pub fn summarize_daily_runs(samples: &[ReviewMetrics]) -> ReviewMetrics {
    samples.iter().copied().fold(
        ReviewMetrics {
            files_seen: 0,
            blocks_seen: 0,
            reviewed_blocks: 0,
        },
        ReviewMetrics::merge,
    )
}
