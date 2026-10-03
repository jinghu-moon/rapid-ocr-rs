use crate::{
    config::RecImage,
    error::{RapidOcrError, Result},
    vision::resize::resize_bgr_inter_linear,
};

pub fn resize_image(img: &RecImage, new_w: usize, new_h: usize) -> Result<RecImage> {
    if new_w == 0 || new_h == 0 {
        return Err(RapidOcrError::InvalidImage(
            "resize target width/height must be greater than zero".to_string(),
        ));
    }

    let src_bgr = img.as_bgr_cow();
    let resized =
        resize_bgr_inter_linear(src_bgr.as_ref(), img.width(), img.height(), new_w, new_h)?;
    RecImage::from_bgr_u8(new_w, new_h, resized)
}

pub fn rotate_180_image(img: &RecImage) -> Result<RecImage> {
    let width = img.width();
    let height = img.height();
    let src = img.as_bgr_cow();
    let src = src.as_ref();
    let mut dst = vec![0_u8; src.len()];

    for y in 0..height {
        for x in 0..width {
            let src_idx = (y * width + x) * 3;
            let dst_y = height - 1 - y;
            let dst_x = width - 1 - x;
            let dst_idx = (dst_y * width + dst_x) * 3;
            dst[dst_idx..dst_idx + 3].copy_from_slice(&src[src_idx..src_idx + 3]);
        }
    }

    RecImage::from_bgr_u8(width, height, dst)
}
