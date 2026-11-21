//! Direct WinRT RFCOMM transport for the CMF Buds control channel.
//!
//! The buds expose their control protocol on the custom RFCOMM service UUID
//! `aeac4a03-dff5-498f-843a-34487cf133eb`. Windows does **not** create a COM
//! port for that service (only for the unrelated standard SPP/A2DP/HFP
//! services), so `serialport` can never reach it. Chrome's Web Serial API
//! reaches it by opening a WinRT `StreamSocket` against the discovered
//! `RfcommDeviceService`; this module does the same.
//!
//! All WinRT calls happen on threads initialised as MTA. A dedicated reader
//! thread converts the blocking `DataReader::LoadAsync` into an mpsc stream,
//! so the device manager keeps the same non-blocking `Read` semantics it has
//! with `serialport`.

use std::{
    io::{self, Read, Write},
    sync::mpsc,
    time::Duration,
};

use windows::{
    core::{AgileReference, GUID},
    Devices::{
        Bluetooth::Rfcomm::{RfcommDeviceService, RfcommServiceId},
        Enumeration::DeviceInformation,
    },
    Networking::Sockets::{SocketProtectionLevel, StreamSocket},
    Storage::Streams::{DataReader, DataWriter, IInputStream},
    Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED},
};

pub const CONTROL_SERVICE_UUID: &str = "aeac4a03-dff5-498f-843a-34487cf133eb";
const UUID: u128 = 0xaeac4a03_dff5_498f_843a_34487cf133eb;

/// Initialise COM for WinRT calls on the current thread. An existing
/// apartment (including RPC_E_CHANGED_MODE) is fine — we only need *some*
/// apartment for the WinRT activation factories to work.
pub fn init_com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Discover and connect to the control service.
pub fn open(address_hint: Option<&str>) -> Result<RfcommStream, String> {
    RfcommStream::open(address_hint)
}

/// A bidirectional RFCOMM stream with serial-port-like `Read`/`Write`.
pub struct RfcommStream {
    writer: DataWriter,
    socket: StreamSocket,
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
    timeout: Duration,
}

impl RfcommStream {
    /// Discover the control service and connect to the first matching device.
    /// `address_hint` (e.g. `2CBEEEE51009`) prefers the service belonging to a
    /// specific pair of buds when more than one headset is paired.
    pub fn open(address_hint: Option<&str>) -> Result<Self, String> {
        let service_id = RfcommServiceId::FromUuid(GUID::from_u128(UUID))
            .map_err(|error| format!("RfcommServiceId::FromUuid: {error}"))?;
        let selector = RfcommDeviceService::GetDeviceSelector(&service_id)
            .map_err(|error| format!("GetDeviceSelector: {error}"))?;
        let services = DeviceInformation::FindAllAsyncAqsFilter(&selector)
            .map_err(|error| format!("FindAllAsyncAqsFilter: {error}"))?
            .get()
            .map_err(|error| format!("device enumeration: {error}"))?;

        let mut fallback: Option<(String, RfcommDeviceService)> = None;
        let count = services
            .Size()
            .map_err(|error| format!("DeviceInformationCollection::Size: {error}"))?;
        for index in 0..count {
            let device = services
                .GetAt(index)
                .map_err(|error| format!("GetAt({index}): {error}"))?;
            let id = device
                .Id()
                .map_err(|error| format!("DeviceInformation::Id: {error}"))?
                .to_string_lossy();
            let service = match RfcommDeviceService::FromIdAsync(
                &windows::core::HSTRING::from(id.as_str()),
            )
            .map_err(|error| format!("FromIdAsync: {error}"))?
            .get()
            {
                Ok(service) => service,
                Err(error) => {
                    tracing::warn!("RFCOMM service {id} unavailable: {error}");
                    continue;
                }
            };

            if address_hint.is_some_and(|address| id.to_uppercase().contains(&address.to_uppercase()))
            {
                return Self::connect(service, id);
            }
            fallback = Some((id, service));
        }

        match fallback {
            Some((id, service)) => Self::connect(service, id),
            None => Err(format!(
                "no paired device exposes RFCOMM service {CONTROL_SERVICE_UUID}"
            )),
        }
    }

