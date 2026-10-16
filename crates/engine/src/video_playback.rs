//! Native video playback manager with background worker threads, bounded ring buffer,
//! monotonic clock pacing, and Play/Pause/Seek/Loop state machines.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use figcraft_format::{FormatError, VideoStreamDecoder};
use figcraft_render::{IntSize, Pixmap};
use web_time::Instant;

/// Ring buffer capacity (number of decoded frames held in memory ahead of the playback clock).
const RING_BUFFER_CAPACITY: usize = 8;

/// Internal commands sent to the background decoder worker thread.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WorkerCommand {
    Play,
    Pause,
    Seek(f64),
    Stop,
}

/// A decoded frame ready for display with its cached `Arc<Pixmap>`.
#[derive(Clone)]
struct DecodedFrame {
    pts_seconds: f64,
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
    pixmap: Option<Arc<Pixmap>>,
}

/// Shared state between the player control interface and the background decoder thread.
struct SharedPlayerState {
    queue: VecDeque<DecodedFrame>,
    command: Option<WorkerCommand>,
    is_playing: bool,
    loop_playback: bool,
    duration_seconds: f64,
    current_pts: f64,
    play_start_instant: Option<Instant>,
    play_start_pts: f64,
    latest_frame: Option<Arc<Vec<u8>>>,
    latest_pixmap: Option<Arc<Pixmap>>,
    frame_width: u32,
    frame_height: u32,
}

/// An active video player driving a background decoding thread and a bounded ring buffer.
pub struct VideoPlayer {
    video_hash: String,
    state: Arc<(Mutex<SharedPlayerState>, Condvar)>,
    running: Arc<AtomicBool>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }
}

impl VideoPlayer {
    /// Spawns a new video player with a dedicated background decoding worker.
    pub fn new(video_hash: String, video_bytes: Arc<Vec<u8>>, loop_playback: bool) -> Result<Self, FormatError> {
        // Probe header and duration upfront
        let probe = VideoStreamDecoder::open(video_bytes.clone())?;
        let duration = probe.duration_seconds();
        let w = probe.width();
        let h = probe.height();

        let initial_state = SharedPlayerState {
            queue: VecDeque::with_capacity(RING_BUFFER_CAPACITY),
            command: None,
            is_playing: false,
            loop_playback,
            duration_seconds: duration,
            current_pts: 0.0,
            play_start_instant: None,
            play_start_pts: 0.0,
            latest_frame: None,
            latest_pixmap: None,
            frame_width: w,
            frame_height: h,
        };

        let state = Arc::new((Mutex::new(initial_state), Condvar::new()));
        let running = Arc::new(AtomicBool::new(true));

        let worker_state = state.clone();
        let worker_running = running.clone();
        let worker_bytes = video_bytes.clone();

        let builder = std::thread::Builder::new().name(format!("figcraft-video-{}", &video_hash[..8.min(video_hash.len())]));
        let worker_handle = builder.spawn(move || {
            worker_loop(worker_bytes, worker_state, worker_running);
        }).map_err(|e| FormatError::Io(format!("failed to spawn video worker: {e}")))?;

        Ok(Self {
            video_hash,
            state,
            running,
            worker_handle: Some(worker_handle),
        })
    }

    pub fn video_hash(&self) -> &str {
        &self.video_hash
    }

    pub fn play(&self) {
        let (lock, cvar) = &*self.state;
        if let Ok(mut s) = lock.lock() {
            if !s.is_playing {
                s.is_playing = true;
                s.play_start_instant = Some(Instant::now());
                s.play_start_pts = s.current_pts;
                s.command = Some(WorkerCommand::Play);
                cvar.notify_all();
            }
        }
    }

    pub fn pause(&self) {
        let (lock, cvar) = &*self.state;
        if let Ok(mut s) = lock.lock() {
            if s.is_playing {
                if let Some(start) = s.play_start_instant {
                    let elapsed = start.elapsed().as_secs_f64();
                    s.current_pts = (s.play_start_pts + elapsed).clamp(0.0, s.duration_seconds);
                }
                s.is_playing = false;
                s.play_start_instant = None;
                s.command = Some(WorkerCommand::Pause);
                cvar.notify_all();
            }
        }
    }

    pub fn toggle(&self) -> bool {
        let is_playing = self.is_playing();
        if is_playing {
            self.pause();
            false
        } else {
            self.play();
            true
        }
    }

