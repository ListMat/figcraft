//! Video asset tests: storage in `videos/<hash>`, poster frame extraction, hash verification,
//! error reporting for damaged members, and backwards compatibility with image readers.

use figcraft_doc::color::{ImageScaleMode, Paint, PaintKind};
use figcraft_doc::{Document, NodeKind};
use std::io::Write;
use std::path::Path;

use super::*;

fn load_test_video() -> Option<Vec<u8>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let video_path = Path::new(manifest_dir).join("../../audits/spike/assets/test_720p.mp4");
    std::fs::read(&video_path).ok()
}

fn load_corrupted_video() -> Option<Vec<u8>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let video_path = Path::new(manifest_dir).join("../../audits/spike/assets/corrupted.mp4");
    std::fs::read(&video_path).ok()
}

fn rebuild_archive(bytes: &[u8], edit: impl Fn(&str, Vec<u8>) -> Option<Vec<u8>>) -> Vec<u8> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = z.file_names().map(str::to_string).collect();
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut w = zip::ZipWriter::new(&mut buf);
    for n in names {
        let data = read_member(&mut z, &n, u64::MAX).unwrap();
        if let Some(data) = edit(&n, data) {
            w.start_file(n, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            w.write_all(&data).unwrap();
        }
    }
    w.finish().unwrap();
    buf.into_inner()
}

#[test]
fn existing_documents_without_video_have_no_video_entries_and_roundtrip_identically() {
    let mut d = Document::new("NoVideo");
    let p = d.first_page().unwrap();
    d.create(p, NodeKind::Rectangle, "R", 10.0, 10.0, 100.0, 50.0).unwrap();

    let bytes = save_bytes(&d, None).unwrap();

    // Verify zip contains no videos/ entries
    let z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let has_video = z.file_names().any(|n| n.starts_with("videos/"));
    assert!(!has_video, "archive must not contain videos/ folder when no video is present");

    // Verify roundtrip
    let (loaded, issues) = load_bytes(&bytes).unwrap();
    assert!(issues.is_empty(), "clean document should have no issues: {issues:?}");
    assert!(loaded.videos.is_empty(), "loaded.videos must be empty");
    assert_eq!(loaded.node_count(), d.node_count());
}

#[test]
fn video_metadata_and_poster_extraction_works_on_valid_mp4() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let (w, h, duration, poster_png) = extract_video_metadata_and_poster(&video_bytes).unwrap();

    assert_eq!(w, 1280);
    assert_eq!(h, 720);
    assert!(duration > 0.0, "duration should be positive");

    // Poster PNG should be valid and decodable
    let img = image::load_from_memory(&poster_png).expect("poster PNG must decode");
    assert_eq!(img.width(), 1280);
    assert_eq!(img.height(), 720);
}

