//! Audio forwarding primitives: the on-wire contract for the `audio`
//! channel plus everything that must be SHARED between the capturing
//! server and the playing client.
//!
//! The contract is deliberately minimal — this is a first channel kind on
//! top of the generic data plane:
//!
//! * One `ChannelOpen(kind: "audio")`, then only `StreamData` frames on
//!   that `connection_id` (downlink only; never compressed, never diffed —
//!   the payload of each frame is exactly one Opus packet).
//! * Fixed format: Opus, 48 kHz, stereo, 20 ms frames — matching what the
//!   `opus-rs` encoder/decoder pair (pure-Rust port of libopus 1.6) is
//!   constructed with. A packet self-delimits; the decoder does not need a
//!   framing header.
//! * Semantics: real-time, lossy-by-design. A dropped packet is a click,
//!   not corruption — unlike the X11 byte stream, the lane sheds packets
//!   when a slow link falls behind. The strict-stream rules of
//!   [`crate::transport`] intentionally do NOT apply here.
//!
//! Process plumbing (spawning `parec` on the server, `paplay` on the
//! client) lives in the respective crates; this
//! module owns the codec state machines and the command candidates so both
//! ends negotiate against identical constants.

use bytes::Bytes;
use opus_rs::{Application, OpusDecoder, OpusEncoder};

/// Sample rate every endpoint uses (Opus native rate; resampler-free on
/// both PipeWire and PulseAudio defaults).
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo: desktop audio downmixes to two channels.
pub const CHANNELS: u16 = 2;
/// Samples per channel per Opus frame — the canonical 20 ms.
pub const FRAME_SAMPLES: usize = 960;
/// Bytes of s16le PCM per frame: 960 samples * 2 channels * 2 bytes.
pub const PCM_FRAME_BYTES: usize = FRAME_SAMPLES * CHANNELS as usize * 2;
/// Output staging for one encoded packet. Opus caps packets at 1275 bytes;
/// one full PCM frame fits easily, so a 4 KiB ceiling is never a constraint.
pub const MAX_PACKET_BYTES: usize = 4096;
/// VBR target for desktop audio; 64 kbit/s is near-transparent stereo and
/// ~2% of a 3 Mbit/s link.
pub const DEFAULT_BITRATE_KBPS: u32 = 64;

/// Server-side state: PCM reassembly + Opus encoder. Feed it arbitrary
/// byte spans of s16le audio (whatever the recorder child wrote and
/// [`FormatGate`] passed through) and it emits whole Opus packets.
/// (The ported codec structs carry no `Debug`, hence manual derive omission here.)
pub struct Packetizer {
    encoder: OpusEncoder,
    carry: Vec<u8>,
    samples: Vec<i16>,
    packet: Vec<u8>,
    seq: u32,
}

impl Packetizer {
    pub fn new(bitrate_kbps: u32) -> Result<Self, &'static str> {
        let mut encoder =
            OpusEncoder::new(SAMPLE_RATE as i32, CHANNELS as usize, Application::Audio)?;
        encoder.bitrate_bps = (bitrate_kbps as usize).saturating_mul(1000) as i32;
        Ok(Self {
            encoder,
            carry: Vec::with_capacity(PCM_FRAME_BYTES * 2),
            samples: vec![0; FRAME_SAMPLES * CHANNELS as usize],
            packet: vec![0u8; MAX_PACKET_BYTES],
            seq: 0,
        })
    }

    /// Push raw recorder bytes; `on_packet` sees `(seq, opus packet)` for
    /// every completed 20 ms frame. Sub-frame leftovers carry into the next
    /// push. An encoder error skips only that frame — a silent 20 ms gap is
    /// a click, not corruption (real-time lane semantics).
    pub fn push(&mut self, data: &[u8], mut on_packet: impl FnMut(u32, &[u8])) {
        if data.is_empty() {
            return;
        }
        self.carry.extend_from_slice(data);
        while self.carry.len() >= PCM_FRAME_BYTES {
            {
                let frame = &self.carry[..PCM_FRAME_BYTES];
                for (slot, pair) in self.samples.iter_mut().zip(frame.as_chunks::<2>().0) {
                    *slot = i16::from_le_bytes(*pair);
                }
                match self
                    .encoder
                    .encode_i16(&self.samples, FRAME_SAMPLES, &mut self.packet)
                {
                    Ok(n) => on_packet(self.seq, &self.packet[..n]),
                    Err(e) => tracing::debug!("audio: opus encode skipped a frame: {e}"),
                }
            }
            self.carry.drain(..PCM_FRAME_BYTES);
            self.seq = self.seq.wrapping_add(1);
        }
    }

    /// Packets emitted so far (diagnostics/tests).
    pub fn seq(&self) -> u32 {
        self.seq
    }
}