    pub fn seek(&self, timestamp_seconds: f64) {
        let (lock, cvar) = &*self.state;
        if let Ok(mut s) = lock.lock() {
            let clamped = timestamp_seconds.clamp(0.0, s.duration_seconds);
            s.current_pts = clamped;
            if s.is_playing {
                s.play_start_instant = Some(Instant::now());
                s.play_start_pts = clamped;
            }
            s.queue.clear();
            s.latest_frame = None;
            s.latest_pixmap = None;
            s.command = Some(WorkerCommand::Seek(clamped));
            cvar.notify_all();
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let (lock, cvar) = &*self.state;
        if let Ok(mut s) = lock.lock() {
            s.command = Some(WorkerCommand::Stop);
            cvar.notify_all();
        }
    }

    pub fn is_playing(&self) -> bool {
        let (lock, _) = &*self.state;
        lock.lock().map(|s| s.is_playing).unwrap_or(false)
    }

    pub fn duration(&self) -> f64 {
        let (lock, _) = &*self.state;
        lock.lock().map(|s| s.duration_seconds).unwrap_or(0.0)
    }

    pub fn current_pts(&self) -> f64 {
        let (lock, _) = &*self.state;
        if let Ok(s) = lock.lock() {
            if s.is_playing {
                if let Some(start) = s.play_start_instant {
                    let elapsed = start.elapsed().as_secs_f64();
                    let pts = s.play_start_pts + elapsed;
                    if s.duration_seconds > 0.0 && s.loop_playback {
                        pts.rem_euclid(s.duration_seconds)
                    } else {
                        pts.min(s.duration_seconds)
                    }
                } else {
                    s.current_pts
                }
            } else {
                s.current_pts
            }
        } else {
            0.0
        }
    }

    /// Queries the latest decoded RGBA frame matching the current playback time.
    pub fn sample_frame(&self) -> Option<Arc<Vec<u8>>> {
        let (lock, _) = &*self.state;
        let s = lock.lock().ok()?;
        s.latest_frame.clone().or_else(|| s.queue.front().map(|f| f.rgba.clone()))
    }

    /// Queries the latest decoded frame as a tiny-skia Pixmap, caching the Arc<Pixmap>
    /// until the playback clock advances to a new frame.
    pub fn sample_pixmap(&self) -> Option<Arc<Pixmap>> {
        self.sample_pixmap_with_change().map(|(pm, _)| pm)
    }

    /// Queries the latest decoded frame as a tiny-skia Pixmap, and reports if the frame was updated.
    pub fn sample_pixmap_with_change(&self) -> Option<(Arc<Pixmap>, bool)> {
        let pts = self.current_pts();
        let (lock, cvar) = &*self.state;
        let mut s = lock.lock().ok()?;

        let mut updated = false;
        while let Some(front) = s.queue.front() {
            if front.pts_seconds <= pts {
                if let Some(f) = s.queue.pop_front() {
                    if let Some(pm) = f.pixmap {
                        s.latest_pixmap = Some(pm);
                    }
                    s.latest_frame = Some(f.rgba);
                    s.frame_width = f.width;
                    s.frame_height = f.height;
                    updated = true;
                }
            } else {
                break;
            }
        }

        if s.latest_pixmap.is_none() {
            if let Some(front) = s.queue.pop_front() {
                if let Some(pm) = front.pixmap {
                    s.latest_pixmap = Some(pm);
                }
                s.latest_frame = Some(front.rgba);
                s.frame_width = front.width;
                s.frame_height = front.height;
                updated = true;
            }
        }

        if updated {
            cvar.notify_one();
        }

        s.latest_pixmap.clone().map(|pm| (pm, updated))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        let (lock, _) = &*self.state;
        if let Ok(s) = lock.lock() {
            (s.frame_width, s.frame_height)
        } else {
            (0, 0)
        }
    }
}

/// Runloop for the background decoding thread.
fn worker_loop(
    video_bytes: Arc<Vec<u8>>,
    state: Arc<(Mutex<SharedPlayerState>, Condvar)>,
    running: Arc<AtomicBool>,
) {
    let mut decoder = match VideoStreamDecoder::open(video_bytes) {
        Ok(d) => d,
        Err(_) => return,
    };

    while running.load(Ordering::Relaxed) {
        let (lock, cvar) = &*state;
        let mut cmd = None;
        let is_playing;
        let queue_len;

        {
            let Ok(mut s) = lock.lock() else { break };
            if let Some(c) = s.command.take() {
                cmd = Some(c);
            }
            is_playing = s.is_playing;
            queue_len = s.queue.len();

            // Wait if not playing, or if ring buffer is full
            if cmd.is_none() && (!is_playing || queue_len >= RING_BUFFER_CAPACITY) {
                let wait_duration = if is_playing {
                    Duration::from_millis(8)
                } else {
                    Duration::from_millis(200)
                };
                let _ = cvar.wait_timeout(s, wait_duration);
                continue;
            }
        }

        // Handle commands
        if let Some(c) = cmd {
            match c {
                WorkerCommand::Stop => break,
                WorkerCommand::Seek(target_time) => {
                    let _ = decoder.seek(target_time);
                }
                WorkerCommand::Pause => continue,
                WorkerCommand::Play => {}
            }
        }

        // Decode next frame if playing and buffer has room
        if is_playing {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    let w = frame.width;
                    let h = frame.height;
                    let size = IntSize::from_wh(w, h);
                    let pixmap = size.and_then(|sz| Pixmap::from_vec((*frame.rgba).clone(), sz).map(Arc::new));
                    let Ok(mut s) = lock.lock() else { break };
                    s.queue.push_back(DecodedFrame {
                        pts_seconds: frame.pts_seconds,
                        width: w,
                        height: h,
                        rgba: frame.rgba,
                        pixmap,
                    });
                }
                Ok(None) => {
                    // Reached end of stream
                    let Ok(mut s) = lock.lock() else { break };
                    if s.loop_playback {
                        let _ = decoder.rewind();
                        s.play_start_instant = Some(Instant::now());
                        s.play_start_pts = 0.0;
                        s.current_pts = 0.0;
                    } else {
                        s.is_playing = false;
                        s.play_start_instant = None;
                    }
                }
                Err(_) => {
                    // Drop corrupted frame and continue
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

/// Global registry of video players for active video assets in an editing session.
#[derive(Default)]
pub struct VideoPlaybackManager {
    players: Mutex<HashMap<String, Arc<VideoPlayer>>>,
    tick: std::sync::atomic::AtomicU64,
}

impl VideoPlaybackManager {
    pub fn new() -> Self {
        Self {
            players: Mutex::new(HashMap::new()),
            tick: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Gets an existing player or creates a new background player for `video_hash`.
    pub fn get_or_create(
        &self,
        video_hash: &str,
        video_bytes: Arc<Vec<u8>>,
        loop_playback: bool,
    ) -> Result<Arc<VideoPlayer>, FormatError> {
        let mut map = self.players.lock().map_err(|e| FormatError::Io(e.to_string()))?;
        if let Some(player) = map.get(video_hash) {
            Ok(player.clone())
        } else {
            let player = Arc::new(VideoPlayer::new(video_hash.to_string(), video_bytes, loop_playback)?);
            map.insert(video_hash.to_string(), player.clone());
            Ok(player)
        }
    }

    pub fn play(&self, video_hash: &str) -> bool {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                player.play();
                return true;
            }
        }
        false
    }

    pub fn pause(&self, video_hash: &str) -> bool {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                player.pause();
                return true;
            }
        }
        false
    }

    pub fn toggle(&self, video_hash: &str) -> Option<bool> {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                return Some(player.toggle());
            }
        }
        None
    }

