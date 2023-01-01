use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{ArgGroup, Parser};
use futures_util::{stream, StreamExt};
use humansize::{format_size, DECIMAL};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
#[command(group(
            ArgGroup::new("op")
                .args(["output", "details"]),
))]
struct Cli {
    /// ID of album to download.
    album_id: String,
    /// Output directory. Album will be downloaded to "$output/$album_name".
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Prints the album's details without downloading.
    #[arg(short, long)]
    details: bool,
    /// Number of files to download in parallel.
    #[arg(short, long, default_value_t = 8)]
    parallelism: usize,
    /// Imgur client ID for accessing the API. Default: $IMGUR_CLIENT_ID
    #[arg(short, long)]
    imgur_client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImgurResponse<T> {
    data: Option<T>,
    status: u64,
}

#[derive(Debug, Deserialize)]
struct ImgurAlbum {
    id: String,
    title: Option<String>,
    images: Vec<ImgurMedia>,
}

#[derive(Debug, Deserialize)]
struct ImgurMedia {
    id: String,
    title: Option<String>,
    description: Option<String>,
    link: String,
    datetime: i64,
    size: u64,
    #[serde(rename = "type")]
    content_type: String,
}

const IMGUR_ALBUM_URL_PREFIX: &str = "https://imgur.com/a/";
fn get_album_id(album_id: &str) -> &str {
    album_id
        .strip_prefix(IMGUR_ALBUM_URL_PREFIX)
        .unwrap_or(album_id)
}

fn get_media_type(content_type: &str) -> &str {
    let (_, content_type) = content_type.split_once("/").unwrap_or(("", "unknown"));
    if content_type == "jpeg" {
        "jpg"
    } else {
        content_type
    }
}

async fn prepare_directory(path: PathBuf) -> Result<()> {
    let metadata = tokio::fs::metadata(path.clone()).await;
    if let Err(e) = metadata {
        match e.kind() {
            std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir_all(path).await?;
                Ok(())
            }
            std::io::ErrorKind::PermissionDenied => {
                Err(e).with_context(|| "Permission denied when retrieving file metadata")
            }
            _ => Err(e).with_context(|| "Unable to retrieve file metadata"),
        }
    } else if metadata.unwrap().is_file() {
        Err(anyhow!("Destination is a file"))
    } else {
        Ok(())
    }
}

async fn download_file(
    client: &Client,
    pb: &ProgressBar,
    download_url: String,
    time_since_epoch: i64,
    destination: &PathBuf,
    temp_destination: &PathBuf,
) -> Result<()> {
    let download_url = reqwest::Url::parse(&download_url)
        .with_context(|| format!("Failed to parse URL: {}", download_url))?;
    let metadata = tokio::fs::metadata(destination.clone()).await;

    // Exit early if destination already exists.
    if metadata.is_ok() {
        return if metadata.unwrap().is_file() {
            Ok(())
        } else {
            Err(anyhow!("Found existing directory"))
        };
    }

    match metadata.unwrap_err().kind() {
        std::io::ErrorKind::NotFound => {
            // Download file.
            let mut file = tokio::fs::File::create(temp_destination).await?;
            let mut res = client.get(download_url).send().await?;
            while let Some(chunk) = res.chunk().await?.as_deref() {
                pb.inc(chunk.len() as u64);
                file.write_all(chunk).await?
            }

            // Rename file.
            tokio::fs::rename(temp_destination, destination)
                .await
                .with_context(|| "Unable to move temporary file")?;

            filetime::set_file_mtime(
                destination,
                filetime::FileTime::from_unix_time(time_since_epoch, 0),
            )
            .with_context(|| "Could not set file modified time")?;

            Ok(())
        }
        std::io::ErrorKind::PermissionDenied => {
            Err(anyhow!("Permission denied when retrieving file metadata",))
        }
        _ => Err(anyhow!("Unable to retrieve file metadata")),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    let client_id = args
        .imgur_client_id
        .unwrap_or_else(|| std::env::var("IMGUR_CLIENT_ID").unwrap_or_else(|_| "".to_owned()));
    let client = Client::builder().build()?;
    let is_display_details_only = args.details;
    let album_id = get_album_id(&args.album_id);

    let response = client
        .get(format!("https://api.imgur.com/3/album/{}", album_id))
        .header("Authorization", format!("Client-ID {}", client_id))
        .send()
        .await?
        .json::<ImgurResponse<ImgurAlbum>>()
        .await?;

    if let Some(data) = response.data {
        let title = data.title.unwrap_or_else(|| data.id);
        println!("Album: {}", title);

        let num_files = data.images.len();
        println!("Number of files: {}", num_files);

        let album_size: u64 = data.images.iter().map(|image| image.size).sum();
        println!("Total size: {}", format_size(album_size, DECIMAL));

        if is_display_details_only || num_files == 0 {
            return Ok(());
        }

        let destination = args.output.unwrap_or_else(|| {
            PathBuf::from(
                title
                    .clone()
                    .replace("\n", " ")
                    .replace(" : ", " - ")
                    .replace(": ", " - ")
                    .replace(":", "-")
                    .replace("/", "-"),
            )
        });

        prepare_directory(destination.clone()).await?;

        let width = {
            let mut width = num_files as i32;
            let mut count = 0;
            while width > 0 {
                width /= 10;
                count += 1;
            }
            count
        };

        let media = data.images.iter().enumerate().map(|(index, media)| {
            let title = media
                .title
                .as_ref()
                .map(|title| format!(" - {}", title))
                .unwrap_or("".to_string());
            let description = media
                .description
                .as_ref()
                .map(|description| format!(" - {}", description))
                .unwrap_or("".to_string());
            let filename = format!(
                "{:0>width$} - {}{}{}.{}",
                index + 1,
                media.id,
                title,
                description,
                get_media_type(&media.content_type),
                width = width
            );
            let url = media.link.clone();

            (url, media.size, filename, media.datetime)
        });

        let m = MultiProgress::new();
        let sty = ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {msg}")
        .unwrap()
        .progress_chars("#>-");

        let errors = stream::iter(media)
            .map(|(url, download_size, filename, time_since_epoch)| {
                let pb = m.clone().add(ProgressBar::new(download_size));
                pb.set_style(sty.clone());
                pb.set_message(filename.clone());
                let temp_filename = format!("~!{}", filename);

                let client = client.clone();
                let destination = destination.clone();
                let time_since_epoch = time_since_epoch.clone();

                async move {
                    let temp_path = destination.join(temp_filename);
                    let path = destination.join(filename.clone());

                    let result =
                        download_file(&client, &pb, url, time_since_epoch, &path, &temp_path).await;
                    if result.is_err() {
                        // TODO: log error?
                        let _success = tokio::fs::remove_file(temp_path).await.is_ok();
                    } else {
                        pb.finish_and_clear();
                    }

                    result.with_context(|| format!("Error downloading file {}", filename))
                }
            })
            .buffer_unordered(args.parallelism)
            .filter_map(|result| async {
                match result {
                    Ok(_) => None,
                    Err(err) => Some(err),
                }
            })
            .collect::<Vec<_>>()
            .await;

        println!(
            "Downloaded {}/{} files.\n",
            num_files - errors.len(),
            num_files
        );
        for error in errors {
            println!("{:?}\n", error);
        }

        Ok(())
    } else {
        println!(
            "Failed to get album details with status code: {}",
            response.status
        );

        Ok(())
    }
}
