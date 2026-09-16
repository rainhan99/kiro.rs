//! Optional, pixel-preserving inbound image preparation.

use std::io::{self, Cursor, Write};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use serde_json::{Value, json};

use super::config::{ImageConfig, ImageStrategy};
use crate::anthropic::types::MessagesRequest;

/// Preserve mode deliberately does not inspect, decode, resize or deduplicate
/// images. The pipeline's converter must also bypass its legacy lossy resizer.
/// Lossless tiling commits changes only after the whole request is admitted.
pub fn prepare_images(payload: &mut MessagesRequest, config: &ImageConfig) -> Result<()> {
    if config.strategy == ImageStrategy::Preserve {
        return Ok(());
    }
    ensure!(
        config.tile_max_base64_bytes > 0 && config.max_tiles > 0 && config.max_pixels > 0,
        "lossless image budgets must be positive"
    );
    let mut messages = payload.messages.clone();
    let mut budget = RequestBudget {
        pixels: 0,
        tiles: 0,
        images: 0,
    };
    for message in &mut messages {
        prepare_content(&mut message.content, config, &mut budget)?;
    }
    payload.messages = messages;
    Ok(())
}

struct RequestBudget {
    pixels: u64,
    tiles: usize,
    images: usize,
}

fn prepare_content(
    content: &mut Value,
    config: &ImageConfig,
    budget: &mut RequestBudget,
) -> Result<()> {
    let Value::Array(blocks) = content else {
        return Ok(());
    };
    let mut output = Vec::with_capacity(blocks.len());
    for block in blocks.iter() {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => output.extend(prepare_image(block, config, budget)?),
            Some("tool_result") => {
                let mut block = block.clone();
                if let Some(content) = block.get_mut("content") {
                    prepare_content(content, config, budget)?;
                }
                output.push(block);
            }
            _ => output.push(block.clone()),
        }
    }
    *blocks = output;
    Ok(())
}

fn prepare_image(
    block: &Value,
    config: &ImageConfig,
    budget: &mut RequestBudget,
) -> Result<Vec<Value>> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .context("image source must be an object")?;
    ensure!(
        source.get("type").and_then(Value::as_str) == Some("base64"),
        "lossless images require a base64 source; remote image retrieval is unsupported"
    );
    let format = match source.get("media_type").and_then(Value::as_str) {
        Some("image/png") => ImageFormat::Png,
        Some("image/jpeg") => ImageFormat::Jpeg,
        Some("image/webp") => ImageFormat::WebP,
        _ => bail!(
            "lossless images support only static PNG, JPEG and WebP; animation is unsupported"
        ),
    };
    let encoded = source
        .get("data")
        .and_then(Value::as_str)
        .context("image base64 data must be a string")?;
    let bytes = BASE64.decode(encoded).context("invalid image base64")?;
    ensure!(
        image::guess_format(&bytes).context("unrecognized image format")? == format,
        "image MIME type does not match the encoded format"
    );
    reject_animation(&bytes, format)?;

    // Decoder allocation limits are applied before decoding any pixel data.
    // PNG may contain 16-bit RGBA, so preserve all eight bytes per pixel.
    let remaining_pixels = config.max_pixels.saturating_sub(budget.pixels);
    ensure!(
        remaining_pixels > 0,
        "request image pixel capacity exceeded"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(remaining_pixels.min(u32::MAX as u64) as u32);
    limits.max_image_height = Some(remaining_pixels.min(u32::MAX as u64) as u32);
    limits.max_alloc = Some(remaining_pixels.saturating_mul(8));
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .context("image header or decoding limits are invalid")?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .context("image dimensions overflow")?;
    ensure!(
        pixels > 0 && pixels <= remaining_pixels,
        "request image pixel capacity exceeded"
    );
    budget.pixels += pixels;
    budget.images += 1;
    let image = DynamicImage::from_decoder(decoder)
        .context("image decoding failed or exceeded allocation limits")?;

    // Keeping an already compliant PNG preserves its pixels and metadata and
    // makes subsequent internal rounds idempotent for our own generated tiles.
    if format == ImageFormat::Png && encoded.len() <= config.tile_max_base64_bytes {
        ensure!(
            budget.tiles < config.max_tiles,
            "request image tile capacity exceeded"
        );
        budget.tiles += 1;
        return Ok(vec![block.clone()]);
    }

    let mut pending = vec![Rectangle {
        x: 0,
        y: 0,
        width,
        height,
    }];
    let mut tiles = Vec::new();
    while let Some(rectangle) = pending.pop() {
        ensure!(
            budget.tiles.saturating_add(pending.len()).saturating_add(1) <= config.max_tiles,
            "request image tile capacity exceeded"
        );
        let crop;
        let tile_image = if rectangle.x == 0
            && rectangle.y == 0
            && rectangle.width == width
            && rectangle.height == height
        {
            &image
        } else {
            crop = image.crop_imm(rectangle.x, rectangle.y, rectangle.width, rectangle.height);
            &crop
        };
        if let Some(png) = encode_bounded_png(tile_image, config.tile_max_base64_bytes)? {
            budget.tiles += 1;
            tiles.push((rectangle, BASE64.encode(png)));
        } else {
            ensure!(
                budget.tiles.saturating_add(pending.len()).saturating_add(2) <= config.max_tiles,
                "request image tile capacity exceeded while preserving pixels"
            );
            let (first, second) = rectangle
                .split()
                .context("image byte cap cannot fit even one lossless pixel")?;
            pending.push(second);
            pending.push(first);
        }
    }
    let mut output = Vec::with_capacity(tiles.len().saturating_mul(2));
    let has_tiles = tiles.len() > 1;
    for (index, (rectangle, data)) in tiles.into_iter().enumerate() {
        if has_tiles {
            let coordinates = json!({"image":budget.images,"tile":index + 1,
                "x":rectangle.x,"y":rectangle.y,"width":rectangle.width,"height":rectangle.height,
                "original_width":width,"original_height":height});
            output.push(json!({"type":"text","text":format!("[kiro-image-tile:{coordinates}]")}));
        }
        let mut tile = block.clone();
        tile["source"] = json!({"type":"base64","media_type":"image/png","data":data});
        output.push(tile);
    }
    Ok(output)
}

