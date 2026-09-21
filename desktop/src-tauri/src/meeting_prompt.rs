use crate::config::STORE_FILENAME;
use meeting_detect::{MeetingState, Source};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_store::StoreExt;

#[cfg(target_os = "macos")]
use tauri_nspanel::{CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt};

#[cfg(target_os = "macos")]
tauri_nspanel::tauri_panel! {
    panel!(MeetingPromptPanel {
        config: {
            // WebKit needs a key-capable host to route clicks to form controls. The
            // NonactivatingPanel style still prevents this from activating Vibe.
            can_become_key_window: true,
            can_become_main_window: false,
            is_floating_panel: true,
            becomes_key_only_if_needed: true,
            hides_on_deactivate: false
        }
    })
}

const WINDOW_LABEL: &str = "meeting-prompt";
const ENABLED_KEY: &str = "recording.meetingDetectionEnabled";
/// Opt-in: start recording as soon as a meeting is detected instead of asking first.
const AUTO_RECORD_KEY: &str = "recording.autoRecordDetectedMeetings";
const EVENT_NAME: &str = "meeting-prompt-state";
/// The main window listens for this and starts a recording with the given sources — the same
/// path the prompt's "Record" button takes.
const START_RECORDING_EVENT: &str = "meeting-prompt-start-recording";
/// Tells the main window that the recording being stopped was cancelled and must be discarded.
const DISCARD_RECORDING_EVENT: &str = "meeting-auto-recording-discarded";
/// A meeting that ends is reported after the detector's own debounce; this grace on top of it
/// lets a dropped microphone that comes straight back keep one recording rather than two. The
/// end-of-call question shows for this long: "stop" ends the recording at once, "keep going"
/// waits for the meeting to come back.
const END_GRACE: Duration = Duration::from_secs(10);
/// Safety net for a call the detector never sees end.
const MAX_AUTO_RECORDING: Duration = Duration::from_secs(3 * 60 * 60);
/// How long the main window gets to actually open the microphone after being asked to.
const START_TIMEOUT: Duration = Duration::from_secs(15);
const WIDTH: f64 = 320.0;
const HEIGHT: f64 = 152.0;
const MARGIN: f64 = 20.0;
#[cfg(target_os = "macos")]
const TOP_MARGIN: f64 = 48.0;
#[cfg(not(target_os = "macos"))]
const TOP_MARGIN: f64 = MARGIN;
/// Idle cadence only. The detector polls itself faster while the microphone signal is changing,
/// so a meeting surfaces within roughly a second of the call opening the microphone.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What the prompt window shows: the question whether to record, the notice that a recording
/// started on its own (and where its transcript should go), or the end-of-call question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Ask,
    Recording,
    Ending,
}

/// Where the transcript of an automatic recording goes: the auto-export destination everyone
/// shares, or the user's personal folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingScope {
    Shared,
    Personal,
}

/// What the main window needs to know about an automatic recording once it stopped.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct FinishedAutoRecording {
    /// None when nobody chose before the call ended; the main window applies its default.
    pub scope: Option<RecordingScope>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MeetingPromptPayload {
    pub source: Source,
    pub mode: PromptMode,
}

/// Mirrors `MeetingRecordingOptions` on the frontend.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordingOptions {
    microphone: bool,
    system_audio: bool,
}

/// A recording this module started on its own, so that it can also end it.
#[derive(Clone, Copy, Debug)]
struct AutoRecording {
    source: Source,
    requested_at: Instant,
    /// Set once the main window confirmed the microphone is open.
    started_at: Option<Instant>,
    /// Set while the detector reports the meeting gone; cleared if it comes back within the grace.
    ended_since: Option<Instant>,
    /// Where the user wants the transcript, once chosen on the notice.
    scope: Option<RecordingScope>,
    /// The user said the call is not over: the meeting being gone is not an end again until the
    /// detector has seen it come back.
    awaiting_return: bool,
    /// A stop was already sent to the main window; the finish clears this recording.
    stop_requested: bool,
}

impl AutoRecording {
    fn new(source: Source) -> Self {
        Self {
            source,
            requested_at: Instant::now(),
            started_at: None,
            ended_since: None,
            scope: None,
            awaiting_return: false,
            stop_requested: false,
        }
    }

