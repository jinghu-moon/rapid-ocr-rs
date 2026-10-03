use std::{
    fs,
    io::{Cursor, Read},
    path::PathBuf,
    time::Duration,
};

use exif::{In, Reader as ExifReader, Tag};
use image::{DynamicImage, GrayImage, ImageBuffer, LumaA, RgbImage, RgbaImage};

use crate::{
    config::RecImage,
    error::{RapidOcrError, Result},
};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum OcrInput {
    Path(PathBuf),
    Url(String),
    Bytes(Vec<u8>),
    Bgr {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    Rgb {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    BgrU8 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    RgbU8 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    GrayU8 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    GrayAlphaU8 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    RgbaU8 {
        width: usize,
        height: usize,
        data: Vec<u8>,
    },
    Image(RecImage),
}

#[derive(Debug, Clone, Copy)]
pub struct LoadImage {
    http_connect_timeout: Duration,
    http_request_timeout: Duration,
}

impl Default for LoadImage {
    fn default() -> Self {
        Self {
            http_connect_timeout: DEFAULT_HTTP_CONNECT_TIMEOUT,
            http_request_timeout: DEFAULT_HTTP_REQUEST_TIMEOUT,
        }
    }
}

impl LoadImage {
    pub fn load(&self, input: OcrInput) -> Result<RecImage> {
        self.load_with_limit(input, u64::MAX, u64::MAX)
    }

    /// Builds a loader with explicit HTTP connect/request timeouts.
    pub fn with_http_timeouts(connect: Duration, request: Duration) -> Self {
        Self {
            http_connect_timeout: connect,
            http_request_timeout: request,
        }
    }

    /// Loads an image with decoded-pixel and encoded-byte limits.
    ///
    /// Encoded bytes, files, and URLs are dimension-probed before the actual
    /// pixel decode so oversized compressed images cannot force large
    /// allocations just to be rejected afterwards. All encoded inputs are also
    /// bounded by `max_encoded_bytes`; URL bodies are streamed with a hard read
    /// cap even when `Content-Length` is absent.
    pub fn load_with_limit(
        &self,
        input: OcrInput,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<RecImage> {
        match input {
            OcrInput::Path(_) | OcrInput::Url(_) | OcrInput::Bytes(_) => {
                let bytes =
                    self.read_encoded_with_limit(input, max_decode_pixels, max_encoded_bytes)?;
                self.decode_bytes_with_exif(&bytes, true, max_decode_pixels)
            }
            other => self.load_raw(other, max_decode_pixels),
        }
    }

    /// Decodes an encoded or remote input into a [`DynamicImage`].
    ///
    /// Shares the same encoded-byte, decoded-pixel, streaming and timeout
    /// enforcement as [`LoadImage::load_with_limit`], and applies EXIF
    /// orientation normalization. Domain-specific conversion (for example
    /// formula tensors) belongs to the caller.
    pub fn load_dynamic_with_limit(
        &self,
        input: OcrInput,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<DynamicImage> {
        let bytes = self.read_encoded_with_limit(input, max_decode_pixels, max_encoded_bytes)?;
        let mut image = image::load_from_memory(&bytes)
            .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?;
        image = apply_exif_orientation(image, exif_orientation_from_bytes(&bytes));
        Ok(image)
    }

    /// Fetches and validates the encoded bytes of an input **before** decoding.
    ///
    /// This is the single place where encoded-size limits, header dimension
    /// probes, URL `Content-Length` checks, streaming read caps and URL
    /// timeouts are enforced. Raw pixel inputs are rejected because they carry
    /// no encoded representation.
    pub fn read_encoded_with_limit(
        &self,
        input: OcrInput,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<Vec<u8>> {
        match input {
            OcrInput::Bytes(bytes) => {
                ensure_encoded_bytes(bytes.len() as u64, max_encoded_bytes)?;
                let (width, height) = image_dimensions_from_bytes(&bytes)?;
                ensure_decode_pixels(width as usize, height as usize, max_decode_pixels)?;
                Ok(bytes)
            }
            OcrInput::Path(path) => self.read_path(path, max_decode_pixels, max_encoded_bytes),
            OcrInput::Url(url) => self.read_url(&url, max_decode_pixels, max_encoded_bytes),
            _ => Err(RapidOcrError::InvalidInput(
                "raw pixel inputs carry no encoded bytes; use load_with_limit".to_string(),
            )),
        }
    }

    fn load_raw(&self, input: OcrInput, max_decode_pixels: u64) -> Result<RecImage> {
        match input {
            OcrInput::Bgr {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                RecImage::from_bgr_u8(width, height, data)
            }
            OcrInput::Rgb {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                RecImage::from_rgb_u8(width, height, data)
            }
            OcrInput::BgrU8 {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                RecImage::from_bgr_u8(width, height, data)
            }
            OcrInput::RgbU8 {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                RecImage::from_rgb_u8(width, height, data)
            }
            OcrInput::GrayU8 {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                ensure_len(width, height, 1, data.len())?;
                let mut bgr = vec![0_u8; width * height * 3];
                for (src, dst) in data.iter().zip(bgr.as_chunks_mut::<3>().0.iter_mut()) {
                    dst[0] = *src;
                    dst[1] = *src;
                    dst[2] = *src;
                }
                RecImage::from_bgr_u8(width, height, bgr)
            }
            OcrInput::GrayAlphaU8 {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                ensure_len(width, height, 2, data.len())?;
                RecImage::from_bgr_u8(width, height, gray_alpha_to_bgr(width, height, &data))
            }
            OcrInput::RgbaU8 {
                width,
                height,
                data,
            } => {
                ensure_decode_pixels(width, height, max_decode_pixels)?;
                ensure_len(width, height, 4, data.len())?;
                RecImage::from_bgr_u8(width, height, rgba_to_bgr(width, height, &data))
            }
            OcrInput::Image(image) => {
                ensure_decode_pixels(image.width(), image.height(), max_decode_pixels)?;
                Ok(image)
            }
            OcrInput::Path(_) | OcrInput::Url(_) | OcrInput::Bytes(_) => {
                Err(RapidOcrError::InvalidInput(
                    "encoded or remote input must use load_with_limit directly".to_string(),
                ))
            }
        }
    }

    fn read_path(
        &self,
        path: PathBuf,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<Vec<u8>> {
        if !path.exists() {
            return Err(RapidOcrError::FileNotFound(path));
        }
        let encoded_len = fs::metadata(&path)?.len();
        ensure_encoded_bytes(encoded_len, max_encoded_bytes)?;
        let (width, height) = image_dimensions_from_file(&path)?;
        ensure_decode_pixels(width as usize, height as usize, max_decode_pixels)?;
        Ok(fs::read(path)?)
    }

    fn read_url(
        &self,
        url: &str,
        max_decode_pixels: u64,
        max_encoded_bytes: u64,
    ) -> Result<Vec<u8>> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(self.http_connect_timeout)
            .timeout(self.http_request_timeout)
            .build()?;
        let response = client.get(url).send()?;
        if !response.status().is_success() {
            return Err(RapidOcrError::Download(format!(
                "failed to fetch image from url {url}: HTTP {}",
                response.status()
            )));
        }
        if let Some(content_length) = response.content_length() {
            ensure_encoded_bytes(content_length, max_encoded_bytes)?;
        }

        let read_limit = max_encoded_bytes.saturating_add(1);
        let mut bytes = Vec::new();
        response.take(read_limit).read_to_end(&mut bytes)?;
        ensure_encoded_bytes(bytes.len() as u64, max_encoded_bytes)?;
        let (width, height) = image_dimensions_from_bytes(&bytes)?;
        ensure_decode_pixels(width as usize, height as usize, max_decode_pixels)?;
        Ok(bytes)
    }

    fn decode_bytes_with_exif(
        &self,
        bytes: &[u8],
        apply_exif_transpose: bool,
        max_decode_pixels: u64,
    ) -> Result<RecImage> {
        let (width, height) = image_dimensions_from_bytes(bytes)?;
        ensure_decode_pixels(width as usize, height as usize, max_decode_pixels)?;

        let orientation = if apply_exif_transpose {
            exif_orientation_from_bytes(bytes)
        } else {
            None
        };

        // Decode with the pure-Rust `image` crate, then apply EXIF orientation.
        let mut dyn_img = image::load_from_memory(bytes)
            .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?;
        if apply_exif_transpose {
            dyn_img = apply_exif_orientation(dyn_img, orientation);
        }
        dynamic_to_rec_image(dyn_img)
    }
}

const DEFAULT_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn image_dimensions_from_bytes(bytes: &[u8]) -> Result<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?
        .into_dimensions()
        .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))
}

fn image_dimensions_from_file(path: &std::path::Path) -> Result<(u32, u32)> {
    image::ImageReader::open(path)
        .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?
        .with_guessed_format()
        .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))?
        .into_dimensions()
        .map_err(|e| RapidOcrError::InvalidImage(e.to_string()))
}

