use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::prefix::AnimationPrefix;

#[derive(Debug, Clone, Copy)]
struct AnimationState {
    pub in_progress: bool,
    pub cancel_idx_counter: usize,
    pub pending_cancel_count: usize,
}

#[derive(Debug)]
pub struct AnimationManager {
    animations: HashMap<String, AnimationState>,
}

impl Default for AnimationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimationManager {
    pub fn new() -> Self {
        Self {
            animations: HashMap::new(),
        }
    }

    pub fn is_cancelled(&self, animation_key: &str) -> bool {
        if let Some(animation_state) = self.animations.get(animation_key) {
            animation_state.pending_cancel_count > 0
        } else {
            false
        }
    }

    pub fn in_progress(&self, animation_key: &str) -> bool {
        if let Some(animation_state) = self.animations.get(animation_key) {
            animation_state.in_progress
        } else {
            false
        }
    }

    pub fn init_cancel(&mut self, animation_key: &str) -> usize {
        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.pending_cancel_count += 1;
            animation_state.cancel_idx_counter += 1;

            // return cancel idx
            animation_state.cancel_idx_counter
        } else {
            0
        }
    }

    pub fn latest_cancel_idx(&mut self, animation_key: &str) -> usize {
        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.cancel_idx_counter
        } else {
            0
        }
    }

    pub fn end_cancel(&mut self, animation_key: &str) {
        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.pending_cancel_count -= 1;
            if animation_state.pending_cancel_count == 0 && !animation_state.in_progress {
                self.animations.remove(animation_key);
            }
        }
    }

    pub fn cancel(&mut self, animation_key: &str) {
        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.in_progress = false;
        }
    }

    pub fn start(&mut self, animation_key: &str) {
        if let Entry::Vacant(e) = self.animations.entry(animation_key.to_string()) {
            e.insert(AnimationState {
                in_progress: true,
                cancel_idx_counter: 0,
                pending_cancel_count: 0,
            });

            return;
        }

        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.in_progress = true;
        }
    }

    pub fn end(&mut self, animation_key: &str) {
        if let Some(animation_state) = self.animations.get_mut(animation_key) {
            animation_state.in_progress = false;

            if animation_state.pending_cancel_count == 0 {
                self.animations.remove(animation_key);
            }
        }
    }

    pub fn count_in_progress(&self, animation_key_prefix: AnimationPrefix) -> usize {
        let prefix = format!("{animation_key_prefix}:");
        self.animations
            .iter()
            .filter(|(key, state)| state.in_progress && key.starts_with(&prefix))
            .count()
    }

    pub fn count(&self) -> usize {
        self.animations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_cancellation_does_not_count_as_running_movement() {
        let mut manager = AnimationManager::new();
        manager.start("movement:1");
        manager.init_cancel("movement:1");
        manager.cancel("movement:1");
        assert_eq!(manager.count_in_progress(AnimationPrefix::Movement), 0);
        manager.end_cancel("movement:1");
        assert_eq!(manager.count_in_progress(AnimationPrefix::Movement), 0);
        assert_eq!(manager.count(), 0);
        manager.start("movement:1");
        assert_eq!(manager.count_in_progress(AnimationPrefix::Movement), 1);
        manager.end("movement:1");
        assert_eq!(manager.count_in_progress(AnimationPrefix::Movement), 0);
    }

    #[test]
    fn counts_only_running_animations_of_the_requested_kind() {
        let mut manager = AnimationManager::new();
        manager.start("movement:1");
        manager.start("movement:2");
        manager.start("transparency:1");
        manager.init_cancel("movement:2");
        manager.end("movement:2");
        assert_eq!(manager.count_in_progress(AnimationPrefix::Movement), 1);
        assert_eq!(manager.count_in_progress(AnimationPrefix::Transparency), 1);
    }
}
