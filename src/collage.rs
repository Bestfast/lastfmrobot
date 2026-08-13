use std::sync::LazyLock;

use ab_glyph::FontVec;
use anyhow::anyhow;
use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::{ImageBuffer, Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

use crate::api_requester::{Album, CLIENT_NOCACHE, cover_art_candidates};
use crate::navidrome;
use crate::config;

const FONT_SIZE: f32 = 24.0;
const TILE_PX: u32 = 300;
pub const MAX_SIZE: u32 = 7;
pub const MIN_SIZE: u32 = 1;

// Cover Art Archive redirects every size to archive.org without checking that the
// thumbnail exists, so a missing size shows up as a failed download here rather than at
// URL-building time. Last.fm has the same problem: the size its api hands out isn't
// always one the cdn actually has. Walk down the sizes until one comes back.
//
// The url we were given is tried first: tiles are only TILE_PX wide, so there is nothing
// to gain from pulling a larger variant when the size that came with the url works.
async fn fetch_album_art(album: Album, username: String) -> Result<Bytes, anyhow::Error> {
    let url = match album.album_art_url {
        Some(url) => url,
        None => return Err(anyhow!("no cover art url")),
    };

    // Gated users get Navidrome art first: release mbid -> Navidrome album -> local
    // cover, skipping the CAA round trip entirely. Navidrome answers a placeholder
    // image for albums without art; the resolver detects it and falls back to CAA.
    if navidrome::enabled_for(&username) {
        if let Some(release_mbid) = &album.release_mbid {
            if let Some(cover_url) = navidrome::resolve_cover_art_release(release_mbid).await
                && let Some(bytes) = crate::api_requester::cover_art_bytes(Some(&cover_url)).await
                && !bytes.is_empty()
            {
                return Ok(bytes);
            }
        }
    }

    // Fall back to CAA (or whatever url the api handed back).
    let mut last_err = anyhow!("no cover art url");

    let candidates = std::iter::once(url.clone())
        .chain(cover_art_candidates(&url).into_iter().filter(|c| *c != url));

    for candidate in candidates {
        match CLIENT_NOCACHE.get(&candidate).send().await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
                Ok(_) => last_err = anyhow!("empty response for {candidate}"),
                Err(e) => last_err = anyhow!(e),
            },
            Err(e) => last_err = anyhow!(e),
        }
    }

    Err(last_err)
}

async fn fetch_album_arts(
    albums: &[&Album],
    username: &str,
) -> Vec<Result<Bytes, anyhow::Error>> {
    let mut handles = Vec::new();
    albums
        .iter()
        .map(|album| ((*album).clone(), username.to_string()))
        .for_each(|(album, username)| {
            let handle = tokio::spawn(fetch_album_art(album, username));
            handles.push(handle);
        });

    let mut bytes_results: Vec<Result<Bytes, anyhow::Error>> = Vec::new();

    for handle in handles {
        bytes_results.push(handle.await.unwrap());
    }

    bytes_results
}

pub async fn create_collage(
    albums: &[Album],
    size: u32,
    text: bool,
    username: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    static FONT: LazyLock<FontVec> = LazyLock::new(|| {
        let font_data = std::fs::read(config::FONT_FILE_PATH).expect("Failed to read font file");
        FontVec::try_from_vec(font_data).expect("Error constructing Font")
    });

    let collage_size: u32 = TILE_PX * size;

    let mut collage = ImageBuffer::from_pixel(collage_size, collage_size, Rgba([0, 0, 0, 255]));

    let albums = albums
        .iter()
        .filter(|x| x.album_art_url.is_some())
        .take((size * size).try_into().unwrap())
        .collect::<Vec<_>>();

    let tiles_bytes_vec = fetch_album_arts(&albums, username).await;

    for (i, album) in albums.iter().enumerate() {
        let tiles_bytes = &tiles_bytes_vec[i];

        let row = i as u32 / size;
        let col = i as u32 % size;
        let tile_x = col * TILE_PX;
        let tile_y = row * TILE_PX;

        match tiles_bytes {
            Ok(bytes) => {
                let mut tile = image::load_from_memory(bytes).ok().unwrap_or_default();
                if tile.width() > TILE_PX {
                    tile = tile.thumbnail(TILE_PX, TILE_PX);
                }
                image::imageops::overlay(&mut collage, &tile, tile_x.into(), tile_y.into());
            }
            Err(_) => {
                // continue;
            }
        };

        // Draw text

        if text {
            let text_color = Rgba([255u8, 255, 255, 255]);
            let outline_color = Rgba([0u8, 0, 0, 255]);
            let mut text_image = RgbaImage::from_pixel(TILE_PX, TILE_PX, Rgba([0, 0, 0, 0]));

            let mut draw_text = |x: i32, y: i32, text: &str, fg: bool| {
                draw_text_mut(
                    &mut text_image,
                    if fg { text_color } else { outline_color },
                    x,
                    y,
                    FONT_SIZE,
                    &*FONT,
                    text,
                )
            };

            let mut draw_text_with_outline = |x: i32, y: i32, text: &str| {
                draw_text(x - 2, y - 2, text, false);
                draw_text(x - 2, y, text, false);
                draw_text(x - 2, y + 2, text, false);
                draw_text(x, y - 2, text, false);
                draw_text(x, y + 2, text, false);
                draw_text(x + 2, y - 2, text, false);
                draw_text(x + 2, y, text, false);
                draw_text(x + 2, y + 2, text, false);

                draw_text(x, y, text, true);
            };

            let tile_size = TILE_PX as i32;

            draw_text_with_outline(10, tile_size - 70, &album.name);
            draw_text_with_outline(10, tile_size - 50, &album.artist);
            draw_text_with_outline(
                10,
                tile_size - 30,
                &format!("{} plays", album.user_playcount),
            );

            image::imageops::overlay(&mut collage, &text_image, tile_x.into(), tile_y.into());
        }
    }

    let mut jpeg_bytes: Vec<u8> = Vec::new();
    let mut encoder = JpegEncoder::new(&mut jpeg_bytes);
    encoder
        .encode_image(&collage)
        .expect("Failed to encode JPEG");

    Ok(jpeg_bytes)
}