/// Client-side state: Opus packet → s16le PCM bytes for the player.
pub struct Depacketizer {
    decoder: OpusDecoder,
    scratch: Vec<f32>,
    out: Vec<u8>,
}

impl Depacketizer {
    pub fn new() -> Result<Self, &'static str> {
        Ok(Self {
            decoder: OpusDecoder::new(SAMPLE_RATE as i32, CHANNELS as usize)?,
            scratch: vec![0.0; FRAME_SAMPLES * CHANNELS as usize],
            out: Vec::with_capacity(PCM_FRAME_BYTES * 4),
        })
    }

    /// Decode one packet into s16le bytes. `None` on a corrupt packet: real
    /// time semantics — drop the frame, keep the stream.
    pub fn push(&mut self, packet: &[u8]) -> Option<Bytes> {
        let n = match self
            .decoder
            .decode(packet, FRAME_SAMPLES, &mut self.scratch)
        {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("audio: dropping undecodable opus packet: {e}");
                return None;
            }
        };
        if n == 0 {
            return None;
        }
        self.out.clear();
        // libopus convention: f32 1.0 maps to PCM16 full scale 32768.
        for s in &self.scratch[..n * CHANNELS as usize] {
            let v = (s * 32768.0).clamp(-32768.0, 32767.0) as i16;
            self.out.extend_from_slice(&v.to_le_bytes());
        }
        Some(Bytes::copy_from_slice(&self.out))
    }
}

/// Recorder output is raw s16le from `parec --raw`, but an operator
/// override that likes to be helpful may prefix a RIFF/WAVE header. The gate sniffs the first bytes once and then either strips
/// exactly the header chunks (up to and including the `data` chunk header)
/// or passes everything through as raw PCM.
#[derive(Debug, Default)]
pub struct FormatGate {
    prefix: Vec<u8>,
    decided: Option<usize>, // PCM start offset within `prefix`
}

impl FormatGate {
    /// WAV walking is bounded: a header beyond 64 KiB is not a header we
    /// understand, and mis-stripping real audio is worse than one frame of
    /// noise — fall through as raw.
    const MAX_HEADER_BYTES: usize = 64 * 1024;

    /// Feed recorder bytes; receive the slice that is audio. Once decided
    /// raw, this is a zero-copy pass-through; during the sniffing window
    /// (only at stream start) bytes are buffered and returned owned.
    pub fn feed<'b>(&mut self, more: &'b [u8]) -> std::borrow::Cow<'b, [u8]> {
        use std::borrow::Cow;
        if self.decided.is_some() {
            return Cow::Borrowed(more);
        }
        self.prefix.extend_from_slice(more);
        if self.prefix.len() >= 12
            && (&self.prefix[0..4] != b"RIFF" || &self.prefix[8..12] != b"WAVE")
        {
            // Not a WAV: everything buffered so far was audio after all.
            self.decided = Some(0);
            return Cow::Owned(std::mem::take(&mut self.prefix));
        }
        if self.prefix.len() >= Self::MAX_HEADER_BYTES {
            // Give up parsing; treat everything so far as audio (worst case
            // the player hears one scratchy frame of header noise).
            self.decided = Some(0);
            return Cow::Owned(std::mem::take(&mut self.prefix));
        }
        if self.prefix.len() < 12 {
            return Cow::Borrowed(&[]);
        }
        // Walk chunks: [id:4][size:4 LE][payload:size][pad:0/1].
        let mut off = 12usize;
        while off + 8 <= self.prefix.len() {
            let id = &self.prefix[off..off + 4];
            let size = u32::from_le_bytes([
                self.prefix[off + 4],
                self.prefix[off + 5],
                self.prefix[off + 6],
                self.prefix[off + 7],
            ]) as usize;
            if id == b"data" {
                self.decided = Some(off + 8);
                let audio = self.prefix.split_off(off + 8);
                self.prefix.clear();
                return Cow::Owned(audio);
            }
            off += 8 + size + (size & 1);
        }
        Cow::Borrowed(&[]) // header incomplete; wait for more bytes
    }

    /// Whether the source was recognized as WAV (diagnostics/tests).
    pub fn is_wav(&self) -> bool {
        self.decided.is_some_and(|s| s != 0)
    }
}