#[test]
fn video_asset_save_and_reopen_preserves_bytes_and_metadata() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let mut d = Document::new("VideoDoc");
    let p = d.first_page().unwrap();

    let (v_hash, poster_hash, w, h, duration) = add_video(&mut d, video_bytes.clone()).unwrap();

    assert_eq!(w, 1280);
    assert_eq!(h, 720);
    assert!(duration > 0.0);
    assert!(d.videos.contains_key(&v_hash));
    assert!(d.images.contains_key(&poster_hash));

    // Create a node with Video paint in fills
    let id = d.create(p, NodeKind::Rectangle, "VideoNode", 0.0, 0.0, 1280.0, 720.0).unwrap();
    let node = d.node_mut(id).unwrap();
    node.fills = vec![Paint::video(&poster_hash, &v_hash, ImageScaleMode::Fill)];

    let bytes = save_bytes(&d, None).unwrap();

    // Inspect archive
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let video_entry_name = format!("videos/{v_hash}");
    let poster_entry_name = format!("images/{poster_hash}");

    let mut read_video = Vec::new();
    {
        let mut video_file = z.by_name(&video_entry_name).expect("videos/<hash> must exist in archive");
        assert_eq!(video_file.compression(), zip::CompressionMethod::Stored, "videos must be stored uncompressed");
        std::io::Read::read_to_end(&mut video_file, &mut read_video).unwrap();
    }
    assert_eq!(read_video, video_bytes);

    assert!(z.by_name(&poster_entry_name).is_ok(), "images/<poster_hash> must exist in archive");

    // Reopen document
    let (loaded, issues) = load_bytes(&bytes).unwrap();
    assert!(issues.is_empty(), "clean load with video should have no issues: {issues:?}");
    assert!(loaded.videos.contains_key(&v_hash), "loaded document must have video entry");
    assert!(loaded.images.contains_key(&poster_hash), "loaded document must have poster image entry");

    let loaded_video = loaded.videos.get(&v_hash).unwrap();
    assert_eq!(*loaded_video.bytes, video_bytes);
    assert_eq!(loaded_video.width, 1280);
    assert_eq!(loaded_video.height, 720);

    let loaded_node = loaded.node(id).unwrap();
    assert_eq!(loaded_node.fills.len(), 1);
    let paint = &loaded_node.fills[0];
    match &paint.kind {
        PaintKind::Image(img) => {
            assert_eq!(img.image, poster_hash);
            let v_ref = img.video.as_ref().expect("paint must have video ref");
            assert_eq!(v_ref.hash, v_hash);
            assert!(v_ref.loop_playback);
            assert!(v_ref.autoplay);
            assert!(!v_ref.muted);
            assert_eq!(v_ref.volume, 1.0);
        }
        _ => panic!("expected PaintKind::Image"),
    }
}

#[test]
fn asset_hash_verification_detects_tampering() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let mut d = Document::new("TamperTest");
    let (v_hash, _, _, _, _) = add_video(&mut d, video_bytes).unwrap();

    let bytes = save_bytes(&d, None).unwrap();

    // Tamper with one byte in the video payload
    let tampered_bytes = rebuild_archive(&bytes, |n, mut data| {
        if n == format!("videos/{v_hash}") {
            let mid = data.len() / 2;
            data[mid] ^= 0xff;
        }
        Some(data)
    });

    let (loaded, issues) = load_bytes(&tampered_bytes).unwrap();
    assert!(!loaded.videos.contains_key(&v_hash), "tampered video must not be kept in doc.videos");
    assert!(issues.iter().any(|i| i.contains(&v_hash) && i.contains("hash mismatch")), "issues should note hash mismatch: {issues:?}");
}

#[test]
fn missing_video_asset_is_reported_without_crashing() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let mut d = Document::new("MissingVideoTest");
    let (v_hash, poster_hash, _, _, _) = add_video(&mut d, video_bytes).unwrap();

    let bytes = save_bytes(&d, None).unwrap();

    // Drop the video payload from the archive
    let missing_video_bytes = rebuild_archive(&bytes, |n, data| if n.starts_with("videos/") { None } else { Some(data) });

    let (loaded, issues) = load_bytes(&missing_video_bytes).unwrap();
    assert!(!loaded.videos.contains_key(&v_hash), "missing video must not be in doc.videos");
    assert!(loaded.images.contains_key(&poster_hash), "poster frame must still be present for rendering fallback");
    assert!(issues.iter().any(|i| i.contains(&v_hash) && i.contains("missing")), "issues should report missing video: {issues:?}");
}

#[test]
fn corrupted_or_unsupported_media_fails_gracefully() {
    let mut d = Document::new("CorruptTest");

    // Invalid random bytes
    let res = add_video(&mut d, b"not a real video".to_vec());
    assert!(res.is_err(), "invalid bytes must return Err");

    // Truncated / corrupted MP4
    if let Some(corrupted) = load_corrupted_video() {
        let res = add_video(&mut d, corrupted);
        assert!(res.is_err(), "corrupted MP4 must return Err");
    }
}

