// #![feature(write_all_vectored)]
#![forbid(unsafe_code)]

use anyhow::Result;
use bytes::Buf;
use clap::{ArgEnum, Parser, Subcommand};
use flate2::bufread::GzDecoder;
// use rayon::prelude::*;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, RANGE};
use reqwest::StatusCode;
use serde_json::Value;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, prelude::*, BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use strum_macros::Display;

const RA_DNLD_BASE: &str = "https://github.com/rust-analyzer/rust-analyzer/releases/";
const RA_REL_API_BASE: &str = "https://api.github.com/repos/rust-analyzer/rust-analyzer/releases/";
const MIRROR: &str = "https://github.91chi.fun//";
const PAR_DNLD_SIZE: u64 = 512 * 1024;
static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
    /// F** the [GFW](https://wikipedia.org/wiki/Great_Firewall)
    #[clap(short = 'a', long, conflicts_with = "check")]
    mirror: bool,

    /// Just to check if there is a available update
    #[clap(short, long)]
    check: bool,

    /// Download rust-analyzer in multi-threaded way
    #[clap(short, long, conflicts_with = "check")]
    mt: bool,

    /// Force to update rust-analyzer in current release channel
    #[clap(short, long, conflicts_with = "check")]
    force: bool,

    #[clap(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Set release channel for rust-analyzer
    Channel {
        /// stable or nightly
        #[clap(arg_enum)]
        rel_chan: ReleaseChannel,

        /// F** the [GFW](https://wikipedia.org/wiki/Great_Firewall)
        #[clap(short = 'a', long, conflicts_with = "check")]
        mirror: bool,

        /// Download rust-analyzer in multi-threaded way
        #[clap(short, long, conflicts_with = "check")]
        mt: bool,
    },
}

#[derive(Display, Debug, ArgEnum, Clone, Copy, PartialEq)]
#[strum(serialize_all = "snake_case")]
enum ReleaseChannel {
    Stable,
    Nightly,
}

#[derive(Debug)]
struct RaVersion {
    commitish: String,
    rel_chan: ReleaseChannel,
}

impl RaVersion {
    fn parse(cli: &Cli) -> Result<Self> {
        if !ra_exec_path().exists() {
            println!("rust-analyzer not found. Downloading ...");
            if let Some(Cmd::Channel {
                rel_chan,
                mirror,
                mt,
            }) = cli.cmd
            {
                let dnld_url = ra_remote(rel_chan, mirror)?.1;
                ra_download(&dnld_url, mt)?;
                std::process::exit(0);
            } else {
                let dnld_url = ra_remote(ReleaseChannel::Stable, cli.mirror)?.1;
                ra_download(&dnld_url, cli.mt)?;
                std::process::exit(0);
            }
        } else {
            let version = ra_version()?;
            let segments: Vec<&str> = version.split(' ').collect();
            let rel_chan = if segments[3].trim() == "stable" {
                ReleaseChannel::Stable
            } else {
                ReleaseChannel::Nightly
            };
            Ok(Self {
                commitish: segments[1].trim().into(),
                rel_chan,
            })
        }
    }

    fn set_channel(&mut self, rel_chan: ReleaseChannel, mirror: bool, mt: bool) -> Result<()> {
        if self.rel_chan != rel_chan {
            println!("Switching to {} channel ...", rel_chan);
            let dnld_url = ra_remote(rel_chan, mirror)?.1;
            ra_download(&dnld_url, mt)?;
            println!("Done");
            Ok(())
        } else {
            println!("You are already in {} channel", rel_chan);
            Ok(())
        }
    }
}

#[derive(Debug)]
struct ParHeadRange {
    start: u64,
    end: u64,
}

impl ParHeadRange {
    pub fn new(start: u64, end: u64) -> Self {
        ParHeadRange { start, end }
    }
}

impl Iterator for ParHeadRange {
    type Item = (HeaderValue, u64);
    fn next(&mut self) -> Option<Self::Item> {
        if self.start > self.end {
            None
        } else {
            let prev_start = self.start;
            self.start += std::cmp::min(PAR_DNLD_SIZE, self.end - self.start + 1);
            Some((
                HeaderValue::from_str(&format!("bytes={}-{}", prev_start, self.start - 1)).unwrap(), // PANIC: Never
                prev_start,
            ))
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut ver = RaVersion::parse(&cli)?;

    let (rel_api, dnld_url) = &ra_remote(ver.rel_chan, cli.mirror)?;
    match cli.cmd {
        Some(Cmd::Channel {
            rel_chan,
            mirror,
            mt,
        }) => ver.set_channel(rel_chan, mirror, mt)?,
        None => {
            let up_to_date = check_update(rel_api, &ver.commitish)?;
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
            ra_download(dnld_url, cli.mt)?;
            println!("Done");
        }
    }

    Ok(())
}

fn check_update(rel_api: &str, curr: &str) -> Result<bool> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .build()?;
    let resp = client.get(rel_api).send()?.text()?;
    let body: Value = serde_json::from_str(&resp)?;
    let latest = body["target_commitish"]
        .as_str()
        .expect("`target_commitish` is not a string");
    Ok(latest.starts_with(curr))
}

fn ra_download(dnld_url: &str, mt: bool) -> Result<()> {
    let temp = dirs_next::download_dir()
        .unwrap() // Never panic, ra_name() guarantees it
        .join("rust-analyzer_ra_updater_temp.gz");
    let mut writer = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)?,
    );

    // FIXME: handle timeout case
    if mt {
        download_mt(dnld_url, &mut writer)?
    } else {
        let now = Instant::now();
        let mut bytes_reader = reqwest::blocking::get(dnld_url)?.bytes()?.reader();
        println!("Download: {:.2}s", now.elapsed().as_secs_f64());
        io::copy(&mut bytes_reader, &mut writer)?;
    }

    ra_replace(&temp)?;
    fs::remove_file(temp)?;

    Ok(())
}

