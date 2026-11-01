//! The dictation loop: hotkey down -> record (model loads in parallel) ->
//! hotkey up -> transcribe and clean up on a worker thread -> paste. Runs on
//! one thread that sleeps on a channel, so idle CPU is zero. The model is
//! dropped after `keep_model_mins` of no use.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::audio::{self, Recorder, TARGET_RATE};
use crate::dictionary::Entry;
use crate::hotkey::{self, HotkeyEvent};
use crate::pill::{self, PillState};
use crate::settings::Settings;
use crate::{cleanup, clipboard, context, learn, model, mute, secret, smart, sounds, stats, transcripts};

/// Keep recording briefly after release; people let go on the last syllable.
const TAIL: Duration = Duration::from_millis(150);
/// Shorter recordings are treated as accidental taps.
const MIN_SECS: f32 = 0.3;
/// In hands-free mode, a press released sooner than this is a tap that
/// keeps listening; a longer press is an ordinary hold.
const TAP_MAX: Duration = Duration::from_millis(350);
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How long the Settings mic test keeps the mic open.
const MIC_TEST: Duration = Duration::from_secs(6);
/// Below this peak level the mic is effectively silent (muted mic).
const SILENCE_PEAK: f32 = 0.003;
const HISTORY_LEN: usize = 20;

pub enum Msg {
    Hotkey(HotkeyEvent),
    Transcribed(Result<Transcript, String>),
    /// Start (true) or stop (false) the Settings mic level test.
    MicTest(bool),
    /// The pill was clicked during hands-free: finish and paste.
    StopHandsFree,
    /// Settings changed; recompute timers.
    Wake,
}