    /// The detector's view of the meeting: starts the end-of-call grace when it goes away and
    /// cancels it when it comes back. Returns whether the end-of-call question should be shown
    /// (`Some(true)`) or taken away (`Some(false)`).
    fn observe(&mut self, meeting_active: bool, now: Instant) -> Option<bool> {
        if meeting_active {
            self.awaiting_return = false;
            self.ended_since.take().map(|_| false)
        } else if self.awaiting_return || self.ended_since.is_some() {
            None
        } else {
            self.ended_since = Some(now);
            Some(true)
        }
    }

    /// The user answered "keep going" to the end-of-call question.
    fn keep_going(&mut self) {
        self.awaiting_return = true;
        self.ended_since = None;
    }

    /// Why this recording should stop now, if it should.
    fn stop_reason(&self, now: Instant) -> Option<&'static str> {
        match self.started_at {
            None if now.duration_since(self.requested_at) >= START_TIMEOUT => Some("the recording never started"),
            None => None,
            Some(started_at) if now.duration_since(started_at) >= MAX_AUTO_RECORDING => Some("the maximum duration was reached"),
            Some(_) => self
                .ended_since
                .filter(|since| now.duration_since(*since) >= END_GRACE)
                .map(|_| "the meeting ended"),
        }
    }
}

#[derive(Default)]
struct PromptLogic {
    detected: Option<Source>,
    current: Option<MeetingPromptPayload>,
    dismissed: bool,
    own_recordings: u32,
}

impl PromptLogic {
    fn detection(&mut self, state: MeetingState) -> bool {
        let before = self.current.clone();
        if !state.recording {
            self.detected = None;
            self.current = None;
            self.dismissed = false;
        } else {
            self.detected = state.source;
            if self.own_recordings > 0 {
                self.dismissed = true;
            }
            self.current = self
                .detected
                .filter(|_| !self.dismissed && self.own_recordings == 0)
                .map(|source| MeetingPromptPayload {
                    source,
                    mode: PromptMode::Ask,
                });
        }
        before != self.current
    }

    fn dismiss(&mut self) -> bool {
        if self.detected.is_some() {
            self.dismissed = true;
        }
        self.current.take().is_some()
    }

    fn recording_started(&mut self) -> bool {
        self.own_recordings = self.own_recordings.saturating_add(1);
        if self.detected.is_some() {
            self.dismissed = true;
        }
        self.current.take().is_some()
    }

    /// Automatic recording takes the place of the question for this microphone session.
    fn suppress_question(&mut self) {
        if self.detected.is_some() {
            self.dismissed = true;
        }
        self.current = None;
    }

    fn recording_stopped(&mut self) {
        self.own_recordings = self.own_recordings.saturating_sub(1);
        // Deliberately do not re-show: starting Vibe dismissed this mic session. A detector
        // observation of inactivity is the only event that clears dismissal.
    }

    fn reset_detection(&mut self) {
        self.detected = None;
        self.current = None;
        self.dismissed = false;
    }
}

struct Worker {
    stop: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[derive(Default)]
struct RuntimeInner {
    logic: PromptLogic,
    worker: Option<Worker>,
    auto: Option<AutoRecording>,
    /// The user cancelled an automatic recording before the microphone opened: stop it as soon
    /// as it does.
    cancel_pending: bool,
    /// The automatic recording that just stopped, until the main window collects it.
    finished: Option<FinishedAutoRecording>,
}

#[derive(Default)]
pub struct MeetingPromptRuntime {
    inner: Mutex<RuntimeInner>,
}

fn store_flag(app: &tauri::AppHandle, key: &str) -> bool {
    app.store(STORE_FILENAME)
        .ok()
        .and_then(|store| store.get(key))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn is_enabled(app: &tauri::AppHandle) -> bool {
    store_flag(app, ENABLED_KEY)
}

fn is_auto_record_enabled(app: &tauri::AppHandle) -> bool {
    store_flag(app, AUTO_RECORD_KEY)
}

fn create_window(app: &tauri::AppHandle) -> Result<WebviewWindow, String> {
    let window = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html?window=meeting-prompt".into()))
        .inner_size(WIDTH, HEIGHT)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .focused(false)
        .focusable(false)
        // The prompt intentionally does not activate Vibe. On macOS, WebKit otherwise
        // discards the first click made while another app (the meeting) is active.
        .accept_first_mouse(true)
        .skip_taskbar(true)
        .transparent(true)
        .shadow(false)
        .visible(false)
        .on_page_load(|window, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let has_state = window
                    .app_handle()
                    .try_state::<MeetingPromptRuntime>()
                    .and_then(|runtime| runtime.inner.lock().ok().map(|inner| inner.logic.current.is_some()))
                    .unwrap_or(false);
                if !has_state {
                    let _ = window.hide();
                }
            }
        })
        .build()
        .map_err(|error| error.to_string())?;

