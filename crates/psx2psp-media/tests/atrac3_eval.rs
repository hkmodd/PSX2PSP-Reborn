//! Native ATRAC3 encoder vs atracdenc, both decoded by FFmpeg's ATRAC3 decoder.
//!
//! `ATRAC3_EVAL_DIR=<dir with 44.1 kHz stereo s16 *.wav> cargo test --release -p psx2psp-media
//! --test atrac3_eval -- --ignored --nocapture` prints SNR per file and rate. Intermediates are
//! written next to the inputs and removed afterwards.

use std::path::{Path, PathBuf};
use std::process::Command;

use psx2psp_media::atrac3;
use psx2psp_media::tools::{Ffmpeg, which};

fn ffmpeg_pcm(ff: &Ffmpeg, at3: &Path) -> Vec<i16> {
    let raw = at3.with_extension("raw");
    let ok = Command::new(&ff.path).args(["-v", "error", "-y", "-i"]).arg(at3).args(["-f", "s16le", "-ac", "2"]).arg(&raw).status().unwrap();
    assert!(ok.success(), "FFmpeg could not decode {}", at3.display());
    let b = std::fs::read(&raw).unwrap();
    std::fs::remove_file(&raw).unwrap();
    b.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect()
}

/// SNR (dB) of `pol · y` against `x` delayed by `lag` stereo frames, over frames `from..to`.
fn snr_at(x: &[i16], y: &[i16], lag: usize, pol: f64, from: usize, to: usize) -> f64 {
    let to = to.min(x.len() / 2).min(y.len() / 2 - lag);
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for i in from * 2..to * 2 {
        let (a, b) = (f64::from(x[i]), pol * f64::from(y[i + 2 * lag]));
        s += a * a;
        e += (a - b) * (a - b);
    }
    10.0 * (s / e.max(1e-9)).log10()
}

/// SNR over the whole signal (edges excluded) at the delay and polarity that maximise it on a
/// one-second window: each encoder has its own delay, and FFmpeg's ATRAC3 decoder inverts
/// polarity against atracdenc's (and our) convention.
fn snr(x: &[i16], y: &[i16]) -> (i64, f64) {
    let mid = x.len() / 4;
    let probe = |&(lag, pol): &(usize, f64)| snr_at(x, y, lag, pol, mid, mid + 44_100);
    let (lag, pol) = (0..4096).flat_map(|l| [(l, 1.0), (l, -1.0)]).max_by(|a, b| probe(a).total_cmp(&probe(b))).unwrap();
    (lag as i64 * pol as i64, snr_at(x, y, lag, pol, 4096, x.len() / 2 - 4096))
}

#[test]
#[ignore = "needs ATRAC3_EVAL_DIR, FFmpeg and atracdenc"]
fn native_vs_atracdenc() {
    let dir = PathBuf::from(std::env::var_os("ATRAC3_EVAL_DIR").expect("ATRAC3_EVAL_DIR"));
    let ff = Ffmpeg::detect().expect("ffmpeg");
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/tools/atracdenc.exe");
    let ade = std::env::var_os("ATRACDENC").map(PathBuf::from).or_else(|| which("atracdenc")).or_else(|| bundled.is_file().then_some(bundled));
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "wav")).collect();
    wavs.sort();
    println!("{:<18} {:>5}  {:>14}  {:>14}  {:>7}", "input", "kbps", "native dB/lag", "atracdenc dB/lag", "native ms");
    for wav in &wavs {
        let pcm = atrac3::read_wav(&std::fs::read(wav).unwrap()).expect("44.1 kHz stereo s16 WAV");
        for kbps in [66, 105, 132] {
            let t = std::time::Instant::now();
            let ours = atrac3::encode(&pcm, kbps, &()).unwrap();
            let ms = t.elapsed().as_millis();
            let stem = wav.file_stem().unwrap().to_string_lossy().into_owned();
            let ours_path = dir.join(format!("{stem}_native_{kbps}.at3"));
            std::fs::write(&ours_path, &ours).unwrap();
            let (lag, db) = snr(&pcm, &ffmpeg_pcm(&ff, &ours_path));
            let theirs = ade.as_ref().map(|ade| {
                let out = dir.join(format!("{stem}_atracdenc_{kbps}.at3"));
                let br = match kbps {
                    66 => "64",
                    105 => "100",
                    _ => "128",
                };
                let ok = Command::new(ade).args(["-e", "atrac3", "-i"]).arg(wav).arg("-o").arg(&out).args(["--bitrate", br]).output().unwrap();
                assert!(ok.status.success());
                let r = snr(&pcm, &ffmpeg_pcm(&ff, &out));
                std::fs::remove_file(&out).unwrap();
                r
            });
            if std::env::var_os("ATRAC3_EVAL_KEEP").is_none() {
                std::fs::remove_file(&ours_path).unwrap();
            }
            let theirs = theirs.map_or("-".into(), |(l, d)| format!("{d:6.2} / {l:4}"));
            println!("{stem:<18} {kbps:>5}  {db:6.2} / {lag:4}  {theirs:>16}  {ms:>7}");
        }
    }
}