pub struct Transcript {
    /// Straight from the speech model.
    pub raw: String,
    /// After cleanup; this is what gets pasted.
    pub text: String,
    pub app: Option<String>,
    pub audio_secs: f32,
    /// Resample plus speech recognition.
    pub took: Duration,
    /// How long the Smart pass took, when it ran (even if it fell back).
    pub smart_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Idle,
    Recording,
    Transcribing,
    NeedsModel,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: u64,
    pub text: String,
    /// Before cleanup, for "copy original".
    pub raw: String,
    pub app: String,
    pub at: u64,
    pub audio_secs: f32,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub active: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

pub struct Shared {
    pub settings: RwLock<Settings>,
    pub dictionary: RwLock<Vec<Entry>>,
    pub status: Mutex<Status>,
    pub history: Mutex<VecDeque<HistoryItem>>,
    pub download: Mutex<Download>,
    pub tx: Sender<Msg>,
    model: Mutex<Option<ParakeetModel>>,
    loading: AtomicBool,
}

fn load_model() -> Result<ParakeetModel, String> {
    let t = Instant::now();
    let m = ParakeetModel::load(&model::dir(), &Quantization::Int8).map_err(|e| e.to_string())?;
    log::info!("model loaded in {} ms", t.elapsed().as_millis());
    Ok(m)
}

impl Shared {
    pub fn new(settings: Settings, dictionary: Vec<Entry>, tx: Sender<Msg>) -> Self {
        Self {
            settings: RwLock::new(settings),
            dictionary: RwLock::new(dictionary),
            status: Mutex::new(Status::Idle),
            history: Mutex::new(VecDeque::new()),
            download: Mutex::new(Download::default()),
            tx,
            model: Mutex::new(None),
            loading: AtomicBool::new(false),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Starts loading the model in the background while the user speaks.
    fn preload(self: &Arc<Self>) {
        if self.loading.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = self.clone();
        std::thread::spawn(move || {
            {
                let mut slot = shared.model.lock().unwrap();
                if slot.is_none() {
                    match load_model() {
                        Ok(m) => *slot = Some(m),
                        Err(e) => log::error!("model load failed: {e}"),
                    }
                }
            }
            shared.loading.store(false, Ordering::SeqCst);
        });
    }

    fn transcribe(&self, samples: &[f32]) -> Result<String, String> {
        let mut slot = self.model.lock().unwrap();
        if slot.is_none() {
            *slot = Some(load_model()?);
        }
        let model = slot.as_mut().expect("model loaded above");
        let mut parts = Vec::new();
        for chunk in audio::split_for_asr(samples, 60.0) {
            let r = model.transcribe_with(chunk, &ParakeetParams::default()).map_err(|e| e.to_string())?;
            let text = r.text.trim();
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
        Ok(parts.join(" "))
    }

    fn unload(&self) {
        if let Ok(mut slot) = self.model.try_lock() {
            if slot.take().is_some() {
                log::info!("model unloaded");
            }
        }
    }

    pub fn model_loaded(&self) -> bool {
        self.model.try_lock().map(|s| s.is_some()).unwrap_or(true)
    }
}

/// Light, then the Smart pass when the text has a cue for it. Any failure
/// (no key, offline, slow, odd reply) pastes the Light text instead.
fn smart_clean(raw: &str, ctx: &cleanup::Context) -> (String, Option<u64>) {
    let light = cleanup::light(raw, ctx);
    if !smart::needs(&light, &ctx.dictionary) || !secret::is_set() {
        return (cleanup::finish(&light, ctx), None);
    }
    let t = Instant::now();
    let result = smart::fix(&light, ctx.app.as_deref(), &ctx.dictionary);
    let ms = t.elapsed().as_millis() as u64;
    match result {
        Ok(fixed) => {
            log::info!("smart pass in {ms} ms");
            (cleanup::finish(&fixed, ctx), Some(ms))
        }
        Err(e) => {
            log::warn!("smart pass failed after {ms} ms, pasting Light: {e}");
            (cleanup::finish(&light, ctx), Some(ms))
        }
    }
}

pub fn set_status(app: &AppHandle, shared: &Shared, status: Status) {
    {
        let mut cur = shared.status.lock().unwrap();
        if *cur == status {
            return;
        }
        *cur = status;
    }
    crate::tray::show_status(app, status, &shared.settings());
    crate::emit_state(app);
}

struct Engine {
    app: AppHandle,
    shared: Arc<Shared>,
    recording: Option<(Recorder, Instant)>,
    mic_test: Option<(Recorder, Instant)>,
    /// App that had focus when the hotkey went down.
    focused_app: Option<String>,
    muted: Option<mute::Muted>,
    muted_count: u32,
    /// Listening after a tap in hands-free mode; the next press (or a pill
    /// click) finishes it.
    hands_free: bool,
    pending: usize,
    last_used: Instant,
}

impl Engine {
    fn idle_status(&self) -> Status {
        if self.recording.is_some() {
            Status::Recording
        } else if self.pending > 0 {
            Status::Transcribing
        } else if !model::is_installed() {
            Status::NeedsModel
        } else {
            Status::Idle
        }
    }

    fn refresh_status(&self) {
        set_status(&self.app, &self.shared, self.idle_status());
    }

    fn timeout(&self) -> Duration {
        let mut wait = Duration::from_secs(3600);
        if let Some((_, started)) = &self.recording {
            wait = wait.min(MAX_RECORDING.saturating_sub(started.elapsed()));
        }
        if let Some((_, started)) = &self.mic_test {
            wait = wait.min(MIC_TEST.saturating_sub(started.elapsed()));
        }
        let keep = self.shared.settings.read().unwrap().keep_model_mins;
        if keep > 0 && self.pending == 0 && self.recording.is_none() && self.shared.model_loaded() {
            let deadline = Duration::from_secs(keep as u64 * 60);
            wait = wait.min(deadline.saturating_sub(self.last_used.elapsed()));
        }
        wait.max(Duration::from_millis(50))
    }

    fn on_tick(&mut self) {
        if self.recording.as_ref().is_some_and(|(_, s)| s.elapsed() >= MAX_RECORDING) {
            log::info!("hit the {} min limit, stopping", MAX_RECORDING.as_secs() / 60);
            self.finish();
        }
        if self.mic_test.as_ref().is_some_and(|(_, s)| s.elapsed() >= MIC_TEST) {
            self.mic_test = None;
        }
        let keep = self.shared.settings.read().unwrap().keep_model_mins;
        let expired = keep >= 0 && self.last_used.elapsed() >= Duration::from_secs(keep as u64 * 60);
        if expired && self.pending == 0 && self.recording.is_none() {
            self.shared.unload();
        }
    }

    fn on_mic_test(&mut self, on: bool) {
        self.mic_test = None;
        if on && self.recording.is_none() {
            let mic = self.shared.settings().mic;
            match Recorder::start(mic.as_deref()) {
                Ok(r) => self.mic_test = Some((r, Instant::now())),
                Err(e) => log::warn!("mic test: {e}"),
            }
        }
    }

    fn on_pressed(&mut self) {
        if self.recording.is_some() {
            if self.hands_free {
                self.finish();
            }
            return;
        }
        self.mic_test = None;
        if !model::is_installed() {
            pill::set(PillState::Message { text: "Still setting up".into(), error: false, ttl_ms: Some(2500) });
            let app = self.app.clone();
            let _ = self.app.run_on_main_thread(move || crate::open_app(&app, None));
            return;
        }
        self.focused_app = context::foreground_exe();
        let s = self.shared.settings();
        let t = Instant::now();
        match Recorder::start(s.mic.as_deref()) {
            Ok(r) => {
                log::info!("mic open in {} ms", t.elapsed().as_millis());
                self.recording = Some((r, Instant::now()));
            }
            Err(e) => {
                log::error!("could not open mic: {e}");
                pill::set(PillState::Message { text: "No microphone".into(), error: true, ttl_ms: Some(5000) });
                return;
            }
        }
        self.muted_count = 0;
        pill::set(PillState::Listening { muted: 0, hands_free: false });
        self.shared.preload();
        if s.cleanup == "smart" && secret::is_set() {
            smart::warm_up();
        }
        if s.sounds {
            sounds::start();
        }
        if hotkey::combo_uses_win(&s.hotkey) {
            hotkey::suppress_start_menu();
        }
        if s.mute_while_dictating {
            let muted = mute::mute_others();
            self.muted_count = muted.count() as u32;
            if self.muted_count > 0 {
                pill::set(PillState::Listening { muted: self.muted_count, hands_free: false });
            }
            self.muted = Some(muted);
        }
        self.refresh_status();
    }

    fn stop_recording(&mut self) -> Option<(Vec<f32>, u32)> {
        let (rec, _) = self.recording.take()?;
        let out = rec.stop();
        if let Some(m) = self.muted.take() {
            m.restore();
        }
        Some(out)
    }

    fn on_released(&mut self) {
        let Some((_, started)) = &self.recording else { return };
        if self.hands_free {
            return;
        }
        if self.shared.settings().hands_free_mode && started.elapsed() < TAP_MAX {
            // A tap: keep listening until the next press.
            self.hands_free = true;
            hotkey::HANDS_FREE.store(true, Ordering::Relaxed);
            pill::set(PillState::Listening { muted: self.muted_count, hands_free: true });
            return;
        }
        self.finish();
    }

    fn reset_hands_free(&mut self) {
        self.hands_free = false;
        hotkey::HANDS_FREE.store(false, Ordering::Relaxed);
    }

    /// Stops without transcribing (Esc, or another key joined the chord).
    fn discard(&mut self) {
        self.reset_hands_free();
        if self.stop_recording().is_some() {
            pill::set(PillState::Idle);
            self.refresh_status();
        }
    }

    /// Stops recording and sends the audio off to be transcribed.
    fn finish(&mut self) {
        self.reset_hands_free();
        if self.recording.is_none() {
            return;
        }
        std::thread::sleep(TAIL);
        let Some((mut samples, mut rate)) = self.stop_recording() else { return };
        // End-to-end tests swap the mic audio for a known 16 kHz recording.
        if let Some(wav) = std::env::var_os("OFFHAND_TEST_WAV") {
            if let Ok(s) = transcribe_rs::audio::read_wav_samples(std::path::Path::new(&wav)) {
                (samples, rate) = (s, TARGET_RATE);
            }
        }
        let s = self.shared.settings();
        if s.sounds {
            sounds::stop();
        }
        let secs = samples.len() as f32 / rate as f32;
        if secs < MIN_SECS {
            pill::set(PillState::Idle);
            self.refresh_status();
            return;
        }
        pill::set(PillState::Writing);
        self.pending += 1;
        self.refresh_status();
        let shared = self.shared.clone();
        let app = self.focused_app.clone();
        let light = s.cleanup != "off";
        let smart = s.cleanup == "smart";
        let dictionary = self.shared.dictionary.read().unwrap().clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let audio = audio::resample(&samples, rate, TARGET_RATE);
            let raw = if audio::peak(&audio) < SILENCE_PEAK { Ok(String::new()) } else { shared.transcribe(&audio) };
            let took = t.elapsed();
            let result = raw.map(|raw| {
                let ctx = cleanup::Context { app: app.clone(), light, dictionary, known: Some(crate::spell::known) };
                let (text, smart_ms) = if smart { smart_clean(&raw, &ctx) } else { (cleanup::clean(&raw, &ctx), None) };
                Transcript { raw, text, app, audio_secs: secs, took, smart_ms }
            });
            let _ = shared.tx.send(Msg::Transcribed(result));
        });
    }

    fn on_cancelled(&mut self) {
        if !self.hands_free {
            self.discard();
        }
    }

    fn on_escape(&mut self) {
        if self.hands_free {
            log::info!("hands-free cancelled");
            self.discard();
        }
    }

    fn on_transcribed(&mut self, result: Result<Transcript, String>) {
        self.pending = self.pending.saturating_sub(1);
        self.last_used = Instant::now();
        let s = self.shared.settings();
        match result {
            Ok(t) if !t.text.is_empty() => {
                let took_ms = t.took.as_millis() as u64;
                log::info!("{:.1}s of audio -> {} chars in {took_ms} ms", t.audio_secs, t.text.chars().count());
                let mut out = t.text.clone();
                if s.trailing_space {
                    out.push(' ');
                }
                clipboard::paste(&out);
                pill::set(PillState::Pasted);
                if s.learn_corrections {
                    learn::watch(&t.text);
                }
                stats::record(t.text.split_whitespace().count() as u64, t.audio_secs);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                let (at, id) = (now.as_secs(), now.as_millis() as u64);
                if s.keep_transcripts {
                    transcripts::append(&transcripts::Entry {
                        id,
                        at,
                        app: t.app.clone(),
                        audio_secs: t.audio_secs,
                        asr_ms: took_ms,
                        smart_ms: t.smart_ms,
                        cleaned: (t.text != t.raw).then(|| t.text.clone()),
                        raw: t.raw.clone(),
                    });
                }
                let mut history = self.shared.history.lock().unwrap();
                history.push_front(HistoryItem {
                    id,
                    text: t.text,
                    raw: t.raw,
                    app: context::display_name(t.app.as_deref()),
                    at,
                    audio_secs: t.audio_secs,
                    took_ms,
                });
                history.truncate(HISTORY_LEN);
            }
            Ok(t) => {
                log::info!("{:.1}s of audio, nothing heard", t.audio_secs);
                pill::set(PillState::Message { text: "Didn't catch that".into(), error: false, ttl_ms: Some(1600) });
            }
            Err(e) => {
                log::error!("transcription failed: {e}");
                pill::set(PillState::Message { text: "Couldn't transcribe".into(), error: true, ttl_ms: Some(4000) });
            }
        }
        if s.keep_model_mins == 0 && self.pending == 0 && self.recording.is_none() {
            self.shared.unload();
        }
        self.refresh_status();
        crate::emit_state(&self.app);
    }
}

/// `offhand --bench file.wav` (16 kHz mono 16-bit): times model load and three
/// transcriptions, for checking speed on a given machine.
pub fn bench(wav: &str) {
    let samples = match transcribe_rs::audio::read_wav_samples(std::path::Path::new(wav)) {
        Ok(s) => s,
        Err(e) => return println!("can't read {wav}: {e}"),
    };
    let t = Instant::now();
    let mut model = match load_model() {
        Ok(m) => m,
        Err(e) => return println!("model load failed: {e}"),
    };
    println!("model load: {} ms", t.elapsed().as_millis());
    let secs = samples.len() as f32 / TARGET_RATE as f32;
    for run in 1..=3 {
        let t = Instant::now();
        match model.transcribe_with(&samples, &ParakeetParams::default()) {
            Ok(r) => println!("run {run}: {secs:.1}s audio in {} ms: {}", t.elapsed().as_millis(), r.text.trim()),
            Err(e) => println!("run {run} failed: {e}"),
        }
    }
}

pub fn run(app: AppHandle, shared: Arc<Shared>, rx: Receiver<Msg>) {
    // cpal opens the mic on this thread and expects an STA.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    mute::recover_after_crash();
    let mut engine = Engine {
        app,
        shared,
        recording: None,
        mic_test: None,
        focused_app: None,
        muted: None,
        muted_count: 0,
        hands_free: false,
        pending: 0,
        last_used: Instant::now(),
    };
    engine.refresh_status();
    loop {
        match rx.recv_timeout(engine.timeout()) {
            Ok(Msg::Hotkey(HotkeyEvent::Pressed)) => engine.on_pressed(),
            Ok(Msg::Hotkey(HotkeyEvent::Released)) => engine.on_released(),
            Ok(Msg::Hotkey(HotkeyEvent::Cancelled)) => engine.on_cancelled(),
            Ok(Msg::Hotkey(HotkeyEvent::Escape)) => engine.on_escape(),
            Ok(Msg::StopHandsFree) => engine.finish(),
            Ok(Msg::Transcribed(result)) => engine.on_transcribed(result),
            Ok(Msg::MicTest(on)) => engine.on_mic_test(on),
            Ok(Msg::Wake) => engine.refresh_status(),
            Err(RecvTimeoutError::Timeout) => engine.on_tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