    window
        .set_size(LogicalSize::new(WIDTH, HEIGHT))
        .map_err(|error| error.to_string())?;

    #[cfg(target_os = "macos")]
    {
        let panel = window.to_panel::<MeetingPromptPanel>().map_err(|error| error.to_string())?;
        panel.set_level(PanelLevel::Floating.value());
        panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
        panel.set_collection_behavior(CollectionBehavior::new().can_join_all_spaces().full_screen_auxiliary().into());
        panel.set_hides_on_deactivate(false);
        panel.set_works_when_modal(true);
        panel.set_transparent(true);
    }

    Ok(window)
}

fn ensure_window(app: &tauri::AppHandle) -> Result<(), String> {
    if app.get_webview_window(WINDOW_LABEL).is_none() {
        create_window(app)?;
    }
    Ok(())
}

fn position_window(app: &tauri::AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let cursor = window.cursor_position().map_err(|error| error.to_string())?;
    let monitor = window
        .monitor_from_point(cursor.x, cursor.y)
        .map_err(|error| error.to_string())?
        .or_else(|| window.primary_monitor().ok().flatten());
    if let Some(monitor) = monitor {
        let scale = monitor.scale_factor();
        let position = monitor.position();
        let size = monitor.size();
        let x = position.x as f64 + size.width as f64 - (WIDTH + MARGIN) * scale;
        let y = position.y as f64 + TOP_MARGIN * scale;
        window
            .set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32))
            .map_err(|error| error.to_string())?;
    } else if let Some(main) = app.get_webview_window("main") {
        let position = main.outer_position().map_err(|error| error.to_string())?;
        window
            .set_position(PhysicalPosition::new(position.x + MARGIN as i32, position.y + MARGIN as i32))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn show_window_without_focus(app: &tauri::AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let _ = window;
    let panel = app
        .get_webview_panel(WINDOW_LABEL)
        .map_err(|_| "meeting prompt panel is not initialized".to_string())?;
    app.run_on_main_thread(move || panel.order_front_regardless())
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "macos"))]
fn show_window_without_focus(_app: &tauri::AppHandle, window: &WebviewWindow) -> Result<(), String> {
    window.show().map_err(|error| error.to_string())
}

fn hide_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
}

/// Positions the window on the monitor under the cursor, shows it and sends it its content.
fn present(app: &tauri::AppHandle, window: &WebviewWindow, state: MeetingPromptPayload) -> Result<(), String> {
    position_window(app, window)?;
    show_window_without_focus(app, window)?;
    repaint_after_show(window);
    tracing::debug!(
        visible = ?window.is_visible(),
        position = ?window.outer_position(),
        size = ?window.outer_size(),
        scale = ?window.scale_factor(),
        "meeting prompt window shown"
    );
    window.emit(EVENT_NAME, state).map_err(|error| error.to_string())
}

/// WebView2 leaves a never-activated window blank when it was resized while hidden, until it is
/// resized again (MicrosoftEdge/WebView2Feedback#2983). Positioning the hidden window on a monitor
/// with another scale factor is such a resize: it happens whenever the cursor is on the other
/// screen of a mixed-DPI setup. Nudging the size once the window is visible makes it paint.
#[cfg(target_os = "windows")]
fn repaint_after_show(window: &WebviewWindow) {
    let _ = window.set_size(LogicalSize::new(WIDTH, HEIGHT + 1.0));
    let _ = window.set_size(LogicalSize::new(WIDTH, HEIGHT));
}

#[cfg(not(target_os = "windows"))]
fn repaint_after_show(_window: &WebviewWindow) {}

