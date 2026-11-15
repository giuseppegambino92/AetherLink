//! USB/IP server: exports the devices of a [`UsbBackend`] to the Linux USB/IP client.
//!
//! ```text
//! # on the Linux client, with `serve` listening on 127.0.0.1:<port>
//! sudo modprobe vhci-hcd
//! usbip --tcp-port <port> list -r 127.0.0.1
//! sudo usbip --tcp-port <port> attach -r 127.0.0.1 -b <busid>
//! ```
//!
//! Bind the listener to loopback: USB/IP has no authentication of its own.

use std::collections::HashMap;
use std::sync::Arc;

use aether_protocol::{UrbComplete, UrbStatus, UrbSubmit, UsbDeviceInfo, UsbTransferType};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::descriptors::ConfigDescriptor;
use crate::error::Result;
use crate::usbip::{
    CmdSubmit, OpMessage, RetUnlink, ST_DEV_ERR, ST_NODEV, ST_OK, USBIP_DIR_IN, UrbPdu,
    UsbipDevice, cmd_submit_to_urb, errno, urb_complete_to_ret_submit,
};

/// A source of USB devices and URB execution: the local libusb [`crate::UsbHost`], or
/// [`crate::remote::RemoteUsb`] forwarding to a peer.
#[async_trait]
pub trait UsbBackend: Send + Sync + 'static {
    async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>>;
    async fn attach(&self, device_id: u32) -> Result<()>;
    async fn detach(&self, device_id: u32) -> Result<()>;
    /// Execute one URB; always completes (errors are reported in [`UrbComplete::status`]).
    async fn submit(&self, device_id: u32, urb: UrbSubmit) -> UrbComplete;
    /// Best-effort cancellation; the URB then completes with [`UrbStatus::Cancelled`]
    /// unless it already finished.
    async fn unlink(&self, device_id: u32, urb_id: u64);
}

/// USB/IP bus id under which a device is exported (`"<bus>-<address>"`).
pub fn busid_of(info: &UsbDeviceInfo) -> String {
    format!("{}-{}", info.bus, info.address)
}

/// Accept USB/IP connections forever.
pub async fn serve(listener: TcpListener, backend: Arc<dyn UsbBackend>) -> Result<()> {
    loop {
        let (sock, peer) = listener.accept().await?;
        let _ = sock.set_nodelay(true);
        let backend = backend.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(sock, backend).await {
                tracing::debug!("usbip connection from {peer} ended: {e}");
            }
        });
    }
}

/// Serve one USB/IP connection (a device list request, or an import followed by URB traffic).
pub async fn handle_connection<S>(mut stream: S, backend: Arc<dyn UsbBackend>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match OpMessage::read(&mut stream).await? {
        OpMessage::ReqDevlist => {
            let devices = match backend.list_devices().await {
                Ok(list) => list
                    .iter()
                    .filter_map(|d| UsbipDevice::from_info(d, &busid_of(d)).ok())
                    .collect(),
                Err(e) => {
                    tracing::warn!("usbip devlist: {e}");
                    Vec::new()
                }
            };
            stream
                .write_all(
                    &OpMessage::RepDevlist {
                        status: ST_OK,
                        devices,
                    }
                    .encode(),
                )
                .await?;
            stream.shutdown().await.ok();
            Ok(())
        }
        OpMessage::ReqImport { busid } => {
            let list = backend.list_devices().await.unwrap_or_default();
            let Some(info) = list.into_iter().find(|d| busid_of(d) == busid) else {
                stream
                    .write_all(
                        &OpMessage::RepImport {
                            status: ST_NODEV,
                            device: None,
                        }
                        .encode(),
                    )
                    .await?;
                return Ok(());
            };
            let dev = match UsbipDevice::from_info(&info, &busid) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("usbip import {busid}: {e}");
                    stream
                        .write_all(
                            &OpMessage::RepImport {
                                status: ST_DEV_ERR,
                                device: None,
                            }
                            .encode(),
                        )
                        .await?;
                    return Ok(());
                }
            };
            if let Err(e) = backend.attach(info.device_id).await {
                tracing::warn!("usbip import {busid}: attach failed: {e}");
                stream
                    .write_all(
                        &OpMessage::RepImport {
                            status: ST_DEV_ERR,
                            device: None,
                        }
                        .encode(),
                    )
                    .await?;
                return Ok(());
            }
            stream
                .write_all(
                    &OpMessage::RepImport {
                        status: ST_OK,
                        device: Some(dev),
                    }
                    .encode(),
                )
                .await?;
            let configs: Vec<ConfigDescriptor> = info
                .config_descriptors
                .iter()
                .filter_map(|c| ConfigDescriptor::parse(c).ok())
                .collect();
            let res = urb_loop(stream, backend.clone(), info.device_id, configs).await;
            let _ = backend.detach(info.device_id).await;
            res
        }
        other => Err(crate::UsbError::Protocol(format!(
            "unexpected USB/IP operation {:#06x}",
            other.code()
        ))),
    }
}