/// A program is usable if it is an absolute path that exists, or is found
/// on `PATH` as an executable file.
pub fn program_exists(program: &str) -> bool {
    if program.contains('/') {
        return is_executable(std::path::Path::new(program));
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(|p| std::env::split_paths(p))
        .any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &std::path::Path) -> bool {
    match std::fs::metadata(path) {
        Ok(md) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                md.is_file() && md.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                md.is_file()
            }
        }
        Err(_) => false,
    }
}

/// Recorder candidates on the server, in preference order. `parec` is the
/// PulseAudio/PipeWire-generic raw capturer: it streams interleaved s16le
/// 48 kHz stereo of `@DEFAULT_SINK@.monitor` — the *output* the desktop is
/// playing, never the microphone — continuously even while the sink is
/// silent. `pw-record` is deliberately absent: its monitor capture needs an
/// explicit node target and its default records the mic, the wrong privacy
/// semantics for a blind fallback.
pub fn record_command_candidates() -> Vec<Vec<String>> {
    [vec![
        "parec".to_string(),
        "--raw".to_string(),
        "--format=s16le".to_string(),
        "--rate=48000".to_string(),
        "--channels=2".to_string(),
        "--device=@DEFAULT_SINK@.monitor".to_string(),
    ]]
    .into_iter()
    .filter(|argv| argv.first().is_some_and(|p| program_exists(p)))
    .collect()
}

/// Player candidates on the client, in preference order. `paplay --raw`
/// consumes interleaved s16le PCM from `/dev/stdin` under both native
/// PulseAudio and PipeWire's pulse shim (`pw-play` rejects raw stdin).
pub fn play_command_candidates() -> Vec<Vec<String>> {
    [vec![
        "paplay".to_string(),
        "--raw".to_string(),
        "--format=s16le".to_string(),
        "--rate=48000".to_string(),
        "--channels=2".to_string(),
        "/dev/stdin".to_string(),
    ]]
    .into_iter()
    .filter(|argv| argv.first().is_some_and(|p| program_exists(p)))
    .collect()
}

