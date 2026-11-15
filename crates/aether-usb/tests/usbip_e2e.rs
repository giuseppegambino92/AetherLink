//! USB/IP client ⇄ usbip_server ⇄ RemoteUsb ⇄ (UsbMessage stream) ⇄ serve_usb_stream ⇄ backend.
//!
//! The backend here is a simulated device (a test double standing in for the libusb
//! `UsbHost`, which needs real hardware); everything between the USB/IP socket and the
//! backend is the production code path.

use std::collections::HashMap;
use std::sync::Arc;

use aether_protocol::{
    UrbComplete, UrbStatus, UrbSubmit, UsbDeviceInfo, UsbDirection, UsbSpeed, UsbTransferType,
};
use aether_usb::remote::{BoxRead, BoxWrite, RemoteUsb, UsbStreamConnector, serve_usb_stream};
use aether_usb::usbip::{
    CmdSubmit, CmdUnlink, OpMessage, ST_NODEV, ST_OK, USBIP_DIR_IN, USBIP_DIR_OUT, UrbPdu, errno,
};
use aether_usb::{Result, UsbBackend, UsbError};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

fn device_descriptor() -> Vec<u8> {
    vec![
        18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x34, 0x12, 0x78, 0x56, 0x00, 0x01, 1, 2, 3, 1,
    ]
}

fn config_descriptor() -> Vec<u8> {
    let mut c = vec![
        9, 2, 0, 0, 1, 1, 0, 0x80, 50, //
        9, 4, 0, 0, 3, 0xFF, 0, 0, 0, // vendor interface
        7, 5, 0x81, 2, 0x00, 0x02, 0, // bulk IN
        7, 5, 0x02, 2, 0x00, 0x02, 0, // bulk OUT
        7, 5, 0x83, 3, 8, 0, 10, // interrupt IN
    ];
    let n = c.len() as u16;
    c[2..4].copy_from_slice(&n.to_le_bytes());
    c
}

/// Simulated vendor device: GET_DESCRIPTOR, bulk loopback (OUT is echoed on IN), and an
/// interrupt IN endpoint that only completes when unlinked.
#[derive(Default)]
struct SimDevice {
    attached: Mutex<bool>,
    loopback: Mutex<Vec<u8>>,
    parked: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    log: Mutex<Vec<String>>,
}

#[async_trait]
impl UsbBackend for SimDevice {
    async fn list_devices(&self) -> Result<Vec<UsbDeviceInfo>> {
        Ok(vec![UsbDeviceInfo {
            device_id: 0x0105,
            bus: 1,
            address: 5,
            vendor_id: 0x1234,
            product_id: 0x5678,
            class: 0,
            subclass: 0,
            protocol: 0,
            speed: UsbSpeed::High,
            manufacturer: Some("AetherLink".into()),
            product: Some("Loopback".into()),
            serial: None,
            device_descriptor: device_descriptor(),
            config_descriptors: vec![config_descriptor()],
        }])
    }

    async fn attach(&self, device_id: u32) -> Result<()> {
        if device_id != 0x0105 {
            return Err(UsbError::NoDevice(device_id));
        }
        *self.attached.lock() = true;
        self.log.lock().push("attach".into());
        Ok(())
    }

    async fn detach(&self, _device_id: u32) -> Result<()> {
        *self.attached.lock() = false;
        self.log.lock().push("detach".into());
        for (_, tx) in self.parked.lock().drain() {
            let _ = tx.send(());
        }
        Ok(())
    }