#[derive(Clone, Copy)]
struct Rectangle {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rectangle {
    fn split(self) -> Option<(Self, Self)> {
        if self.width >= self.height && self.width > 1 {
            let width = self.width / 2;
            Some((
                Self { width, ..self },
                Self {
                    x: self.x + width,
                    width: self.width - width,
                    ..self
                },
            ))
        } else if self.height > 1 {
            let height = self.height / 2;
            Some((
                Self { height, ..self },
                Self {
                    y: self.y + height,
                    height: self.height - height,
                    ..self
                },
            ))
        } else {
            None
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    cap: usize,
    exceeded: bool,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("lossless PNG output byte cap exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_bounded_png(image: &DynamicImage, base64_cap: usize) -> Result<Option<Vec<u8>>> {
    // STANDARD base64 has four output bytes per three input bytes, including
    // padding. Never allocate an unbounded encoded tile merely to measure it.
    let cap = (base64_cap / 4).saturating_mul(3);
    let mut writer = BoundedWriter {
        bytes: Vec::with_capacity(cap.min(4096)),
        cap,
        exceeded: false,
    };
    let result = image::codecs::png::PngEncoder::new(&mut writer).write_image(
        image.as_bytes(),
        image.width(),
        image.height(),
        image.color().into(),
    );
    if writer.exceeded {
        return Ok(None);
    }
    result.context("lossless PNG encoding failed")?;
    Ok(Some(writer.bytes))
}

fn reject_animation(bytes: &[u8], format: ImageFormat) -> Result<()> {
    match format {
        ImageFormat::Png => {
            let mut offset = 8usize;
            while offset < bytes.len() {
                let header = bytes
                    .get(offset..offset.saturating_add(8))
                    .context("truncated PNG chunk")?;
                let length =
                    u32::from_be_bytes(header[..4].try_into().expect("PNG length bytes")) as usize;
                ensure!(
                    &header[4..8] != b"acTL",
                    "animated PNG is unsupported in lossless image mode"
                );
                let end = offset
                    .checked_add(length)
                    .and_then(|n| n.checked_add(12))
                    .context("PNG chunk length overflow")?;
                ensure!(end <= bytes.len(), "truncated PNG chunk");
                if &header[4..8] == b"IEND" {
                    break;
                }
                offset = end;
            }
        }
        ImageFormat::WebP => {
            let mut offset = 12usize;
            while offset < bytes.len() {
                let header = bytes
                    .get(offset..offset.saturating_add(8))
                    .context("truncated WebP chunk")?;
                let length = u32::from_le_bytes(header[4..8].try_into().expect("WebP length bytes"))
                    as usize;
                let kind = &header[..4];
                ensure!(
                    kind != b"ANIM" && kind != b"ANMF",
                    "animated WebP is unsupported in lossless image mode"
                );
                let end = offset
                    .checked_add(length)
                    .and_then(|n| n.checked_add(8))
                    .context("WebP chunk length overflow")?;
                ensure!(end <= bytes.len(), "truncated WebP chunk");
                if kind == b"VP8X" {
                    let flags = *bytes.get(offset + 8).context("truncated WebP flags")?;
                    ensure!(
                        flags & 0x02 == 0,
                        "animated WebP is unsupported in lossless image mode"
                    );
                }
                offset = end
                    .checked_add(length % 2)
                    .context("WebP padding overflow")?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
    use serde_json::json;
    use std::io::Cursor;

    fn config() -> ImageConfig {
        ImageConfig {
            strategy: ImageStrategy::LosslessTiles,
            tile_max_base64_bytes: 256,
            max_tiles: 128,
            max_pixels: 100_000,
        }
    }

    fn image_bytes(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
            let n = (x.wrapping_mul(9781) ^ y.wrapping_mul(6271))
                .wrapping_mul(214013)
                .wrapping_add(2531011);
            Rgba([
                (n >> 8) as u8,
                (n >> 16) as u8,
                (n >> 24) as u8,
                255 - (x % 10) as u8,
            ])
        }));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn block(bytes: &[u8], mime: &str) -> Value {
        json!({"type":"image","source":{"type":"base64","media_type":mime,"data":BASE64.encode(bytes)}})
    }

    fn request(content: Value) -> MessagesRequest {
        serde_json::from_value(
            json!({"model":"test","messages":[{"role":"user","content":content}]}),
        )
        .unwrap()
    }

    #[test]
    fn preserve_is_an_exact_noop_even_for_sources_it_cannot_decode() {
        let content =
            json!([{"type":"image","source":{"type":"url","url":"https://example.invalid/image"}}]);
        let mut req = request(content.clone());
        prepare_images(&mut req, &ImageConfig::default()).unwrap();
        assert_eq!(req.messages[0].content, content);
    }

    #[test]
    fn tiled_png_pixels_reconstruct_exactly_and_labels_are_separate() {
        let original = image_bytes(32, 16, ImageFormat::Png);
        let source = image::load_from_memory(&original).unwrap().to_rgba8();
        let mut req = request(json!([block(&original, "image/png")]));
        prepare_images(&mut req, &config()).unwrap();
        let blocks = req.messages[0].content.as_array().unwrap();
        let mut restored = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(32, 16);
        let mut covered = vec![false; 32 * 16];
        assert!(blocks.len() > 2);
        for pair in blocks.chunks_exact(2) {
            assert_eq!(pair[0]["type"], "text");
            let label = pair[0]["text"].as_str().unwrap();
            let coordinates: Value = serde_json::from_str(
                label
                    .strip_prefix("[kiro-image-tile:")
                    .unwrap()
                    .strip_suffix(']')
                    .unwrap(),
            )
            .unwrap();
            let x = coordinates["x"].as_u64().unwrap() as u32;
            let y = coordinates["y"].as_u64().unwrap() as u32;
            assert_eq!(coordinates["original_width"], 32);
            assert_eq!(coordinates["original_height"], 16);
            let encoded = pair[1]["source"]["data"].as_str().unwrap();
            assert!(encoded.len() <= 256);
            assert_eq!(pair[1]["source"]["media_type"], "image/png");
            let tile = image::load_from_memory(&BASE64.decode(encoded).unwrap())
                .unwrap()
                .to_rgba8();
            for (dx, dy, pixel) in tile.enumerate_pixels() {
                let index = ((y + dy) * 32 + x + dx) as usize;
                assert!(!covered[index]);
                covered[index] = true;
                restored.put_pixel(x + dx, y + dy, *pixel);
            }
        }
        assert!(covered.into_iter().all(|v| v));
        assert_eq!(source, restored);
        let once = req.messages[0].content.clone();
        prepare_images(&mut req, &config()).unwrap();
        assert_eq!(req.messages[0].content, once);
    }

    #[test]
    fn nested_tool_result_images_keep_pairing_and_non_image_data() {
        let png = image_bytes(32, 16, ImageFormat::Png);
        let mut req = request(
            json!([{"type":"tool_result","tool_use_id":"tool-1","is_error":false,"content":[{"type":"text","text":"result"},block(&png,"image/png")]}]),
        );
        prepare_images(&mut req, &config()).unwrap();
        let result = &req.messages[0].content[0];
        assert_eq!(result["tool_use_id"], "tool-1");
        assert_eq!(result["is_error"], false);
        assert_eq!(result["content"][0]["text"], "result");
        assert!(
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["type"] == "image")
        );
    }

    #[test]
    fn per_request_pixel_and_tile_limits_are_global_and_failure_is_atomic() {
        let png = image_bytes(8, 8, ImageFormat::Png);
        let content = json!([block(&png, "image/png"), block(&png, "image/png")]);
        let mut req = request(content.clone());
        let mut limits = config();
        limits.tile_max_base64_bytes = 100_000;
        limits.max_pixels = 100;
        assert!(prepare_images(&mut req, &limits).is_err());
        assert_eq!(req.messages[0].content, content);
        limits.max_pixels = 1000;
        limits.max_tiles = 1;
        assert!(prepare_images(&mut req, &limits).is_err());
        assert_eq!(req.messages[0].content, content);
    }

    #[test]
    fn unsupported_urls_mime_mismatch_and_animation_fail_explicitly() {
        let mut url = request(
            json!([{"type":"image","source":{"type":"url","url":"https://example.invalid"}}]),
        );
        assert!(prepare_images(&mut url, &config()).is_err());
        let png = image_bytes(8, 8, ImageFormat::Png);
        let mut wrong = request(json!([block(&png, "image/jpeg")]));
        assert!(prepare_images(&mut wrong, &config()).is_err());
        let mut gif = request(json!([block(b"GIF89a", "image/gif")]));
        assert!(prepare_images(&mut gif, &config()).is_err());
        let mut animated = png;
        let mut animation_chunk = vec![
            0, 0, 0, 8, b'a', b'c', b'T', b'L', 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        animated.splice(33..33, animation_chunk.drain(..));
        let mut apng = request(json!([block(&animated, "image/png")]));
        assert!(
            prepare_images(&mut apng, &config())
                .unwrap_err()
                .to_string()
                .contains("animat")
        );
        let webp = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0\0\0\0\0\0\0";
        let mut webp = request(json!([block(webp, "image/webp")]));
        assert!(
            prepare_images(&mut webp, &config())
                .unwrap_err()
                .to_string()
                .contains("animat")
        );
    }

    #[test]
    fn jpeg_and_webp_decode_to_identical_pixels_after_lossless_png_conversion() {
        let mut limits = config();
        limits.tile_max_base64_bytes = 100_000;
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 21) as u8, (y * 29) as u8, 77])
        }));
        for (format, mime) in [
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image.write_to(&mut encoded, format).unwrap();
            let encoded = encoded.into_inner();
            let original = image::load_from_memory(&encoded).unwrap().to_rgba8();
            let mut req = request(json!([block(&encoded, mime)]));
            prepare_images(&mut req, &limits).unwrap();
            let result = &req.messages[0].content[0]["source"];
            assert_eq!(result["media_type"], "image/png");
            let restored =
                image::load_from_memory(&BASE64.decode(result["data"].as_str().unwrap()).unwrap())
                    .unwrap()
                    .to_rgba8();
            assert_eq!(original, restored);
        }
    }

    #[test]
    fn png_tiling_preserves_all_sixteen_bits_and_alpha() {
        let original = ImageBuffer::from_fn(16, 8, |x, y| {
            let n = (x * 9781 + y * 6271) as u16;
            image::Rgba([n, n.wrapping_add(123), n.wrapping_add(4097), 65_535 - n])
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba16(original.clone())
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let mut req = request(json!([block(&encoded.into_inner(), "image/png")]));
        let mut limits = config();
        limits.tile_max_base64_bytes = 256;
        prepare_images(&mut req, &limits).unwrap();
        let blocks = req.messages[0].content.as_array().unwrap();
        assert!(blocks.len() > 2);
        let mut restored = ImageBuffer::new(16, 8);
        for pair in blocks.chunks_exact(2) {
            let label = pair[0]["text"].as_str().unwrap();
            let coordinates: Value = serde_json::from_str(
                label
                    .strip_prefix("[kiro-image-tile:")
                    .unwrap()
                    .strip_suffix(']')
                    .unwrap(),
            )
            .unwrap();
            let x = coordinates["x"].as_u64().unwrap() as u32;
            let y = coordinates["y"].as_u64().unwrap() as u32;
            let png = BASE64
                .decode(pair[1]["source"]["data"].as_str().unwrap())
                .unwrap();
            let tile = image::load_from_memory(&png).unwrap();
            assert_eq!(tile.color(), image::ColorType::Rgba16);
            for (dx, dy, pixel) in tile.to_rgba16().enumerate_pixels() {
                restored.put_pixel(x + dx, y + dy, *pixel);
            }
        }
        assert_eq!(restored, original);
    }

    #[test]
    fn decoded_pixel_budget_rejects_a_small_compressed_large_image() {
        let image = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(40, 40, Rgba([0, 0, 0, 0])));
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Png).unwrap();
        let mut req = request(json!([block(&encoded.into_inner(), "image/png")]));
        let mut limits = config();
        limits.max_pixels = 1000;
        let error = prepare_images(&mut req, &limits).unwrap_err();
        assert!(error.to_string().contains("pixel capacity"));
    }
}
