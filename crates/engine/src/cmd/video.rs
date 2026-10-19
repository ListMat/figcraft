//! Video playback commands: play, pause, seek, toggle, and state queries.

use figcraft_color::PaintKind;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::params::{self, num, string};
use crate::{CommandSpec, EngineError, Result, Session, bad, spec};

/// Extracts the video hash, bytes, and loop setting for the target node or explicit hash.
fn resolve_video(s: &Session, p: &Value, cmd: &str) -> Result<(String, Arc<Vec<u8>>, bool)> {
    let doc = s.doc()?;
    if let Some(hash) = string(p, "hash") {
        let v = doc.videos.get(&hash).ok_or_else(|| EngineError::Other(format!("video {hash} not found in document")))?;
        return Ok((hash, v.bytes.clone(), true));
    }

    let id = params::id(p, "id").or_else(|| s.selection().first().copied()).ok_or_else(|| bad(cmd, "select a video layer or pass `id`"))?;

    let node = doc.node(id).ok_or(EngineError::NoNode(id))?;
    for paint in &node.fills {
        if let PaintKind::Image(img) = &paint.kind
            && let Some(v_ref) = &img.video
            && let Some(v) = doc.videos.get(&v_ref.hash)
        {
            return Ok((v_ref.hash.clone(), v.bytes.clone(), v_ref.loop_playback));
        }
    }
    Err(EngineError::Other(format!("node {id} has no video fill")))
}

fn play_video(s: &mut Session, p: &Value) -> Result<Value> {
    let (hash, bytes, loop_playback) = resolve_video(s, p, "video.play")?;
    let player = s.video_playback.get_or_create(&hash, bytes, loop_playback).map_err(|e| EngineError::Other(e.to_string()))?;
    player.play();
    Ok(json!({
        "hash": hash,
        "playing": true,
        "pts": player.current_pts(),
        "duration": player.duration(),
    }))
}

fn pause_video(s: &mut Session, p: &Value) -> Result<Value> {
    let (hash, _, _) = resolve_video(s, p, "video.pause")?;
    s.video_playback.pause(&hash);
    let pts = s.video_playback.current_pts(&hash);
    Ok(json!({
        "hash": hash,
        "playing": false,
        "pts": pts,
    }))
}

fn toggle_video(s: &mut Session, p: &Value) -> Result<Value> {
    let (hash, bytes, loop_playback) = resolve_video(s, p, "video.toggle")?;
    let player = s.video_playback.get_or_create(&hash, bytes, loop_playback).map_err(|e| EngineError::Other(e.to_string()))?;
    let is_playing = player.toggle();
    Ok(json!({
        "hash": hash,
        "playing": is_playing,
        "pts": player.current_pts(),
        "duration": player.duration(),
    }))
}

fn seek_video(s: &mut Session, p: &Value) -> Result<Value> {
    let (hash, bytes, loop_playback) = resolve_video(s, p, "video.seek")?;
    let player = s.video_playback.get_or_create(&hash, bytes, loop_playback).map_err(|e| EngineError::Other(e.to_string()))?;
    let target_time = num(p, "time").unwrap_or(0.0);
    player.seek(target_time);
    s.video_playback.sync_render_frames();
    Ok(json!({
        "hash": hash,
        "pts": player.current_pts(),
        "duration": player.duration(),
    }))
}

fn video_state(s: &mut Session, p: &Value) -> Result<Value> {
    let (hash, bytes, loop_playback) = resolve_video(s, p, "video.state")?;
    let player = s.video_playback.get_or_create(&hash, bytes, loop_playback).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({
        "hash": hash,
        "playing": player.is_playing(),
        "pts": player.current_pts(),
        "duration": player.duration(),
        "hasFrame": player.sample_frame().is_some(),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec!("video.play", "Play video", "`id`: node id (or selected node), `hash`: optional video hash", false, play_video),
        spec!("video.pause", "Pause video", "`id`: node id (or selected node), `hash`: optional video hash", false, pause_video),
        spec!("video.toggle", "Toggle video play/pause", "`id`: node id (or selected node), `hash`: optional video hash", false, toggle_video),
        spec!("video.seek", "Seek video", "`time`: target timestamp in seconds, `id`: node id (or selected node)", false, seek_video),
        spec!("video.state", "Get video playback state", "`id`: node id (or selected node), `hash`: optional video hash", false, video_state),
    ]
}
