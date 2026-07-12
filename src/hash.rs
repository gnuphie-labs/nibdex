// SPDX-License-Identifier: MIT

use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio::fs;

pub async fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).await?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}