#[tokio::main]
async fn download_mt(dnld_url: &str, writer: &mut BufWriter<File>) -> Result<()> {
    use crossbeam::channel::unbounded;
    use tokio::sync::mpsc::unbounded_channel;

    #[cfg(feature = "debug")]
    console_subscriber::init();

    let resp_start = Instant::now();
    let client = Arc::new(reqwest::Client::new());
    let resp_header = client.head(dnld_url).send().await?;
    println!(
        "Response with CONTENT_LENGTH: {:.2}s",
        resp_start.elapsed().as_secs_f64()
    );

    let content_length = resp_header
        .headers()
        .get(CONTENT_LENGTH)
        .expect("response doesn't include the content length");
    let size =
        u64::from_str(content_length.to_str()?).expect("Error: Invalid Content-Length header");
    let mut chunks_cnt = size / PAR_DNLD_SIZE + 1;
    let (tx, mut rx) = unbounded_channel();
    let tx = Arc::new(tx);
    let spawn_start = Instant::now();

    let mut fut_size = None;
    ParHeadRange::new(0, size - 1).for_each(|(range, start)| {
        let url = dnld_url.to_owned();
        let client = client.clone();
        let txc = tx.clone();
        let fut = async move {
            let par_resp = client.get(url).header(RANGE, range).send().await?;
            let status = par_resp.status();
            if !(status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT) {
                anyhow::bail!("Error: Unexpected server response: {}", status);
            }
            let par_bytes = par_resp.bytes().await?;
            txc.send((par_bytes, start))?;
            Ok::<(), anyhow::Error>(())
        };
        if fut_size.is_none() {
            fut_size.replace(std::mem::size_of_val(&fut));
        }

        tokio::spawn(fut);
    });
    println!("Task size: {} bytes", fut_size.unwrap_or_default());

    println!(
        "Spawn {} tasks: {}us",
        chunks_cnt,
        spawn_start.elapsed().as_micros(),
    );

    let (f_tx, f_rx) = unbounded();
    tokio::spawn(async move {
        while chunks_cnt != 0 {
            if let Some((par_bytes, start)) = rx.recv().await {
                f_tx.send((par_bytes, start))?;
                chunks_cnt -= 1;
            }
        }
        println!("Download: {:.2}s", spawn_start.elapsed().as_secs_f64());
        Ok::<(), anyhow::Error>(())
    });
    while let Ok((par_bytes, start)) = f_rx.recv() {
        writer.seek(SeekFrom::Start(start))?;
        writer.write_all(par_bytes.as_ref())?;
    }

    // use std::io::IoSlice;
    // let mut buf = vec![];
    // while let Ok((par_bytes, start)) = f_rx.recv() {
    //     buf.push((par_bytes, start));
    // }
    // buf.par_sort_by_key(|(_, start)| *start);
    // let mut ra: Vec<IoSlice> = buf.iter().map(|(b, _)| IoSlice::new(b.as_ref())).collect();
    // writer.write_all_vectored(&mut ra)?;
    Ok::<(), anyhow::Error>(())
}

fn ra_replace(path: impl AsRef<Path>) -> Result<()> {
    let reader = BufReader::new(File::open(path)?);
    let mut gz = GzDecoder::new(reader);
    let mut target = BufWriter::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(ra_exec_path())?,
    );
    io::copy(&mut gz, &mut target)?;
    Ok(())
}

fn ra_version() -> Result<String> {
    let ra_output = Command::new(ra_exec_path())
        .current_dir(ra_home())
        .arg("--version")
        .output()?;
    Ok(String::from_utf8(ra_output.stdout)?)
}

#[inline]
fn ra_remote(rel_chan: ReleaseChannel, mirror: bool) -> Result<(String, String)> {
    let (api_tag, dnld_tag) = match rel_chan {
        ReleaseChannel::Stable => ("latest", "latest/download/"),
        ReleaseChannel::Nightly => ("tags/nightly", "download/nightly/"),
    };
    let rel_api = format!("{}{}", RA_REL_API_BASE, api_tag);
    let mut dnld_url = format!("{}{}{}", RA_DNLD_BASE, dnld_tag, ra_name());
    if mirror {
        dnld_url.insert_str(0, MIRROR);
    }
    Ok((rel_api, dnld_url))
}

#[inline]
fn ra_exec_path() -> PathBuf {
    let mut ra_exec = "rust-analyzer".to_owned();
    if cfg!(target_os = "windows") {
        ra_exec.push_str(".exe")
    }
    PathBuf::from(ra_home()).join(ra_exec)
}

#[inline]
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

#[inline]
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
            if cfg!(target_env = "gnu") {
                "unknown-linux-gnu"
            } else if cfg!(target_env = "musl") {
                "unknown-linux-musl"
            } else {
                ""
            }
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
