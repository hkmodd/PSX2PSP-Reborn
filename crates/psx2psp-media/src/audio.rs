//! Native audio decode for SND0.AT3: symphonia (MP3, AAC/M4A/MP4, ALAC, FLAC, Ogg Vorbis, WAV,
//! AIFF) → 44.1 kHz stereo s16, the same span, fade and layout [`crate::tools::Ffmpeg::to_wav`]
//! produces. Anything symphonia cannot open (Opus, WebM/MKV, WMA…) returns `None`, and the
//! caller falls back to FFmpeg.

use std::path::Path;

use psx2psp_core::Monitor;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::MediaError;

/// Output rate: the only one ATRAC3 on the PSP plays.
pub const RATE: u32 = 44_100;
/// Length of the closing fade-out (a loop point that does not click), as in `Ffmpeg::to_wav`.
const FADE_SECS: f64 = 1.0;

struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    rate: u32,
    frames: Option<u64>,
}

impl Source {
    /// The first track symphonia can decode (a video file's audio track included).
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let opts = FormatOptions { enable_gapless: true, ..Default::default() };
        let format = symphonia::default::get_probe().format(&hint, stream, &opts, &MetadataOptions::default()).ok()?.format;
        let (track, decoder, rate, frames) = format.tracks().iter().find_map(|t| {
            let rate = t.codec_params.sample_rate.filter(|&r| r > 0)?;
            let dec = symphonia::default::get_codecs().make(&t.codec_params, &DecoderOptions::default()).ok()?;
            Some((t.id, dec, rate, t.codec_params.n_frames))
        })?;
        Some(Self { format, decoder, track, rate, frames })
    }
}

/// Length in seconds from the container, when it records one (no decoding).
pub fn duration(path: &Path) -> Option<f64> {
    let s = Source::open(path)?;
    Some(s.frames? as f64 / f64::from(s.rate))
}

/// Decodes `[start, start + max_secs)` of `path` to interleaved 44.1 kHz stereo s16 with a
/// one-second linear fade-out ending at `max_secs`. Mono is duplicated; of more channels the
/// first two (front left/right) are kept. `Ok(None)`: symphonia cannot decode this file.
pub fn decode(path: &Path, start: f64, max_secs: f64, mon: &dyn Monitor) -> Result<Option<Vec<i16>>, MediaError> {
    let Some(mut s) = Source::open(path) else { return Ok(None) };
    let rate = f64::from(s.rate);
    let skip = (start.max(0.0) * rate).round() as u64;
    // A few input samples past the end feed the resampler's last outputs.
    let want = skip + (max_secs * rate).ceil() as u64 + 64;
    let (mut seen, mut pcm) = (0u64, Vec::<f32>::new());
    'packets: loop {
        if mon.cancelled() {
            return Err(MediaError::Cancelled);
        }
        let packet = match s.format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => return Ok(None),
        };
        if packet.track_id() != s.track {
            continue;
        }
        let decoded = match s.decoder.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) => continue, // one corrupt frame is skipped, as players do
            Err(_) => return Ok(None),
        };
        let ch = decoded.spec().channels.count();
        if ch == 0 {
            return Ok(None);
        }
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buf.copy_interleaved_ref(decoded);
        for f in buf.samples().chunks_exact(ch) {
            if seen >= want {
                break 'packets;
            }
            if seen >= skip {
                pcm.extend_from_slice(&[f[0], f[usize::from(ch > 1)]]);
            }
            seen += 1;
        }
    }
    let mut out = resample(&pcm, s.rate, RATE);
    out.truncate(2 * ((max_secs * f64::from(RATE)).round() as usize));
    Ok(Some(finish(&mut out, max_secs)))
}

/// Applies the closing fade and converts to s16.
fn finish(x: &mut [f32], max_secs: f64) -> Vec<i16> {
    let fade_from = (max_secs - FADE_SECS).max(0.0);
    for (i, f) in x.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let t = i as f64 / f64::from(RATE);
        if t > fade_from {
            let g = ((max_secs - t) / FADE_SECS).clamp(0.0, 1.0) as f32;
            f[0] *= g;
            f[1] *= g;
        }
    }
    x.iter().map(|&v| (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16).collect()
}

