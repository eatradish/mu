use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::exit,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use dialoguer::{Input, Select, console, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_LENGTH, HeaderMap, HeaderValue},
};
use serde::Deserialize;
use std::fmt::Display;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Deserialize)]
struct SearchResult {
    result: SearchResultInner,
}

#[derive(Debug, Deserialize)]
struct SearchResultInner {
    list: Vec<SongDetail>,
}

#[derive(Debug, Deserialize)]
struct SongDetail {
    platform: String,
    id: String,
    name: String,
    singers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SongDownloadUrl {
    success: bool,
    result: Option<String>,
}

impl Display for SongDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.singers.join(", "))?;
        write!(f, " - ")?;
        write!(f, "{}", self.name)?;
        Ok(())
    }
}

#[derive(Debug, Parser)]
struct Args {
    /// Song search name
    name: String,
    /// file format
    #[arg(short, long, default_value = "flac")]
    format: Format,
    /// Download file platform
    #[arg(long, default_value = "kuwo")]
    platform: Platform,
    /// Download to
    #[arg(short, long, default_value = ".")]
    path: PathBuf,
}

#[derive(Debug, ValueEnum, Clone, Copy)]
enum Format {
    Flac,
    Mp3128,
    Mp3320,
}

#[derive(Debug, ValueEnum, Clone, Copy)]
enum Platform {
    Kuwo,
    Kugou,
    Migu,
}

impl Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Platform::Kuwo => "kuwo",
                Platform::Kugou => "kugou",
                Platform::Migu => "migu",
            }
        )
    }
}

impl Format {
    fn download_url_str(&self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3128 => "128",
            Format::Mp3320 => "320",
        }
    }

    fn file_format(&self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Mp3128 | Format::Mp3320 => "mp3",
        }
    }
}

#[tokio::main]
async fn main() {
    let Args {
        name,
        path,
        format,
        platform,
    } = Args::parse();

    ctrlc::set_handler(|| {
        let s = console::Term::stdout();
        s.show_cursor().ok();
    })
    .expect("Failed to set ctrlc handler");

    let client = client().unwrap();
    let result_list = search(&client, &name, platform).await.unwrap();
    let list = result_list.result.list;

    let select = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select")
        .default(0)
        .items(&list)
        .interact()
        .inspect_err(|e| {
            let dialoguer::Error::IO(e) = e;
            if e.kind() == ErrorKind::Interrupted {
                exit(130);
            }
        })
        .unwrap();

    let song = &list[select];
    let result = get_download_url(&client, song, format).await.unwrap();

    download(&client, &result, &path, &song.to_string(), format)
        .await
        .unwrap();
}

async fn search(client: &Client, name: &str, platform: Platform) -> Result<SearchResult> {
    let resp = client
        .get(format!("https://api.flac.life/search/{platform}"))
        .query(&[("keyword", name), ("page", "1"), ("size", "30")])
        .headers(json_header()?)
        .send()
        .await?
        .error_for_status()?;

    let json = resp.json::<_>().await?;

    Ok(json)
}

async fn get_download_url(client: &Client, song: &SongDetail, format: Format) -> Result<String> {
    let mu_unlock_file = dirs::cache_dir()
        .context("Failed to get cache dir")?
        .join("mu_unlock");

    let unlock_code = tokio::fs::read_to_string(&mu_unlock_file)
        .await
        .map(|s| s.trim().to_string());

    let unlock_code = if let Ok(unlock_code) = unlock_code {
        unlock_code
    } else {
        input_unlock_code(&mu_unlock_file).await?
    };

    let json = build_download_url_resp(client, song, unlock_code, format).await?;

    if json.success {
        Ok(json.result.unwrap())
    } else {
        let unlock_code = input_unlock_code(&mu_unlock_file).await?;
        let json = build_download_url_resp(client, song, unlock_code, format).await?;

        if json.success {
            Ok(json.result.unwrap())
        } else {
            bail!("Failed to get download url")
        }
    }
}

async fn input_unlock_code(mu_unlock_file: &Path) -> Result<String, anyhow::Error> {
    let code = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Unlock code (Wechat @黑话君，输入“音乐密码”)")
        .interact()?;

    tokio::fs::write(mu_unlock_file, &code).await?;

    Ok(code)
}

async fn build_download_url_resp(
    client: &Client,
    song: &SongDetail,
    unlock_code: String,
    format: Format,
) -> Result<SongDownloadUrl, anyhow::Error> {
    let json = client
        .get(format!(
            "https://api.flac.life/url/{}/{}/{}",
            song.platform,
            song.id,
            format.download_url_str()
        ))
        .headers(json_header()?)
        .header("unlockcode", unlock_code)
        .send()
        .await?
        .error_for_status()?
        .json::<SongDownloadUrl>()
        .await?;

    Ok(json)
}

async fn download(
    client: &Client,
    url: &str,
    path: &Path,
    name: &str,
    format: Format,
) -> Result<()> {
    let mut f =
        tokio::fs::File::create(path.join(format!("{name}.{}", format.file_format()))).await?;

    let mut resp = client.get(url).send().await?.error_for_status()?;

    let resp_head = resp.headers();

    let total_size = resp_head
        .get(CONTENT_LENGTH)
        .map(|x| x.to_owned())
        .unwrap_or(HeaderValue::from(0));

    let total_size = total_size
        .to_str()
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or_default();

    let pb = ProgressBar::new(total_size).with_style(
        ProgressStyle::with_template("{spinner:.green} ({decimal_bytes}/{decimal_total_bytes}) [{wide_bar:.cyan/blue}] {percent}%")
            .unwrap()
            .progress_chars("=>-"),
    );

    while let Ok(Some(chunk)) = resp.chunk().await {
        f.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }

    f.shutdown().await?;
    pb.finish_and_clear();

    Ok(())
}

fn client() -> Result<Client, anyhow::Error> {
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:146.0) Gecko/20100101 Firefox/146.0")
        .build()?;

    Ok(client)
}

fn json_header() -> Result<HeaderMap, anyhow::Error> {
    let mut headers = HeaderMap::new();

    headers.insert(
        ACCEPT,
        "application/json, text/javascript, */*; q=0.01"
            .parse()
            .context("Failed to parse accept value")?,
    );

    Ok(headers)
}
