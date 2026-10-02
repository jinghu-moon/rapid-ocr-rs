//! PP-FormulaNet_plus 图像预处理。
//!
//! 该实现固定 RapidDoc `PPPreProcess` 在 `img_size=(384,384)` 时的行为：
//! 裁剪非白边距、短边缩放到 384、缩略图限制长边到 384、黑色画布居中填充、
//! `mean=0.7931` / `std=0.1738` 归一化、BGR2GRAY 通道语义以及最终
//! `[N,1,384,384]` 单通道张量。
//!
//! 注意：这里不复用普通 OCR 的 `ocr/pipeline` 预处理，因为公式模型的裁剪、
//! resize、padding 和通道语义与 CTC OCR 完全不同。

use image::{DynamicImage, Rgb, RgbImage, imageops};
use ndarray::{Array4, Axis};

use crate::error::{RapidOcrError, Result};

pub const FORMULA_INPUT_SIZE: u32 = 384;
pub const FORMULA_MEAN: f32 = 0.7931;
pub const FORMULA_STD: f32 = 0.1738;

pub type FormulaTensor = Array4<f32>;

/// PP-FormulaNet_plus 预处理入口。
#[derive(Debug, Clone, Copy)]
pub struct FormulaPreprocessor {
    input_size: u32,
}

impl Default for FormulaPreprocessor {
    fn default() -> Self {
        Self::new()
    }
}

impl FormulaPreprocessor {
    pub fn new() -> Self {
        Self {
            input_size: FORMULA_INPUT_SIZE,
        }
    }

    pub fn with_input_size(input_size: u32) -> Result<Self> {
        if input_size == 0 {
            return Err(RapidOcrError::InvalidInput(
                "formula input size must be greater than zero".to_string(),
            ));
        }
        Ok(Self { input_size })
    }

    pub fn input_size(&self) -> u32 {
        self.input_size
    }

    pub fn preprocess(&self, image: &DynamicImage) -> Result<FormulaTensor> {
        let rgb = dynamic_to_rgb8(image);
        self.preprocess_rgb_image(&rgb)
    }

    pub fn preprocess_rgb8(&self, width: u32, height: u32, data: &[u8]) -> Result<FormulaTensor> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(3))
            .ok_or_else(|| {
                RapidOcrError::InvalidInput("formula image size overflow".to_string())
            })?;
        if data.len() != expected {
            return Err(RapidOcrError::InvalidImage(format!(
                "formula RGB input size mismatch: expected {expected}, got {}",
                data.len()
            )));
        }
        let image = RgbImage::from_raw(width, height, data.to_vec())
            .ok_or_else(|| RapidOcrError::InvalidImage("invalid formula RGB input".to_string()))?;
        self.preprocess_rgb_image(&image)
    }

    pub fn preprocess_rgba8(&self, width: u32, height: u32, data: &[u8]) -> Result<FormulaTensor> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| {
                RapidOcrError::InvalidInput("formula image size overflow".to_string())
            })?;
        if data.len() != expected {
            return Err(RapidOcrError::InvalidImage(format!(
                "formula RGBA input size mismatch: expected {expected}, got {}",
                data.len()
            )));
        }
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for px in data.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
        let image = RgbImage::from_raw(width, height, rgb)
            .ok_or_else(|| RapidOcrError::InvalidImage("invalid formula RGBA input".to_string()))?;
        self.preprocess_rgb_image(&image)
    }

    pub fn preprocess_gray8(&self, width: u32, height: u32, data: &[u8]) -> Result<FormulaTensor> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| {
                RapidOcrError::InvalidInput("formula image size overflow".to_string())
            })?;
        if data.len() != expected {
            return Err(RapidOcrError::InvalidImage(format!(
                "formula gray input size mismatch: expected {expected}, got {}",
                data.len()
            )));
        }
        let mut rgb = Vec::with_capacity(expected * 3);
        for value in data {
            rgb.extend_from_slice(&[*value, *value, *value]);
        }
        let image = RgbImage::from_raw(width, height, rgb)
            .ok_or_else(|| RapidOcrError::InvalidImage("invalid formula gray input".to_string()))?;
        self.preprocess_rgb_image(&image)
    }

    pub fn preprocess_batch(&self, images: &[DynamicImage]) -> Result<FormulaTensor> {
        let mut batch = Array4::<f32>::zeros((
            images.len(),
            1,
            self.input_size as usize,
            self.input_size as usize,
        ));
        for (index, image) in images.iter().enumerate() {
            let single = self.preprocess(image)?;
            batch
                .index_axis_mut(Axis(0), index)
                .assign(&single.index_axis(Axis(0), 0));
        }
        Ok(batch)
    }

    fn preprocess_rgb_image(&self, rgb: &RgbImage) -> Result<FormulaTensor> {
        if rgb.width() == 0 || rgb.height() == 0 {
            return Err(RapidOcrError::InvalidImage(
                "formula image width/height must be greater than zero".to_string(),
            ));
        }

        let cropped = crop_margin(rgb);
        let (first_w, first_h) =
            first_resize_size(cropped.width(), cropped.height(), self.input_size);
        let first = resize_rgb8_pillow(&cropped, first_w, first_h, PillowFilter::Bilinear);
        let final_image =
            thumbnail_rgb8_pillow(&first, self.input_size, self.input_size).unwrap_or(first);
        let canvas = pad_to_black_square(&final_image, self.input_size);
        let luma = normalized_luma_tensor(&canvas);
        let side = self.input_size as usize;
        Array4::from_shape_vec((1, 1, side, side), luma).map_err(|error| {
            RapidOcrError::InvalidImage(format!("formula tensor shape error: {error}"))
        })
    }
}

