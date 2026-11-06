//! `lr`: Local Router's command line. For now it drives the speech engine
//! directly, which is how the engine is tested against Scribely beta.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use localrouter_hf::{Cache, CacheLocation, CacheState, ModelPlaces, Place, Setup};
use localrouter_speech::{Aheads, Asr, AsrOptions, Diarizer, Strategy, catalog};

#[derive(Parser)]
#[command(name = "lr", version, about = "Local Router command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Where models are (the Askantis models folder and the Hugging Face
    /// cache), which speech models are there, and where new ones go.
    Models,
    /// Download the speech models into the folder for new models, unless
    /// they're already in one of the folders.
    Fetch {
        /// Whisper model id (see `lr models`).
        #[arg(long, default_value = catalog::WHISPER_TURBO_Q8.id)]
        whisper: String,
        /// Where to put them: `askantis`, `huggingface` or a folder. Default:
        /// what Local Router uses, else the Hugging Face cache if this
        /// computer uses Hugging Face, else the Askantis folder.
        #[arg(long)]
        to: Option<String>,
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
        Command::Fetch { whisper, to } => fetch(&whisper, to.as_deref()),
        Command::Diarize { file, json } => diarize(file, json),
        Command::Transcribe { file, language, whisper, aheads, no_diarize, per_turn, json } => {
            transcribe(file, language, whisper, aheads, no_diarize, per_turn, json)
        }
    }
}

/// The model folders and where new models go.
struct Places {
    places: ModelPlaces,
    target: (Place, PathBuf),
}

impl Places {
    /// The target: `--to`, else what Local Router's service uses (its
    /// discovery file), else the rule for tinkerers (PROTOCOL.md 2).
    fn resolve(to: Option<&str>) -> Places {
        let places = ModelPlaces::resolve();
        let target = match to {
            Some(to) => match Place::parse(to) {
                Some(p) => (p, places.path(p, "")),
                None => (Place::Custom, PathBuf::from(to)),
            },
            None => match localrouter_core::Endpoint::read(&localrouter_core::endpoint_path()) {
                Ok(ep) if ep.download_target.is_some() => {
                    let place = ep.download_target.as_deref().and_then(Place::parse).unwrap_or(Place::Askantis);
                    let path = ep.download_path.map(PathBuf::from).unwrap_or_else(|| places.path(place, ""));
                    (place, path)
                }
                _ => {
                    let place = places.default_place(Setup::Tinker);
                    (place, places.path(place, ""))
                }
            },
        };
        Places { places, target }
    }

    fn place_of(&self, path: &std::path::Path) -> Place {
        if path == self.places.askantis {
            Place::Askantis
        } else if path == self.places.huggingface {
            Place::Huggingface
        } else {
            Place::Custom
        }
    }

    /// Folders that can be read now, target first.
    fn usable(&self) -> Vec<(Place, Cache)> {
        self.places
            .roots(&self.target.1)
            .into_iter()
            .filter(|p| CacheLocation::check(p) == CacheState::Ok)
            .map(|p| (self.place_of(&p), Cache::new(p)))
            .collect()
    }

    /// The folder for new models, if it can be used.
    fn target_cache(&self) -> Result<Cache> {
        let path = &self.target.1;
        match CacheLocation::check(path) {
            CacheState::Ok => Ok(Cache::new(path)),
            CacheState::MissingVolume => bail!("{} is on a drive that isn't connected", path.display()),
            CacheState::NotAllowed => bail!("macOS doesn't allow access to {} (Privacy & Security → Files & Folders)", path.display()),
            CacheState::Unwritable => bail!("{} can't be written", path.display()),
        }
    }
}

/// The first folder that has a Whisper model.
fn find_whisper(roots: &[(Place, Cache)], m: catalog::WhisperModel) -> Result<Option<(Place, PathBuf)>> {
    for (place, cache) in roots {
        if let Some(p) = catalog::whisper_present(cache, m)? {
            return Ok(Some((*place, p)));
        }
    }
    Ok(None)
}

/// The first folder that has every speaker model.
fn find_speakers(roots: &[(Place, Cache)]) -> Result<Option<(Place, PathBuf)>> {
    for (place, cache) in roots {
        if let Some(p) = catalog::speaker_models_present(cache)? {
            return Ok(Some((*place, p)));
        }
    }
    Ok(None)
}

fn models() -> Result<()> {
    let p = Places::resolve(None);
    let state = |path: &std::path::Path| CacheLocation::check(path).as_str();
    println!("Askantis models folder: {} ({})", p.places.askantis.display(), state(&p.places.askantis));
    println!("Hugging Face cache:     {} ({})", p.places.huggingface.display(), state(&p.places.huggingface));
    println!("New models go to:       {} ({})", p.target.0.as_str(), p.target.1.display());
    let roots = p.usable();
    let line = |found: Option<(Place, PathBuf)>, missing: String| match found {
        Some((place, path)) => format!("present  {:<11}  {}", place.as_str(), path.display()),
        None => missing,
    };
    for m in catalog::LISTED_WHISPER_MODELS {
        println!("  {:<32} {}", m.id, line(find_whisper(&roots, *m)?, format!("missing  ~{} MB", m.approx_bytes / 1_000_000)));
    }
    println!("  {:<32} {}", "speaker-diarization", line(find_speakers(&roots)?, "missing".into()));
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

fn fetch(whisper: &str, to: Option<&str>) -> Result<()> {
    let p = Places::resolve(to);
    let model = catalog::LISTED_WHISPER_MODELS
        .iter()
        .copied()
        .find(|m| m.id == whisper)
        .with_context(|| format!("unknown Whisper model {whisper} (choose one of `lr models`)"))?;
    let roots = p.usable();
    let found_whisper = find_whisper(&roots, model)?;
    let found_speakers = find_speakers(&roots)?;
    let target = if found_whisper.is_none() || found_speakers.is_none() {
        if p.target.0 == Place::Askantis {
            p.places.prepare_askantis().with_context(|| format!("creating {}", p.places.askantis.display()))?;
        }
        Some(p.target_cache()?)
    } else {
        None
    };
    let path = match (found_whisper, &target) {
        (Some((_, path)), _) => path,
        (None, Some(cache)) => catalog::ensure_whisper(cache, model, &mut |d, t| progress_line(model.file, d, t))?,
        (None, None) => unreachable!(),
    };
    eprintln!("\n{}", path.display());
    let dir = match (found_speakers, &target) {
        (Some((_, dir)), _) => dir,
        (None, Some(cache)) => catalog::ensure_speaker_models(cache, &mut |f, d, t| progress_line(f, d, t))?,
        (None, None) => unreachable!(),
    };
    eprintln!("\n{}", dir.display());
    Ok(())
}

/// Every folder with models, for reading.
fn roots() -> Vec<(Place, Cache)> {
    Places::resolve(None).usable()
}

fn diarize(file: PathBuf, json: Option<PathBuf>) -> Result<()> {
    let t0 = Instant::now();
    let roots = roots();
    let (_, dir) = find_speakers(&roots)?.context("the speaker models aren't downloaded yet; run `lr fetch`")?;
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
    let roots = roots();
    let model_path = match catalog::whisper_model(&whisper) {
        Some(m) => find_whisper(&roots, m)?
            .with_context(|| format!("{} isn't downloaded yet; run `lr fetch --whisper {}`", m.id, m.id))?
            .1,
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
        let (_, dir) = find_speakers(&roots)?.context("the speaker models aren't downloaded yet; run `lr fetch`")?;
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