/// Per-seqnum state: `Some(unlink_seqnum)` once an unlink was requested.
type PendingUrbs = Arc<Mutex<HashMap<u32, Option<u32>>>>;

async fn urb_loop<S>(
    stream: S,
    backend: Arc<dyn UsbBackend>,
    device_id: u32,
    configs: Vec<ConfigDescriptor>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(buf) = rx.recv().await {
            if wr.write_all(&buf).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    let pending: PendingUrbs = Arc::new(Mutex::new(HashMap::new()));
    let ep_type = move |addr: u8| -> Option<UsbTransferType> {
        configs.iter().find_map(|c| c.endpoint_type(addr))
    };

    let res = loop {
        let pdu = match UrbPdu::read(&mut rd, |_| false).await {
            Ok(p) => p,
            Err(crate::UsbError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(e) => break Err(e),
        };
        match pdu {
            UrbPdu::CmdSubmit(cmd) => submit(&backend, device_id, &pending, &tx, cmd, &ep_type),
            UrbPdu::CmdUnlink(u) => {
                let target = u.unlink_seqnum;
                let found = match pending.lock().get_mut(&target) {
                    Some(slot) => {
                        *slot = Some(u.seqnum);
                        true
                    }
                    None => false,
                };
                if found {
                    let b = backend.clone();
                    tokio::spawn(async move { b.unlink(device_id, target as u64).await });
                } else {
                    // Already completed (and RET_SUBMIT sent): nothing to unlink.
                    let _ = tx.send(
                        UrbPdu::RetUnlink(RetUnlink {
                            seqnum: u.seqnum,
                            status: 0,
                        })
                        .encode(),
                    );
                }
            }
            other => {
                break Err(crate::UsbError::Protocol(format!(
                    "unexpected PDU from client: {other:?}"
                )));
            }
        }
    };
    // Cancel whatever is still in flight; the backend detach (by the caller) cleans up.
    let outstanding: Vec<u32> = pending.lock().keys().copied().collect();
    for s in outstanding {
        backend.unlink(device_id, s as u64).await;
    }
    drop(tx);
    let _ = writer.await;
    res
}

fn submit(
    backend: &Arc<dyn UsbBackend>,
    device_id: u32,
    pending: &PendingUrbs,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    cmd: CmdSubmit,
    ep_type: &impl Fn(u8) -> Option<UsbTransferType>,
) {
    let seqnum = cmd.seqnum;
    let dir_in = cmd.direction == USBIP_DIR_IN;
    let urb = cmd_submit_to_urb(&cmd, ep_type);
    pending.lock().insert(seqnum, None);
    let (backend, pending, tx) = (backend.clone(), pending.clone(), tx.clone());
    tokio::spawn(async move {
        let done = if urb.transfer_type == UsbTransferType::Isochronous {
            UrbComplete {
                urb_id: urb.urb_id,
                status: UrbStatus::Error,
                data: Vec::new(),
                actual_length: 0,
            }
        } else {
            backend.submit(device_id, urb).await
        };
        let unlink = pending.lock().remove(&seqnum).flatten();
        match unlink {
            Some(unlink_seq) if done.status == UrbStatus::Cancelled => {
                let _ = tx.send(
                    UrbPdu::RetUnlink(RetUnlink {
                        seqnum: unlink_seq,
                        status: -errno::ECONNRESET,
                    })
                    .encode(),
                );
            }
            Some(unlink_seq) => {
                let _ = tx.send(
                    UrbPdu::RetSubmit(urb_complete_to_ret_submit(seqnum, &done, dir_in)).encode(),
                );
                let _ = tx.send(
                    UrbPdu::RetUnlink(RetUnlink {
                        seqnum: unlink_seq,
                        status: 0,
                    })
                    .encode(),
                );
            }
            None => {
                let _ = tx.send(
                    UrbPdu::RetSubmit(urb_complete_to_ret_submit(seqnum, &done, dir_in)).encode(),
                );
            }
        }
    });
}
