//! SND0 preview playback: the decoded SND0.AT3 loops on the default output device while the
//! Game screen is shown, as on the XMB, and its position is the clock the ICON1 movie follows.
//!
//! One cpal output stream lives on its own thread for the whole session (cpal streams are not
//! `Send` on every backend). The callback never blocks: it swaps sounds only when the generation
//! counter moves, and publishes its position through atomics. If the device goes away
//! (headphones unplugged, default device changed) the thread rebuilds the stream.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

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
        let playing = shared.active.load(Ordering::Acquire) && gen_seen == g;
        let gain = if shared.muted.load(Ordering::Acquire) { 0.0 } else { 1.0 };
        let (Some(snd), true) = (sound.as_ref(), playing) else {
            out.fill(T::from_sample(0.0f32));
            return;
        };
        let ts = info.timestamp();
        let latency = ts.playback.saturating_duration_since(ts.callback).as_secs_f64();
        // Publish what is being heard now: buffer start minus the output latency.
        let heard = pos / f64::from(snd.rate) - latency;
        shared.pos.store(heard.to_bits(), Ordering::Release);
        shared.stamp.store(shared.base.elapsed().as_nanos() as u64, Ordering::Release);
        shared.pos_gen.store(g, Ordering::Release);
        let step = f64::from(snd.rate) / rate;
        for frame in out.chunks_mut(channels) {
            let (l, r) = snd.at(pos);
            pos += step;
            match frame {
                [m] => *m = T::from_sample((l + r) * 0.5 * gain),
                [a, b, rest @ ..] => {
                    *a = T::from_sample(l * gain);
                    *b = T::from_sample(r * gain);
                    rest.fill(T::from_sample(0.0f32));
                }
                [] => {}
            }
        }
        // Keep the f64 position small (precision) without changing where the loop is.
        let n = snd.frames() as f64;
        if pos >= n * 64.0 {
            pos -= n * 32.0;
        }
    };
    let shared = s.clone();
    let err = move |e: cpal::Error| {
        *shared.status.lock().unwrap() = format!("audio error: {e}");
        shared.rebuild.store(true, Ordering::Release);
    };
    device.build_output_stream(*config, data, err, None).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