fn show_state(app: &tauri::AppHandle, state: MeetingPromptPayload) -> Result<(), String> {
    if !is_enabled(app) {
        return Ok(());
    }
    tracing::debug!(source = ?state.source, mode = ?state.mode, "showing meeting prompt");
    let window = match app.get_webview_window(WINDOW_LABEL) {
        Some(window) => window,
        None => create_window(app)?,
    };
    present(app, &window, state)
}

fn apply_detection(app: &tauri::AppHandle, state: MeetingState) {
    tracing::debug!(recording = state.recording, source = ?state.source, "meeting detector state changed");
    let Some(runtime) = app.try_state::<MeetingPromptRuntime>() else {
        return;
    };
    let auto_record = is_auto_record_enabled(app);
    let (changed, next, start, ending) = {
        let Ok(mut inner) = runtime.inner.lock() else {
            return;
        };
        let changed = inner.logic.detection(state.clone());
        // Bookkeeping for a recording this module started: the meeting going away starts the
        // end-of-call grace and its question; coming back within it cancels both.
        let now = Instant::now();
        let ending = inner
            .auto
            .as_mut()
            .and_then(|auto| auto.observe(state.recording, now).map(|show| (auto.source, show)));
        let start =
            auto_record && state.recording && inner.auto.is_none() && inner.logic.own_recordings == 0 && !inner.logic.dismissed;
        if let Some(source) = state.source.filter(|_| start) {
            inner.auto = Some(AutoRecording::new(source));
            inner.logic.suppress_question();
        }
        (
            changed,
            inner.logic.current.clone(),
            start.then_some(state.source).flatten(),
            ending,
        )
    };
    if let Some(source) = start {
        start_auto_recording(app, source);
        return;
    }
    match ending {
        Some((source, true)) => {
            let payload = MeetingPromptPayload {
                source,
                mode: PromptMode::Ending,
            };
            if let Err(error) = show_state(app, payload) {
                tracing::error!("could not show the end-of-call question: {error}");
            }
        }
        Some((_, false)) => hide_window(app),
        None => {}
    }
    if !changed {
        return;
    }
    match next {
        Some(state) => {
            if let Err(error) = show_state(app, state) {
                tracing::error!("could not show meeting prompt: {error}");
            }
        }
        None => hide_window(app),
    }
}

fn start_auto_recording(app: &tauri::AppHandle, source: Source) {
    tracing::info!(?source, "recording detected meeting automatically");
    let options = RecordingOptions {
        microphone: true,
        system_audio: true,
    };
    if let Err(error) = app.emit(START_RECORDING_EVENT, options) {
        tracing::error!("could not ask the main window to record: {error}");
        clear_auto(app);
        return;
    }
    let payload = MeetingPromptPayload {
        source,
        mode: PromptMode::Recording,
    };
    if let Err(error) = show_state(app, payload) {
        tracing::error!("could not show the recording notice: {error}");
    }
}

fn clear_auto(app: &tauri::AppHandle) -> Option<AutoRecording> {
    let runtime = app.try_state::<MeetingPromptRuntime>()?;
    let mut inner = runtime.inner.lock().ok()?;
    inner.auto.take()
}

/// Called on every detector poll: ends an automatic recording whose meeting is over, that never
/// started, or that has run for too long. The recording itself stays until the main window
/// reports the finish, so that its scope can still be collected.
fn tick_auto(app: &tauri::AppHandle) {
    let Some(runtime) = app.try_state::<MeetingPromptRuntime>() else {
        return;
    };
    let reason = {
        let Ok(mut inner) = runtime.inner.lock() else {
            return;
        };
        let now = Instant::now();
        let Some(auto) = inner.auto.as_mut() else {
            return;
        };
        if auto.stop_requested {
            return;
        }
        let Some(reason) = auto.stop_reason(now) else {
            return;
        };
        let started = auto.started_at.is_some();
        auto.stop_requested = true;
        if !started {
            // A recording that never opened the microphone has nothing to stop.
            inner.auto = None;
        }
        started.then_some(reason)
    };
    let Some(reason) = reason else {
        tracing::warn!("automatic recording was requested but never started");
        return;
    };
    tracing::info!("stopping automatic recording: {reason}");
    hide_window(app);
    if let Err(error) = app.emit("stop_record", ()) {
        tracing::error!("could not stop the automatic recording: {error}");
    }
}

