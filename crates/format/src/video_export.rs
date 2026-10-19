//! Pure-Rust video (MP4/H.264) and animated GIF export pipeline.
//!
//! Converts time-sampled RGBA frames into conformant H.264 Annex-B streams,
//! muxes them into MP4 containers via `mp4::Mp4Writer`, or encodes animated GIFs.

use crate::FormatError;
use rusty_h264::{Encoder, EncoderConfig, YuvFrame};
use std::io::Cursor;

/// An extracted Annex-B NAL unit with its type.
#[derive(Clone, Copy, Debug)]
struct AnnexBNal<'a> {
    nal_type: u8,
    data: &'a [u8],
}

/// Parses an Annex-B byte stream into individual NAL units by finding 3-byte and 4-byte start codes.
fn parse_annex_b(stream: &[u8]) -> Vec<AnnexBNal<'_>> {
    let mut nals = Vec::new();
    let len = stream.len();
    if len < 4 {
        return nals;
    }

    let mut start_indices = Vec::new();
    let mut i = 0;
    while i + 2 < len {
        if stream[i] == 0 && stream[i + 1] == 0 {
            if stream[i + 2] == 1 {
                start_indices.push((i, 3));
                i += 3;
                continue;
            } else if i + 3 < len && stream[i + 2] == 0 && stream[i + 3] == 1 {
                start_indices.push((i, 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    for (idx, &(start_pos, code_len)) in start_indices.iter().enumerate() {
        let nal_start = start_pos + code_len;
        let nal_end = if idx + 1 < start_indices.len() { start_indices[idx + 1].0 } else { len };
        if nal_start < nal_end && nal_end <= len {
            let slice = &stream[nal_start..nal_end];
            if let Some(&first_byte) = slice.first() {
                let nal_type = first_byte & 0x1F;
                nals.push(AnnexBNal { nal_type, data: slice });
            }
        }
    }
    nals
}

/// Converts an uncompressed RGBA8 buffer to planar YUV 4:2:0 (`YuvFrame`).
/// Dimensions are aligned up to even boundaries as required by H.264.
pub fn rgba_to_yuv(rgba: &[u8], width: usize, height: usize) -> YuvFrame {
    let w_even = (width + 1) & !1;
    let h_even = (height + 1) & !1;
    let mut y = vec![0u8; w_even * h_even];
    let mut u = vec![128u8; (w_even / 2) * (h_even / 2)];
    let mut v = vec![128u8; (w_even / 2) * (h_even / 2)];

    for row in 0..height {
        for col in 0..width {
            let px_idx = (row * width + col) * 4;
            if let (Some(&r_val), Some(&g_val), Some(&b_val)) = (rgba.get(px_idx), rgba.get(px_idx + 1), rgba.get(px_idx + 2)) {
                let r = i32::from(r_val);
                let g = i32::from(g_val);
                let b = i32::from(b_val);

                // Full-range BT.601 integer conversion
                let y_val = ((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255) as u8;
                y[row * w_even + col] = y_val;

                if row % 2 == 0 && col % 2 == 0 {
                    let u_val = (((-43 * r - 84 * g + 127 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                    let v_val = (((127 * r - 106 * g - 21 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                    let uv_idx = (row / 2) * (w_even / 2) + (col / 2);
                    if uv_idx < u.len() {
                        u[uv_idx] = u_val;
                        v[uv_idx] = v_val;
                    }
                }
            }
        }
    }

    YuvFrame { width: w_even, height: h_even, y, u, v }
}

/// Encodes an animated sequence of RGBA frames into a standalone `.mp4` container.
///
/// Frames are generated lazily one-by-one via `frame_gen` to ensure constant $O(1)$
/// memory usage during the entire export process regardless of total duration.
pub fn export_mp4<F>(width: u32, height: u32, fps: u32, total_frames: usize, mut frame_gen: F) -> Result<Vec<u8>, FormatError>
where
    F: FnMut(usize) -> Result<Vec<u8>, FormatError>,
{
    if width == 0 || height == 0 || fps == 0 || total_frames == 0 {
        return Err(FormatError::Corrupt("invalid export dimensions or duration".into()));
    }
    if width > 8192 || height > 8192 || total_frames > 60_000 {
        return Err(FormatError::Corrupt("video export limits exceeded (max 8192px, 60k frames)".into()));
    }

    let w_even = ((width + 1) & !1) as usize;
    let h_even = ((height + 1) & !1) as usize;

    let mut cfg = EncoderConfig::baseline(w_even, h_even);
    cfg.framerate = fps as f32;
    cfg.gop_size = (fps * 2).clamp(10, 120);

    let mut enc = Encoder::new(cfg).map_err(|e| FormatError::Corrupt(format!("h264 encoder init failed: {e:?}")))?;

    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;

    struct QueuedSample {
        is_sync: bool,
        payload: Vec<u8>,
    }
    let mut queued_samples: Vec<QueuedSample> = Vec::new();

    let mut cursor = Cursor::new(Vec::new());
    let mut mp4_writer: Option<mp4::Mp4Writer<&mut Cursor<Vec<u8>>>> = None;

    let timescale: u32 = 1000;
    let sample_dur = (timescale / fps.max(1)).max(1);

    let process_bitstream = |stream: &[u8], sps_out: &mut Option<Vec<u8>>, pps_out: &mut Option<Vec<u8>>, samples_out: &mut Vec<QueuedSample>| {
        let nals = parse_annex_b(stream);
        let mut sample_data = Vec::new();
        let mut is_sync = false;

        for nal in nals {
            match nal.nal_type {
                7 => {
                    if sps_out.is_none() {
                        *sps_out = Some(nal.data.to_vec());
                    }
                }
                8 => {
                    if pps_out.is_none() {
                        *pps_out = Some(nal.data.to_vec());
                    }
                }
                5 => {
                    is_sync = true;
                    let len_bytes = (nal.data.len() as u32).to_be_bytes();
                    sample_data.extend_from_slice(&len_bytes);
                    sample_data.extend_from_slice(nal.data);
                }
                1 | 6 => {
                    let len_bytes = (nal.data.len() as u32).to_be_bytes();
                    sample_data.extend_from_slice(&len_bytes);
                    sample_data.extend_from_slice(nal.data);
                }
                _ => {}
            }
        }

        if !sample_data.is_empty() {
            samples_out.push(QueuedSample { is_sync, payload: sample_data });
        }
    };

    let mut frame_index = 0u64;

    for i in 0..total_frames {
        let rgba = frame_gen(i)?;
        let yuv = rgba_to_yuv(&rgba, width as usize, height as usize);
        let stream = enc.encode(&yuv);

        process_bitstream(&stream, &mut sps, &mut pps, &mut queued_samples);

        // Once SPS and PPS are identified, initialize Mp4Writer and flush queued samples
        if mp4_writer.is_none() {
            if let (Some(sps_bytes), Some(pps_bytes)) = (&sps, &pps) {
                let mp4_cfg = mp4::Mp4Config {
                    major_brand: str::parse("isom").map_err(|e| FormatError::Corrupt(format!("brand parse error: {e}")))?,
                    minor_version: 512,
                    compatible_brands: vec![
                        str::parse("isom").map_err(|e| FormatError::Corrupt(format!("brand parse error: {e}")))?,
                        str::parse("iso2").map_err(|e| FormatError::Corrupt(format!("brand parse error: {e}")))?,
                        str::parse("avc1").map_err(|e| FormatError::Corrupt(format!("brand parse error: {e}")))?,
                        str::parse("mp41").map_err(|e| FormatError::Corrupt(format!("brand parse error: {e}")))?,
                    ],
                    timescale,
                };

                let mut writer =
                    mp4::Mp4Writer::write_start(&mut cursor, &mp4_cfg).map_err(|e| FormatError::Corrupt(format!("mp4 write_start failed: {e}")))?;

                let avc_conf = mp4::AvcConfig {
                    width: w_even as u16,
                    height: h_even as u16,
                    seq_param_set: sps_bytes.clone(),
                    pic_param_set: pps_bytes.clone(),
                };

                writer.add_track(&avc_conf.into()).map_err(|e| FormatError::Corrupt(format!("mp4 add_track failed: {e}")))?;

                for s in queued_samples.drain(..) {
                    let sample = mp4::Mp4Sample {
                        start_time: frame_index * u64::from(sample_dur),
                        duration: sample_dur,
                        rendering_offset: 0,
                        is_sync: s.is_sync,
                        bytes: mp4::Bytes::from(s.payload),
                    };
                    writer.write_sample(1, &sample).map_err(|e| FormatError::Corrupt(format!("mp4 write_sample failed: {e}")))?;
                    frame_index += 1;
                }

                mp4_writer = Some(writer);
            }
        } else if let Some(writer) = mp4_writer.as_mut() {
            for s in queued_samples.drain(..) {
                let sample = mp4::Mp4Sample {
                    start_time: frame_index * u64::from(sample_dur),
                    duration: sample_dur,
                    rendering_offset: 0,
                    is_sync: s.is_sync,
                    bytes: mp4::Bytes::from(s.payload),
                };
                writer.write_sample(1, &sample).map_err(|e| FormatError::Corrupt(format!("mp4 write_sample failed: {e}")))?;
                frame_index += 1;
            }
        }
    }

    // Flush any remaining buffered frames in the encoder
    let flushed = enc.flush();
    process_bitstream(&flushed, &mut sps, &mut pps, &mut queued_samples);

    if let Some(writer) = mp4_writer.as_mut() {
        for s in queued_samples.drain(..) {
            let sample = mp4::Mp4Sample {
                start_time: frame_index * u64::from(sample_dur),
                duration: sample_dur,
                rendering_offset: 0,
                is_sync: s.is_sync,
                bytes: mp4::Bytes::from(s.payload),
            };
            writer.write_sample(1, &sample).map_err(|e| FormatError::Corrupt(format!("mp4 write_sample failed: {e}")))?;
            frame_index += 1;
        }
        writer.write_end().map_err(|e| FormatError::Corrupt(format!("mp4 write_end failed: {e}")))?;
    } else {
        return Err(FormatError::Corrupt("failed to capture video parameter sets (SPS/PPS)".into()));
    }

    Ok(cursor.into_inner())
}

/// Encodes an animated sequence of RGBA frames into a looping GIF file.
pub fn export_gif<F>(width: u32, height: u32, fps: u32, total_frames: usize, mut frame_gen: F) -> Result<Vec<u8>, FormatError>
where
    F: FnMut(usize) -> Result<Vec<u8>, FormatError>,
{
    if width == 0 || height == 0 || fps == 0 || total_frames == 0 {
        return Err(FormatError::Corrupt("invalid export dimensions or duration".into()));
    }
    if width > 4096 || height > 4096 || total_frames > 2000 {
        return Err(FormatError::Corrupt("gif export limits exceeded (max 4096px, 2000 frames)".into()));
    }

    let mut buf = Vec::new();
    let delay_ms = (1000 / fps.max(1)).max(10);

    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut buf);
        encoder.set_repeat(image::codecs::gif::Repeat::Infinite).map_err(|e| FormatError::Corrupt(format!("gif repeat setting failed: {e}")))?;

        for i in 0..total_frames {
            let rgba = frame_gen(i)?;
            let img = image::RgbaImage::from_raw(width, height, rgba)
                .ok_or_else(|| FormatError::Corrupt("invalid frame buffer size for dimensions".into()))?;
            let frame = image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(delay_ms, 1));
            encoder.encode_frame(frame).map_err(|e| FormatError::Corrupt(format!("gif encode frame failed: {e}")))?;
        }
    }

    Ok(buf)
}
