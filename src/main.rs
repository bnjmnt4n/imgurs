use std::convert::TryInto;
use std::path::PathBuf;

use clap::{ArgGroup, Parser};
use futures_util::{future, stream, StreamExt};
use humansize::{format_size, DECIMAL};
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

async fn prepare_directory(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = tokio::fs::metadata(path.clone()).await;
    if let Err(e) = metadata {
        match e.kind() {
            std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir_all(path).await?;
                Ok(())
            }
            // TODO: fix box usage?
            std::io::ErrorKind::PermissionDenied => Err(Box::new(Error::new(
                "Permission denied when retrieving file metadata",
            ))),
            _ => Err(Box::new(Error::new("Unable to retrieve file metadata"))),
        }
    } else if metadata.unwrap().is_file() {
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
                // TODO: save as temp file and rename.
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

        let media = data
            .images
            .iter()
            .enumerate()
            .map(|(index, media)| -> Result<_, String> {
                let filename = format!(
                    "{:0>width$} - {}.{}",
                    index + 1,
                    media.id,
                    get_media_type(&media.content_type),
                    width = width
                );
                let url = reqwest::Url::parse(&media.link).map_err(|_| media.link.to_owned())?;

                Ok((url, filename))
            });

        let pb = ProgressBar::new(num_files.try_into().unwrap());

        // TODO: collect errors.
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
            .buffer_unordered(args.parallelism)
            .for_each(|_| {
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
