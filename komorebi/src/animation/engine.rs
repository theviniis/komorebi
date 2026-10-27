use color_eyre::eyre;

use serde::Deserialize;
use serde::Serialize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use super::ANIMATION_DURATION_GLOBAL;
use super::ANIMATION_FPS;
use super::ANIMATION_MANAGER;
use super::RenderDispatcher;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AnimationEngine;

impl AnimationEngine {
    pub fn wait_for_all_animations() {
        let max_duration = Duration::from_secs(20);
        let spent_duration = Instant::now();

        while ANIMATION_MANAGER.lock().count() > 0 {
            if spent_duration.elapsed() >= max_duration {
                break;
            }

            std::thread::sleep(Duration::from_millis(
                ANIMATION_DURATION_GLOBAL.load(Ordering::SeqCst),
            ));
        }
    }

    /// Returns true if the animation needs to continue
    pub fn cancel(animation_key: &str) -> bool {
        // should be more than 0
        let cancel_idx = ANIMATION_MANAGER.lock().init_cancel(animation_key);
        let max_duration = Duration::from_secs(5);
        let spent_duration = Instant::now();

        while ANIMATION_MANAGER.lock().in_progress(animation_key) {
            if spent_duration.elapsed() >= max_duration {
                ANIMATION_MANAGER.lock().end(animation_key);
            }

            std::thread::sleep(Duration::from_millis(250 / 2));
        }

        let latest_cancel_idx = ANIMATION_MANAGER.lock().latest_cancel_idx(animation_key);

        ANIMATION_MANAGER.lock().end_cancel(animation_key);

        latest_cancel_idx == cancel_idx
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn animate(
        render_dispatcher: impl RenderDispatcher + Send + 'static,
        duration: Duration,
    ) -> eyre::Result<()> {
        std::thread::spawn(move || {
            let animation_key = render_dispatcher.get_animation_key();
            if ANIMATION_MANAGER.lock().in_progress(animation_key.as_str()) {
                let should_animate = Self::cancel(animation_key.as_str());

                if !should_animate {
                    return Ok(());
                }
            }

            // Register before pre_render notifies dependent managers, so they
            // observe this movement as active even during setup.
            ANIMATION_MANAGER.lock().start(animation_key.as_str());
            if let Err(error) = render_dispatcher.pre_render() {
                ANIMATION_MANAGER.lock().end(animation_key.as_str());
                render_dispatcher.cleanup_on_cancel();
                tracing::warn!(%animation_key, "animation setup failed: {error}");
                return Err(error);
            }

            let target_frame_time =
                Duration::from_millis(1000 / ANIMATION_FPS.load(Ordering::Relaxed));
            let mut progress = 0.0;
            let animation_start = Instant::now();

            // start animation
            while progress < 1.0 {
                // check if animation is cancelled
                if ANIMATION_MANAGER
                    .lock()
                    .is_cancelled(animation_key.as_str())
                {
                    // cancel animation
                    ANIMATION_MANAGER.lock().cancel(animation_key.as_str());
                    render_dispatcher.cleanup_on_cancel();
                    return Ok(());
                }

                let frame_start = Instant::now();
                // calculate progress
                progress =
                    animation_start.elapsed().as_millis() as f64 / duration.as_millis() as f64;
                render_dispatcher.render(progress).ok();

                // sleep until next frame
                let frame_time_elapsed = frame_start.elapsed();

                if frame_time_elapsed < target_frame_time {
                    std::thread::sleep(target_frame_time - frame_time_elapsed);
                }
            }

            ANIMATION_MANAGER.lock().end(animation_key.as_str());

            // limit progress to 1.0 if animation took longer
            if progress != 1.0 {
                progress = 1.0;

                // process animation for 1.0 to set target position
                render_dispatcher.render(progress).ok();
            }

            if let Err(error) = render_dispatcher.post_render() {
                render_dispatcher.cleanup_on_cancel();
                tracing::warn!(%animation_key, "animation finalisation failed: {error}");
                return Err(error);
            }
            Ok(())
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct FailedSetup {
        cleaned: mpsc::Sender<bool>,
    }

    impl RenderDispatcher for FailedSetup {
        fn get_animation_key(&self) -> String {
            "movement:test-failed-setup".to_string()
        }

        fn pre_render(&self) -> eyre::Result<()> {
            assert!(
                ANIMATION_MANAGER
                    .lock()
                    .in_progress(&self.get_animation_key())
            );
            eyre::bail!("simulated setup failure")
        }

        fn render(&self, _: f64) -> eyre::Result<()> {
            panic!("failed setup must not render")
        }

        fn post_render(&self) -> eyre::Result<()> {
            panic!("failed setup must not finish normally")
        }

        fn cleanup_on_cancel(&self) {
            self.cleaned
                .send(
                    ANIMATION_MANAGER
                        .lock()
                        .in_progress(&self.get_animation_key()),
                )
                .unwrap();
        }
    }

    #[test]
    fn failed_setup_releases_animation_and_runs_cleanup() {
        let (cleaned, receiver) = mpsc::channel();
        AnimationEngine::animate(FailedSetup { cleaned }, Duration::from_millis(1)).unwrap();
        assert!(!receiver.recv_timeout(Duration::from_secs(5)).unwrap());
    }
}