fn start_worker(app: &tauri::AppHandle) -> Result<(), String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
    if inner.worker.is_some() {
        return Ok(());
    }
    tracing::debug!("starting meeting detector");
    let (stop, stop_receiver) = mpsc::channel();
    let worker_app = app.clone();
    let handle = std::thread::Builder::new()
        .name("meeting-prompt".into())
        .spawn(move || {
            let detector = meeting_detect::watch(POLL_INTERVAL);
            loop {
                match stop_receiver.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }
                match detector.recv_timeout(Duration::from_millis(200)) {
                    Ok(state) => apply_detection(&worker_app, state),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                tick_auto(&worker_app);
            }
            drop(detector);
        })
        .map_err(|error| error.to_string())?;
    inner.worker = Some(Worker {
        stop,
        handle: Some(handle),
    });
    Ok(())
}

fn stop_worker(app: &tauri::AppHandle) -> Result<(), String> {
    let Some(runtime) = app.try_state::<MeetingPromptRuntime>() else {
        return Ok(());
    };
    let worker = {
        let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
        // Keep Vibe's own recording count across a disable/re-enable cycle. Otherwise enabling
        // detection during an existing recording could prompt for Vibe's microphone session.
        inner.logic.reset_detection();
        // An automatic recording in flight keeps going; without the detector nobody would end it,
        // so it becomes an ordinary recording the user stops.
        inner.auto = None;
        inner.worker.take()
    };
    if let Some(worker) = worker {
        worker.stop();
    }
    hide_window(app);
    Ok(())
}

/// Install runtime state and start polling only when the persisted opt-in is enabled.
pub fn initialize(app: &tauri::AppHandle) -> Result<(), String> {
    if app.try_state::<MeetingPromptRuntime>().is_none() {
        app.manage(MeetingPromptRuntime::default());
    }
    // Build the interactive panel while Vibe is launching. It stays hidden and costs no detector
    // polling while the feature is disabled; retaining it is what lets macOS deliver clicks later
    // without activating the app or bringing the main window over the meeting.
    ensure_window(app)?;
    if is_enabled(app) {
        start_worker(app)?;
    }
    Ok(())
}

#[tauri::command]
pub fn get_meeting_detection_enabled(app: tauri::AppHandle) -> bool {
    is_enabled(&app)
}

#[tauri::command]
pub fn set_meeting_detection_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let store = app.store(STORE_FILENAME).map_err(|error| error.to_string())?;
    store.set(ENABLED_KEY, serde_json::Value::Bool(enabled));
    store.save().map_err(|error| error.to_string())?;
    if enabled {
        ensure_window(&app)?;
        start_worker(&app)
    } else {
        stop_worker(&app)?;
        Ok(())
    }
}

#[tauri::command]
pub fn get_meeting_prompt_state(app: tauri::AppHandle) -> Result<Option<MeetingPromptPayload>, String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    runtime
        .inner
        .lock()
        .map(|inner| inner.logic.current.clone())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn dismiss_meeting_prompt(app: tauri::AppHandle) -> Result<(), String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    runtime.inner.lock().map_err(|error| error.to_string())?.logic.dismiss();
    // The window may be showing the notice of an automatic recording, which is not a question
    // the logic tracks; hiding an already hidden window costs nothing.
    hide_window(&app);
    Ok(())
}

/// The user declined the recording that started on its own: stop it and throw the audio away.
#[tauri::command]
pub fn cancel_auto_recording(app: tauri::AppHandle) -> Result<(), String> {
    let cancelled = clear_auto(&app);
    if let Some(runtime) = app.try_state::<MeetingPromptRuntime>() {
        if let Ok(mut inner) = runtime.inner.lock() {
            inner.logic.suppress_question();
        }
    }
    hide_window(&app);
    let Some(auto) = cancelled else {
        return Ok(());
    };
    tracing::info!(source = ?auto.source, "automatic recording cancelled by the user");
    // Discard first, so the main window knows what the stop means before the finish arrives.
    app.emit(DISCARD_RECORDING_EVENT, ()).map_err(|error| error.to_string())?;
    if auto.started_at.is_none() {
        // Nothing is recording yet, so there is nothing to stop; the main window may still be
        // opening the microphone. Stop it the moment it reports in.
        if let Some(runtime) = app.try_state::<MeetingPromptRuntime>() {
            if let Ok(mut inner) = runtime.inner.lock() {
                inner.cancel_pending = true;
            }
        }
        return Ok(());
    }
    app.emit("stop_record", ()).map_err(|error| error.to_string())
}

