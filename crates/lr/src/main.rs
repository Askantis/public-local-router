//! `lr`: Local Router's command line. For now it drives the speech engine
//! directly, which is how the engine is tested against Scribely beta.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use localrouter_hf::{Cache, CacheLocation, CacheState};
use localrouter_speech::{Aheads, Asr, AsrOptions, Diarizer, Strategy, catalog};

#[derive(Parser)]
#[command(name = "lr", version, about = "Local Router command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Where the Hugging Face cache is, and which speech models are in it.
    Models,
    /// Download the speech models into the Hugging Face cache.
    Fetch {
        /// Whisper model id (see `lr models`).
        #[arg(long, default_value = catalog::WHISPER_TURBO_Q8.id)]
        whisper: String,
    },
    /// Who speaks when, as JSON turns.
    Diarize {
        file: PathBuf,
        /// Write the turns as JSON to this file instead of printing them.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Transcribe an audio file, with speakers.
    Transcribe {
        file: PathBuf,
        /// ISO 639-1 language, `gsw` for Swiss German, or `auto`.
        #[arg(long, default_value = "auto")]
        language: String,
        /// Whisper model id from the catalog, or a path to a ggml file.
        #[arg(long, default_value = catalog::WHISPER_TURBO_Q8.id)]
        whisper: String,
        /// Alignment heads for a fine-tuned model, as "layer:head,layer:head".
        #[arg(long)]
        aheads: Option<String>,
        /// Don't separate speakers.
        #[arg(long)]
        no_diarize: bool,
        /// Transcribe each speaker turn alone (like Scribely beta) instead of
        /// windows that span speaker changes.
        #[arg(long)]
        per_turn: bool,
        /// Write the transcript as JSON to this file instead of printing text.
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Models => models(),
        Command::Fetch { whisper } => fetch(&whisper),
        Command::Diarize { file, json } => diarize(file, json),
        Command::Transcribe { file, language, whisper, aheads, no_diarize, per_turn, json } => {
            transcribe(file, language, whisper, aheads, no_diarize, per_turn, json)
        }
    }
}

fn cache() -> Result<Cache> {
    let loc = CacheLocation::resolve();
    match loc.state {
        CacheState::Ok => Ok(Cache::new(loc.hub)),
        CacheState::MissingVolume => bail!("the Hugging Face cache {} is on a drive that isn't connected", loc.hub.display()),
        CacheState::Unwritable => bail!("the Hugging Face cache {} can't be written", loc.hub.display()),
    }
}

fn models() -> Result<()> {
    let loc = CacheLocation::resolve();
    println!("Hugging Face cache: {} ({:?})", loc.hub.display(), loc.state);
    let cache = Cache::new(&loc.hub);
    for m in catalog::WHISPER_MODELS {
        let state = match catalog::whisper_present(&cache, *m)? {
            Some(p) => format!("present  {}", p.display()),
            None => format!("missing  ~{} MB", m.approx_bytes / 1_000_000),
        };
        println!("  {:<32} {state}", m.id);
    }
    let speakers = match catalog::speaker_models_present(&cache)? {
        Some(p) => format!("present  {}", p.display()),
        None => "missing".into(),
    };
    println!("  {:<32} {speakers}", "speaker-diarization");
    Ok(())
}

fn progress_line(label: &str, done: u64, total: Option<u64>) {
    let mb = |b: u64| b as f64 / 1e6;
    match total {
        Some(t) if t > 0 => eprint!("\r{label}: {:.0} / {:.0} MB ({:.0}%)   ", mb(done), mb(t), done as f64 * 100.0 / t as f64),
        _ => eprint!("\r{label}: {:.0} MB   ", mb(done)),
    }
    let _ = std::io::stderr().flush();
}

fn fetch(whisper: &str) -> Result<()> {
    let cache = cache()?;
    let model = catalog::whisper_model(whisper).with_context(|| format!("unknown Whisper model {whisper}"))?;
    let path = catalog::ensure_whisper(&cache, model, &mut |d, t| progress_line(model.file, d, t))?;
    eprintln!("\n{}", path.display());
    let dir = catalog::ensure_speaker_models(&cache, &mut |f, d, t| progress_line(f, d, t))?;
    eprintln!("\n{}", dir.display());
    Ok(())
}

