//! SND0 preview playback: the decoded SND0.AT3 loops on the default output device while the
//! Game screen is shown, as on the XMB, and its position is the clock the ICON1 movie follows.
//! Under it, the app's soundtrack loops quietly and steps aside while SND0 is audible.
//!
//! One cpal output stream lives on its own thread for the whole session (cpal streams are not
//! `Send` on every backend). The callback never blocks: it swaps sounds only when the generation
//! counter moves, and publishes its position through atomics. If the device goes away
//! (headphones unplugged, default device changed) the thread rebuilds the stream.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

/// Soundtrack fade in/out and ducking time constant, seconds.
const MUSIC_FADE: f64 = 0.25;
/// Length of the crossfade that makes the soundtrack loop seamless, seconds.
const LOOP_FADE: f64 = 1.5;

/// Decoded SND0: interleaved stereo s16.
pub struct Sound {
    pub rate: u32,
    pub pcm: Vec<i16>,
}

impl Sound {
    pub fn new(rate: u32, pcm: Vec<i16>) -> Arc<Self> {
        debug_assert!(rate > 0 && pcm.len() >= 2 && pcm.len().is_multiple_of(2));
        Arc::new(Self { rate, pcm })
    }

    fn frames(&self) -> usize {
        self.pcm.len() / 2
    }

    pub fn secs(&self) -> f64 {
        self.frames() as f64 / f64::from(self.rate)
    }

    /// Linear interpolation at fractional frame `pos` (wrapping: the XMB loops SND0).
    fn at(&self, pos: f64) -> (f32, f32) {
        let n = self.frames();
        let i = pos as usize % n;
        let j = (i + 1) % n;
        let t = (pos - pos.floor()) as f32;
        let s = |k: usize, c: usize| f32::from(self.pcm[k * 2 + c]) / 32768.0;
        (s(i, 0) + (s(j, 0) - s(i, 0)) * t, s(i, 1) + (s(j, 1) - s(i, 1)) * t)
    }
}

impl fmt::Debug for Sound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sound({:.1} s @ {} Hz)", self.secs(), self.rate)
    }
}

struct Shared {
    /// Sound to play; read by the callback only when `generation` changes.
    next: Mutex<Option<Arc<Sound>>>,
    generation: AtomicU64,
    /// Playing (Game screen, sound loaded). Paused output is silence and the clock stops.
    active: AtomicBool,
    muted: AtomicBool,
    /// Seconds of the sound heard so far (unwrapped, latency-compensated) as f64 bits…
    pos: AtomicU64,
    /// …at this many ns after `base`.
    stamp: AtomicU64,
    /// Generation `pos` belongs to (0 = nothing played yet).
    pos_gen: AtomicU64,
    base: Instant,
    rebuild: AtomicBool,
    /// Output format, or why there is no output.
    status: Mutex<String>,
    /// Soundtrack, read by the callback when `music_gen` changes.
    music_next: Mutex<Option<Arc<Sound>>>,
    music_gen: AtomicU64,
    music_on: AtomicBool,
    /// Linear soundtrack gain as f32 bits.
    music_gain: AtomicU32,
}

/// Session-wide audio output.
pub struct Player {
    shared: Arc<Shared>,
}

/// Cheap handle the XMB widget reads to sync the ICON1 movie to SND0.
#[derive(Clone)]
pub struct Clock(Arc<Shared>);

impl Clock {
    /// Seconds since the current sound started (wrapping is left to each consumer), `None` when
    /// nothing is playing — the movie then runs on its own timer.
    pub fn secs(&self) -> Option<f32> {
        let s = &self.0;
        let g = s.generation.load(Ordering::Acquire);
        if !s.active.load(Ordering::Acquire) || g == 0 || s.pos_gen.load(Ordering::Acquire) != g {
            return None;
        }
        let pos = f64::from_bits(s.pos.load(Ordering::Acquire));
        let stamp = Duration::from_nanos(s.stamp.load(Ordering::Acquire));
        // Between callbacks the clock advances with wall time (capped at one callback period).
        let since = s.base.elapsed().saturating_sub(stamp).min(Duration::from_millis(200));
        Some((pos + since.as_secs_f64()).max(0.0) as f32)
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Clock({:?})", self.secs())
    }
}

