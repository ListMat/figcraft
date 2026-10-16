//! Sequential video stream decoder with keyframe indexing, seeking, and YUV420-to-RGBA conversion.

use std::sync::Arc;
use crate::FormatError;

/// An uncompressed, decoded video frame ready for presentation.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    /// Presentation timestamp in seconds from the start of the video.
    pub pts_seconds: f64,
    pub width: u32,
    pub height: u32,
    /// RGBA pixels (4 bytes per pixel, premultiplied).
    pub rgba: Arc<Vec<u8>>,
}

/// Metadata for a single sample (frame) in the MP4 container.
#[derive(Clone, Copy, Debug)]
pub struct SampleMeta {
    pub index: u32,
    pub pts_seconds: f64,
    pub is_keyframe: bool,
}

/// Stream decoder for H.264 video tracks in MP4 containers.
pub struct VideoStreamDecoder {
    _bytes: Arc<Vec<u8>>,
    reader: mp4::Mp4Reader<std::io::Cursor<Arc<[u8]>>>,
    track_id: u32,
    width: u32,
    height: u32,
    timescale: u32,
    duration_seconds: f64,
    fps: f64,
    sample_count: u32,
    sps: Vec<u8>,
    pps: Vec<u8>,
    samples: Vec<SampleMeta>,
    keyframes: Vec<u32>,
    current_sample: u32,
    decoder: rusty_h264::Decoder,
}

