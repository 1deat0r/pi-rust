//! Animated loader component — port of the upstream tui loader.
//!
//! The upstream loader owns a timer and asks its TUI to repaint whenever the
//! current frame changes. Rust's component tree is rendered by an owning
//! event loop, so the repaint hook exposed here is deliberately a
//! thread-safe wake-up signal. It must not mutate TUI state directly; the
//! owner should use it to wake its event loop and perform the next render on
//! that loop's thread.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::components::text::Text;
use crate::tui::Component;

/// The spinner frames used by upstream when no indicator override is given.
pub const DEFAULT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The upstream default interval between animated frames.
pub const DEFAULT_INTERVAL_MS: u64 = 80;

/// A color/styling function used for the spinner or message.
pub type LoaderColorFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// A thread-safe repaint request hook.
///
/// The callback may run on the loader's timer thread. It should signal the
/// owning event loop only; it must not touch terminal or component state from
/// the callback itself.
pub type RequestRenderFn = Arc<dyn Fn() + Send + Sync>;

/// Indicator-specific options matching upstream LoaderIndicatorOptions.
///
/// frames: None means that the indicator object omitted frames, and uses the
/// default braille frames. Some(vec![]) intentionally hides the indicator.
/// Supplying an indicator object also makes frames verbatim, which is how
/// upstream distinguishes themed default frames from a caller's custom
/// indicator.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoaderIndicatorOptions {
    pub frames: Option<Vec<String>>,
    /// Milliseconds. Values less than or equal to zero use the upstream
    /// 80-millisecond default. f64 preserves fractional values and permits
    /// the Rust API to represent all invalid values accepted by JavaScript,
    /// including negative, NaN, and infinite values.
    pub interval_ms: Option<f64>,
}

/// Numeric values accepted by LoaderIndicatorOptions::with_interval_ms.
///
/// Supporting the standard integer types keeps ordinary Rust call sites
/// concise while f64 preserves fractional and non-finite JavaScript values.
pub trait IntoLoaderIntervalMs {
    fn into_loader_interval_ms(self) -> f64;
}

macro_rules! impl_loader_interval_ms {
    ($($type:ty),+ $(,)?) => {
        $(
            impl IntoLoaderIntervalMs for $type {
                fn into_loader_interval_ms(self) -> f64 {
                    self as f64
                }
            }
        )+
    };
}

