use std::collections::VecDeque;
use linet_core::files::{TransferState, TransferUpdate};

const MAX_HISTORY: usize = 64;

#[derive(Default)]
pub struct History(VecDeque<TransferUpdate>);

impl History {
    pub fn record(&mut self, update: TransferUpdate) {
        self.0.retain(|old| (old.id, old.incoming, &old.peer) != (update.id, update.incoming, &update.peer));
        self.0.push_back(update);
        while self.0.len() > MAX_HISTORY { self.0.pop_front(); }
    }

    pub fn json(&self) -> Vec<serde_json::Value> {
        self.0.iter().map(|u| {
            let (state, detail) = match &u.state {
                TransferState::Progress => ("progress", String::new()),
                TransferState::Done { path } => ("done", path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()),
                TransferState::Failed(reason) => ("failed", reason.clone()),
            };
            serde_json::json!({"id":u.id.to_string(), "peer":u.peer,"name":u.name,"incoming":u.incoming,
                "done":u.done,"total":u.total,"state":state,"detail":detail})
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(id: u64) -> TransferUpdate {
        TransferUpdate { id, peer:"phone".into(), name:"a".into(), incoming:false, done:0,total:10,purpose:None,state:TransferState::Progress }
    }
    #[test]
    fn history_is_bounded_and_terminal_state_replaces_progress() {
        let mut history = History::default();
        for id in 0..100 { history.record(update(id)); }
        assert_eq!(history.json().len(), MAX_HISTORY);
        let mut last = update(99);
        last.state = TransferState::Failed("offline".into());
        history.record(last);
        let json = history.json();
        assert_eq!(json.len(), MAX_HISTORY);
        assert_eq!(json.last().unwrap()["state"], "failed");
        assert_eq!(json.last().unwrap()["id"], "99");
    }
    #[test]
    fn ids_preserve_all_64_bits_in_javascript() {
        let mut history = History::default(); history.record(update(u64::MAX));
        assert_eq!(history.json()[0]["id"], u64::MAX.to_string());
    }
}