impl Player {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            next: Mutex::new(None),
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            pos: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
            pos_gen: AtomicU64::new(0),
            base: Instant::now(),
            rebuild: AtomicBool::new(false),
            status: Mutex::new("starting".into()),
            music_next: Mutex::new(None),
            music_gen: AtomicU64::new(0),
            music_on: AtomicBool::new(false),
            music_gain: AtomicU32::new(0),
        });
        let s = shared.clone();
        let spawned = std::thread::Builder::new().name("snd0-audio".into()).spawn(move || output_thread(&s));
        if let Err(e) = spawned {
            *shared.status.lock().unwrap() = format!("no audio thread: {e}");
        }
        Self { shared }
    }

    pub fn clock(&self) -> Clock {
        Clock(self.shared.clone())
    }

    /// Loads `sound` and restarts playback (and the movie clock) from 0.
    pub fn set(&self, sound: Option<Arc<Sound>>) {
        *self.shared.next.lock().unwrap() = sound;
        self.restart();
    }

    /// Restarts from 0, as the XMB does each time the game icon gets focus.
    pub fn restart(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn set_active(&self, on: bool) {
        self.shared.active.store(on, Ordering::Release);
    }

    pub fn set_muted(&self, on: bool) {
        self.shared.muted.store(on, Ordering::Release);
    }

    /// Decodes the `mp3` soundtrack on a background thread; it loops once ready (if on).
    pub fn load_music(&self, mp3: &'static [u8]) {
        let s = self.shared.clone();
        let _ = std::thread::Builder::new()
            .name("soundtrack".into())
            .spawn(move || {
                if let Ok(sound) = decode_mp3(mp3) {
                    *s.music_next.lock().unwrap() = Some(sound);
                    s.music_gen.fetch_add(1, Ordering::AcqRel);
                }
            });
    }

    /// Soundtrack on/off and its linear gain; changes fade in about a quarter second.
    pub fn set_music(&self, on: bool, gain: f32) {
        self.shared
            .music_gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Release);
        self.shared.music_on.store(on, Ordering::Release);
    }

    /// Output format ("48000 Hz · 2 ch") or the reason there is no audio.
    pub fn status(&self) -> String {
        self.shared.status.lock().unwrap().clone()
    }
}

fn output_thread(s: &Arc<Shared>) {
    loop {
        s.rebuild.store(false, Ordering::Release);
        let stream = open(s);
        let ok = stream.is_ok();
        if let Err(e) = &stream {
            *s.status.lock().unwrap() = format!("no audio output: {e}");
        }
        // Park until the stream reports a device error; retry a failed open every few seconds.
        let wait = if ok { Duration::from_millis(500) } else { Duration::from_secs(3) };
        loop {
            std::thread::park_timeout(wait);
            if !ok || s.rebuild.load(Ordering::Acquire) {
                break;
            }
        }
        drop(stream);
    }
}

