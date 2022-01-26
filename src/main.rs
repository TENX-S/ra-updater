// #![feature(write_all_vectored)]
use anyhow::Result;
use bytes::Buf;
use clap::{Parser, Subcommand};
use flate2::bufread::GzDecoder;
// use rayon::prelude::*;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, RANGE};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::Instant;

const RA_DL_BASE: &str = "https://github.com/rust-analyzer/rust-analyzer/releases/";
const RA_REL_API_BASE: &str = "https://api.github.com/repos/rust-analyzer/rust-analyzer/releases/";
const MIRROR: &str = "https://github.91chi.fun//";
const PAR_DL_BUF_SIZE: u64 = 512 * 1024;
static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

#[derive(Debug)]
struct RaUpdater {
    cli: Cli,
    ver: RaVersion,
}

impl RaUpdater {
    fn new() -> Result<Self> {
        Ok(Self {
            cli: Cli::parse(),
            ver: RaVersion::load_from_file()?,
        })
    }
}

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    /// Accelerate downloads for users in China
    #[clap(short = 'a', long, conflicts_with = "check")]
    mirror: bool,

    /// Just to check if there is a available update
    #[clap(short, long)]
    check: bool,

    /// Download rust-analyzer in multi-threaded way
    #[clap(short, long, conflicts_with = "check")]
    mt: bool,

    /// Force to update rust-analyzer
    #[clap(short, long, conflicts_with = "check")]
    force: bool,

    #[clap(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug)]
struct PartialRangeIter {
    start: u64,
    end: u64,
}

impl PartialRangeIter {
    pub fn new(start: u64, end: u64) -> Self {
        PartialRangeIter { start, end }
    }
}