impl_loader_interval_ms!(f32, f64, i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

impl LoaderIndicatorOptions {
    /// Configure custom animation frames. An empty vector hides the
    /// indicator, matching upstream.
    pub fn with_frames<I, S>(mut self, frames: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.frames = Some(frames.into_iter().map(Into::into).collect());
        self
    }

    /// Configure the animation interval in milliseconds.
    pub fn with_interval_ms<T>(mut self, interval_ms: T) -> Self
    where
        T: IntoLoaderIntervalMs,
    {
        self.interval_ms = Some(interval_ms.into_loader_interval_ms());
        self
    }
}

/// Rust-native construction options for Loader.
///
/// The default uses the upstream braille indicator, an 80 ms interval, and
/// identity styling functions. To reproduce an upstream custom indicator
/// object, set indicator to Some(LoaderIndicatorOptions::default()); this
/// keeps the default frames but renders them verbatim instead of passing them
/// through spinner_color.
pub struct LoaderOptions {
    pub indicator: Option<LoaderIndicatorOptions>,
    pub spinner_color: LoaderColorFn,
    pub message_color: LoaderColorFn,
    pub request_render: Option<RequestRenderFn>,
}

impl Default for LoaderOptions {
    fn default() -> Self {
        Self {
            indicator: None,
            spinner_color: identity_color(),
            message_color: identity_color(),
            request_render: None,
        }
    }
}

impl LoaderOptions {
    /// Set the indicator override. None restores themed/default indicator
    /// behavior; Some enables verbatim custom-indicator behavior.
    pub fn with_indicator(mut self, indicator: Option<LoaderIndicatorOptions>) -> Self {
        self.indicator = indicator;
        self
    }

    /// Set the spinner color/styling callback.
    pub fn with_spinner_color<F>(mut self, spinner_color: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        self.spinner_color = Arc::new(spinner_color);
        self
    }

    /// Set the message color/styling callback.
    pub fn with_message_color<F>(mut self, message_color: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        self.message_color = Arc::new(message_color);
        self
    }

    /// Set the thread-safe repaint signal used by animation updates.
    pub fn with_request_render<F>(mut self, request_render: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.request_render = Some(Arc::new(request_render));
        self
    }
}

fn identity_color() -> LoaderColorFn {
    Arc::new(str::to_owned)
}

struct LoaderState {
    frames: Vec<String>,
    interval: Duration,
    current_frame: usize,
    render_indicator_verbatim: bool,
    message: String,
    spinner_color: LoaderColorFn,
    message_color: LoaderColorFn,
    request_render: Option<RequestRenderFn>,
    /// The styled string corresponding to the current frame. Keeping this
    /// display value in state mirrors upstream Text.setText: callbacks run
    /// when the loader updates, while ordinary rendering remains read-only.
    display: String,
}

impl LoaderState {
    fn display_for_current_frame(&self) -> String {
        let frame = self
            .frames
            .get(self.current_frame)
            .map(String::as_str)
            .unwrap_or("");
        let rendered_frame = if self.render_indicator_verbatim {
            frame.to_owned()
        } else {
            (self.spinner_color)(frame)
        };
        let indicator = if !frame.is_empty() {
            format!("{rendered_frame} ")
        } else {
            String::new()
        };
        format!("{indicator}{}", (self.message_color)(&self.message))
    }
}

/// The timer owned by a loader.
///
/// The sender and join handle live behind mutexes so Loader remains
/// Send + Sync and can be placed in a shared component. The worker only holds
/// a Weak reference to the render state, so dropping a loader cannot be
/// prevented by its own timer thread.
struct AnimationController {
    stop_sender: Mutex<Option<Sender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl AnimationController {
    fn new() -> Self {
        Self {
            stop_sender: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn stop(&self) {
        let sender = self
            .stop_sender
            .lock()
            .expect("loader stop sender mutex poisoned")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }

        let worker = self
            .worker
            .lock()
            .expect("loader worker mutex poisoned")
            .take();
        if let Some(worker) = worker {
            // A request-render hook is not allowed to call back into loader
            // lifecycle methods, but avoid self-joining if a caller violates
            // that contract anyway. The thread will finish after its next
            // stop check and its handle is safely dropped here.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn start(&self, state: &Arc<Mutex<LoaderState>>) {
        self.stop();

        let (sender, receiver) = mpsc::channel();
        let weak_state = Arc::downgrade(state);
        let interval = state.lock().expect("loader state mutex poisoned").interval;
        let worker = thread::Builder::new()
            .name("pi-tui-loader".to_string())
            .spawn(move || animation_worker(weak_state, receiver, interval))
            .expect("failed to start pi-tui loader animation thread");

        *self
            .stop_sender
            .lock()
            .expect("loader stop sender mutex poisoned") = Some(sender);
        *self.worker.lock().expect("loader worker mutex poisoned") = Some(worker);
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
fn animation_worker(
    weak_state: Weak<Mutex<LoaderState>>,
    receiver: Receiver<()>,
    interval: Duration,
) {
    loop {
        match receiver.recv_timeout(interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let Some(state) = weak_state.upgrade() else {
            return;
        };

        let request_render = {
            let mut state = state.lock().expect("loader state mutex poisoned");
            if state.frames.len() <= 1 {
                return;
            }
            state.current_frame = (state.current_frame + 1) % state.frames.len();
            state.display = state.display_for_current_frame();
            state.request_render.clone()
        };

        if let Some(request_render) = request_render {
            // A repaint hook is an integration boundary. A panic in an
            // embedding callback must not kill the loader's timer or unwind
            // through the worker thread.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                request_render();
            }));
        }
    }
}

/// Loader component that displays a message and an optional animated
/// indicator.
pub struct Loader {
    state: Arc<Mutex<LoaderState>>,
    animation: AnimationController,
}

impl Loader {
    /// Construct a loader using the upstream default indicator and interval.
    ///
    /// This signature is retained for existing pi-rust call sites.
    pub fn new(message: impl Into<String>) -> Self {
        Self::with_options(message, LoaderOptions::default())
    }

    /// Construct a loader with Rust-native styling, indicator, and repaint
    /// options.
    pub fn with_options(message: impl Into<String>, options: LoaderOptions) -> Self {
        let indicator = options.indicator.clone();
        let (frames, interval, render_indicator_verbatim) = normalize_indicator(indicator.as_ref());
        let state = Arc::new(Mutex::new(LoaderState {
            frames,
            interval,
            current_frame: 0,
            render_indicator_verbatim,
            message: message.into(),
            spinner_color: options.spinner_color,
            message_color: options.message_color,
            request_render: options.request_render,
            display: String::new(),
        }));
        let loader = Self {
            state,
            animation: AnimationController::new(),
        };
        loader.refresh_display();
        loader.restart_animation();
        loader
    }

    /// Return the current message without styling.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn message(&self) -> String {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .message
            .clone()
    }

    /// Replace the message and request a repaint, matching upstream
    /// setMessage.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .message = message.into();
        self.refresh_display();
    }

    /// Replace the indicator and immediately restart animation.
    ///
    /// None restores themed/default frames. Some preserves the exact
    /// upstream custom-indicator semantics, including empty and one-frame
    /// arrays and verbatim frame rendering.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        // Stop first so a timer tick cannot interleave with the configuration
        // update. Upstream is single-threaded; this ordering is the safe Rust
        // equivalent of its synchronous field update followed by start().
        self.stop();
        let (frames, interval, render_indicator_verbatim) = normalize_indicator(indicator.as_ref());
        {
            let mut state = self.state.lock().expect("loader state mutex poisoned");
            state.frames = frames;
            state.interval = interval;
            state.current_frame = 0;
            state.render_indicator_verbatim = render_indicator_verbatim;
        }
        self.start();
    }

    /// Start or restart animation without resetting the current frame.
    pub fn start(&mut self) {
        self.refresh_display();
        self.restart_animation();
    }

    /// Stop animation. Stopping does not change the current frame or
    /// rendered message, matching upstream.
    pub fn stop(&mut self) {
        self.animation.stop();
    }

    /// Install or replace the thread-safe repaint signal.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn set_request_render_callback(&mut self, callback: Option<RequestRenderFn>) {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .request_render = callback;
    }

    /// Convenience setter for a repaint callback that can be called from the
    /// loader timer thread.
    pub fn set_request_render<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.set_request_render_callback(Some(Arc::new(callback)));
    }

    /// Return a copy of the configured frames.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn frames(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .frames
            .clone()
    }

    /// Return the normalized animation interval.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn interval(&self) -> Duration {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .interval
    }

    /// Return the current zero-based frame index.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn current_frame(&self) -> usize {
        self.state
            .lock()
            .expect("loader state mutex poisoned")
            .current_frame
    }

    /// Whether an interval worker is currently installed.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn is_running(&self) -> bool {
        self.animation
            .worker
            .lock()
            .expect("loader worker mutex poisoned")
            .is_some()
    }

    /// Advance one frame immediately.
    ///
    /// This deterministic seam is useful to event loops that own their own
    /// timer and to tests. It intentionally does not depend on wall-clock
    /// sleeps. The normal interval worker calls the same state transition.
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    pub fn advance_frame(&self) {
        let request_render = {
            let mut state = self.state.lock().expect("loader state mutex poisoned");
            if state.frames.len() <= 1 {
                return;
            }
            state.current_frame = (state.current_frame + 1) % state.frames.len();
            state.display = state.display_for_current_frame();
            state.request_render.clone()
        };
        if let Some(request_render) = request_render {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                request_render();
            }));
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn refresh_display(&self) {
        let request_render = {
            let mut state = self.state.lock().expect("loader state mutex poisoned");
            state.display = state.display_for_current_frame();
            state.request_render.clone()
        };
        if let Some(request_render) = request_render {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                request_render();
            }));
        }
    }

    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn restart_animation(&self) {
        let animated = self
            .state
            .lock()
            .expect("loader state mutex poisoned")
            .frames
            .len()
            > 1;
        if animated {
            let state = self.state.clone();
            self.animation.start(&state);
        } else {
            self.animation.stop();
        }
    }
}