fn diarize(file: PathBuf, json: Option<PathBuf>) -> Result<()> {
    let t0 = Instant::now();
    let cache = cache()?;
    let dir = catalog::speaker_models_present(&cache)?
        .context("the speaker models aren't downloaded yet; run `lr fetch`")?;
    let audio = localrouter_audio::decode_file(&file)?;
    let mut diarizer = Diarizer::load(&dir)?;
    let t_load = t0.elapsed();
    let turns = localrouter_speech::label_speakers(diarizer.run(&audio.samples)?);
    eprintln!(
        "{:.1} s of audio: {} turns, {} speakers, {:.0}× real time after {:.2} s loading",
        audio.duration_s(),
        turns.len(),
        turns.iter().map(|t| &t.speaker).collect::<std::collections::HashSet<_>>().len(),
        audio.duration_s() / (t0.elapsed() - t_load).as_secs_f64().max(1e-6),
        t_load.as_secs_f64(),
    );
    match json {
        Some(path) => std::fs::write(&path, serde_json::to_string_pretty(&turns)?)?,
        None => {
            for t in &turns {
                println!("{} {} {}", ts(t.start), ts(t.end), t.speaker);
            }
        }
    }
    Ok(())
}

fn parse_aheads(s: &str) -> Result<Vec<(i32, i32)>> {
    s.split(',')
        .map(|pair| {
            let (l, h) = pair.split_once(':').context("aheads look like 2:4,3:6")?;
            Ok((l.trim().parse()?, h.trim().parse()?))
        })
        .collect()
}

fn transcribe(
    file: PathBuf,
    language: String,
    whisper: String,
    aheads: Option<String>,
    no_diarize: bool,
    per_turn: bool,
    json: Option<PathBuf>,
) -> Result<()> {
    let t0 = Instant::now();
    let cache = cache()?;
    let model_path = match catalog::whisper_model(&whisper) {
        Some(m) => catalog::whisper_present(&cache, m)?
            .with_context(|| format!("{} isn't downloaded yet; run `lr fetch --whisper {}`", m.id, m.id))?,
        None => PathBuf::from(&whisper),
    };
    let aheads = match aheads {
        Some(s) => Aheads::Custom(parse_aheads(&s)?),
        None => Aheads::LargeV3Turbo,
    };
    let audio = localrouter_audio::decode_file(&file)?;
    let t_decode = t0.elapsed();
    let asr = Asr::load(&model_path, aheads)?;
    let mut diarizer = if no_diarize {
        None
    } else {
        let dir = catalog::speaker_models_present(&cache)?
            .context("the speaker models aren't downloaded yet; run `lr fetch`")?;
        Some(Diarizer::load(&dir)?)
    };
    let t_load = t0.elapsed();
    let opts = AsrOptions { language: (language != "auto").then_some(language), ..Default::default() };
    let strategy = if per_turn { Strategy::Turns } else { Strategy::Windows };
    let transcript = localrouter_speech::transcribe_with(&audio, &asr, diarizer.as_mut(), &opts, strategy, &mut |i, n| {
        eprint!("\rtranscribing {i}/{n}   ");
        let _ = std::io::stderr().flush();
    })?;
    let total = t0.elapsed();
    eprintln!(
        "\n{:.1} s of audio in {:.2} s (decode {:.2} s, load {:.2} s): {:.1}× real time",
        audio.duration_s(),
        total.as_secs_f64(),
        t_decode.as_secs_f64(),
        (t_load - t_decode).as_secs_f64(),
        audio.duration_s() / (total - t_load).as_secs_f64().max(1e-6),
    );
    match json {
        Some(path) => std::fs::write(&path, serde_json::to_string_pretty(&transcript)?)?,
        None => {
            for t in &transcript.turns {
                println!("[{} - {}] {}: {}", ts(t.start), ts(t.end), t.speaker, t.text);
            }
        }
    }
    Ok(())
}

fn ts(s: f64) -> String {
    let ms = (s * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:02}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms / 10 % 100)
}