    pub fn seek(&self, video_hash: &str, timestamp_seconds: f64) -> bool {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                player.seek(timestamp_seconds);
                return true;
            }
        }
        false
    }

    pub fn is_playing(&self, video_hash: &str) -> bool {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                return player.is_playing();
            }
        }
        false
    }

    pub fn current_pts(&self, video_hash: &str) -> f64 {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                return player.current_pts();
            }
        }
        0.0
    }

    pub fn duration(&self, video_hash: &str) -> f64 {
        if let Ok(map) = self.players.lock() {
            if let Some(player) = map.get(video_hash) {
                return player.duration();
            }
        }
        0.0
    }

    pub fn stop(&self, video_hash: &str) -> bool {
        if let Ok(mut map) = self.players.lock() {
            if let Some(player) = map.remove(video_hash) {
                player.stop();
                figcraft_render::images::clear_video_frame(video_hash);
                return true;
            }
        }
        false
    }

    pub fn clear(&self) {
        if let Ok(mut map) = self.players.lock() {
            for (hash, player) in map.drain() {
                player.stop();
                figcraft_render::images::clear_video_frame(&hash);
            }
        }
    }

    /// Ensures that all video assets present in the document have an active player spawned
    /// and playing (looping).
    pub fn ensure_document_videos(&self, doc: &figcraft_doc::Document) {
        for (hash, data) in &doc.videos {
            if let Ok(player) = self.get_or_create(hash, data.bytes.clone(), true) {
                if !player.is_playing() {
                    player.play();
                }
            }
        }
    }

    /// Synchronizes active playback frames with the render engine's video frame registry.
    /// Returns true if any video is actively playing.
    pub fn sync_render_frames(&self) -> bool {
        let map = match self.players.lock() {
            Ok(m) => m,
            Err(_) => return false,
        };

        let mut any_playing = false;
        let mut any_updated = false;
        for (hash, player) in map.iter() {
            if player.is_playing() {
                any_playing = true;
            }
            if let Some((pixmap, changed)) = player.sample_pixmap_with_change() {
                if changed {
                    figcraft_render::images::set_video_frame(hash, pixmap);
                    any_updated = true;
                }
            }
        }
        if any_updated {
            self.tick.fetch_add(1, Ordering::Relaxed);
        }
        any_playing
    }

    pub fn tick(&self) -> u64 {
        self.tick.load(Ordering::Relaxed)
    }
}