/// Where the transcript of the recording that started on its own should go.
#[tauri::command]
pub fn choose_recording_scope(app: tauri::AppHandle, scope: RecordingScope) -> Result<(), String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    {
        let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
        if let Some(auto) = inner.auto.as_mut() {
            auto.scope = Some(scope);
        }
    }
    tracing::info!(?scope, "transcript destination chosen by the user");
    hide_window(&app);
    Ok(())
}

/// The call looked over but the user says it is not: keep recording until the detector sees the
/// meeting again and then end.
#[tauri::command]
pub fn continue_auto_recording(app: tauri::AppHandle) -> Result<(), String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    {
        let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
        if let Some(auto) = inner.auto.as_mut() {
            auto.keep_going();
        }
    }
    tracing::info!("automatic recording continues at the user's request");
    hide_window(&app);
    Ok(())
}

/// The user confirmed the call is over: stop now instead of waiting out the grace.
#[tauri::command]
pub fn stop_auto_recording(app: tauri::AppHandle) -> Result<(), String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    hide_window(&app);
    let started = {
        let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
        let Some(auto) = inner.auto.as_mut() else {
            return Ok(());
        };
        if auto.stop_requested {
            return Ok(());
        }
        auto.stop_requested = true;
        auto.started_at.is_some()
    };
    if !started {
        // The microphone is not even open yet: there is nothing worth keeping.
        return cancel_auto_recording(app.clone());
    }
    tracing::info!("stopping automatic recording: the user ended it");
    app.emit("stop_record", ()).map_err(|error| error.to_string())
}

/// The automatic recording that just stopped, once: the main window collects it on `record_finish`
/// to decide where the transcript goes.
#[tauri::command]
pub fn take_finished_auto_recording(app: tauri::AppHandle) -> Result<Option<FinishedAutoRecording>, String> {
    let runtime = app
        .try_state::<MeetingPromptRuntime>()
        .ok_or_else(|| "meeting prompt runtime is not initialized".to_string())?;
    let mut inner = runtime.inner.lock().map_err(|error| error.to_string())?;
    Ok(inner.finished.take())
}