pub(crate) fn ensure_decode_pixels(
    width: usize,
    height: usize,
    max_decode_pixels: u64,
) -> Result<()> {
    let pixels = (width as u64)
        .checked_mul(height as u64)
        .ok_or_else(|| RapidOcrError::InvalidInput("image dimensions overflow".to_string()))?;
    if pixels > max_decode_pixels {
        return Err(RapidOcrError::InvalidImage(format!(
            "image has {pixels} pixels, limit is {max_decode_pixels}"
        )));
    }
    Ok(())
}

fn ensure_encoded_bytes(actual: u64, max_encoded_bytes: u64) -> Result<()> {
    if actual > max_encoded_bytes {
        return Err(RapidOcrError::InvalidImage(format!(
            "encoded image has {actual} bytes, limit is {max_encoded_bytes}"
        )));
    }
    Ok(())
}

fn exif_orientation_from_bytes(bytes: &[u8]) -> Option<u32> {
    let mut cursor = Cursor::new(bytes);
    let exif = ExifReader::new().read_from_container(&mut cursor).ok()?;
    let field = exif.get_field(Tag::Orientation, In::PRIMARY)?;
    field.value.get_uint(0)
}

#[cfg(test)]
fn exif_transpose_from_bytes(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    apply_exif_orientation(img, exif_orientation_from_bytes(bytes))
}