#[test]
fn legacy_reader_compatibility() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let mut d = Document::new("LegacyCompat");
    let p = d.first_page().unwrap();
    let (v_hash, poster_hash, _, _, _) = add_video(&mut d, video_bytes).unwrap();

    let id = d.create(p, NodeKind::Rectangle, "R", 0.0, 0.0, 400.0, 300.0).unwrap();
    d.node_mut(id).unwrap().fills = vec![Paint::video(&poster_hash, &v_hash, ImageScaleMode::Fill)];

    let bytes = save_bytes(&d, None).unwrap();

    // Verify a reader that only accesses images and ignores video
    let (loaded, _) = load_bytes(&bytes).unwrap();
    let node = loaded.node(id).unwrap();
    match &node.fills[0].kind {
        PaintKind::Image(img) => {
            assert_eq!(img.image, poster_hash);
        }
        _ => panic!("expected PaintKind::Image"),
    }
    assert!(loaded.images.contains_key(&poster_hash));
}

#[test]
fn stream_decoder_decodes_frames_and_seeks() {
    let Some(video_bytes) = load_test_video() else {
        return;
    };
    let video_bytes = Arc::new(video_bytes);
    let mut decoder = VideoStreamDecoder::open(video_bytes).expect("stream decoder should open MP4");

    assert_eq!(decoder.width(), 1280);
    assert_eq!(decoder.height(), 720);
    assert!(decoder.sample_count() > 0);
    assert!(decoder.duration_seconds() > 0.0);
    assert!(!decoder.keyframes().is_empty());
    assert_eq!(decoder.keyframes()[0], 1); // first frame is keyframe

    // Decode first frame
    let frame0 = decoder.next_frame().unwrap().expect("first frame must decode");
    assert_eq!(frame0.width, 1280);
    assert_eq!(frame0.height, 720);
    assert_eq!(frame0.pts_seconds, 0.0);
    assert_eq!(frame0.rgba.len(), 1280 * 720 * 4);

    // Decode second frame
    let frame1 = decoder.next_frame().unwrap().expect("second frame must decode");
    assert!(frame1.pts_seconds > 0.0);

    // Test seek to 2.0 seconds
    let seek_frame = decoder.seek(2.0).unwrap().expect("seek should yield a frame");
    assert!(seek_frame.pts_seconds >= 1.5 && seek_frame.pts_seconds <= 2.5);

    // Rewind back to beginning
    decoder.rewind().unwrap();
    assert_eq!(decoder.current_sample(), 1);
    let rewound_frame = decoder.next_frame().unwrap().expect("rewound frame must decode");
    assert_eq!(rewound_frame.pts_seconds, 0.0);
}

#[test]
fn video_export_mp4_encodes_and_decodes_back_roundtrip() {
    let w = 64u32;
    let h = 64u32;
    let fps = 10u32;
    let total_frames = 10usize;

    let mp4_bytes = export_mp4(w, h, fps, total_frames, |frame_idx| {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let color = if frame_idx % 2 == 0 { [255, 0, 0, 255] } else { [0, 255, 0, 255] };
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        Ok(rgba)
    })
    .expect("mp4 export must succeed");

    assert!(!mp4_bytes.is_empty(), "mp4 bytes must not be empty");

    // Roundtrip verification: load the generated mp4 with VideoStreamDecoder
    let mut decoder = VideoStreamDecoder::open(Arc::new(mp4_bytes)).expect("exported mp4 must be readable by VideoStreamDecoder");
    assert_eq!(decoder.width(), w);
    assert_eq!(decoder.height(), h);
    assert_eq!(decoder.sample_count(), total_frames as u32);

    let first = decoder.next_frame().unwrap().expect("first frame must decode");
    assert_eq!(first.width, w);
    assert_eq!(first.height, h);
    assert_eq!(first.rgba.len(), (w * h * 4) as usize);
}

#[test]
fn video_export_gif_encodes_valid_looping_gif() {
    let w = 32u32;
    let h = 32u32;
    let fps = 5u32;
    let total_frames = 5usize;

    let gif_bytes = export_gif(w, h, fps, total_frames, |frame_idx| {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let color = if frame_idx % 2 == 0 { [0, 0, 255, 255] } else { [255, 255, 0, 255] };
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        Ok(rgba)
    })
    .expect("gif export must succeed");

    assert!(!gif_bytes.is_empty(), "gif bytes must not be empty");
    let img = image::load_from_memory(&gif_bytes).expect("exported gif must be valid image");
    assert_eq!(img.width(), w);
    assert_eq!(img.height(), h);
}