    async fn submit(&self, _device_id: u32, urb: UrbSubmit) -> UrbComplete {
        let ok = |data: Vec<u8>, n: usize| UrbComplete {
            urb_id: urb.urb_id,
            status: UrbStatus::Ok,
            data,
            actual_length: n as u32,
        };
        match (urb.transfer_type, urb.direction, urb.endpoint) {
            (UsbTransferType::Control, UsbDirection::In, _) => {
                let s = urb.setup.unwrap();
                if s[0] == 0x80 && s[1] == 6 {
                    let d = match s[3] {
                        1 => device_descriptor(),
                        2 => config_descriptor(),
                        _ => {
                            return UrbComplete {
                                urb_id: urb.urb_id,
                                status: UrbStatus::Stall,
                                data: vec![],
                                actual_length: 0,
                            };
                        }
                    };
                    let n = d
                        .len()
                        .min(u16::from_le_bytes([s[6], s[7]]) as usize)
                        .min(urb.length as usize);
                    ok(d[..n].to_vec(), n)
                } else {
                    UrbComplete {
                        urb_id: urb.urb_id,
                        status: UrbStatus::Stall,
                        data: vec![],
                        actual_length: 0,
                    }
                }
            }
            (UsbTransferType::Control, UsbDirection::Out, _) => ok(vec![], 0),
            (UsbTransferType::Bulk, UsbDirection::Out, 0x02) => {
                let n = urb.data.len();
                self.loopback.lock().extend_from_slice(&urb.data);
                ok(vec![], n)
            }
            (UsbTransferType::Bulk, UsbDirection::In, 0x81) => {
                let mut lb = self.loopback.lock();
                let n = lb.len().min(urb.length as usize);
                let d: Vec<u8> = lb.drain(..n).collect();
                ok(d, n)
            }
            (UsbTransferType::Interrupt, UsbDirection::In, 0x83) => {
                let (tx, rx) = oneshot::channel();
                self.parked.lock().insert(urb.urb_id, tx);
                let _ = rx.await;
                UrbComplete {
                    urb_id: urb.urb_id,
                    status: UrbStatus::Cancelled,
                    data: vec![],
                    actual_length: 0,
                }
            }
            _ => UrbComplete {
                urb_id: urb.urb_id,
                status: UrbStatus::Error,
                data: vec![],
                actual_length: 0,
            },
        }
    }

    async fn unlink(&self, _device_id: u32, urb_id: u64) {
        if let Some(tx) = self.parked.lock().remove(&urb_id) {
            let _ = tx.send(());
        }
    }
}

/// In-memory stand-in for the QUIC connection: every `open` creates a duplex pipe and
/// runs the host-side stream server on the far end.
struct DuplexConnector {
    host: Arc<dyn UsbBackend>,
}

#[async_trait]
impl UsbStreamConnector for DuplexConnector {
    async fn open(&self, _device_id: u32) -> std::io::Result<(BoxRead, BoxWrite)> {
        let (client, host_end) = tokio::io::duplex(1 << 16);
        let backend = self.host.clone();
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(host_end);
            let _ = serve_usb_stream(backend, r, w).await;
        });
        let (r, w) = tokio::io::split(client);
        Ok((Box::new(r), Box::new(w)))
    }
}

async fn setup() -> (std::net::SocketAddr, Arc<SimDevice>) {
    let sim = Arc::new(SimDevice::default());
    let remote = Arc::new(RemoteUsb::new(Arc::new(DuplexConnector {
        host: sim.clone(),
    })));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(aether_usb::usbip_server::serve(listener, remote));
    (addr, sim)
}

fn submit(seq: u32, dir: u32, ep: u32, len: u32, setup: [u8; 8], data: Vec<u8>) -> Vec<u8> {
    UrbPdu::CmdSubmit(CmdSubmit {
        seqnum: seq,
        devid: 0x0001_0005,
        direction: dir,
        ep,
        transfer_buffer_length: len,
        setup,
        data,
        ..Default::default()
    })
    .encode()
}

async fn send(
    s: &mut TcpStream,
    dir_in: &Arc<Mutex<HashMap<u32, bool>>>,
    seq: u32,
    din: bool,
    bytes: Vec<u8>,
) {
    dir_in.lock().insert(seq, din);
    s.write_all(&bytes).await.unwrap();
}