fn apply_exif_orientation(img: DynamicImage, orientation: Option<u32>) -> DynamicImage {
    let orientation = orientation.unwrap_or(1);

    // Keep parity with PIL.ImageOps.exif_transpose orientation mapping.
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.fliph().rotate90(),
        6 => img.rotate90(),
        7 => img.fliph().rotate270(),
        8 => img.rotate270(),
        _ => img,
    }
}

fn dynamic_to_rec_image(img: DynamicImage) -> Result<RecImage> {
    match img {
        DynamicImage::ImageLuma8(gray) => RecImage::from_bgr_u8(
            gray.width() as usize,
            gray.height() as usize,
            gray_to_bgr(gray),
        ),
        DynamicImage::ImageLumaA8(gray_alpha) => {
            let width = gray_alpha.width() as usize;
            let height = gray_alpha.height() as usize;
            RecImage::from_bgr_u8(width, height, gray_alpha_to_bgr_img(gray_alpha))
        }
        DynamicImage::ImageRgba8(rgba) => {
            let width = rgba.width() as usize;
            let height = rgba.height() as usize;
            RecImage::from_bgr_u8(width, height, rgba_to_bgr_img(rgba))
        }
        other => {
            let rgb = other.to_rgb8();
            RecImage::from_bgr_u8(rgb.width() as usize, rgb.height() as usize, rgb_to_bgr(rgb))
        }
    }
}

fn gray_to_bgr(gray: GrayImage) -> Vec<u8> {
    let mut bgr = vec![0_u8; gray.width() as usize * gray.height() as usize * 3];
    for (src, dst) in gray
        .as_raw()
        .iter()
        .zip(bgr.as_chunks_mut::<3>().0.iter_mut())
    {
        dst[0] = *src;
        dst[1] = *src;
        dst[2] = *src;
    }
    bgr
}

fn rgb_to_bgr(rgb: RgbImage) -> Vec<u8> {
    let mut bgr = vec![0_u8; rgb.width() as usize * rgb.height() as usize * 3];
    for (src, dst) in rgb
        .as_raw()
        .as_chunks::<3>()
        .0
        .iter()
        .zip(bgr.as_chunks_mut::<3>().0.iter_mut())
    {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
    }
    bgr
}

fn gray_alpha_to_bgr_img(gray_alpha: ImageBuffer<LumaA<u8>, Vec<u8>>) -> Vec<u8> {
    gray_alpha_to_bgr(
        gray_alpha.width() as usize,
        gray_alpha.height() as usize,
        gray_alpha.as_raw(),
    )
}

fn gray_alpha_to_bgr(width: usize, height: usize, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0_u8; width * height * 3];
    for (src, dst) in data
        .as_chunks::<2>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<3>().0.iter_mut())
    {
        let gray = src[0] as f32;
        let alpha = src[1] as f32 / 255.0;
        // Same behavior as Python's bitwise approach with white background.
        let value = (gray * alpha + 255.0 * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8;
        dst[0] = value;
        dst[1] = value;
        dst[2] = value;
    }
    out
}

