//! AetherLink USB redirection over `UsbMessage` streams.
//!
//! * Host: [`serve_usb_stream`] answers one `StreamKind::Usb { device_id }` stream using a
//!   local [`UsbBackend`] (normally [`crate::UsbHost`]). A stream carries `ListDevices`, or
//!   an `Attach` followed by `Submit`/`Unlink`/`Detach` for that one device.
//! * Client: [`RemoteUsb`] implements [`UsbBackend`] by opening such streams through a
//!   [`UsbStreamConnector`] (implemented by `aether-core` on top of the QUIC connection),
//!   so it can be exported locally with [`crate::usbip_server::serve`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aether_protocol::framing::{read_frame, write_frame};
use aether_protocol::{UrbComplete, UrbStatus, UrbSubmit, UsbDeviceInfo, UsbMessage};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{Result, UsbError};
use crate::usbip_server::UsbBackend;

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// Opens a new USB stream to the host. `device_id` 0 is used for device listing.
#[async_trait]
pub trait UsbStreamConnector: Send + Sync + 'static {
    async fn open(&self, device_id: u32) -> std::io::Result<(BoxRead, BoxWrite)>;
}

/// Host side: serve one USB stream until the peer closes it. The attached device (if any)
/// is detached at the end, which also cancels its in-flight URBs.
pub async fn serve_usb_stream<R, W>(backend: Arc<dyn UsbBackend>, mut r: R, mut w: W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<UsbMessage>();
    let reader = async move {
        let mut attached: Option<u32> = None;
        let res = loop {
            let msg = match read_frame::<_, UsbMessage>(&mut r).await {
                Ok(Some(m)) => m,
                Ok(None) => break Ok(()),
                Err(e) => break Err(UsbError::from(e)),
            };
            match msg {
                UsbMessage::ListDevices => {
                    let list = backend.list_devices().await.unwrap_or_else(|e| {
                        tracing::warn!("usb list_devices: {e}");
                        Vec::new()
                    });
                    let _ = tx.send(UsbMessage::Devices(list));
                }
                UsbMessage::Attach { device_id } => {
                    let reply = if attached.is_some() {
                        UsbMessage::Attached {
                            device_id,
                            ok: false,
                            error: Some("stream already has a device".into()),
                        }
                    } else {
                        match backend.attach(device_id).await {
                            Ok(()) => {
                                attached = Some(device_id);
                                UsbMessage::Attached {
                                    device_id,
                                    ok: true,
                                    error: None,
                                }
                            }
                            Err(e) => UsbMessage::Attached {
                                device_id,
                                ok: false,
                                error: Some(e.to_string()),
                            },
                        }
                    };
                    let _ = tx.send(reply);
                }
                UsbMessage::Detach { device_id } => {
                    if attached == Some(device_id) {
                        attached = None;
                        let _ = backend.detach(device_id).await;
                    }
                }
                UsbMessage::Submit(urb) => match attached {
                    Some(dev) => {
                        let (b, tx) = (backend.clone(), tx.clone());
                        tokio::spawn(async move {
                            let done = b.submit(dev, urb).await;
                            let gone = done.status == UrbStatus::NoDevice;
                            let _ = tx.send(UsbMessage::Complete(done));
                            if gone {
                                let _ = tx.send(UsbMessage::DeviceGone { device_id: dev });
                            }
                        });
                    }
                    None => {
                        let _ = tx.send(UsbMessage::Complete(UrbComplete {
                            urb_id: urb.urb_id,
                            status: UrbStatus::NoDevice,
                            data: Vec::new(),
                            actual_length: 0,
                        }));
                    }
                },
                UsbMessage::Unlink { urb_id } => {
                    if let Some(dev) = attached {
                        backend.unlink(dev, urb_id).await;
                    }
                }
                other => tracing::debug!("usb host stream: ignoring {other:?}"),
            }
        };
        if let Some(dev) = attached {
            let _ = backend.detach(dev).await;
        }
        res
    };
    let writer = async move {
        while let Some(m) = rx.recv().await {
            write_frame(&mut w, &m).await?;
            if rx.is_empty() {
                w.flush().await?;
            }
        }
        let _ = w.shutdown().await;
        Ok::<(), UsbError>(())
    };
    tokio::try_join!(reader, writer)?;
    Ok(())
}

type UrbWaiters = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<UrbComplete>>>>>;

struct DeviceLink {
    tx: mpsc::UnboundedSender<UsbMessage>,
    urbs: UrbWaiters,
    reader: JoinHandle<()>,
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.reader.abort();
        self.urbs.lock().take();
    }
}

