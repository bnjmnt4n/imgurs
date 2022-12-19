use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{ArgGroup, Parser};
use futures_util::{future, stream, StreamExt};
use humansize::{format_size, DECIMAL};
use indicatif::ProgressBar;
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
    #[allow(unused)]
    title: Option<String>,
    #[allow(unused)]
    description: Option<String>,
    link: String,
    size: u64,
    #[serde(rename = "type")]
    content_type: String,
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

async fn download_file(client: &Client, download_url: String, destination: &PathBuf) -> Result<()> {
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
            let mut file = tokio::fs::File::create(destination).await?;
            let mut res = client.get(download_url).send().await?;
            while let Some(chunk) = res.chunk().await?.as_deref() {
                file.write_all(chunk).await?
            }

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

    let response = client
        .get(format!("https://api.imgur.com/3/album/{}", args.album_id))
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
            let temp_filename = format!("~!{}", filename);
            let url = media.link.clone();

            (url, filename, temp_filename)
        });

        let pb = ProgressBar::new(num_files.try_into().unwrap());

        // TODO: collect errors.
        stream::iter(media)
            .map(|(url, filename, temp_filename)| async {
                let temp_path = destination.join(temp_filename);
                let path = destination.join(filename);

                let result: Result<()> = match download_file(&client, url, &temp_path).await {
                    Ok(ok) => {
                        tokio::fs::rename(temp_path, path)
                            .await
                            .with_context(|| "Unable to move temporary file")?;
                        Ok(ok)
                    }
                    Err(err) => {
                        // TODO: log error?
                        let _success = tokio::fs::remove_file(temp_path).await.is_ok();
                        Err(err)
                    }
                };

                result
            })
            .buffer_unordered(args.parallelism)
            .for_each(|result| {
                if let Err(error) = result {
                    println!("Error occured: {}", error);
                }
                pb.inc(1);
                future::ready(())
            })
            .await;

        pb.finish_with_message("Completed!");

        Ok(())
    } else {
        println!(
            "Failed to get album details with status code: {}",
            response.status
        );

        Ok(())
    }
}