fn rgba_to_bgr_img(rgba: RgbaImage) -> Vec<u8> {
    rgba_to_bgr(rgba.width() as usize, rgba.height() as usize, rgba.as_raw())
}

fn rgba_to_bgr(width: usize, height: usize, data: &[u8]) -> Vec<u8> {
    let bg = auto_background_for_rgba(data);
    let mut out = vec![0_u8; width * height * 3];

    for (src, dst) in data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<3>().0.iter_mut())
    {
        let r = src[0] as f32;
        let g = src[1] as f32;
        let b = src[2] as f32;
        let a = src[3] as f32 / 255.0;

        let blended_r = (r * a + bg[0] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        let blended_g = (g * a + bg[1] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        let blended_b = (b * a + bg[2] as f32 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;

        dst[0] = blended_b;
        dst[1] = blended_g;
        dst[2] = blended_r;
    }
    out
}

fn auto_background_for_rgba(data: &[u8]) -> [u8; 3] {
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for px in data.as_chunks::<4>().0 {
        let alpha = px[3];
        if alpha == 0 {
            continue;
        }
        let r = px[0] as f64;
        let g = px[1] as f64;
        let b = px[2] as f64;
        let luminance = 0.299_f64 * r + 0.587_f64 * g + 0.114_f64 * b;
        sum += luminance;
        count += 1;
    }

    if count == 0 {
        return [255, 255, 255];
    }
    let avg = sum / count as f64;
    if avg < 128.0 {
        [255, 255, 255]
    } else {
        [0, 0, 0]
    }
}

fn ensure_len(width: usize, height: usize, channels: usize, actual_len: usize) -> Result<()> {
    let expected = width
        .checked_mul(height)
        .and_then(|v| v.checked_mul(channels))
        .ok_or_else(|| RapidOcrError::InvalidImage("image dimensions overflow".to_string()))?;
    if expected != actual_len {
        return Err(RapidOcrError::InvalidImage(format!(
            "raw input size mismatch: expected {expected}, got {actual_len}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use image::{DynamicImage, ImageFormat, RgbImage};

    use super::{LoadImage, OcrInput, exif_transpose_from_bytes};

    fn corrupted_png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::new(width, height));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("test image should encode");

        let mut pos = 8;
        let mut corrupted = false;
        while pos + 12 <= bytes.len() {
            let len =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            let kind = &bytes[pos + 4..pos + 8];
            let data_start = pos + 8;
            let data_end = data_start + len;
            if kind == b"IDAT" && data_start < data_end {
                bytes[data_start] ^= 0xFF;
                corrupted = true;
                break;
            }
            pos = data_end + 4;
        }
        assert!(corrupted, "test image should contain IDAT data");
        bytes
    }

    #[test]
    fn gray_alpha_input_is_supported() {
        let loader = LoadImage::default();
        let image = loader
            .load(OcrInput::GrayAlphaU8 {
                width: 2,
                height: 1,
                data: vec![0, 0, 255, 255],
            })
            .expect("gray alpha should load");
        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 1);
    }

    #[test]
    fn rgba_input_is_supported() {
        let loader = LoadImage::default();
        let image = loader
            .load(OcrInput::RgbaU8 {
                width: 1,
                height: 1,
                data: vec![0, 0, 0, 255],
            })
            .expect("rgba should load");
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);
    }

    #[test]
    fn bgr_contract_input_is_supported() {
        let loader = LoadImage::default();
        let image = loader
            .load(OcrInput::Bgr {
                width: 1,
                height: 1,
                data: vec![0, 0, 0],
            })
            .expect("bgr contract input should load");
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);
    }

    #[test]
    fn exif_transpose_is_noop_when_exif_is_missing() {
        let rgb = RgbImage::from_raw(2, 1, vec![255, 0, 0, 0, 255, 0]).expect("valid rgb image");
        let img = DynamicImage::ImageRgb8(rgb);

        // Minimal EXIF payload with orientation=6 would be parsed in integration tests.
        // Here we only verify helper is a no-op when EXIF payload is absent.
        let out = exif_transpose_from_bytes(img.clone(), &[]);
        assert_eq!(out.width(), img.width());
        assert_eq!(out.height(), img.height());
    }

    #[test]
    fn encoded_input_is_rejected_before_decode_when_too_large() {
        let bytes = corrupted_png(8, 8);
        let err = LoadImage::default()
            .load_with_limit(OcrInput::Bytes(bytes), 4, u64::MAX)
            .expect_err("oversized image must be rejected");
        assert!(
            err.to_string().contains("limit is 4"),
            "expected pixel-limit error, got: {err}"
        );
    }

    #[test]
    fn raw_input_is_rejected_before_conversion_when_too_large() {
        let err = LoadImage::default()
            .load_with_limit(
                OcrInput::RgbaU8 {
                    width: 8,
                    height: 8,
                    data: vec![0; 8 * 8 * 4],
                },
                4,
                u64::MAX,
            )
            .expect_err("oversized raw image must be rejected");
        assert!(
            err.to_string().contains("limit is 4"),
            "expected pixel-limit error, got: {err}"
        );
    }

    #[test]
    fn file_input_is_rejected_before_full_read_when_too_large() {
        let bytes = corrupted_png(8, 8);
        let path = std::env::temp_dir().join(format!(
            "rapid_ocr_rs_pixel_limit_{}.png",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).expect("test image should be written");
        let result =
            LoadImage::default().load_with_limit(OcrInput::Path(path.clone()), 4, u64::MAX);
        let _ = std::fs::remove_file(&path);
        let err = result.expect_err("oversized file image must be rejected");
        assert!(
            err.to_string().contains("limit is 4"),
            "expected pixel-limit error, got: {err}"
        );
    }

    #[test]
    fn url_loading_checks_pixel_limit_before_decode() {
        let body = corrupted_png(8, 8);
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server should bind");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(header.as_bytes())
                .expect("server should write header");
            stream.write_all(&body).expect("server should write body");
        });

        let err = LoadImage::default()
            .load_with_limit(
                OcrInput::Url(format!("http://{address}/image.png")),
                4,
                u64::MAX,
            )
            .expect_err("oversized URL image must be rejected");
        server.join().expect("server thread should finish");
        assert!(
            err.to_string().contains("limit is 4"),
            "expected pixel-limit error, got: {err}"
        );
    }

    #[test]
    fn file_input_is_rejected_before_read_when_encoded_too_large() {
        let bytes = corrupted_png(8, 8);
        let path = std::env::temp_dir().join(format!(
            "rapid_ocr_rs_encoded_limit_{}.png",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).expect("test image should be written");
        let result = LoadImage::default().load_with_limit(OcrInput::Path(path.clone()), 64, 32);
        let _ = std::fs::remove_file(&path);
        let err = result.expect_err("oversized encoded file must be rejected");
        assert!(
            err.to_string().contains("encoded image has")
                && err.to_string().contains("limit is 32"),
            "expected encoded-size error, got: {err}"
        );
    }

    #[test]
    fn url_loading_checks_content_length_limit() {
        let body = vec![0_u8; 4096];
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server should bind");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });

        let err = LoadImage::default()
            .load_with_limit(OcrInput::Url(format!("http://{address}/large.png")), 64, 32)
            .expect_err("oversized content-length must be rejected");
        server.join().expect("server thread should finish");
        assert!(
            err.to_string().contains("encoded image has")
                && err.to_string().contains("limit is 32"),
            "expected encoded-size error, got: {err}"
        );
    }

    #[test]
    fn url_loading_enforces_streaming_encoded_limit_without_content_length() {
        let body = vec![0_u8; 4096];
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server should bind");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let header = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        });

        let err = LoadImage::default()
            .load_with_limit(
                OcrInput::Url(format!("http://{address}/stream.png")),
                64,
                32,
            )
            .expect_err("streamed oversized body must be rejected");
        server.join().expect("server thread should finish");
        assert!(
            err.to_string().contains("encoded image has")
                && err.to_string().contains("limit is 32"),
            "expected encoded-size error, got: {err}"
        );
    }

    #[test]
    fn url_loading_honors_request_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local server should bind");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(500));
        });

        let loader =
            LoadImage::with_http_timeouts(Duration::from_millis(100), Duration::from_millis(150));
        let err = loader
            .load(OcrInput::Url(format!("http://{address}/hang.png")))
            .expect_err("hung URL must time out");
        server.join().expect("server thread should finish");
        assert!(
            matches!(err, crate::error::RapidOcrError::Reqwest(_)),
            "expected reqwest timeout error, got: {err}"
        );
    }
}
