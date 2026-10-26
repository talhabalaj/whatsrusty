//! Voice notes: Ogg/Opus (what WhatsApp sends) -> 16-bit PCM WAV the UI can play directly.
//! Pure Rust (`ogg` + `opus-decoder`); other audio formats go through macOS `afconvert`.

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use opus_decoder::OpusDecoder;

/// Opus always decodes at 48 kHz; RFC 7845 granule positions count 48 kHz samples.
const RATE: u32 = 48_000;

/// Decoded PCM, interleaved.
pub struct Pcm {
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}

pub fn is_ogg(data: &[u8]) -> bool {
    data.starts_with(b"OggS")
}

/// Decode an Ogg/Opus file (RFC 7845) to PCM: honours the pre-skip and end trimming.
pub fn decode_ogg_opus(data: &[u8]) -> Result<Pcm> {
    let mut reader = ogg::PacketReader::new(Cursor::new(data));
    let head = reader.read_packet().context("reading Ogg")?.context("empty Ogg stream")?;
    let head = head.data;
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        bail!("not an Opus stream");
    }
    let channels = head[9];
    if !(1..=2).contains(&channels) {
        bail!("unsupported channel count {channels}");
    }
    let pre_skip = u16::from_le_bytes([head[10], head[11]]) as usize;
    let mut decoder = OpusDecoder::new(RATE, channels as usize).map_err(|e| anyhow::anyhow!("opus init: {e}"))?;
    let ch = channels as usize;
    let mut frame = vec![0i16; OpusDecoder::MAX_FRAME_SIZE_48K * ch];
    let mut samples: Vec<i16> = Vec::new();
    let mut last_granule: Option<u64> = None;
    let mut first_packet = true;

    while let Some(packet) = reader.read_packet().context("reading Ogg")? {
        if first_packet {
            first_packet = false;
            // The comment header ("OpusTags") follows the ID header.
            if packet.data.starts_with(b"OpusTags") {
                continue;
            }
        }
        let n = match decoder.decode(&packet.data, &mut frame, false) {
            Ok(n) => n,
            Err(e) => {
                // A corrupt packet: conceal it rather than drop the whole note.
                log::debug!("opus packet: {e}");
                decoder.decode(&[], &mut frame, false).unwrap_or(0)
            }
        };
        samples.extend_from_slice(&frame[..n * ch]);
        if packet.last_in_page() {
            last_granule = Some(packet.absgp_page());
        }
    }

    let total_frames = samples.len() / ch;
    // End trimming: the final granule position is the exact sample count incl. pre-skip.
    let end = match last_granule {
        Some(g) if (g as usize) < total_frames && g as usize >= pre_skip => g as usize,
        _ => total_frames,
    };
    let start = pre_skip.min(end);
    let samples = samples[start * ch..end * ch].to_vec();
    Ok(Pcm { channels: channels as u16, sample_rate: RATE, samples })
}

/// 16-bit PCM RIFF/WAVE.
pub fn wav_bytes(pcm: &Pcm) -> Vec<u8> {
    let data_len = (pcm.samples.len() * 2) as u32;
    let block_align = pcm.channels * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&pcm.channels.to_le_bytes());
    out.extend_from_slice(&pcm.sample_rate.to_le_bytes());
    out.extend_from_slice(&(pcm.sample_rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in &pcm.samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Convert downloaded audio at `src` into a WAV at `dst` (written atomically).
/// Blocking: call from `spawn_blocking`.
pub fn to_wav(src: &Path, dst: &Path) -> Result<()> {
    let data = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    if is_ogg(&data) {
        let pcm = decode_ogg_opus(&data)?;
        crate::media::write_atomic(dst, &wav_bytes(&pcm))
    } else {
        // MP3 / AAC / M4A audio files: let Core Audio do it.
        let tmp = crate::media::TempFile::new(crate::media::temp_path(dst));
        let status = std::process::Command::new("/usr/bin/afconvert")
            .args(["-f", "WAVE", "-d", "LEI16"])
            .arg(src)
            .arg(tmp.path())
            .status()
            .context("running afconvert")?;
        if !status.success() {
            bail!("afconvert failed ({status})");
        }
        crate::media::restrict_file(tmp.path());
        std::fs::rename(tmp.path(), dst)?;
        tmp.keep();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 s of a 440 Hz sine, encoded like a WhatsApp voice note (16 kHz mono, VoIP
    /// application, 16 kbit/s, so SILK/hybrid frames):
    /// `ffmpeg -f lavfi -i sine=f=440:d=1 -ar 16000 -ac 1 -c:a libopus -application voip -b:a 16k sine_voip.ogg`
    const SINE_VOIP: &[u8] = include_bytes!("../tests/fixtures/sine_voip.ogg");

    #[test]
    fn decodes_voice_note_opus_to_wav() {
        assert!(is_ogg(SINE_VOIP));
        let pcm = decode_ogg_opus(SINE_VOIP).unwrap();
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.sample_rate, 48_000);
        // 1 s at 48 kHz after pre-skip and end trimming (allow a frame of slack).
        let n = pcm.samples.len() as i64;
        assert!((n - 48_000).abs() <= 960, "got {n} samples");
        let rms = (pcm.samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / n as f64).sqrt();
        assert!(rms > 1000.0, "decoded audio is (near) silent: rms {rms}");

        let wav = wav_bytes(&pcm);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, wav.len() - 8);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize, pcm.samples.len() * 2);
    }

    #[test]
    fn rejects_non_opus() {
        assert!(decode_ogg_opus(b"not ogg at all").is_err());
    }
}