/// Client-side [`UsbBackend`] that forwards to a remote host over USB streams.
pub struct RemoteUsb {
    connector: Arc<dyn UsbStreamConnector>,
    links: Mutex<HashMap<u32, Arc<DeviceLink>>>,
    timeout: Duration,
}

impl RemoteUsb {
    pub fn new(connector: Arc<dyn UsbStreamConnector>) -> Self {
        Self {
            connector,
            links: Mutex::new(HashMap::new()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Device ids currently attached through this client.
    pub fn attached(&self) -> Vec<u32> {
        self.links.lock().keys().copied().collect()
    }
}

#[async_trait]
impl UsbBackend for RemoteUsb {
    async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        let (mut r, mut w) = self.connector.open(0).await?;
        write_frame(&mut w, &UsbMessage::ListDevices).await?;
        w.flush().await?;
        let fut = async {
            loop {
                match read_frame::<_, UsbMessage>(&mut r).await? {
                    Some(UsbMessage::Devices(d)) => return Ok(d),
                    Some(_) => continue,
                    None => return Err(UsbError::Closed),
                }
            }
        };
        let res = tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| UsbError::Backend("list timed out".into()))?;
        let _ = w.shutdown().await;
        res
    }

    async fn attach(&self, device_id: u32) -> Result<()> {
        if self.links.lock().contains_key(&device_id) {
            return Ok(());
        }
        let (mut r, mut w) = self.connector.open(device_id).await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<UsbMessage>();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if write_frame(&mut w, &m).await.is_err()
                    || (rx.is_empty() && w.flush().await.is_err())
                {
                    return;
                }
            }
            let _ = w.shutdown().await;
        });
        let urbs: UrbWaiters = Arc::new(Mutex::new(Some(HashMap::new())));
        let (att_tx, att_rx) = oneshot::channel::<std::result::Result<(), String>>();
        let u = urbs.clone();
        let reader = tokio::spawn(async move {
            let mut att_tx = Some(att_tx);
            while let Ok(Some(m)) = read_frame::<_, UsbMessage>(&mut r).await {
                match m {
                    UsbMessage::Complete(c) => {
                        if let Some(w) = u.lock().as_mut().and_then(|m| m.remove(&c.urb_id)) {
                            let _ = w.send(c);
                        }
                    }
                    UsbMessage::Attached { ok, error, .. } => {
                        if let Some(t) = att_tx.take() {
                            let _ = t.send(if ok {
                                Ok(())
                            } else {
                                Err(error.unwrap_or_default())
                            });
                        }
                    }
                    UsbMessage::DeviceGone { .. } => break,
                    _ => {}
                }
            }
            // Wake every waiter (they report NoDevice).
            u.lock().take();
        });
        tx.send(UsbMessage::Attach { device_id })
            .map_err(|_| UsbError::Closed)?;
        let res = tokio::time::timeout(self.timeout, att_rx).await;
        match res {
            Ok(Ok(Ok(()))) => {
                self.links
                    .lock()
                    .insert(device_id, Arc::new(DeviceLink { tx, urbs, reader }));
                Ok(())
            }
            Ok(Ok(Err(e))) => {
                reader.abort();
                Err(UsbError::Backend(e))
            }
            Ok(Err(_)) => {
                reader.abort();
                Err(UsbError::Closed)
            }
            Err(_) => {
                reader.abort();
                Err(UsbError::Backend("attach timed out".into()))
            }
        }
    }

    async fn detach(&self, device_id: u32) -> Result<()> {
        let link = self
            .links
            .lock()
            .remove(&device_id)
            .ok_or(UsbError::NotAttached(device_id))?;
        let _ = link.tx.send(UsbMessage::Detach { device_id });
        Ok(())
    }

    async fn submit(&self, device_id: u32, urb: UrbSubmit) -> UrbComplete {
        let urb_id = urb.urb_id;
        let gone = UrbComplete {
            urb_id,
            status: UrbStatus::NoDevice,
            data: Vec::new(),
            actual_length: 0,
        };
        let Some(link) = self.links.lock().get(&device_id).cloned() else {
            return gone;
        };
        let (otx, orx) = oneshot::channel();
        match link.urbs.lock().as_mut() {
            Some(m) => {
                m.insert(urb_id, otx);
            }
            None => return gone,
        }
        if link.tx.send(UsbMessage::Submit(urb)).is_err() {
            return gone;
        }
        drop(link);
        orx.await.unwrap_or(gone)
    }

    async fn unlink(&self, device_id: u32, urb_id: u64) {
        if let Some(link) = self.links.lock().get(&device_id) {
            let _ = link.tx.send(UsbMessage::Unlink { urb_id });
        }
    }
}
