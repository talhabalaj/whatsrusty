//! Voice notes: Ogg/Opus (what WhatsApp sends) -> 16-bit PCM WAV the UI can play directly.
//! Pure Rust (`ogg` + `opus-decoder`); other audio formats go through macOS `afconvert`.
//! Recorded notes go the other way: PCM -> Ogg/Opus (libopus via `opus-codec`).

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

/// A recorded voice note, encoded the way WhatsApp phones send them.
pub struct VoiceNote {
    pub ogg: Vec<u8>,
    pub seconds: u32,
    /// 64 samples, 0..=100.
    pub waveform: Vec<u8>,
}

const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];
const VOICE_BITRATE: i32 = 24_000;
/// Packets per Ogg page (20 ms each: about a second per page).
const PACKETS_PER_PAGE: usize = 50;

/// Encode mono PCM as an Ogg/Opus voice note (RFC 7845): OpusHead and OpusTags on pages
/// of their own, 20 ms VoIP frames, pre-skip from the encoder's lookahead, granule
/// positions in 48 kHz units and an exact end on the last page.
pub fn encode_voice_note(samples: &[i16], sample_rate: u32) -> Result<VoiceNote> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};
    use opus_codec::{Application, Bandwidth, Bitrate, Channels, Encoder, SampleRate, Signal};

    if samples.is_empty() || sample_rate == 0 {
        bail!("empty recording");
    }
    let (pcm, rate) = if OPUS_RATES.contains(&sample_rate) {
        (std::borrow::Cow::Borrowed(samples), sample_rate)
    } else {
        (std::borrow::Cow::Owned(resample_linear(samples, sample_rate, RATE)), RATE)
    };
    let opus_rate = match rate {
        8_000 => SampleRate::Hz8000,
        12_000 => SampleRate::Hz12000,
        16_000 => SampleRate::Hz16000,
        24_000 => SampleRate::Hz24000,
        _ => SampleRate::Hz48000,
    };
    let opus_err = |what: &'static str| move |e: opus_codec::Error| anyhow::anyhow!("opus {what}: {e}");
    let mut encoder = Encoder::new(opus_rate, Channels::Mono, Application::Voip).map_err(opus_err("init"))?;
    encoder.set_bitrate(Bitrate::Custom(VOICE_BITRATE)).map_err(opus_err("bitrate"))?;
    // Wideband speech stays in SILK mode, like the voice notes phones send.
    encoder.set_max_bandwidth(Bandwidth::Wideband).map_err(opus_err("bandwidth"))?;
    encoder.set_signal(Signal::Voice).map_err(opus_err("signal"))?;
    let lookahead = encoder.lookahead().map_err(opus_err("lookahead"))?.max(0) as usize;
    let to_48k = (RATE / rate) as usize;
    let pre_skip = lookahead * to_48k;
    let frame = rate as usize / 50;

    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
    head.extend_from_slice(&rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family
    let vendor = b"WhatsRusty";
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());

    let serial = 0x5752_5354; // "WRST"
    let mut out = Vec::new();
    let mut writer = PacketWriter::new(&mut out);
    writer.write_packet(head, serial, PacketWriteEndInfo::EndPage, 0)?;
    writer.write_packet(tags, serial, PacketWriteEndInfo::EndPage, 0)?;

    // Feed the lookahead's worth of silence after the end so the last samples come out.
    let frames = (pcm.len() + lookahead).div_ceil(frame);
    let end_granule = (pre_skip + pcm.len() * to_48k) as u64;
    let mut buf = vec![0i16; frame];
    let mut packet = vec![0u8; 4000];
    for i in 0..frames {
        let start = (i * frame).min(pcm.len());
        let end = ((i + 1) * frame).min(pcm.len());
        buf.fill(0);
        buf[..end - start].copy_from_slice(&pcm[start..end]);
        let n = encoder.encode(&buf, &mut packet).map_err(opus_err("encode"))?;
        let last = i + 1 == frames;
        let granule = if last { end_granule } else { ((i + 1) * frame * to_48k) as u64 };
        let info = if last {
            PacketWriteEndInfo::EndStream
        } else if (i + 1) % PACKETS_PER_PAGE == 0 {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(packet[..n].to_vec(), serial, info, granule)?;
    }
    drop(writer);
    let seconds = (samples.len() as f64 / f64::from(sample_rate)).round().max(1.) as u32;
    Ok(VoiceNote { ogg: out, seconds, waveform: waveform(samples, 64) })
}