fn dynamic_to_rgb8(image: &DynamicImage) -> RgbImage {
    match image {
        DynamicImage::ImageRgb8(rgb) => rgb.clone(),
        DynamicImage::ImageRgba8(rgba) => {
            let (width, height) = rgba.dimensions();
            let mut out = RgbImage::new(width, height);
            for (x, y, pixel) in rgba.enumerate_pixels() {
                out.put_pixel(x, y, Rgb([pixel[0], pixel[1], pixel[2]]));
            }
            out
        }
        DynamicImage::ImageLuma8(gray) => {
            let (width, height) = gray.dimensions();
            let mut out = RgbImage::new(width, height);
            for (x, y, pixel) in gray.enumerate_pixels() {
                out.put_pixel(x, y, Rgb([pixel[0], pixel[0], pixel[0]]));
            }
            out
        }
        DynamicImage::ImageLumaA8(gray_alpha) => {
            let (width, height) = gray_alpha.dimensions();
            let mut out = RgbImage::new(width, height);
            for (x, y, pixel) in gray_alpha.enumerate_pixels() {
                out.put_pixel(x, y, Rgb([pixel[0], pixel[0], pixel[0]]));
            }
            out
        }
        other => other.to_rgb8(),
    }
}

fn pil_luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114 + 500) / 1000) as u8
}

fn crop_margin(image: &RgbImage) -> RgbImage {
    let width = image.width();
    let height = image.height();
    let mut luma = vec![0u8; (width * height) as usize];
    for (index, pixel) in image.pixels().enumerate() {
        luma[index] = pil_luma(pixel[0], pixel[1], pixel[2]);
    }

    let min = *luma.iter().min().unwrap_or(&0);
    let max = *luma.iter().max().unwrap_or(&0);
    if max == min {
        return image.clone();
    }

    let denominator = (max - min) as f64;
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            let normalized = (luma[(y * width + x) as usize] - min) as f64 / denominator * 255.0;
            if normalized < 200.0 {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                found = true;
            }
        }
    }

    if !found {
        return image.clone();
    }
    imageops::crop_imm(image, min_x, min_y, max_x - min_x + 1, max_y - min_y + 1).to_image()
}

