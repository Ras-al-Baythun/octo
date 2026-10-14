use std::collections::VecDeque;

use parking_lot::Mutex;

/// Tracks the most recent search/radio responses as ordered queues so we can
/// implement a sliding-window prewarm: when the client scrobbles song N, we
/// look up which queue N belongs to and prewarm songs N+1..N+8 from there.
///
/// Stateless across restarts (in-memory only) and bounded by entry count.
/// Subsonic has no notion of a per-client session so we just keep the last
/// few queues globally; if a user is in two clients at once the most recent
/// queue still wins. That's good enough for "skip-fast" prewarming.
#[derive(Default)]
pub struct RadioQueueStore {
    /// Most recent first.
    queues: Mutex<VecDeque<Vec<String>>>,
}

impl RadioQueueStore {
    pub const MAX_QUEUES: usize = 32;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<I, S>(&self, song_ids: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let ids: Vec<String> = song_ids
            .into_iter()
            .map(Into::into)
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return;
        }
        let mut queues = self.queues.lock();
        queues.push_front(ids);
        queues.truncate(Self::MAX_QUEUES);
    }

    /// Find the most-recently-registered queue containing `song_id` and return up to `count`
    /// ids that come after it. Returns an empty list when the song isn't tracked: caller
    /// should treat that as "nothing to prewarm" rather than an error.
    pub fn get_upcoming_from(&self, song_id: &str, count: i32) -> Vec<String> {
        if song_id.is_empty() || count <= 0 {
            return Vec::new();
        }
        let mut queues = self.queues.lock();
        for i in 0..queues.len() {
            let Some(idx) = queues[i].iter().position(|id| id == song_id) else {
                continue;
            };
            // Move this queue to front so subsequent scrobbles in it stay fast.
            let queue = queues.remove(i).expect("the index is in range");
            let upcoming = queue.iter().skip(idx + 1).take(count as usize).cloned().collect();
            queues.push_front(queue);
            return upcoming;
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upcoming_songs_come_from_the_newest_queue_holding_the_song() {
        let store = RadioQueueStore::new();
        store.register(["a", "b", "c", "d"]);
        store.register(["x", "b", "y", ""]);
        assert_eq!(store.get_upcoming_from("b", 8), vec!["y"]);
        assert_eq!(store.get_upcoming_from("a", 2), vec!["b", "c"]);
        // That queue moved to the front, so "b" is now found there.
        assert_eq!(store.get_upcoming_from("b", 8), vec!["c", "d"]);
        assert!(store.get_upcoming_from("nope", 8).is_empty());
        assert!(store.get_upcoming_from("a", 0).is_empty());
        assert!(store.get_upcoming_from("", 3).is_empty());
    }

    #[test]
    fn only_the_last_32_queues_are_kept_and_empty_ones_are_not_registered() {
        let store = RadioQueueStore::new();
        store.register(["first", "next"]);
        store.register(Vec::<String>::new());
        store.register([""]);
        for i in 0..RadioQueueStore::MAX_QUEUES {
            store.register([format!("q{i}")]);
        }
        assert!(store.get_upcoming_from("first", 1).is_empty());
        assert_eq!(store.queues.lock().len(), RadioQueueStore::MAX_QUEUES);
    }
}
