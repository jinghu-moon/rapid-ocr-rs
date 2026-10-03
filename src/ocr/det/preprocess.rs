use rayon::prelude::*;

use crate::{
    config::RecImage,
    error::{RapidOcrError, Result},
    vision::resize::{LinearResizeScratch, resize_bgr_inter_linear_into_with_scratch},
};

#[derive(Debug, Clone)]
pub struct DetPreProcess {
    pub limit_side_len: usize,
    pub limit_type: String,
    pub mean: [f32; 3],
    pub std: [f32; 3],
}

impl Default for DetPreProcess {
    fn default() -> Self {
        Self {
            limit_side_len: 736,
            limit_type: "min".to_string(),
            mean: [0.5, 0.5, 0.5],
            std: [0.5, 0.5, 0.5],
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct DetPreprocessScratch {
    pub tmp_bgr: Vec<u8>,
    pub resize: LinearResizeScratch,
}

impl DetPreProcess {
    pub(crate) fn run_into_buffer_with_scratch(
        &self,
        img: &RecImage,
        buffer: &mut Vec<f32>,
        scratch: &mut DetPreprocessScratch,
        limit_side_len: Option<usize>,
    ) -> Result<(usize, usize)> {
        let (resize_h, resize_w) =
            self.compute_resize_hw(img, limit_side_len.unwrap_or(self.limit_side_len))?;
        let expected_len = 3usize
            .checked_mul(resize_h)
            .and_then(|v| v.checked_mul(resize_w))
            .ok_or_else(|| {
                RapidOcrError::InvalidInput("det output buffer size overflow".to_string())
            })?;
        buffer.resize(expected_len, 0.0);

        let src = img.as_bgr_cow();
        resize_bgr_inter_linear_into_with_scratch(
            src.as_ref(),
            img.width(),
            img.height(),
            resize_w,
            resize_h,
            &mut scratch.tmp_bgr,
            &mut scratch.resize,
        )?;
        self.normalize_bgr_into_slice(
            &scratch.tmp_bgr,
            resize_w,
            resize_h,
            &mut buffer[..expected_len],
        )?;

        Ok((resize_h, resize_w))
    }

    fn compute_resize_hw(&self, img: &RecImage, limit_side_len: usize) -> Result<(usize, usize)> {
        let h = img.height();
        let w = img.width();
        if h == 0 || w == 0 {
            return Err(RapidOcrError::InvalidImage(
                "detector image width/height cannot be zero".to_string(),
            ));
        }

        let ratio = if self.limit_type == "max" {
            if h.max(w) > limit_side_len {
                limit_side_len as f32 / h.max(w) as f32
            } else {
                1.0
            }
        } else if h.min(w) < limit_side_len {
            limit_side_len as f32 / h.min(w) as f32
        } else {
            1.0
        };

        // Keep parity with Python: int(h * ratio), int(w * ratio), then round to 32-multiples.
        let mut resize_h = ((h as f32 * ratio) as usize).max(1);
        let mut resize_w = ((w as f32 * ratio) as usize).max(1);
        resize_h = ((resize_h as f32 / 32.0).round_ties_even() as usize * 32).max(32);
        resize_w = ((resize_w as f32 / 32.0).round_ties_even() as usize * 32).max(32);
        Ok((resize_h, resize_w))
    }

    fn normalize_bgr_into_slice(
        &self,
        src: &[u8],
        width: usize,
        height: usize,
        out_slice: &mut [f32],
    ) -> Result<()> {
        let expected_len = 3usize
            .checked_mul(height)
            .and_then(|v| v.checked_mul(width))
            .ok_or_else(|| {
                RapidOcrError::InvalidInput("det output buffer size overflow".to_string())
            })?;
        let expected_src_len = expected_len;
        if src.len() != expected_src_len {
            return Err(RapidOcrError::InvalidInput(format!(
                "det source BGR size mismatch: expected {expected_src_len}, got {}",
                src.len()
            )));
        }
        if out_slice.len() != expected_len {
            return Err(RapidOcrError::InvalidInput(format!(
                "det output buffer size mismatch: expected {expected_len}, got {}",
                out_slice.len()
            )));
        }

        let plane_stride = width * height;
        let row_src_stride = width * 3;
        let row_parallel = should_parallelize_rows(width, height);
        let norm_mul = [
            (1.0 / 255.0) / self.std[0],
            (1.0 / 255.0) / self.std[1],
            (1.0 / 255.0) / self.std[2],
        ];
        let norm_add = [
            -self.mean[0] / self.std[0],
            -self.mean[1] / self.std[1],
            -self.mean[2] / self.std[2],
        ];
        let use_avx2 = std::arch::is_x86_feature_detected!("avx2");

        if row_parallel {
            let out_addr = out_slice.as_mut_ptr() as usize;
            let src_addr = src.as_ptr() as usize;
            (0..height).into_par_iter().for_each(|y| {
                let out_ptr = out_addr as *mut f32;
                let src_ptr = src_addr as *const u8;
                // Safety:
                // - `y` is unique per parallel iteration, so each worker writes disjoint row ranges.
                // - `out_ptr` points to a contiguous CHW buffer of size `3 * width * height`.
                // - `src_ptr` points to a contiguous BGR buffer of size `3 * width * height`.
                unsafe {
                    let row_ptr = src_ptr.add(y * row_src_stride);
                    if use_avx2 {
                        write_normalized_row_avx2(
                            row_ptr,
                            out_ptr,
                            y,
                            width,
                            plane_stride,
                            norm_mul,
                            norm_add,
                        );
                    } else {
                        write_normalized_row_scalar(
                            row_ptr,
                            out_ptr,
                            y,
                            width,
                            plane_stride,
                            norm_mul,
                            norm_add,
                        );
                    }
                }
            });
        } else {
            for y in 0..height {
                // Safety:
                // - `y` stays within `[0, height)`.
                // - Source and destination pointers are derived from validated contiguous buffers.
                unsafe {
                    let row_ptr = src.as_ptr().add(y * row_src_stride);
                    if use_avx2 {
                        write_normalized_row_avx2(
                            row_ptr,
                            out_slice.as_mut_ptr(),
                            y,
                            width,
                            plane_stride,
                            norm_mul,
                            norm_add,
                        );
                    } else {
                        write_normalized_row_scalar(
                            row_ptr,
                            out_slice.as_mut_ptr(),
                            y,
                            width,
                            plane_stride,
                            norm_mul,
                            norm_add,
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[inline]
fn should_parallelize_rows(width: usize, height: usize) -> bool {
    width
        .checked_mul(height)
        .is_some_and(|v| v >= 512 * 512 && rayon::current_num_threads() > 1)
}

#[inline]
unsafe fn write_normalized_row_scalar(
    src_row_ptr: *const u8,
    out_ptr: *mut f32,
    y: usize,
    width: usize,
    plane_stride: usize,
    norm_mul: [f32; 3],
    norm_add: [f32; 3],
) {
    // Safety:
    // - Caller guarantees all pointers are valid for the computed row range.
    // - The row writes target disjoint regions when used in parallel.
    unsafe {
        let mut src_px = src_row_ptr;
        let row_base = y * width;
        for x in 0..width {
            let row_offset = row_base + x;
            let b = (*src_px) as f32 * norm_mul[0] + norm_add[0];
            let g = (*src_px.add(1)) as f32 * norm_mul[1] + norm_add[1];
            let r = (*src_px.add(2)) as f32 * norm_mul[2] + norm_add[2];

            *out_ptr.add(row_offset) = b;
            *out_ptr.add(plane_stride + row_offset) = g;
            *out_ptr.add(plane_stride * 2 + row_offset) = r;
            src_px = src_px.add(3);
        }
    }
}

#[target_feature(enable = "avx2")]
unsafe fn write_normalized_row_avx2(
    src_row_ptr: *const u8,
    out_ptr: *mut f32,
    y: usize,
    width: usize,
    plane_stride: usize,
    norm_mul: [f32; 3],
    norm_add: [f32; 3],
) {
    use std::arch::x86_64::{
        __m256, __m256i, _mm256_add_ps, _mm256_cvtepi32_ps, _mm256_mul_ps, _mm256_set1_ps,
        _mm256_setr_epi32, _mm256_storeu_ps,
    };

    let row_base = y * width;
    let mut x = 0usize;
    let b_mul: __m256 = _mm256_set1_ps(norm_mul[0]);
    let g_mul: __m256 = _mm256_set1_ps(norm_mul[1]);
    let r_mul: __m256 = _mm256_set1_ps(norm_mul[2]);
    let b_add: __m256 = _mm256_set1_ps(norm_add[0]);
    let g_add: __m256 = _mm256_set1_ps(norm_add[1]);
    let r_add: __m256 = _mm256_set1_ps(norm_add[2]);

    while x + 8 <= width {
        // De-interleave 8 BGR pixels into channel vectors.
        let mut b = [0_i32; 8];
        let mut g = [0_i32; 8];
        let mut r = [0_i32; 8];
        for lane in 0..8 {
            // Safety:
            // - Caller guarantees `src_row_ptr` is valid for the current row.
            // - `x + lane < width` and each pixel has 3 channels.
            unsafe {
                let p = src_row_ptr.add((x + lane) * 3);
                b[lane] = *p as i32;
                g[lane] = *p.add(1) as i32;
                r[lane] = *p.add(2) as i32;
            }
        }

        let b_i32: __m256i = _mm256_setr_epi32(b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]);
        let g_i32: __m256i = _mm256_setr_epi32(g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]);
        let r_i32: __m256i = _mm256_setr_epi32(r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]);

        let b_f32 = _mm256_add_ps(_mm256_mul_ps(_mm256_cvtepi32_ps(b_i32), b_mul), b_add);
        let g_f32 = _mm256_add_ps(_mm256_mul_ps(_mm256_cvtepi32_ps(g_i32), g_mul), g_add);
        let r_f32 = _mm256_add_ps(_mm256_mul_ps(_mm256_cvtepi32_ps(r_i32), r_mul), r_add);

        let row_offset = row_base + x;
        // Safety:
        // - Destination pointer is a contiguous CHW buffer sized for the full image.
        // - Row offsets are in-bounds for this row and channel planes.
        unsafe {
            _mm256_storeu_ps(out_ptr.add(row_offset), b_f32);
            _mm256_storeu_ps(out_ptr.add(plane_stride + row_offset), g_f32);
            _mm256_storeu_ps(out_ptr.add(plane_stride * 2 + row_offset), r_f32);
        }
        x += 8;
    }

    if x < width {
        // Safety:
        // - Tail starts within current source row; each iteration advances by one pixel.
        let mut src_px = unsafe { src_row_ptr.add(x * 3) };
        for px in x..width {
            let row_offset = row_base + px;
            // Safety:
            // - Source and destination pointers remain in-bounds for the tail range.
            unsafe {
                let b = (*src_px) as f32 * norm_mul[0] + norm_add[0];
                let g = (*src_px.add(1)) as f32 * norm_mul[1] + norm_add[1];
                let r = (*src_px.add(2)) as f32 * norm_mul[2] + norm_add[2];
                *out_ptr.add(row_offset) = b;
                *out_ptr.add(plane_stride + row_offset) = g;
                *out_ptr.add(plane_stride * 2 + row_offset) = r;
                src_px = src_px.add(3);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DetPreProcess, write_normalized_row_avx2, write_normalized_row_scalar};

    /// 运行时检查 AVX2 可用性；不可用时测试**必须**明确跳过而不是假装通过。
    fn avx2_available() -> bool {
        if std::arch::is_x86_feature_detected!("avx2") {
            true
        } else {
            eprintln!("skipping AVX2 parity assertion: this CPU reports no AVX2 support");
            false
        }
    }

    /// 归一化系数。
    ///
    /// 数值刻意取成：三通道互不相同（否则“通道串位”无法被发现）、且都不等于 1.0
    /// （否则平凡乘法会掩盖 `mul`/`add` 顺序错误）。用 `f32` 字面量而不是算术表达式，
    /// 是为了避免 clippy 的 `eq_op` 误报，同时让常量本身一眼可读。
    const NORM_MUL: [f32; 3] = [2.0, 1.0, 4.0];
    const NORM_ADD: [f32; 3] = [-1.6, -1.0, -4.8];

    fn synthetic_bgr(width: usize, height: usize) -> Vec<u8> {
        // 伪随机但确定：三个通道必须互不相同，否则通道串位无法被发现。
        (0..width * height * 3)
            .map(|i| ((i * 37 + i / 7 + 11) % 256) as u8)
            .collect()
    }

    /// 未写入位置的哨兵值。
    ///
    /// 刻意使用一个**唯一位模式**的 NaN，而不是 `f32::NAN`：`f32::NAN != f32::NAN`，
    /// 直接用 `assert_eq!` 比较带未写入位置的缓冲区会因为“NaN 不等于自己”而假失败，
    /// 把真正的实现差异淹掉。改用位比较后，未写入位置与任何真实输出都不相等。
    const SENTINEL: f32 = f32::from_bits(0x7FC0_1234);

    fn assert_bits_equal(a: &[f32], b: &[f32], context: &str) {
        assert_eq!(a.len(), b.len(), "{context}: length mismatch");
        for (index, (left, right)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(
                left.to_bits(),
                right.to_bits(),
                "{context}: bit-exact mismatch at index {index}: {left} vs {right}"
            );
        }
    }

    /// 用 AVX2 与 scalar 两条实现写同一张图，逐位比较整个 CHW 输出。
    fn assert_row_writers_agree(width: usize, height: usize, extra_plane_stride: usize) {
        if !avx2_available() {
            return;
        }
        let src = synthetic_bgr(width, height);
        let plane_stride = width * height + extra_plane_stride;
        let total = plane_stride * 2 + width * height;

        let mut scalar_out = vec![SENTINEL; total];
        let mut avx2_out = vec![SENTINEL; total];

        for y in 0..height {
            // Safety: both functions are called with the exact contract used by
            // `normalize_bgr_into_slice`: `src` holds `width * height * 3` bytes and
            // each destination buffer holds at least `2 * plane_stride + width * height`
            // f32 values, so every channel row write stays in bounds.
            unsafe {
                let row_ptr = src.as_ptr().add(y * width * 3);
                write_normalized_row_scalar(
                    row_ptr,
                    scalar_out.as_mut_ptr(),
                    y,
                    width,
                    plane_stride,
                    NORM_MUL,
                    NORM_ADD,
                );
                write_normalized_row_avx2(
                    row_ptr,
                    avx2_out.as_mut_ptr(),
                    y,
                    width,
                    plane_stride,
                    NORM_MUL,
                    NORM_ADD,
                );
            }
        }

        assert_bits_equal(
            &scalar_out,
            &avx2_out,
            &format!(
                "AVX2 and scalar normalization disagree for width={width} height={height} \
                 extra_plane_stride={extra_plane_stride}"
            ),
        );
    }

    #[test]
    fn avx2_and_scalar_row_writers_agree_on_block_and_tail_widths() {
        // 8 的倍数走纯向量路径；其余宽度必定走向量 + 标量尾巴两条路径混合。
        for width in [1usize, 2, 7, 8, 9, 15, 16, 17, 23, 24, 31, 33, 64, 65] {
            for height in [1usize, 3, 8] {
                assert_row_writers_agree(width, height, 0);
            }
        }
    }

    #[test]
    fn avx2_and_scalar_row_writers_agree_across_plane_strides() {
        // `plane_stride` 决定三个通道平面之间的偏移；非紧凑 stride 用来证明
        // 两条实现使用同一个 `plane_stride`，而不是各自硬编码的紧凑布局。
        for extra in [0usize, 1, 7, 8, 33] {
            for width in [5usize, 8, 11, 19] {
                assert_row_writers_agree(width, 4, extra);
            }
        }
    }

    #[test]
    fn avx2_and_scalar_row_writers_agree_with_row_offset_path() {
        // `normalize_bgr_into_slice` 的并行分支把 `out_slice.as_mut_ptr()` 作为
        // 行基准传入，行位置由 `y * width` 计算。这里用带前缀偏移的缓冲区复现该
        // 调用形态，证明“out_ptr 行偏移”路径同样一致。
        if !avx2_available() {
            return;
        }
        const PREFIX: usize = 13;
        for width in [7usize, 8, 13, 16, 21] {
            let height = 5usize;
            let src = synthetic_bgr(width, height);
            let plane_stride = width * height;
            let total = PREFIX + plane_stride * 2 + width * height;

            let mut scalar_buf = vec![SENTINEL; total];
            let mut avx2_buf = vec![SENTINEL; total];

            for y in 0..height {
                // Safety: pointers start `PREFIX` floats into a buffer that is
                // `PREFIX` floats larger than the row-writer contract requires.
                unsafe {
                    let row_ptr = src.as_ptr().add(y * width * 3);
                    write_normalized_row_scalar(
                        row_ptr,
                        scalar_buf.as_mut_ptr().add(PREFIX),
                        y,
                        width,
                        plane_stride,
                        NORM_MUL,
                        NORM_ADD,
                    );
                    write_normalized_row_avx2(
                        row_ptr,
                        avx2_buf.as_mut_ptr().add(PREFIX),
                        y,
                        width,
                        plane_stride,
                        NORM_MUL,
                        NORM_ADD,
                    );
                }
            }

            assert_bits_equal(
                &scalar_buf,
                &avx2_buf,
                &format!("row-offset path disagree for width={width}"),
            );
            // 前缀必须未被写入：证明行偏移恰好是 `y * width`，没有越界回写。
            assert!(
                scalar_buf[..PREFIX]
                    .iter()
                    .all(|v| v.to_bits() == SENTINEL.to_bits()),
                "scalar writer wrote before the row base for width={width}"
            );
            assert!(
                avx2_buf[..PREFIX]
                    .iter()
                    .all(|v| v.to_bits() == SENTINEL.to_bits()),
                "avx2 writer wrote before the row base for width={width}"
            );
        }
    }

    #[test]
    fn normalize_bgr_into_slice_matches_naive_reference() {
        // 端到端（含 `is_x86_feature_detected` 分派）对照一份不依赖任何 SIMD 的
        // 朴素参考实现，锁死数值与 CHW 布局。
        let prep = DetPreProcess {
            limit_side_len: 32,
            limit_type: "max".to_string(),
            mean: [0.4, 0.5, 0.6],
            std: [0.25, 0.5, 0.125],
        };

        for (width, height) in [(1usize, 1usize), (7, 3), (8, 4), (13, 5), (40, 12)] {
            let src = synthetic_bgr(width, height);
            let plane_stride = width * height;
            let mut out = vec![SENTINEL; plane_stride * 3];
            prep.normalize_bgr_into_slice(&src, width, height, &mut out)
                .expect("normalization must succeed");

            // 参考实现用与生产代码**相同的算子顺序**（先乘后加，系数分别预计算），
            // 因此这里可以要求逐位相同；布局断言则独立于浮点误差。
            let mul = [
                (1.0_f32 / 255.0) / prep.std[0],
                (1.0_f32 / 255.0) / prep.std[1],
                (1.0_f32 / 255.0) / prep.std[2],
            ];
            let add = [
                -prep.mean[0] / prep.std[0],
                -prep.mean[1] / prep.std[1],
                -prep.mean[2] / prep.std[2],
            ];

            for y in 0..height {
                for x in 0..width {
                    let row_offset = y * width + x;
                    let pixel = (y * width + x) * 3;
                    let expected = [
                        src[pixel] as f32 * mul[0] + add[0],
                        src[pixel + 1] as f32 * mul[1] + add[1],
                        src[pixel + 2] as f32 * mul[2] + add[2],
                    ];
                    for (channel, want) in expected.iter().enumerate() {
                        let got = out[plane_stride * channel + row_offset];
                        assert_eq!(
                            got.to_bits(),
                            want.to_bits(),
                            "channel {channel} mismatch at ({x},{y}) for {width}x{height}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn normalize_bgr_into_slice_rejects_size_mismatch() {
        let prep = DetPreProcess::default();
        let src = synthetic_bgr(4, 4);
        let mut out = vec![0.0_f32; 4 * 4 * 3 - 1];
        let err = prep
            .normalize_bgr_into_slice(&src, 4, 4, &mut out)
            .expect_err("short destination must be rejected");
        assert!(
            err.to_string().contains("output buffer size mismatch"),
            "{err}"
        );

        let mut ok = vec![0.0_f32; 4 * 4 * 3];
        let err = prep
            .normalize_bgr_into_slice(&src[..src.len() - 1], 4, 4, &mut ok)
            .expect_err("short source must be rejected");
        assert!(
            err.to_string().contains("source BGR size mismatch"),
            "{err}"
        );
    }
}