#[tauri::command]
pub fn meeting_prompt_ready(window: tauri::WebviewWindow) -> Result<(), String> {
    let app = window.app_handle();
    let state = get_meeting_prompt_state(app.clone())?;
    if let Some(state) = state {
        present(app, &window, state)?;
    } else {
        window.hide().map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn recording_started(app: &tauri::AppHandle) {
    let Some(runtime) = app.try_state::<MeetingPromptRuntime>() else {
        return;
    };
    let (hide, stop) = runtime
        .inner
        .lock()
        .map(|mut inner| {
            if let Some(auto) = inner.auto.as_mut() {
                auto.started_at.get_or_insert_with(Instant::now);
            }
            let stop = std::mem::take(&mut inner.cancel_pending);
            // The question hides when a recording starts; the notice of an automatic one stays.
            (inner.logic.recording_started() && inner.auto.is_none(), stop)
        })
        .unwrap_or((false, false));
    if hide {
        hide_window(app);
    }
    if stop {
        tracing::info!("stopping the automatic recording that was cancelled before it started");
        if let Err(error) = app.emit("stop_record", ()) {
            tracing::error!("could not stop the cancelled recording: {error}");
        }
    }
}

pub fn recording_stopped(app: &tauri::AppHandle) {
    let Some(runtime) = app.try_state::<MeetingPromptRuntime>() else {
        return;
    };
    if let Ok(mut inner) = runtime.inner.lock() {
        inner.logic.recording_stopped();
        // Kept for the main window, which asks where the transcript goes once the file exists.
        if let Some(auto) = inner.auto.take() {
            inner.finished = Some(FinishedAutoRecording { scope: auto.scope });
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoom(active: bool) -> MeetingState {
        MeetingState {
            recording: active,
            source: active.then_some(Source::Zoom),
        }
    }

    #[test]
    fn dismissal_lasts_until_the_detected_session_ends() {
        let mut state = PromptLogic::default();
        state.detection(zoom(true));
        assert!(state.current.is_some());
        state.dismiss();
        state.detection(zoom(true));
        assert!(state.current.is_none());
        state.detection(zoom(false));
        state.detection(zoom(true));
        assert!(state.current.is_some());
    }

    #[test]
    fn vibe_recording_dismisses_the_whole_current_mic_session() {
        let mut state = PromptLogic::default();
        state.detection(zoom(true));
        assert!(state.recording_started());
        state.recording_stopped();
        state.detection(zoom(true));
        assert!(state.current.is_none());
        state.detection(zoom(false));
        state.detection(zoom(true));
        assert!(state.current.is_some());
    }

    #[test]
    fn an_automatic_recording_stops_only_after_the_meeting_stayed_gone_for_the_grace() {
        let t0 = Instant::now();
        let mut auto = AutoRecording::new(Source::Meet);
        assert_eq!(auto.stop_reason(t0), None);
        assert_eq!(auto.stop_reason(t0 + START_TIMEOUT), Some("the recording never started"));
        auto.started_at = Some(t0 + Duration::from_secs(1));
        assert_eq!(auto.stop_reason(t0 + START_TIMEOUT), None);
        auto.ended_since = Some(t0 + Duration::from_secs(60));
        assert_eq!(auto.stop_reason(t0 + Duration::from_secs(70)), None);
        assert_eq!(
            auto.stop_reason(t0 + Duration::from_secs(60) + END_GRACE),
            Some("the meeting ended")
        );
        auto.ended_since = None;
        assert_eq!(
            auto.stop_reason(t0 + Duration::from_secs(1) + MAX_AUTO_RECORDING),
            Some("the maximum duration was reached")
        );
    }

    #[test]
    fn the_end_of_call_question_shows_once_and_goes_away_when_the_meeting_returns() {
        let t0 = Instant::now();
        let mut auto = AutoRecording::new(Source::Meet);
        assert_eq!(auto.observe(true, t0), None);
        assert_eq!(auto.observe(false, t0), Some(true));
        assert_eq!(auto.observe(false, t0 + Duration::from_secs(1)), None);
        assert_eq!(auto.ended_since, Some(t0));
        assert_eq!(auto.observe(true, t0 + Duration::from_secs(2)), Some(false));
        assert_eq!(auto.ended_since, None);
    }

    #[test]
    fn keeping_going_after_an_apparent_end_waits_for_the_meeting_to_come_back() {
        let t0 = Instant::now();
        let mut auto = AutoRecording::new(Source::Meet);
        auto.started_at = Some(t0);
        assert_eq!(auto.observe(false, t0), Some(true));
        auto.keep_going();
        assert_eq!(auto.stop_reason(t0 + END_GRACE), None);
        assert_eq!(auto.observe(false, t0 + Duration::from_secs(5)), None);
        assert_eq!(auto.observe(true, t0 + Duration::from_secs(6)), None);
        assert_eq!(auto.observe(false, t0 + Duration::from_secs(7)), Some(true));
        assert_eq!(
            auto.stop_reason(t0 + Duration::from_secs(7) + END_GRACE),
            Some("the meeting ended")
        );
    }

    #[test]
    fn suppressing_the_question_dismisses_the_current_mic_session() {
        let mut state = PromptLogic::default();
        state.detection(zoom(true));
        assert!(state.current.is_some());
        state.suppress_question();
        assert!(state.current.is_none());
        assert!(!state.detection(zoom(true)));
        state.detection(zoom(false));
        state.detection(zoom(true));
        assert!(state.current.is_some());
    }

    #[test]
    fn meeting_detected_during_vibe_recording_is_also_suppressed_until_mic_release() {
        let mut state = PromptLogic::default();
        state.recording_started();
        state.detection(zoom(true));
        state.recording_stopped();
        assert!(state.current.is_none());
        state.detection(zoom(false));
        state.detection(zoom(true));
        assert!(state.current.is_some());
    }
}