fn open(s: &Arc<Shared>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("no default output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, s),
        SampleFormat::I16 => build::<i16>(&device, &config, s),
        SampleFormat::U16 => build::<u16>(&device, &config, s),
        SampleFormat::I32 => build::<i32>(&device, &config, s),
        SampleFormat::F64 => build::<f64>(&device, &config, s),
        SampleFormat::U8 => build::<u8>(&device, &config, s),
        SampleFormat::I8 => build::<i8>(&device, &config, s),
        other => return Err(format!("unsupported sample format {other}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    *s.status.lock().unwrap() = format!("{} Hz · {} ch", config.sample_rate, config.channels);
    Ok(stream)
}

fn build<T: SizedSample + FromSample<f32>>(device: &cpal::Device, config: &StreamConfig, s: &Arc<Shared>) -> Result<cpal::Stream, String> {
    let (rate, channels) = (f64::from(config.sample_rate), usize::from(config.channels).max(1));
    let shared = s.clone();
    let mut sound: Option<Arc<Sound>> = None;
    let mut gen_seen = 0u64;
    // Position in source frames; reset on every generation change.
    let mut pos = 0f64;
    let mut music: Option<Arc<Sound>> = None;
    let mut music_seen = 0u64;
    // The soundtrack keeps its place while silent, so it resumes where it faded out.
    let mut mpos = 0f64;
    let mut mgain = 0f32;
    let fade = (1.0 - (-1.0 / (MUSIC_FADE * rate)).exp()) as f32;
    let data = move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
        let g = shared.generation.load(Ordering::Acquire);
        if g != gen_seen {
            // `set` holds the lock only to swap an Arc; on contention retry next callback.
            if let Ok(next) = shared.next.try_lock() {
                sound = next.clone();
                gen_seen = g;
                pos = 0.0;
            }
        }
        let mg = shared.music_gen.load(Ordering::Acquire);
        if mg != music_seen
            && let Ok(next) = shared.music_next.try_lock()
        {
            music = next.clone();
            music_seen = mg;
            mpos = 0.0;
        }
        let playing = shared.active.load(Ordering::Acquire) && gen_seen == g;
        let snd = sound.as_ref().filter(|_| playing);
        let gain = if shared.muted.load(Ordering::Acquire) {
            0.0
        } else {
            1.0
        };
        if let Some(snd) = snd {
            let ts = info.timestamp();
            let latency = ts
                .playback
                .saturating_duration_since(ts.callback)
                .as_secs_f64();
            // Publish what is being heard now: buffer start minus the output latency.
            let heard = pos / f64::from(snd.rate) - latency;
            shared.pos.store(heard.to_bits(), Ordering::Release);
            shared
                .stamp
                .store(shared.base.elapsed().as_nanos() as u64, Ordering::Release);
            shared.pos_gen.store(g, Ordering::Release);
        }
        // The soundtrack steps aside while the SND0 preview is audible.
        let target = if shared.music_on.load(Ordering::Acquire) && (snd.is_none() || gain == 0.0) {
            f32::from_bits(shared.music_gain.load(Ordering::Acquire))
        } else {
            0.0
        };
        let step = snd.map_or(0.0, |s| f64::from(s.rate) / rate);
        let mstep = music.as_ref().map_or(0.0, |m| f64::from(m.rate) / rate);
        for frame in out.chunks_mut(channels) {
            let (mut l, mut r) = (0.0, 0.0);
            if let Some(s) = snd {
                (l, r) = s.at(pos);
                (l, r) = (l * gain, r * gain);
                pos += step;
            }
            mgain += (target - mgain) * fade;
            if target == 0.0 && mgain < 1e-5 {
                mgain = 0.0;
            }
            if let Some(m) = music.as_ref().filter(|_| mgain > 0.0) {
                let (a, b) = m.at(mpos);
                (l, r) = (l + a * mgain, r + b * mgain);
                mpos += mstep;
            }
            let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
            match frame {
                [m] => *m = T::from_sample((l + r) * 0.5),
                [a, b, rest @ ..] => {
                    *a = T::from_sample(l);
                    *b = T::from_sample(r);
                    rest.fill(T::from_sample(0.0f32));
                }
                [] => {}
            }
        }
        // Keep the f64 positions small (precision) without changing where the loops are.
        if let Some(s) = snd {
            let n = s.frames() as f64;
            if pos >= n * 64.0 {
                pos -= n * 32.0;
            }
        }
        if let Some(m) = &music {
            mpos %= m.frames() as f64;
        }
    };
    let shared = s.clone();
    let err = move |e: cpal::Error| {
        *shared.status.lock().unwrap() = format!("audio error: {e}");
        shared.rebuild.store(true, Ordering::Release);
    };
    device.build_output_stream(*config, data, err, None).map_err(|e| e.to_string())
}

/// Decodes an MP3 to stereo s16 and folds its tail into its head, so it loops without a seam.
pub fn decode_mp3(mp3: &'static [u8]) -> Result<Arc<Sound>, String> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::probe::Hint;

    let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(mp3)), Default::default());
    let options = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let probed = symphonia::default::get_probe()
        .format(
            Hint::new().with_extension("mp3"),
            stream,
            &options,
            &Default::default(),
        )
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let (id, rate) = (
        track.id,
        track.codec_params.sample_rate.ok_or("no sample rate")?,
    );
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .map_err(|e| e.to_string())?;
    let mut pcm = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) => continue, // one bad frame is skipped, as players do
            Err(e) => return Err(e.to_string()),
        };
        let channels = decoded.spec().channels.count().max(1);
        let mut buf = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        buf.copy_interleaved_ref(decoded);
        for f in buf.samples().chunks_exact(channels) {
            pcm.extend_from_slice(&[f[0], f[if channels > 1 { 1 } else { 0 }]]);
        }
    }
    seamless(&mut pcm, (LOOP_FADE * f64::from(rate)) as usize);
    if pcm.len() < 2 {
        return Err("no audio".into());
    }
    Ok(Sound::new(rate, pcm))
}