    fn connect(service: RfcommDeviceService, id: String) -> Result<Self, String> {
        let host = service
            .ConnectionHostName()
            .map_err(|error| format!("ConnectionHostName: {error}"))?;
        let service_name = service
            .ConnectionServiceName()
            .map_err(|error| format!("ConnectionServiceName: {error}"))?;
        let protection = service
            .MaxProtectionLevel()
            .unwrap_or(SocketProtectionLevel::BluetoothEncryptionAllowNullAuthentication);

        let socket = StreamSocket::new().map_err(|error| format!("StreamSocket::new: {error}"))?;
        socket
            .ConnectWithProtectionLevelAsync(&host, &service_name, protection)
            .map_err(|error| format!("ConnectWithProtectionLevelAsync: {error}"))?
            .get()
            .map_err(|error| format!("RFCOMM connect to {id}: {error}"))?;

        let input: IInputStream = socket
            .InputStream()
            .map_err(|error| format!("InputStream: {error}"))?;
        let writer = DataWriter::CreateDataWriter(
            &socket
                .OutputStream()
                .map_err(|error| format!("OutputStream: {error}"))?,
        )
        .map_err(|error| format!("CreateDataWriter: {error}"))?;

        // WinRT interfaces are not `Send`; an agile reference proxies the
        // input stream safely into the reader thread's MTA.
        let agile_input = AgileReference::new(&input)
            .map_err(|error| format!("AgileReference for input stream: {error}"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("cmf-rfcomm-reader".to_string())
            .spawn(move || {
                init_com();
                let input = match agile_input.resolve() {
                    Ok(input) => input,
                    Err(error) => {
                        tracing::warn!("RFCOMM agile reference resolve failed: {error}");
                        return;
                    }
                };
                let reader = match DataReader::CreateDataReader(&input) {
                    Ok(reader) => reader,
                    Err(error) => {
                        tracing::warn!("CreateDataReader failed: {error}");
                        return;
                    }
                };
                loop {
                    let loaded = match reader.LoadAsync(1).and_then(|operation| operation.get()) {
                        Ok(loaded) => loaded,
                        Err(error) => {
                            tracing::debug!("RFCOMM reader stopped: {error}");
                            break;
                        }
                    };
                    if loaded == 0 {
                        break;
                    }
                    let available = reader.UnconsumedBufferLength().unwrap_or(0) as usize;
                    let mut bytes = vec![0u8; available.max(1)];
                    if let Err(error) = reader.ReadBytes(&mut bytes) {
                        tracing::debug!("RFCOMM ReadBytes failed: {error}");
                        break;
                    }
                    if tx.send(bytes).is_err() {
                        break;
                    }
                }
                let _ = reader.DetachStream();
            })
            .map_err(|error| format!("spawn RFCOMM reader: {error}"))?;

        Ok(Self {
            writer,
            socket,
            rx,
            pending: Vec::new(),
            timeout: Duration::from_millis(80),
        })
    }

    pub fn label(&self) -> String {
        format!("RFCOMM {CONTROL_SERVICE_UUID}")
    }
}

impl Read for RfcommStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.pending.is_empty() {
            match self.rx.recv_timeout(self.timeout) {
                Ok(bytes) => self.pending = bytes,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "RFCOMM read timeout"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let count = self.pending.len().min(buffer.len());
        buffer[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
    }
}

impl Write for RfcommStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer
            .WriteBytes(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        self.writer
            .StoreAsync()
            .and_then(|operation| operation.get())
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RfcommStream {
    fn drop(&mut self) {
        let _ = self.socket.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual hardware check: `cargo test -- --ignored --nocapture live_probe`.
    #[test]
    #[ignore = "requires paired CMF Buds Pro 2"]
    fn live_probe() {
        init_com();
        let mut stream = open(None).expect("connect to control RFCOMM service");
        println!("RFCOMM stream opened");
        // Battery read fixture from NOTES.md.
        let frame = [0x55, 0x60, 0x01, 0x07, 0xC0, 0x00, 0x00, 0x01, 0xAC, 0xDF];
        stream.write_all(&frame).expect("write frame");
        println!("TX {}", crate::protocol::hex(&frame));
        let mut buffer = [0u8; 256];
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(4) {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    println!("stream closed");
                    break;
                }
                Ok(count) => println!("RX {}", crate::protocol::hex(&buffer[..count])),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => continue,
                Err(error) => {
                    println!("read error: {error}");
                    break;
                }
            }
        }
    }
}