/// Resolve the argv to spawn: explicit `[runtime]` override wins verbatim
/// (the operator owns the exact command), otherwise the first supported
/// candidate, otherwise nothing (audio stays off with a warning).
pub fn resolve_command(
    override_: &Option<Vec<String>>,
    candidates: Vec<Vec<String>>,
) -> Option<Vec<String>> {
    if let Some(argv) = override_ {
        if argv.is_empty() {
            return None;
        }
        return Some(argv.clone());
    }
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_pcm(frames: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(frames * PCM_FRAME_BYTES);
        for i in 0..frames * FRAME_SAMPLES {
            let s = (i as f64 * 0.05).sin() * 12_000.0;
            for _ in 0..CHANNELS {
                v.extend_from_slice(&(s as i16).to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn packetizer_slices_arbitrary_spans_into_exact_frames() {
        let mut p = Packetizer::new(DEFAULT_BITRATE_KBPS).unwrap();
        let audio = sine_pcm(3);
        let mut got: Vec<(u32, usize)> = Vec::new();
        // Feed in awkward 1234-byte spans: frames must still come out whole.
        for chunk in audio.chunks(1234) {
            p.push(chunk, |seq: u32, pkt: &[u8]| got.push((seq, pkt.len())));
        }
        assert_eq!(got.len(), 3, "one packet per 20 ms frame");
        assert_eq!(got[0].0, 0);
        assert_eq!(got[2].0, 2);
        assert!(
            got.iter().all(|(_, n)| *n > 0 && *n <= MAX_PACKET_BYTES),
            "packet sizes sane: {got:?}"
        );
    }

    #[test]
    fn encode_decode_roundtrip_recovers_energy() {
        let mut p = Packetizer::new(DEFAULT_BITRATE_KBPS).unwrap();
        let mut d = Depacketizer::new().unwrap();
        let audio = sine_pcm(4);
        let mut out = Vec::new();
        p.push(&audio, |_: u32, pkt: &[u8]| {
            if let Some(bytes) = d.push(pkt) {
                out.extend_from_slice(&bytes);
            }
        });
        assert_eq!(out.len(), audio.len(), "four frames in and out");
        // RMS sanity: a 64 kbit/s roundtrip of a sine keeps most energy.
        let rms = |v: &[u8]| {
            let mut acc = 0f64;
            for pair in v.as_chunks::<2>().0 {
                let s = i16::from_le_bytes(*pair) as f64;
                acc += s * s;
            }
            (acc / (v.len() as f64 / 2.0)).sqrt()
        };
        let (a, b) = (rms(&audio), rms(&out));
        assert!(b > a * 0.5, "roundtrip RMS {b:.0} vs source {a:.0}");
    }

    #[test]
    fn depacketizer_survives_garbage_packets() {
        let mut d = Depacketizer::new().unwrap();
        assert!(
            d.push(&[0xFF; 32]).is_none(),
            "corrupt packet drops, no panic"
        );
        // A real packet still decodes after garbage: state is not poisoned.
        let mut p = Packetizer::new(DEFAULT_BITRATE_KBPS).unwrap();
        let mut ok = None;
        p.push(&sine_pcm(1), |_: u32, pkt: &[u8]| ok = Some(pkt.to_vec()));
        let pcm = d.push(&ok.unwrap());
        assert_eq!(pcm.map(|b| b.len()), Some(PCM_FRAME_BYTES));
    }

    #[test]
    fn gate_passes_raw_pcm_through_immediately() {
        let mut g = FormatGate::default();
        let pcm = sine_pcm(1);
        assert_eq!(&*g.feed(&pcm[..40]), &pcm[..40]);
        assert!(!g.is_wav());
    }

    #[test]
    fn gate_strips_wav_header_across_chunk_splits() {
        // Canonical 44-byte header, delivered in hostile 7-byte spans.
        let mut wav = b"RIFFxxxyWAVEfmt ".to_vec();
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1u8; 16]); // fmt chunk body (id+len inside 16)
        wav.extend(b"data");
        wav.extend_from_slice(&4096u32.to_le_bytes());
        let pcm = sine_pcm(2);
        wav.extend_from_slice(&pcm);

        let mut g = FormatGate::default();
        let mut audio = Vec::new();
        for chunk in wav.chunks(7) {
            audio.extend_from_slice(&g.feed(chunk));
        }
        assert!(g.is_wav());
        assert_eq!(audio, pcm, "only header bytes were stripped");
    }

    #[test]
    fn gate_handles_extra_chunks_before_data() {
        let mut wav = b"RIFFxxxyWAVE".to_vec();
        wav.extend(b"LIST");
        wav.extend_from_slice(&8u32.to_le_bytes());
        wav.extend_from_slice(b"isomisom");
        wav.extend(b"fact");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0u8; 4]);
        wav.extend(b"data");
        wav.extend_from_slice(&2u32.to_le_bytes());
        wav.extend_from_slice(&[7u8, 9]);

        let mut g = FormatGate::default();
        let audio = g.feed(&wav).to_vec();
        assert!(g.is_wav());
        assert_eq!(audio, vec![7, 9]);
    }

    #[test]
    fn command_override_is_verbatim_candidate_probe_is_not_broken() {
        let over = Some(vec!["weird".into(), "cmd".into()]);
        assert_eq!(
            resolve_command(&over, vec![]),
            Some(vec!["weird".to_string(), "cmd".to_string()])
        );
        assert_eq!(resolve_command(&Some(vec![]), vec![vec!["x".into()]]), None);
        let mut argvs = record_command_candidates();
        argvs.extend(play_command_candidates());
        // Candidate lists must at least be well-formed argvs even when the
        // probe machine has no audio stack installed.
        for argv in &argvs {
            assert!(!argv.is_empty() && program_exists(&argv[0]), "{argv:?}");
        }
    }
}