impl VideoStreamDecoder {
    /// Opens an MP4 video bitstream for streaming and seeking.
    pub fn open(bytes: Arc<Vec<u8>>) -> Result<Self, FormatError> {
        let slice_arc: Arc<[u8]> = bytes.as_slice().into();
        let cursor = std::io::Cursor::new(slice_arc);
        let mut mp4 = mp4::Mp4Reader::read_header(cursor, bytes.len() as u64)
            .map_err(|e| FormatError::Corrupt(format!("failed to read MP4 header: {e}")))?;

        let mut video_track_id = None;
        let mut track_w = 0;
        let mut track_h = 0;
        let mut timescale = 1000;

        for track in mp4.tracks().values() {
            if track.track_type().map_err(|e| FormatError::Corrupt(e.to_string()))? == mp4::TrackType::Video {
                video_track_id = Some(track.track_id());
                track_w = track.width() as u32;
                track_h = track.height() as u32;
                timescale = track.timescale();
                break;
            }
        }

        let track_id = video_track_id.ok_or_else(|| FormatError::Corrupt("no video track found in MP4".into()))?;
        let sample_count = mp4.sample_count(track_id).map_err(|e| FormatError::Corrupt(e.to_string()))?;
        if sample_count == 0 {
            return Err(FormatError::Corrupt("video contains no samples".into()));
        }
        let duration_seconds = mp4.duration().as_secs_f64();
        let fps = if duration_seconds > 0.0 {
            f64::from(sample_count) / duration_seconds
        } else {
            30.0
        };

        let (sps, pps) = {
            let track = mp4.tracks().get(&track_id).ok_or_else(|| FormatError::Corrupt("missing video track".into()))?;
            let stsd = track
                .trak
                .mdia
                .minf
                .stbl
                .stsd
                .avc1
                .as_ref()
                .ok_or_else(|| FormatError::Corrupt("unsupported video codec: only H.264/AVC is supported".into()))?;

            if stsd.avcc.sequence_parameter_sets.is_empty() || stsd.avcc.picture_parameter_sets.is_empty() {
                return Err(FormatError::Corrupt("missing SPS or PPS in AVC configuration".into()));
            }

            let s = stsd.avcc.sequence_parameter_sets.first().map(|s| s.bytes.clone()).ok_or_else(|| FormatError::Corrupt("missing SPS".into()))?;
            let p = stsd.avcc.picture_parameter_sets.first().map(|p| p.bytes.clone()).ok_or_else(|| FormatError::Corrupt("missing PPS".into()))?;
            (s, p)
        };

        let mut samples = Vec::with_capacity(sample_count as usize);
        let mut keyframes = Vec::new();

        for i in 1..=sample_count {
            if let Ok(Some(s)) = mp4.read_sample(track_id, i) {
                let pts_seconds = if timescale > 0 {
                    s.start_time as f64 / f64::from(timescale)
                } else {
                    f64::from(i - 1) / fps
                };
                if s.is_sync {
                    keyframes.push(i);
                }
                samples.push(SampleMeta {
                    index: i,
                    pts_seconds,
                    is_keyframe: s.is_sync,
                });
            }
        }

        if keyframes.is_empty() && sample_count > 0 {
            keyframes.push(1);
        }

        Ok(Self {
            _bytes: bytes,
            reader: mp4,
            track_id,
            width: track_w,
            height: track_h,
            timescale,
            duration_seconds,
            fps,
            sample_count,
            sps,
            pps,
            samples,
            keyframes,
            current_sample: 1,
            decoder: rusty_h264::Decoder::new(),
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn duration_seconds(&self) -> f64 {
        self.duration_seconds
    }

    pub fn fps(&self) -> f64 {
        self.fps
    }

    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    pub fn current_sample(&self) -> u32 {
        self.current_sample
    }

    pub fn keyframes(&self) -> &[u32] {
        &self.keyframes
    }

    pub fn samples(&self) -> &[SampleMeta] {
        &self.samples
    }

    /// Rewinds decoding state back to the first sample.
    pub fn rewind(&mut self) -> Result<(), FormatError> {
        self.current_sample = 1;
        self.decoder = rusty_h264::Decoder::new();
        Ok(())
    }

    /// Converts an MP4 length-prefixed AVC sample to Annex B NAL byte stream.
    fn sample_to_annex_b(&self, sample_bytes: &[u8], prepend_sps_pps: bool) -> Vec<u8> {
        let mut annex_b = Vec::with_capacity(sample_bytes.len() + 64);
        if prepend_sps_pps {
            annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            annex_b.extend_from_slice(&self.sps);
            annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            annex_b.extend_from_slice(&self.pps);
        }

        let mut offset = 0;
        while offset + 4 <= sample_bytes.len() {
            let b = sample_bytes;
            let len = u32::from_be_bytes([
                b.get(offset).copied().unwrap_or(0),
                b.get(offset + 1).copied().unwrap_or(0),
                b.get(offset + 2).copied().unwrap_or(0),
                b.get(offset + 3).copied().unwrap_or(0),
            ]) as usize;
            offset += 4;
            if offset + len <= sample_bytes.len() {
                annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                annex_b.extend_from_slice(&b[offset..offset + len]);
                offset += len;
            } else {
                break;
            }
        }
        annex_b
    }

    /// Decodes and returns the next frame in sequence, or `None` if the end of stream is reached.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, FormatError> {
        while self.current_sample <= self.sample_count {
            let sample_idx = self.current_sample;
            self.current_sample += 1;

            let sample = self.reader.read_sample(self.track_id, sample_idx)
                .map_err(|e| FormatError::Corrupt(e.to_string()))?
                .ok_or_else(|| FormatError::Corrupt(format!("could not read sample {sample_idx}")))?;

            let is_keyframe = sample.is_sync || sample_idx == 1;
            let annex_b = self.sample_to_annex_b(&sample.bytes, is_keyframe);

            let decoded = self.decoder.decode_stream(&annex_b)
                .map_err(|e| FormatError::Corrupt(format!("decode error at sample {sample_idx}: {e:?}")))?;

            if let Some(f0) = decoded.first() {
                let fw = f0.width;
                let fh = f0.height;
                if fw == 0 || fh == 0 {
                    return Err(FormatError::Corrupt("invalid frame dimensions".into()));
                }

                let mut rgba = vec![0u8; fw * fh * 4];
                crate::yuv420_to_rgba(&f0.y, &f0.u, &f0.v, fw, fh, &mut rgba);

                let pts_seconds = if self.timescale > 0 {
                    sample.start_time as f64 / f64::from(self.timescale)
                } else {
                    f64::from(sample_idx.saturating_sub(1)) / self.fps
                };

                // Update tracked width/height if track header lacked them
                if self.width == 0 {
                    self.width = fw as u32;
                }
                if self.height == 0 {
                    self.height = fh as u32;
                }

                return Ok(Some(VideoFrame {
                    pts_seconds,
                    width: fw as u32,
                    height: fh as u32,
                    rgba: Arc::new(rgba),
                }));
            }
        }

        Ok(None)
    }

    /// Seeks to the nearest keyframe at or before `timestamp_seconds`, then decodes forward to `timestamp_seconds`.
    pub fn seek(&mut self, timestamp_seconds: f64) -> Result<Option<VideoFrame>, FormatError> {
        let target_pts = timestamp_seconds.clamp(0.0, self.duration_seconds);

        // Find target sample index
        let target_idx = match self.samples.binary_search_by(|s| s.pts_seconds.total_cmp(&target_pts)) {
            Ok(idx) => self.samples.get(idx).map(|s| s.index).unwrap_or(1),
            Err(idx) => {
                let idx = idx.saturating_sub(1);
                self.samples.get(idx).map(|s| s.index).unwrap_or(1)
            }
        };

        // Find nearest preceding keyframe
        let keyframe_idx = self.keyframes
            .iter()
            .copied()
            .filter(|&k| k <= target_idx)
            .last()
            .unwrap_or(1);

        // Reset decoder state and seek
        self.decoder = rusty_h264::Decoder::new();
        self.current_sample = keyframe_idx;

        let mut last_frame = None;
        while self.current_sample <= target_idx {
            if let Some(frame) = self.next_frame()? {
                last_frame = Some(frame);
            }
        }

        Ok(last_frame)
    }
}