async fn read(s: &mut TcpStream, dir_in: &Arc<Mutex<HashMap<u32, bool>>>) -> UrbPdu {
    let d = dir_in.clone();
    UrbPdu::read(s, move |seq| d.lock().get(&seq).copied().unwrap_or(false))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usbip_devlist_import_and_urbs() {
    let (addr, sim) = setup().await;

    // OP_REQ_DEVLIST
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&OpMessage::ReqDevlist.encode()).await.unwrap();
    let OpMessage::RepDevlist { status, devices } = OpMessage::read(&mut s).await.unwrap() else {
        panic!()
    };
    assert_eq!(status, ST_OK);
    assert_eq!(devices.len(), 1);
    let d = &devices[0];
    assert_eq!(
        (d.busid.as_str(), d.busnum, d.devnum, d.speed),
        ("1-5", 1, 5, 3)
    );
    assert_eq!(
        (d.id_vendor, d.id_product, d.bcd_device),
        (0x1234, 0x5678, 0x0100)
    );
    assert_eq!(d.interfaces.len(), 1);
    assert_eq!(d.interfaces[0].class, 0xFF);

    // Import of an unknown bus id.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &OpMessage::ReqImport {
            busid: "9-9".into(),
        }
        .encode(),
    )
    .await
    .unwrap();
    assert_eq!(
        OpMessage::read(&mut s).await.unwrap(),
        OpMessage::RepImport {
            status: ST_NODEV,
            device: None
        }
    );

    // OP_REQ_IMPORT, then URB traffic on the same connection.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &OpMessage::ReqImport {
            busid: "1-5".into(),
        }
        .encode(),
    )
    .await
    .unwrap();
    let OpMessage::RepImport { status, device } = OpMessage::read(&mut s).await.unwrap() else {
        panic!()
    };
    assert_eq!(status, ST_OK);
    assert_eq!(device.unwrap().busid, "1-5");
    assert!(*sim.attached.lock());

    let dir_in: Arc<Mutex<HashMap<u32, bool>>> = Arc::default();

    // 1. Parked interrupt IN (seq 1), then GET_DESCRIPTOR (seq 2) must still complete.
    send(
        &mut s,
        &dir_in,
        1,
        true,
        submit(1, USBIP_DIR_IN, 3, 8, [0; 8], vec![]),
    )
    .await;
    send(
        &mut s,
        &dir_in,
        2,
        true,
        submit(2, USBIP_DIR_IN, 0, 64, [0x80, 6, 0, 1, 0, 0, 64, 0], vec![]),
    )
    .await;
    let UrbPdu::RetSubmit(r) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!((r.seqnum, r.status, r.actual_length), (2, 0, 18));
    assert_eq!(r.data, device_descriptor());

    // 2. Bulk OUT then bulk IN loopback.
    send(
        &mut s,
        &dir_in,
        3,
        false,
        submit(3, USBIP_DIR_OUT, 2, 5, [0; 8], b"hello".to_vec()),
    )
    .await;
    let UrbPdu::RetSubmit(r) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!(
        (r.seqnum, r.status, r.actual_length, r.data.len()),
        (3, 0, 5, 0)
    );
    send(
        &mut s,
        &dir_in,
        4,
        true,
        submit(4, USBIP_DIR_IN, 1, 512, [0; 8], vec![]),
    )
    .await;
    let UrbPdu::RetSubmit(r) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!((r.seqnum, r.status), (4, 0));
    assert_eq!(r.data, b"hello");

    // 3. Unsupported request → STALL (-EPIPE).
    send(
        &mut s,
        &dir_in,
        5,
        true,
        submit(5, USBIP_DIR_IN, 0, 8, [0x80, 6, 0, 7, 0, 0, 8, 0], vec![]),
    )
    .await;
    let UrbPdu::RetSubmit(r) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!((r.seqnum, r.status), (5, -errno::EPIPE));

    // 4. Unlink the parked interrupt URB → RET_UNLINK(-ECONNRESET), no RET_SUBMIT for it.
    s.write_all(
        &UrbPdu::CmdUnlink(CmdUnlink {
            seqnum: 6,
            unlink_seqnum: 1,
            ..Default::default()
        })
        .encode(),
    )
    .await
    .unwrap();
    let UrbPdu::RetUnlink(u) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!((u.seqnum, u.status), (6, -errno::ECONNRESET));

    // 5. Unlinking something already completed → RET_UNLINK status 0.
    s.write_all(
        &UrbPdu::CmdUnlink(CmdUnlink {
            seqnum: 7,
            unlink_seqnum: 4,
            ..Default::default()
        })
        .encode(),
    )
    .await
    .unwrap();
    let UrbPdu::RetUnlink(u) = read(&mut s, &dir_in).await else {
        panic!()
    };
    assert_eq!((u.seqnum, u.status), (7, 0));

    // Closing the USB/IP connection detaches the device on the host.
    drop(s);
    for _ in 0..100 {
        if !*sim.attached.lock() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!*sim.attached.lock(), "device detached after disconnect");
    assert_eq!(sim.log.lock().as_slice(), ["attach", "detach"]);
}

#[test]
fn client_support_is_honest() {
    let r = aether_usb::client_support();
    if cfg!(target_os = "linux") {
        assert!(r.is_ok());
    } else {
        assert!(matches!(r, Err(UsbError::Unsupported(_))));
    }
}