/// Equal-power crossfade of the last `fade` frames into the first ones, then drops the tail:
/// playback runs off the end straight into the blended head.
fn seamless(pcm: &mut Vec<i16>, fade: usize) {
    let n = pcm.len() / 2;
    let x = fade.min(n / 4);
    for i in 0..x {
        let t = (i as f32 + 0.5) / x as f32 * std::f32::consts::FRAC_PI_2;
        let (fin, fout) = (t.sin(), t.cos());
        for c in 0..2 {
            let (head, tail) = (
                f32::from(pcm[i * 2 + c]),
                f32::from(pcm[(n - x + i) * 2 + c]),
            );
            pcm[i * 2 + c] = (head * fin + tail * fout).round().clamp(-32768.0, 32767.0) as i16;
        }
    }
    pcm.truncate((n - x) * 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_seam_is_continuous() {
        // A ramp has a hard jump from its end back to its start; after folding, the last frame
        // flows into the first and the body is untouched.
        let mut pcm: Vec<i16> = (0..1000)
            .flat_map(|i| [i as i16 * 10, -(i as i16) * 10])
            .collect();
        seamless(&mut pcm, 100);
        assert_eq!(pcm.len(), 900 * 2);
        let (last, first) = (pcm[899 * 2], pcm[0]);
        assert!(
            (i32::from(first) - i32::from(last)).abs() <= 20,
            "seam {last} → {first}"
        );
        assert_eq!(pcm[100 * 2], 1000, "past the fade the head is the original");
        assert_eq!(pcm[500 * 2 + 1], -5000);
    }

    #[test]
    fn soundtrack_decodes() {
        let s = decode_mp3(include_bytes!("../../../resources/Firmware Glitch.mp3")).unwrap();
        assert_eq!(s.rate, 48_000);
        // 55.17 s minus the 1.5 s loop crossfade.
        assert!((s.secs() - (55.17 - LOOP_FADE)).abs() < 0.1, "{s:?}");
        let peak = s.pcm.iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(peak > 20_000, "real music, not silence: peak {peak}");
    }

    #[test]
    fn interpolates_and_loops() {
        let s = Sound::new(4, vec![0, 0, 16384, -16384, 0, 0, -16384, 16384]);
        assert_eq!(s.at(1.0), (0.5, -0.5));
        assert_eq!(s.at(0.5), (0.25, -0.25));
        assert_eq!(s.at(5.0), s.at(1.0), "wraps past the end");
        let (l, r) = s.at(3.5); // between the last frame and the first
        assert!((l + 0.25).abs() < 1e-6 && (r - 0.25).abs() < 1e-6);
        assert!((s.secs() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn clock_is_none_until_playing() {
        let _hw = crate::hardware_test();
        let p = Player::new();
        let c = p.clock();
        assert_eq!(c.secs(), None);
        p.set(Some(Sound::new(44_100, vec![0; 88_200])));
        assert_eq!(c.secs(), None, "inactive");
        p.set_active(true);
        // With an output device the clock starts within a few callbacks; without one it stays None.
        let t0 = Instant::now();
        while c.secs().is_none() && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!("audio output: {}", p.status());
        if let Some(a) = c.secs() {
            eprintln!("clock live at {a:.3} s");
            std::thread::sleep(Duration::from_millis(150));
            let b = c.secs().unwrap();
            assert!(b > a && b - a < 0.4, "clock advances with playback: {a} → {b}");
            p.restart();
            std::thread::sleep(Duration::from_millis(100));
            assert!(c.secs().is_none_or(|t| t < 0.3), "restart rewinds");
        }
    }
}
