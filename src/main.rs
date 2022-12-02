use std::convert::TryInto;
use std::path::PathBuf;

use clap::Parser;
use futures_util::{future, stream, StreamExt};
use indicatif::ProgressBar;
use reqwest::Client;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
struct Error {
    message: String,
}
impl Error {
    fn new(message: &str) -> Error {
        Error {
            message: message.to_owned(),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Error {}

#[derive(Parser)]
struct Cli {
    album_id: String,
    destination: Option<PathBuf>,
    parallelism: Option<usize>,
    imgur_client_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImgurResponse<T> {
    data: Option<T>,
    status: i64,
}

#[derive(Debug, Deserialize)]
struct ImgurAlbum {
    id: String,
    title: String,
    images: Vec<ImgurMedia>,
}

#[derive(Debug, Deserialize)]
struct ImgurMedia {
    id: String,
    title: Option<String>,
    description: Option<String>,
    link: String,
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

async fn prepare_directory(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = tokio::fs::metadata(path.clone()).await;
    if let Err(e) = metadata {
        return match e.kind() {
            std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir_all(path).await?;
                Ok(())
            }
            std::io::ErrorKind::PermissionDenied => Err(Box::new(Error::new(
                "Permission denied when retrieving file metadata",
            ))),
            _ => Err(Box::new(Error::new("Unable to retrieve file metadata"))),
        };
    }

    if metadata.unwrap().is_file() {
        Err(Box::new(Error::new("Destination is a file")))
    } else {
        Ok(())
    }
}

async fn download_file(
    client: &Client,
    download_url: reqwest::Url,
    destination: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = tokio::fs::metadata(destination.clone()).await;
    match metadata {
        // Exit early if file already exists.
        Ok(metadata) => {
            if metadata.is_file() {
                Ok(())
            } else {
                Err(Box::new(Error::new("Found existing directory")))
            }
        }
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => {
                let mut file = tokio::fs::File::create(destination).await?;
                let mut res = client.get(download_url).send().await?;

                while let Some(chunk) = res.chunk().await?.as_deref() {
                    file.write_all(chunk).await?
                }

                Ok(())
            }
            std::io::ErrorKind::PermissionDenied => Err(Box::new(Error::new(
                "Permission denied when retrieving file metadata",
            ))),
            _ => Err(Box::new(Error::new("Unable to retrieve file metadata"))),
        },
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Cli::parse();

    let client_id = args
        .imgur_client_id
        .unwrap_or_else(|| std::env::var("IMGUR_CLIENT_ID").unwrap_or_else(|_| "".to_owned()));
    let parallelism = args.parallelism.unwrap_or(8);
    let client = Client::builder().build()?;

    let response = client
        .get(format!("https://api.imgur.com/3/album/{}", args.album_id))
        .header("Authorization", format!("Client-ID {}", client_id))
        .send()
        .await?
        .json::<ImgurResponse<ImgurAlbum>>()
        .await?;

    if let Some(data) = response.data {
        println!("Album {}: {}", data.id, data.title);

        let size = data.images.len();
        println!("Number of files: {}", size);

        if size == 0 {
            return Ok(());
        }

        let destination = args.destination.unwrap_or_else(|| {
            PathBuf::from(
                data.title
                    .clone()
                    .replace(":", "-")
                    .replace("/", "-")
                    .replace(".", "-"),
            )
        });

        prepare_directory(destination.clone()).await?;

        let width = {
            let mut width = size as i32;
            let mut count = 0;
            while width > 0 {
                width /= 10;
                count += 1;
            }
            count
        };

        let media = data
            .images
            .iter()
            .enumerate()
            .map(|(i, media)| -> Result<_, String> {
                let filename = format!(
                    "{:0>width$} - {}.{}",
                    i,
                    media.id,
                    get_media_type(&media.content_type),
                    width = width
                );
                let url = reqwest::Url::parse(&media.link).map_err(|_| media.link.to_owned())?;

                Ok((url, filename))
            });

        let pb = ProgressBar::new(size.try_into().unwrap());

        stream::iter(media)
            .map(|result| async {
                let result: Result<(), Box<dyn std::error::Error>> = match result {
                    Ok((url, filename)) => {
                        let path = destination.join(filename);
                        download_file(&client, url, path).await
                    }
                    Err(link) => {
                        println!("Failed to parse URL {}", link);
                        Err(Box::new(Error::new(&format!(
                            "Failed to parse URL: {}",
                            link
                        ))))
                    }
                };
                result
            })
            .buffer_unordered(parallelism)
            .for_each(|_| {
                pb.inc(1);
                future::ready(())
            })
            .await;

        pb.finish_with_message("Completed!");

        Ok(())
    } else {
        println!("Failed to download with status code: {}", response.status);

        Ok(())
    }
}