/// Loudness per slice (mean absolute amplitude), scaled so the loudest is 100.
pub fn waveform(samples: &[i16], n: usize) -> Vec<u8> {
    let levels: Vec<f64> = (0..n)
        .map(|i| {
            let slice = &samples[i * samples.len() / n..(i + 1) * samples.len() / n];
            if slice.is_empty() {
                return 0.;
            }
            slice.iter().map(|s| f64::from(*s).abs()).sum::<f64>() / slice.len() as f64
        })
        .collect();
    let max = levels.iter().copied().fold(0., f64::max);
    levels.iter().map(|l| if max > 0. { (l / max * 100.).round() as u8 } else { 0 }).collect()
}

fn resample_linear(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    let n = (samples.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    let step = f64::from(from) / f64::from(to);
    (0..n)
        .map(|i| {
            let pos = i as f64 * step;
            let j = pos as usize;
            let a = f64::from(samples[j.min(samples.len() - 1)]);
            let b = f64::from(samples[(j + 1).min(samples.len() - 1)]);
            (a + (b - a) * (pos - j as f64)) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f64) -> Vec<i16> {
        (0..(f64::from(rate) * secs) as usize)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                // Louder towards the end, so the waveform has a shape.
                ((t * 440. * std::f64::consts::TAU).sin() * 4000. * (1. + t)) as i16
            })
            .collect()
    }

    #[test]
    fn encodes_voice_note_that_decodes_back() {
        for rate in [48_000, 16_000, 44_100] {
            let pcm = tone(rate, 2.5);
            let note = encode_voice_note(&pcm, rate).unwrap();
            assert!(is_ogg(&note.ogg));
            assert_eq!(note.seconds, 3);
            assert_eq!(note.waveform.len(), 64);
            assert!(note.waveform.iter().all(|v| *v <= 100));
            assert_eq!(note.waveform.iter().copied().max(), Some(100));
            assert!(note.waveform[60] > note.waveform[3], "{:?}", note.waveform);

            let decoded = decode_ogg_opus(&note.ogg).unwrap();
            assert_eq!(decoded.channels, 1);
            let n = decoded.samples.len() as i64;
            assert!((n - 120_000).abs() <= 960, "rate {rate}: got {n} samples");
            let rms = (decoded.samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / n as f64).sqrt();
            assert!(rms > 1000.0, "rate {rate}: decoded audio is (near) silent: rms {rms}");
        }
    }

    #[test]
    fn voice_note_pages_follow_rfc_7845() {
        let note = encode_voice_note(&tone(16_000, 1.2), 16_000).unwrap();
        let mut reader = ogg::PacketReader::new(Cursor::new(&note.ogg));
        let head = reader.read_packet().unwrap().unwrap();
        assert!(head.first_in_stream() && head.last_in_page());
        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(u32::from_le_bytes(head.data[12..16].try_into().unwrap()), 16_000);
        let pre_skip = u64::from(u16::from_le_bytes([head.data[10], head.data[11]]));
        assert!(pre_skip > 0);
        let tags = reader.read_packet().unwrap().unwrap();
        assert!(tags.data.starts_with(b"OpusTags") && tags.last_in_page());
        let mut last = None;
        while let Some(p) = reader.read_packet().unwrap() {
            last = Some(p);
        }
        let last = last.unwrap();
        assert!(last.last_in_stream());
        assert_eq!(last.absgp_page(), pre_skip + 57_600);
    }

    /// Manual: `WA_TEST_OGG_OUT=/tmp/note.ogg cargo test -p wa-backend writes_a_voice_note -- --ignored`, then
    /// check it with another decoder (ffprobe, a phone).
    #[test]
    #[ignore]
    fn writes_a_voice_note() {
        let note = encode_voice_note(&tone(48_000, 3.0), 48_000).unwrap();
        std::fs::write(std::env::var("WA_TEST_OGG_OUT").unwrap(), note.ogg).unwrap();
    }

    #[test]
    fn rejects_empty_recording() {
        assert!(encode_voice_note(&[], 48_000).is_err());
    }

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