/// Kaiser-windowed sinc resampling of interleaved stereo, as a polyphase table with linear
/// interpolation between phases. −6 dB at 91 % of the lower Nyquist, stopband ≈ −80 dB from just
/// above it (48 → 44.1 kHz: flat to ~18.5 kHz, rejected from ~21.6 kHz).
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return x.to_vec();
    }
    const PHASES: usize = 512;
    const BETA: f64 = 8.0;
    let cutoff = (f64::from(to) / f64::from(from)).min(1.0) * 0.91;
    let half = (32.0 / cutoff).ceil() as usize;
    let width = 2 * half;
    let i0 = |v: f64| {
        // Modified Bessel function of the first kind, order 0 (power series).
        let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
        while term > 1e-12 * sum {
            term *= (v / (2.0 * k)).powi(2);
            sum += term;
            k += 1.0;
        }
        sum
    };
    let kernel = |d: f64| {
        let u = d / half as f64;
        if u.abs() >= 1.0 {
            return 0.0;
        }
        let sinc = if d == 0.0 { 1.0 } else { (std::f64::consts::PI * cutoff * d).sin() / (std::f64::consts::PI * cutoff * d) };
        cutoff * sinc * i0(BETA * (1.0 - u * u).sqrt()) / i0(BETA)
    };
    // Row p holds the taps for a fractional position p/PHASES; each row sums to 1 (flat DC).
    let table: Vec<f32> = (0..=PHASES)
        .flat_map(|p| {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..width).map(|j| kernel(j as f64 - half as f64 + 1.0 - frac)).collect();
            let sum: f64 = row.iter().sum();
            row.into_iter().map(move |v| (v / sum) as f32)
        })
        .collect();
    let n_in = x.len() / 2;
    let n_out = (n_in as u64 * u64::from(to) / u64::from(from)) as usize;
    let mut out = Vec::with_capacity(2 * n_out);
    let mut taps = vec![0.0f32; width];
    for i in 0..n_out as u64 {
        let num = i * u64::from(from);
        let (n0, rem) = ((num / u64::from(to)) as usize, num % u64::from(to));
        let pf = rem as f64 / f64::from(to) * PHASES as f64;
        let (p, w) = (pf as usize, (pf - pf.floor()) as f32);
        let (a, b) = (&table[p * width..(p + 1) * width], &table[(p + 1) * width..(p + 2) * width]);
        for (t, (&a, &b)) in taps.iter_mut().zip(a.iter().zip(b)) {
            *t = a + w * (b - a);
        }
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for (j, &t) in taps.iter().enumerate() {
            let Some(k) = (n0 + j + 1).checked_sub(half) else { continue };
            if k < n_in {
                l += x[2 * k] * t;
                r += x[2 * k + 1] * t;
            }
        }
        out.extend_from_slice(&[l, r]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, secs: f64) -> Vec<f32> {
        (0..(f64::from(rate) * secs) as usize)
            .flat_map(|i| {
                let v = (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / f64::from(rate)).sin()) as f32;
                [v, -v]
            })
            .collect()
    }

    /// SNR (dB) of `y` against the ideal tone at the output rate, away from the edges.
    fn tone_snr(y: &[f32], hz: f64) -> f64 {
        let want = tone(RATE, hz, y.len() as f64 / 2.0 / f64::from(RATE));
        let (mut s, mut e) = (0.0f64, 0.0f64);
        for i in 2000..y.len().min(want.len()) - 2000 {
            s += f64::from(want[i]).powi(2);
            e += f64::from(want[i] - y[i]).powi(2);
        }
        10.0 * (s / e).log10()
    }

    #[test]
    fn resampler_is_clean_down_and_up() {
        for (from, hz) in [(48_000, 1000.0), (48_000, 15_000.0), (22_050, 3000.0), (32_000, 9000.0)] {
            let y = resample(&tone(from, hz, 0.5), from, RATE);
            assert_eq!(y.len() / 2, (f64::from(from) * 0.5) as usize * RATE as usize / from as usize);
            let snr = tone_snr(&y, hz);
            assert!(snr > 60.0, "{from} Hz → 44.1 kHz, {hz} Hz tone: {snr:.1} dB");
        }
        let x = tone(RATE, 1000.0, 0.1);
        assert_eq!(resample(&x, RATE, RATE), x);
    }

    /// Content above the new Nyquist must not fold back into the passband.
    #[test]
    fn resampler_rejects_aliases() {
        let y = resample(&tone(48_000, 23_000.0, 0.5), 48_000, RATE);
        let mid = &y[4000..y.len() - 4000];
        let rms = (mid.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        assert!(20.0 * (rms / (0.5 / 2f64.sqrt())).log10() < -60.0, "alias at {rms}");
    }

    fn wav(path: &Path, rate: u32, channels: u16, pcm: &[i16]) {
        let mut b = b"RIFF".to_vec();
        b.extend((36 + 2 * pcm.len() as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(channels.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend((rate * 2 * u32::from(channels)).to_le_bytes());
        b.extend((2 * channels).to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend((2 * pcm.len() as u32).to_le_bytes());
        b.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        std::fs::write(path, b).unwrap();
    }

    /// Mono 48 kHz WAV, trimmed and faded: duplicated to stereo, resampled, exact span and fade.
    #[test]
    fn decodes_trims_and_fades() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.wav");
        // A ramp of steps: second k holds the value 1000·(k+1), so the start offset is visible.
        let pcm: Vec<i16> = (0..48_000 * 6).map(|i| (1000 * (i / 48_000 + 1)) as i16).collect();
        wav(&p, 48_000, 1, &pcm);
        assert!((duration(&p).unwrap() - 6.0).abs() < 1e-9);
        let y = decode(&p, 2.0, 3.0, &()).unwrap().unwrap();
        assert_eq!(y.len(), 2 * 3 * 44_100);
        let at = |secs: f64| {
            let i = (secs * 44_100.0) as usize;
            (y[2 * i], y[2 * i + 1])
        };
        assert_eq!(at(0.5), (3000, 3000), "second 2 of the source, both channels");
        assert_eq!(at(1.5), (4000, 4000));
        assert_eq!(at(2.5).0, 2500, "halfway through the fade-out");
        assert!(y[y.len() - 1].abs() <= 1, "fades to silence");
        // Past the end: whatever is left, then nothing.
        assert_eq!(decode(&p, 5.0, 3.0, &()).unwrap().unwrap().len(), 2 * 44_100);
        assert!(decode(&p, 9.0, 3.0, &()).unwrap().unwrap().is_empty());
    }

    #[test]
    fn unknown_input_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.opus");
        std::fs::write(&p, b"OggS not really").unwrap();
        assert!(decode(&p, 0.0, 1.0, &()).unwrap().is_none());
        assert!(decode(&dir.path().join("missing.mp3"), 0.0, 1.0, &()).unwrap().is_none());
        assert!(duration(&p).is_none());
    }

    #[test]
    fn cancellation_stops_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.wav");
        wav(&p, 44_100, 2, &vec![0; 44_100 * 2]);
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert!(matches!(decode(&p, 0.0, 1.0, &cancel), Err(MediaError::Cancelled)));
    }
}
