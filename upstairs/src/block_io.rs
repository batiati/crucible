// Copyright 2021 Oxide Computer Company

use super::*;

use std::fs::{File, OpenOptions};
use std::io::SeekFrom;
use std::sync::atomic::{AtomicU32, Ordering};

/// Implement BlockIO for a file
pub struct FileBlockIO {
    uuid: Uuid,
    block_size: u64,
    total_size: u64,
    file: Mutex<File>,
}

impl FileBlockIO {
    pub fn new(id: Uuid, block_size: u64, path: String) -> Result<Self> {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Err(e) => {
                bail!("Error: e {} No extent file found for {:?}", e, path);
            }
            Ok(f) => {
                let total_size = f.metadata()?.len();

                Ok(Self {
                    uuid: id,
                    block_size,
                    total_size: total_size as u64,
                    file: Mutex::new(f),
                })
            }
        }
    }
}

#[async_trait]
impl BlockIO for FileBlockIO {
    async fn activate(&self) -> Result<(), CrucibleError> {
        Ok(())
    }

    async fn deactivate(&self) -> Result<(), CrucibleError> {
        Ok(())
    }

    async fn query_is_active(&self) -> Result<bool, CrucibleError> {
        Ok(true)
    }

    async fn total_size(&self) -> Result<u64, CrucibleError> {
        Ok(self.total_size)
    }

    async fn get_block_size(&self) -> Result<u64, CrucibleError> {
        Ok(self.block_size)
    }

    async fn get_uuid(&self) -> Result<Uuid, CrucibleError> {
        Ok(self.uuid)
    }

    async fn read(
        &self,
        offset: Block,
        data: Buffer,
    ) -> Result<(), CrucibleError> {
        let mut data_vec = data.as_vec().await;
        let mut owned_vec = data.owned_vec().await;

        let start = offset.value * self.block_size;

        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut data_vec[..])?;

        for i in 0..data_vec.len() {
            owned_vec[i] = true;
        }

        Ok(())
    }

    async fn write(
        &self,
        offset: Block,
        data: Bytes,
    ) -> Result<(), CrucibleError> {
        let start = offset.value * self.block_size;

        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(start))?;
        file.write_all(&data[..])?;

        Ok(())
    }

    async fn write_unwritten(
        &self,
        _offset: Block,
        _data: Bytes,
    ) -> Result<(), CrucibleError> {
        crucible_bail!(
            Unsupported,
            "write_unwritten unsupported for FileBlockIO"
        )
    }

    async fn flush(
        &self,
        _snapshot_details: Option<SnapshotDetails>,
    ) -> Result<(), CrucibleError> {
        let mut file = self.file.lock().await;
        file.flush()?;
        Ok(())
    }

    async fn show_work(&self) -> Result<WQCounts, CrucibleError> {
        Ok(WQCounts {
            up_count: 0,
            ds_count: 0,
        })
    }
}

// Implement BlockIO over an HTTP(S) url
use reqwest::header::{CONTENT_LENGTH, RANGE};
use reqwest::Client;
use std::str::FromStr;

pub struct ReqwestBlockIO {
    uuid: Uuid,
    block_size: u64,
    total_size: u64,
    client: Client,
    url: String,
    count: AtomicU32, // Used for dtrace probes
}

impl ReqwestBlockIO {
    pub async fn new(
        id: Uuid,
        block_size: u64,
        url: String,
    ) -> Result<Self, CrucibleError> {
        let client = Client::new();

        let response = client
            .head(&url)
            .send()
            .await
            .map_err(|e| CrucibleError::GenericError(e.to_string()))?;
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .ok_or("no content length!")
            .map_err(|e| CrucibleError::GenericError(e.to_string()))?;
        let total_size = u64::from_str(
            content_length
                .to_str()
                .map_err(|e| CrucibleError::GenericError(e.to_string()))?,
        )
        .map_err(|e| CrucibleError::GenericError(e.to_string()))?;

        Ok(Self {
            uuid: id,
            block_size,
            total_size: total_size as u64,
            client,
            url,
            count: AtomicU32::new(0),
        })
    }

    // Increment the counter to allow all IOs to have a unique number
    // for dtrace probes.
    pub fn next_count(&self) -> u32 {
        self.count.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl BlockIO for ReqwestBlockIO {
    async fn activate(&self) -> Result<(), CrucibleError> {
        Ok(())
    }

    async fn deactivate(&self) -> Result<(), CrucibleError> {
        Ok(())
    }

    async fn query_is_active(&self) -> Result<bool, CrucibleError> {
        Ok(true)
    }

    async fn total_size(&self) -> Result<u64, CrucibleError> {
        Ok(self.total_size)
    }

    async fn get_block_size(&self) -> Result<u64, CrucibleError> {
        Ok(self.block_size)
    }

    async fn get_uuid(&self) -> Result<Uuid, CrucibleError> {
        Ok(self.uuid)
    }

    async fn read(
        &self,
        offset: Block,
        data: Buffer,
    ) -> Result<(), CrucibleError> {
        let cc = self.next_count();
        cdt::reqwest__read__start!(|| (cc, self.uuid));

        let mut data_vec = data.as_vec().await;
        let mut owned_vec = data.owned_vec().await;

        let start = offset.value * self.block_size;

        let response = self
            .client
            .get(&self.url)
            .header(
                RANGE,
                format!(
                    "bytes={}-{}",
                    start,
                    start + data_vec.len() as u64 - 1
                ),
            )
            .send()
            .await
            .map_err(|e| CrucibleError::GenericError(e.to_string()))?;

        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .ok_or("no content length!")
            .map_err(|e| CrucibleError::GenericError(e.to_string()))?;
        let total_size = u64::from_str(
            content_length
                .to_str()
                .map_err(|e| CrucibleError::GenericError(e.to_string()))?,
        )
        .map_err(|e| CrucibleError::GenericError(e.to_string()))?;

        assert_eq!(total_size, data_vec.len() as u64);

        let bytes = response
            .bytes()
            .await
            .map_err(|e| CrucibleError::GenericError(e.to_string()))?;

        for i in 0..data_vec.len() {
            data_vec[i] = bytes[i];
            owned_vec[i] = true;
        }

        cdt::reqwest__read__done!(|| (cc, self.uuid));
        Ok(())
    }

    async fn write(
        &self,
        _offset: Block,
        _data: Bytes,
    ) -> Result<(), CrucibleError> {
        crucible_bail!(Unsupported, "write unsupported for ReqwestBlockIO")
    }

    async fn write_unwritten(
        &self,
        _offset: Block,
        _data: Bytes,
    ) -> Result<(), CrucibleError> {
        crucible_bail!(
            Unsupported,
            "write_unwritten unsupported for ReqwestBlockIO"
        )
    }

    async fn flush(
        &self,
        _snapshot_details: Option<SnapshotDetails>,
    ) -> Result<(), CrucibleError> {
        Ok(())
    }

    async fn show_work(&self) -> Result<WQCounts, CrucibleError> {
        Ok(WQCounts {
            up_count: 0,
            ds_count: 0,
        })
    }
}
