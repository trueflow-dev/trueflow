use crate::review::ReviewDecision;

pub trait ReviewStore {
    fn persist_batch(&self, decisions: &[ReviewDecision]) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    pub persist_attempts: std::sync::atomic::AtomicUsize,
}

impl ReviewStore for MemoryStore {
    fn persist_batch(&self, decisions: &[ReviewDecision]) -> Result<(), String> {
        if decisions.is_empty() {
            return Err("cannot persist empty review batch".to_string());
        }

        self.persist_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}