fn normalize_indicator(
    indicator: Option<&LoaderIndicatorOptions>,
) -> (Vec<String>, Duration, bool) {
    let render_indicator_verbatim = indicator.is_some();
    let frames = indicator
        .and_then(|indicator| indicator.frames.clone())
        .unwrap_or_else(|| {
            DEFAULT_FRAMES
                .iter()
                .map(|frame| (*frame).to_string())
                .collect()
        });
    let interval_ms = indicator
        .and_then(|indicator| indicator.interval_ms)
        .filter(|interval_ms| interval_ms.is_finite() && *interval_ms > 0.0)
        .unwrap_or(DEFAULT_INTERVAL_MS as f64);
    let interval = if interval_ms / 1_000.0 >= Duration::MAX.as_secs_f64() {
        Duration::MAX
    } else {
        Duration::from_secs_f64(interval_ms / 1_000.0)
    };
    // Node timers clamp positive sub-millisecond values that round to zero
    // instead of allowing a busy loop. Preserve that safety boundary in the
    // Rust worker as well.
    let interval = if interval.is_zero() {
        Duration::from_millis(1)
    } else {
        interval
    };
    (frames, interval, render_indicator_verbatim)
}

impl Default for Loader {
    fn default() -> Self {
        Self::new("Loading...")
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.animation.stop();
    }
}

impl Component for Loader {
    #[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // checked invariants
    fn render(&self, width: usize) -> Vec<String> {
        let display = self
            .state
            .lock()
            .expect("loader state mutex poisoned")
            .display
            .clone();
        // Upstream Loader extends Text("", 1, 0), then prepends one blank
        // line. Reusing Rust Text preserves width-aware ANSI
        // wrapping/padding behavior for styled indicators.
        let text = Text::new(display, 1, 0, None);
        let mut lines = Vec::with_capacity(2);
        lines.push(String::new());
        lines.extend(text.render(width));
        lines
    }
}