impl Iterator for PartialRangeIter {
    type Item = (HeaderValue, u64);
    fn next(&mut self) -> Option<Self::Item> {
        if self.start > self.end {
            None
        } else {
            let prev_start = self.start;
            self.start += std::cmp::min(PAR_DL_BUF_SIZE, self.end - self.start + 1);
            Some((
                HeaderValue::from_str(&format!("bytes={}-{}", prev_start, self.start - 1))
                    .expect("string provided by format!"),
                prev_start,
            ))
        }
    }
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Set update channel for rust-analyer
    Channel {
        /// stable or nightly
        value: String,

        /// Accelerate downloads for users in China
        #[clap(short = 'a', long, conflicts_with = "check")]
        mirror: bool,

        /// Download rust-analyzer in multi-threaded way
        #[clap(short, long, conflicts_with = "check")]
        mt: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct RaVersion {
    commitish: String,
    channel: String,
}

impl RaVersion {
    fn load_from_file() -> Result<Self> {
        let cfg_path = PathBuf::from(ra_home()).join("config.toml");
        if !cfg_path.exists() {
            let version = Self::load_from_exec();
            version.save()?;
            Ok(version)
        } else {
            let mut reader = BufReader::new(File::open(cfg_path)?);
            let mut content = String::new();
            reader.read_to_string(&mut content)?;
            Ok(toml::from_str(&content)?)
        }
    }

    fn load_from_exec() -> Self {
        let ra_output = Command::new(ra_path())
            .current_dir(ra_home())
            .arg("--version")
            .output()
            .expect("Failed to exec rust-analyzer!");
        let version =
            String::from_utf8(ra_output.stdout).expect("Failed to get output of rust-analyzer");
        let segments: Vec<&str> = version.split(" ").collect();
        Self {
            commitish: segments[1].trim().into(),
            channel: segments[3].trim().into(),
        }
    }

    fn set_channel(&mut self, channel: &str, mirror: bool, mt: bool) -> Result<()> {
        if self.channel != channel {
            println!("Switching to {} channel ...", channel);
            let dl_url = ra_remote(channel, mirror)?.1;
            ra_update(&dl_url, mt)?;
            *self = Self::load_from_exec();
            self.save()?
        }
        println!("Done");
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let cfg_path = PathBuf::from(ra_home()).join("config.toml");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(cfg_path)?;
        let mut writer = BufWriter::new(file);
        let content = toml::to_string(self)?;
        writer.write_all(content.as_bytes())?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let ra_updater = RaUpdater::new()?;
    let cli = ra_updater.cli;
    let mut ver = ra_updater.ver;

    let (api, dl_url) = &ra_remote(&ver.channel, cli.mirror)?;
    match cli.cmd {
        Some(Cmd::Channel {
            ref value,
            mirror,
            mt,
        }) => match value.as_ref() {
            "stable" | "nightly" => ver.set_channel(value, mirror, mt)?,
            _ => {}
        },
        None => {
            let up_to_date = check_update(api, &ver.commitish)?;
            if cli.check {
                if up_to_date {
                    println!("Already up-to-date")
                } else {
                    println!("Update available")
                }
                std::process::exit(0);
            }
            if up_to_date && !cli.force {
                println!("Already up-to-date");
                std::process::exit(0);
            }
            println!("Updating ...");
            ra_update(dl_url, cli.mt)?;
            println!("Done");
        }
    }

    Ok(())
}

fn ra_remote(channel: &str, mirror: bool) -> Result<(String, String)> {
    let mut api_tag = "latest";
    let mut dl_tag = "latest/download/";
    if channel == "nightly" {
        api_tag = "tags/nightly";
        dl_tag = "download/nightly/";
    }
    let api_url = format!("{}{}", RA_REL_API_BASE, api_tag);
    let mut dl_url = format!("{}{}{}", RA_DL_BASE, dl_tag, ra_name());
    if mirror {
        dl_url.insert_str(0, MIRROR);
    }
    Ok((api_url, dl_url))
}

fn check_update(api: &str, curr: &str) -> Result<bool> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()?;
    let resp = client.get(api).send()?.text()?;
    let body: Value = serde_json::from_str(&resp)?;
    let latest = body["target_commitish"]
        .as_str()
        .expect("`target_commitish` is not a string");
    Ok(latest.starts_with(curr))
}

fn ra_update(url: &str, mt: bool) -> Result<()> {
    let tmp_path = dirs_next::download_dir()
        .unwrap()
        .join("rust-analyzer_ra_updater.gz");
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?,
    );
    let now = Instant::now();
    if mt {
        use crossbeam::channel::unbounded;
        use tokio::runtime::{Handle, Runtime};
        use tokio::sync::mpsc::unbounded_channel;

        let rt = Runtime::new()?;
        rt.block_on(async {
            let client = Arc::new(reqwest::Client::new());
            let resp_header = client.head(url).send().await?;
            let content_length = resp_header
                .headers()
                .get(CONTENT_LENGTH)
                .expect("response doesn't include the content length");
            let size = u64::from_str(content_length.to_str()?)
                .expect("Error: Invalid Content-Length header");
            let mut dl = vec![];
            let (tx, mut rx) = unbounded_channel();
            let tx = Arc::new(tx);
            PartialRangeIter::new(0, size - 1).for_each(|(range, start)| {
                let url = url.to_owned();
                let client = client.clone();
                let txc = tx.clone();
                dl.push(tokio::spawn(async move {
                    let par_resp = client.get(url).header(RANGE, range).send().await?;
                    let status = par_resp.status();
                    if !(status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT) {
                        panic!("Error: Unexpected server response: {}", status);
                    }
                    let par_bytes = par_resp.bytes().await?;
                    txc.send((par_bytes, start))?;
                    Ok::<(), anyhow::Error>(())
                }));
            });
            println!("spawn time: {}", now.elapsed().as_secs_f64());
            let mut chunks_cnt = size / PAR_DL_BUF_SIZE + 1;
            println!("cnt: {}", chunks_cnt);

            tokio::task::block_in_place(move || {
                let (tx, _rx) = unbounded();
                Handle::current().block_on(async move {
                    while chunks_cnt != 0 {
                        if let Some((par_bytes, start)) = rx.recv().await {
                            tx.send((par_bytes, start))?;
                            chunks_cnt -= 1;
                        }
                    }
                    println!("Download: {}", now.elapsed().as_secs_f64());
                    Ok::<(), anyhow::Error>(())
                })?;
                while let Ok((par_bytes, start)) = _rx.recv() {
                    writer.seek(SeekFrom::Start(start))?;
                    writer.write_all(par_bytes.as_ref())?;
                }
                // use std::io::IoSlice;
                // let mut buf = vec![];
                // while let Ok((par_bytes, start)) = _rx.recv() {
                //     buf.push((par_bytes, start));
                // }
                // buf.par_sort_by_key(|(_, start)| *start);
                // let mut ra: Vec<IoSlice> = buf.iter().map(|(b, _)| IoSlice::new(b.as_ref())).collect();
                // writer.write_all_vectored(&mut ra)?;

                Ok::<(), anyhow::Error>(())
            })?;
            ra_extract(&tmp_path)?;
            fs::remove_file(tmp_path)?;
            Ok::<(), anyhow::Error>(())
        })?;

        return Ok(());
    }

    let mut bytes_reader = reqwest::blocking::get(url)?.bytes()?.reader();
    println!("Download: {}", now.elapsed().as_secs_f64());
    io::copy(&mut bytes_reader, &mut writer).unwrap();
    ra_extract(&tmp_path)?;
    fs::remove_file(tmp_path)?;

    Ok(())
}

fn ra_extract(path: impl AsRef<Path>) -> Result<()> {
    let reader = BufReader::new(File::open(path)?);
    let mut gz = GzDecoder::new(reader);
    let mut target = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(ra_path())
            .expect("Error: rust-analyzer is being used!"),
    );
    io::copy(&mut gz, &mut target)?;
    Ok(())
}

#[inline]
fn ra_path() -> PathBuf {
    PathBuf::from(ra_home()).join(ra_exec())
}

fn ra_home() -> String {
    if let Ok(ra_dir) = env::var("RA_HOME") {
        if !PathBuf::from(&ra_dir).exists() {
            eprintln!("The directory `{}` set by `RA_HOME` does not exist", ra_dir);
            std::process::exit(-1);
        } else {
            ra_dir
        }
    } else {
        eprintln!("Please set the RA_HOME env variable!");
        std::process::exit(-1);
    }
}

fn ra_name() -> String {
    let arch = {
        if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            ""
        }
    };
    let distributor = {
        if cfg!(target_os = "windows") {
            "pc-windows-msvc"
        } else if cfg!(target_os = "macos") {
            "apple-darwin"
        } else if cfg!(target_os = "linux") {
            "unknown-linux"
        } else {
            ""
        }
    };
    if arch.is_empty() || distributor.is_empty() {
        eprintln!("Not supported platform!");
        std::process::exit(-1);
    }
    format!("rust-analyzer-{}-{}.gz", arch, distributor)
}

#[inline]
fn ra_exec() -> String {
    let mut ra_exec = "rust-analyzer".to_owned();
    if cfg!(windows) {
        ra_exec.push_str(".exe")
    }
    ra_exec
}
