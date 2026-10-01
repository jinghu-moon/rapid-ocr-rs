use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

use crate::error::{RapidOcrError, Result};

pub fn default_model_store_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("rapid-ocr-rs")
            .join("models");
    }

    PathBuf::from("models")
}

pub fn verify_existing_file(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    if !path.exists() {
        return Err(RapidOcrError::FileNotFound(path));
    }
    if !path.is_file() {
        return Err(RapidOcrError::Config(format!(
            "expected a file path, got directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

pub fn ensure_downloaded(
    file_url: &str,
    expected_sha256: Option<&str>,
    save_dir: impl AsRef<Path>,
) -> Result<PathBuf> {
    let save_dir = save_dir.as_ref();
    fs::create_dir_all(save_dir)?;

    let file_name = extract_file_name(file_url)?;
    let target_path = save_dir.join(file_name);

    if target_path.exists() {
        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(&target_path)?;
            if actual.eq_ignore_ascii_case(expected) {
                return Ok(target_path);
            }
        } else {
            return Ok(target_path);
        }
    }

    let tmp_path = target_path.with_extension("part");
    if tmp_path.exists() {
        let _ = fs::remove_file(&tmp_path);
    }

    let client = build_http_client()?;
    let mut response = client
        .get(file_url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (compatible; rapid-ocr-rs model downloader)",
        )
        .header(reqwest::header::REFERER, "https://www.modelscope.cn/")
        .send()?;
    if !response.status().is_success() {
        return Err(RapidOcrError::Download(format!(
            "failed to download {file_url}: HTTP {}",
            response.status()
        )));
    }

    let mut hasher = Sha256::new();
    let mut file = fs::File::create(&tmp_path)?;
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read])?;
    }
    file.flush()?;
    file.sync_all()?;

    if let Some(expected) = expected_sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&tmp_path);
            return Err(RapidOcrError::HashMismatch {
                path: target_path,
                expected: expected.to_string(),
                actual,
            });
        }
    }

    if target_path.exists() {
        fs::remove_file(&target_path)?;
    }
    fs::rename(&tmp_path, &target_path)?;

    Ok(target_path)
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(Into::into)
}

fn extract_file_name(url: &str) -> Result<String> {
    let trimmed = url.split('?').next().unwrap_or(url);
    let file_name = trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RapidOcrError::Download(format!("cannot extract file name from url: {url}"))
        })?;
    Ok(file_name.to_string())
}

pub fn sha256_file(path: impl AsRef<Path>) -> Result<String> {
    let mut file = fs::File::open(path.as_ref())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::sha256_file;

    #[test]
    fn sha256_file_hashes_contents_without_loading_api_changes() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-sha256-{}-{suffix}.tmp",
            std::process::id()
        ));
        fs::write(&path, b"hello").expect("test fixture should be writable");

        let actual = sha256_file(&path).expect("hash should succeed");

        fs::remove_file(&path).expect("test fixture should be removable");
        assert_eq!(
            actual,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
