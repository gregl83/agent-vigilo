use std::{
    io::{
        BufWriter,
        stdout,
        Stdout,
        Write,
    },
    sync::Mutex
};

use tokio::sync::OnceCell;
use tracing::{debug, error};


pub struct Buffer {
    buffer: Mutex<BufWriter<Stdout>>,
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            buffer: Mutex::new(BufWriter::new(stdout()))
        }
    }

    /// Write line to buffer
    pub fn write_line(&self, msg: impl AsRef<str>) -> anyhow::Result<()> {
        let mut guard = self.buffer.lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire buffer lock"))?;;
        writeln!(guard, "{}", msg.as_ref())?;
        Ok(())
    }

    /// Flush buffer to stdout
    pub fn flush(&self) -> anyhow::Result<()> {
        let mut guard = self.buffer.lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire buffer lock"))?;
        guard.flush()?;
        Ok(())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Err(_) = self.flush() {
            error!("error flushing output buffer during drop");
        } else {
            debug!("output buffer flushed successfully on drop");
        }
    }
}

pub struct Context {
    pub(crate) cell: OnceCell<Buffer>,
}

impl Context {
    pub async fn get(&self) -> anyhow::Result<&Buffer> {
        self.cell.get_or_try_init(|| async {
            debug!("initializing out write buffer");
            Ok(Buffer::new())
        }).await
    }
}