fn first_resize_size(width: u32, height: u32, short_side: u32) -> (u32, u32) {
    if width <= height {
        (
            short_side,
            ((short_side as u64 * height as u64) / width as u64) as u32,
        )
    } else {
        (
            ((short_side as u64 * width as u64) / height as u64) as u32,
            short_side,
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum PillowFilter {
    Bilinear,
    Bicubic,
}

impl PillowFilter {
    fn support(self) -> f64 {
        match self {
            Self::Bilinear => 1.0,
            Self::Bicubic => 2.0,
        }
    }

    fn weight(self, x: f64) -> f64 {
        let x = x.abs();
        match self {
            Self::Bilinear => {
                if x < 1.0 {
                    1.0 - x
                } else {
                    0.0
                }
            }
            Self::Bicubic => {
                const A: f64 = -0.5;
                if x < 1.0 {
                    ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * A
                } else {
                    0.0
                }
            }
        }
    }
}

struct Coefficients {
    bounds: Vec<usize>,
    weights: Vec<i32>,
    ksize: usize,
}

fn precompute_coefficients(
    in_size: usize,
    in0: f64,
    in1: f64,
    out_size: usize,
    filter: PillowFilter,
) -> Coefficients {
    let scale = (in1 - in0) / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = filter.support() * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let precision_scale = (1_i64 << 22) as f64;

    let mut bounds = vec![0usize; out_size * 2];
    let mut weights = vec![0i32; out_size * ksize];

    for xx in 0..out_size {
        let center = in0 + (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        let mut xmin = (center - support + 0.5) as i32;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i32;
        if xmax > in_size as i32 {
            xmax = in_size as i32;
        }
        let count = (xmax - xmin) as usize;

        let mut row = vec![0.0_f64; count];
        let mut sum = 0.0_f64;
        for (k, value) in row.iter_mut().enumerate() {
            let x = k as f64 + xmin as f64 - center + 0.5;
            let weight = filter.weight(x * ss);
            *value = weight;
            sum += weight;
        }
        if sum != 0.0 {
            for value in &mut row {
                *value /= sum;
            }
        }

        let base = xx * ksize;
        for (k, value) in row.into_iter().enumerate() {
            let fixed = if value < 0.0 {
                (value * precision_scale - 0.5) as i32
            } else {
                (value * precision_scale + 0.5) as i32
            };
            weights[base + k] = fixed;
        }
        bounds[xx * 2] = xmin as usize;
        bounds[xx * 2 + 1] = count;
    }

    Coefficients {
        bounds,
        weights,
        ksize,
    }
}

fn clip8(value: i64) -> u8 {
    (value >> 22).clamp(0, 255) as u8
}

fn resize_rgb8_pillow(
    image: &RgbImage,
    out_width: u32,
    out_height: u32,
    filter: PillowFilter,
) -> RgbImage {
    resize_rgb8_pillow_box(
        image,
        out_width,
        out_height,
        filter,
        0.0,
        image.width() as f64,
        0.0,
        image.height() as f64,
    )
}

#[allow(clippy::too_many_arguments)]
fn resize_rgb8_pillow_box(
    image: &RgbImage,
    out_width: u32,
    out_height: u32,
    filter: PillowFilter,
    in0_x: f64,
    in1_x: f64,
    in0_y: f64,
    in1_y: f64,
) -> RgbImage {
    let in_width = image.width();
    let in_height = image.height();
    if out_width == in_width
        && out_height == in_height
        && in0_x == 0.0
        && in1_x == in_width as f64
        && in0_y == 0.0
        && in1_y == in_height as f64
    {
        return image.clone();
    }

    let horizontal =
        precompute_coefficients(in_width as usize, in0_x, in1_x, out_width as usize, filter);
    let mut temp = RgbImage::new(out_width, in_height);
    for y in 0..in_height {
        for x_out in 0..out_width {
            let base = x_out as usize * horizontal.ksize;
            let xmin = horizontal.bounds[x_out as usize * 2];
            let count = horizontal.bounds[x_out as usize * 2 + 1];
            let mut sums = [1_i64 << 21; 3];
            for k in 0..count {
                let pixel = image.get_pixel((xmin + k) as u32, y);
                let weight = horizontal.weights[base + k] as i64;
                for channel in 0..3 {
                    sums[channel] += pixel[channel] as i64 * weight;
                }
            }
            temp.put_pixel(
                x_out,
                y,
                Rgb([clip8(sums[0]), clip8(sums[1]), clip8(sums[2])]),
            );
        }
    }

    let vertical = precompute_coefficients(
        in_height as usize,
        in0_y,
        in1_y,
        out_height as usize,
        filter,
    );
    let mut out = RgbImage::new(out_width, out_height);
    for y_out in 0..out_height {
        let base = y_out as usize * vertical.ksize;
        let ymin = vertical.bounds[y_out as usize * 2];
        let count = vertical.bounds[y_out as usize * 2 + 1];
        for x in 0..out_width {
            let mut sums = [1_i64 << 21; 3];
            for k in 0..count {
                let pixel = temp.get_pixel(x, (ymin + k) as u32);
                let weight = vertical.weights[base + k] as i64;
                for channel in 0..3 {
                    sums[channel] += pixel[channel] as i64 * weight;
                }
            }
            out.put_pixel(
                x,
                y_out,
                Rgb([clip8(sums[0]), clip8(sums[1]), clip8(sums[2])]),
            );
        }
    }
    out
}

fn division_uint32(divider: u64, result_bits: u32) -> u32 {
    let max_dividend = ((1_u64 << result_bits) * divider) as f32;
    let max_int = ((1_u64 << 30) as f32) * 4.0;
    (max_int / max_dividend) as u32
}

fn reduce_rgb8_pillow(image: &RgbImage, scale_x: u32, scale_y: u32) -> RgbImage {
    let width = image.width();
    let height = image.height();
    let out_width = width.div_ceil(scale_x);
    let out_height = height.div_ceil(scale_y);
    let mut out = RgbImage::new(out_width, out_height);

    for y_out in 0..out_height {
        let y_start = y_out * scale_y;
        let y_end = (y_start + scale_y).min(height);
        let area_y = y_end - y_start;
        for x_out in 0..out_width {
            let x_start = x_out * scale_x;
            let x_end = (x_start + scale_x).min(width);
            let area_x = x_end - x_start;
            let area = area_x as u64 * area_y as u64;
            let multiplier = division_uint32(area, 8) as u64;
            let amend = area / 2;
            let mut sums = [0_u64; 3];
            for y in y_start..y_end {
                for x in x_start..x_end {
                    let pixel = image.get_pixel(x, y);
                    for channel in 0..3 {
                        sums[channel] += pixel[channel] as u64;
                    }
                }
            }
            out.put_pixel(
                x_out,
                y_out,
                Rgb([
                    (((sums[0] + amend) * multiplier) >> 24) as u8,
                    (((sums[1] + amend) * multiplier) >> 24) as u8,
                    (((sums[2] + amend) * multiplier) >> 24) as u8,
                ]),
            );
        }
    }
    out
}

fn pillow_round_aspect(number: f64, key: impl Fn(f64) -> f64) -> u32 {
    let floor = number.floor();
    let ceil = number.ceil();
    let chosen = if key(floor) <= key(ceil) { floor } else { ceil };
    chosen.max(1.0) as u32
}

fn pillow_thumbnail_size(
    width: u32,
    height: u32,
    box_width: u32,
    box_height: u32,
) -> Option<(u32, u32)> {
    if box_width >= width && box_height >= height {
        return None;
    }
    let aspect = width as f64 / height as f64;
    let mut x = box_width as f64;
    let mut y = box_height as f64;
    if x / y >= aspect {
        x = pillow_round_aspect(y * aspect, |n| (aspect - n / y).abs()) as f64;
    } else {
        y = pillow_round_aspect(x / aspect, |n| {
            if n == 0.0 {
                0.0
            } else {
                (aspect - x / n).abs()
            }
        }) as f64;
    }
    Some((x as u32, y as u32))
}

fn thumbnail_rgb8_pillow(image: &RgbImage, box_width: u32, box_height: u32) -> Option<RgbImage> {
    let (target_width, target_height) =
        pillow_thumbnail_size(image.width(), image.height(), box_width, box_height)?;
    if target_width == image.width() && target_height == image.height() {
        return None;
    }

    let factor_x = ((image.width() as f64) / target_width as f64 / 2.0)
        .floor()
        .max(1.0) as u32;
    let factor_y = ((image.height() as f64) / target_height as f64 / 2.0)
        .floor()
        .max(1.0) as u32;
    let reduced = if factor_x > 1 || factor_y > 1 {
        reduce_rgb8_pillow(image, factor_x, factor_y)
    } else {
        image.clone()
    };
    let reduced_box_width = image.width() as f64 / factor_x as f64;
    let reduced_box_height = image.height() as f64 / factor_y as f64;
    Some(resize_rgb8_pillow_box(
        &reduced,
        target_width,
        target_height,
        PillowFilter::Bicubic,
        0.0,
        reduced_box_width,
        0.0,
        reduced_box_height,
    ))
}

fn pad_to_black_square(image: &RgbImage, side: u32) -> RgbImage {
    let mut canvas = RgbImage::from_pixel(side, side, Rgb([0, 0, 0]));
    let left = side.saturating_sub(image.width()) / 2;
    let top = side.saturating_sub(image.height()) / 2;
    for y in 0..image.height() {
        for x in 0..image.width() {
            if left + x < side && top + y < side {
                canvas.put_pixel(left + x, top + y, *image.get_pixel(x, y));
            }
        }
    }
    canvas
}

fn normalized_luma_tensor(image: &RgbImage) -> Vec<f32> {
    let padded_height = image.height().div_ceil(16) * 16;
    let padded_width = image.width().div_ceil(16) * 16;
    let mut out = vec![1.0_f32; (padded_height * padded_width) as usize];

    for y in 0..image.height() {
        for x in 0..image.width() {
            let pixel = image.get_pixel(x, y);
            const SCALE: f32 = 1.0 / 255.0;
            let r = (pixel[0] as f32 * SCALE - FORMULA_MEAN) / FORMULA_STD;
            let g = (pixel[1] as f32 * SCALE - FORMULA_MEAN) / FORMULA_STD;
            let b = (pixel[2] as f32 * SCALE - FORMULA_MEAN) / FORMULA_STD;
            let luma = b.mul_add(0.299, g.mul_add(0.587, r * 0.114));
            out[(y * padded_width + x) as usize] = luma;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage, RgbaImage};
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    #[derive(Debug, Deserialize)]
    struct GoldenManifest {
        images: Vec<GoldenImage>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenImage {
        image: String,
        golden: String,
        sha256_raw_f32le: String,
        min: f32,
        max: f32,
        mean: f32,
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formula-golden")
    }

    fn load_npy(path: &Path) -> (Vec<usize>, Vec<f32>) {
        let bytes = std::fs::read(path).expect("golden npy should exist");
        assert!(bytes.starts_with(b"\x93NUMPY"));
        let major = bytes[6];
        let header_len = match major {
            1 => u16::from_le_bytes([bytes[8], bytes[9]]) as usize,
            2 | 3 => u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize,
            other => panic!("unsupported npy version {other}"),
        };
        let header_start = if major == 1 { 10 } else { 12 };
        let header = std::str::from_utf8(&bytes[header_start..header_start + header_len])
            .expect("npy header should be utf-8");
        assert!(header.contains("'descr': '<f4'"));
        assert!(header.contains("'fortran_order': False"));

        let shape_start = header
            .find("'shape': (")
            .expect("npy header should contain shape")
            + "'shape': (".len();
        let shape_end = header[shape_start..]
            .find(')')
            .expect("npy shape should end")
            + shape_start;
        let shape: Vec<usize> = header[shape_start..shape_end]
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                if part.is_empty() {
                    None
                } else {
                    Some(part.parse().expect("npy shape item"))
                }
            })
            .collect();

        let data_start = header_start + header_len;
        let values: Vec<f32> = bytes[data_start..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        assert_eq!(values.len(), shape.iter().product::<usize>());
        (shape, values)
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f32, f32::max)
    }

    fn tensor_values(tensor: &FormulaTensor) -> Vec<f32> {
        tensor.iter().copied().collect()
    }

    #[test]
    fn golden_tensors_match_python_reference() {
        let dir = fixture_dir();
        let manifest: GoldenManifest = serde_json::from_str(
            &std::fs::read_to_string(dir.join("manifest.json")).expect("manifest should exist"),
        )
        .expect("manifest should parse");
        let preprocessor = FormulaPreprocessor::new();

        for entry in manifest.images {
            let image = image::open(dir.join(&entry.image))
                .unwrap_or_else(|error| panic!("failed to open {}: {error}", entry.image));
            let tensor = preprocessor
                .preprocess(&image)
                .expect("preprocess should succeed");
            assert_eq!(
                tensor.shape(),
                &[
                    1,
                    1,
                    FORMULA_INPUT_SIZE as usize,
                    FORMULA_INPUT_SIZE as usize
                ]
            );

            let values = tensor_values(&tensor);
            let (shape, golden) = load_npy(&dir.join(&entry.golden));
            assert_eq!(
                shape,
                vec![
                    1,
                    1,
                    FORMULA_INPUT_SIZE as usize,
                    FORMULA_INPUT_SIZE as usize
                ]
            );

            // Pillow/OpenCV SIMD resampling can differ by <= 1 ULP from the
            // Python reference. Stage 4's acceptance threshold is max_abs <=
            // 1e-5; the SHA-256 below fixes the Python golden tensor itself.
            let diff = max_abs_diff(&values, &golden);
            assert!(
                diff <= 1e-5,
                "{} tensor differs from Python reference: max_abs={diff}",
                entry.image
            );

            let min = values.iter().copied().fold(f32::INFINITY, f32::min);
            let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let mean = (values.iter().map(|v| *v as f64).sum::<f64>() / values.len() as f64) as f32;
            assert!(
                (min - entry.min).abs() <= 1e-5,
                "{} min mismatch",
                entry.image
            );
            assert!(
                (max - entry.max).abs() <= 1e-5,
                "{} max mismatch",
                entry.image
            );
            assert!(
                (mean - entry.mean).abs() <= 1e-5,
                "{} mean mismatch",
                entry.image
            );

            let mut golden_hasher = Sha256::new();
            for value in &golden {
                golden_hasher.update(value.to_le_bytes());
            }
            assert_eq!(
                format!("{:x}", golden_hasher.finalize()),
                entry.sha256_raw_f32le,
                "{} golden sha256 mismatch",
                entry.image
            );
        }
    }

    #[test]
    fn transparent_input_drops_alpha_like_pil_convert_rgb() {
        let preprocessor = FormulaPreprocessor::new();
        let rgba = RgbaImage::from_raw(
            2,
            2,
            vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0],
        )
        .expect("valid rgba");
        let rgb = RgbImage::from_raw(2, 2, vec![255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0])
            .expect("valid rgb");

        let from_rgba = preprocessor
            .preprocess(&DynamicImage::ImageRgba8(rgba))
            .expect("rgba preprocess");
        let from_rgb = preprocessor
            .preprocess(&DynamicImage::ImageRgb8(rgb))
            .expect("rgb preprocess");
        assert!(max_abs_diff(&tensor_values(&from_rgba), &tensor_values(&from_rgb)) <= 1e-6);
    }

    #[test]
    fn channel_order_uses_bgr2gray_contract() {
        let preprocessor = FormulaPreprocessor::new();
        let red = preprocessor
            .preprocess_rgb8(1, 1, &[255, 0, 0])
            .expect("red preprocess");
        let blue = preprocessor
            .preprocess_rgb8(1, 1, &[0, 0, 255])
            .expect("blue preprocess");
        let red_value = red[[0, 0, 0, 0]];
        let blue_value = blue[[0, 0, 0, 0]];
        let white_norm = (1.0_f32 - FORMULA_MEAN) / FORMULA_STD;
        let black_norm = (0.0_f32 - FORMULA_MEAN) / FORMULA_STD;
        let expected_red = 0.114 * white_norm + (0.587 + 0.299) * black_norm;
        let expected_blue = 0.299 * white_norm + (0.114 + 0.587) * black_norm;
        assert!(
            (red_value - expected_red).abs() <= 1e-5,
            "red_value={red_value}"
        );
        assert!(
            (blue_value - expected_blue).abs() <= 1e-5,
            "blue_value={blue_value}"
        );
        assert!(red_value < blue_value);
    }

    #[test]
    fn batch_preserves_sample_order() {
        let preprocessor = FormulaPreprocessor::new();
        let black = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([0, 0, 0])));
        let white = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([255, 255, 255])));
        let batch = preprocessor
            .preprocess_batch(&[black.clone(), white.clone()])
            .expect("batch preprocess");
        assert_eq!(batch.shape(), &[2, 1, 384, 384]);

        let black_single = preprocessor.preprocess(&black).expect("black single");
        let white_single = preprocessor.preprocess(&white).expect("white single");
        assert_eq!(
            batch
                .index_axis(Axis(0), 0)
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            tensor_values(&black_single)
        );
        assert_eq!(
            batch
                .index_axis(Axis(0), 1)
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            tensor_values(&white_single)
        );
    }

    #[test]
    fn empty_batch_is_supported() {
        let preprocessor = FormulaPreprocessor::new();
        let batch = preprocessor.preprocess_batch(&[]).expect("empty batch");
        assert_eq!(batch.shape(), &[0, 1, 384, 384]);
    }

    #[test]
    fn invalid_rgb_length_is_rejected() {
        let preprocessor = FormulaPreprocessor::new();
        let error = preprocessor
            .preprocess_rgb8(2, 2, &[0, 0, 0])
            .expect_err("short rgb input must fail");
        assert!(error.to_string().contains("size mismatch"));
    }
}
