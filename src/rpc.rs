//! SA01A2/SA02A1 BLE RPC. Wire layout and tags follow ble_rpc_transport.md
//! and ble_rpc.proto in blufi-web; no generated code or protoc is required.
use crate::model::*;
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use prost::Message;
use std::time::Duration;

const ATT_PAYLOAD: usize = 20;
const LAST: u8 = 1;
const RESPONSE: u8 = 2;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const WIFI_TIMEOUT: Duration = Duration::from_secs(45);

fn request_frame(command: u16, body: &[u8]) -> Result<Vec<u8>> {
    ensure!(body.len() <= u16::MAX as usize, "RPC 请求过长");
    let mut frame = Vec::with_capacity(6 + body.len());
    frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
    frame.extend_from_slice(&command.to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(body);
    ensure!(
        frame.len().div_ceil(ATT_PAYLOAD - 2) <= 256,
        "RPC 分片超过 256"
    );
    Ok(frame)
}

#[derive(Default)]
struct Reassembly {
    data: Vec<u8>,
    chunks: usize,
}
impl Reassembly {
    fn push(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>> {
        let result = self.push_inner(chunk);
        if result.is_err() {
            self.data.clear();
            self.chunks = 0;
        }
        result
    }
    fn push_inner(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>> {
        ensure!(chunk.len() >= 2, "RPC 分片缺少头部");
        ensure!(self.chunks < 256, "RPC 分片超过 256");
        ensure!(chunk[0] == self.chunks as u8, "RPC 分片序号错误");
        ensure!(chunk[1] & !(LAST | RESPONSE) == 0, "RPC 未知标志位");
        ensure!(chunk[1] & RESPONSE != 0, "RPC 收到非响应分片");
        ensure!(
            self.data.len() + chunk.len() - 2 <= u16::MAX as usize + 4,
            "RPC 响应过长"
        );
        self.data.extend_from_slice(&chunk[2..]);
        self.chunks += 1;
        let last = chunk[1] & LAST != 0;
        if self.data.len() < 4 {
            ensure!(!last, "RPC 响应头不完整");
            ensure!(self.chunks < 256, "RPC 分片超过 256");
            return Ok(None);
        }
        let expected = 4 + u16::from_be_bytes([self.data[0], self.data[1]]) as usize;
        ensure!(self.data.len() <= expected, "RPC 响应长度超出声明值");
        ensure!(
            last == (self.data.len() == expected),
            "RPC LAST 标志与响应长度不符"
        );
        if last {
            self.chunks = 0;
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        ensure!(self.chunks < 256, "RPC 分片超过 256");
        Ok(None)
    }
}

struct Rpc {
    io: Box<dyn PacketIo>,
    kind: DeviceKind,
    poisoned: bool,
}

pub async fn open(io: Box<dyn PacketIo>, kind: DeviceKind) -> Result<Box<dyn DeviceOps>> {
    ensure!(
        matches!(kind, DeviceKind::Sa01a2 | DeviceKind::Sa02a1),
        "非 RPC 设备型号"
    );
    Ok(Box::new(Rpc {
        io,
        kind,
        poisoned: false,
    }))
}

impl Rpc {
    async fn request(&mut self, command: u16, body: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        ensure!(!self.poisoned, "RPC 会话已失效，请重新连接");
        let frame = request_frame(command, body)?;
        // &mut self serializes all requests. Responses have no command/request ID.
        // Mark before awaiting: cancellation, timeout, I/O and framing failures all
        // permanently invalidate this session, so a late response cannot be reused.
        self.poisoned = true;
        let mut response = tokio::time::timeout(timeout, async {
            let count = frame.len().div_ceil(ATT_PAYLOAD - 2);
            for (index, payload) in frame.chunks(ATT_PAYLOAD - 2).enumerate() {
                let mut chunk = [0u8; ATT_PAYLOAD];
                chunk[0] = index as u8;
                chunk[1] = if index + 1 == count { LAST } else { 0 };
                chunk[2..2 + payload.len()].copy_from_slice(payload);
                self.io.write(&chunk[..2 + payload.len()]).await?;
            }
            let mut decoder = Reassembly::default();
            loop {
                if let Some(frame) = decoder.push(&self.io.read().await?)? {
                    return Ok::<_, anyhow::Error>(frame);
                }
            }
        })
        .await
        .context("RPC 请求超时，会话已失效")??;
        // A device error is a complete, correctly framed response, not transport loss.
        self.poisoned = false;
        let error = u16::from_be_bytes([response[2], response[3]]);
        ensure!(
            error == 0,
            "RPC 命令 0x{command:04X} 失败，设备错误码 {error} (0x{error:04X})"
        );
        response.drain(..4);
        Ok(response)
    }
    async fn update<M: Message>(
        &mut self,
        command: u16,
        message: M,
        timeout: Duration,
    ) -> Result<()> {
        let response = self
            .request(command, &message.encode_to_vec(), timeout)
            .await?;
        if !response.is_empty() {
            self.poisoned = true;
            bail!("RPC 空响应包含意外数据");
        }
        Ok(())
    }
    fn decode<M: Message + Default>(&mut self, body: &[u8]) -> Result<M> {
        match M::decode(body) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.poisoned = true;
                Err(error.into())
            }
        }
    }
}

#[async_trait]
impl DeviceOps for Rpc {
    async fn info(&mut self) -> Result<DeviceInfo> {
        let body = self.request(0x1001, &[], COMMAND_TIMEOUT).await?;
        let info: Info = self.decode(&body)?;
        let (modem_version, iccid) = modem_details(self.kind, info.lte)?;
        Ok(DeviceInfo {
            sn: info.sn,
            firmware: info.fw_version,
            storage_total: info.storage_total,
            battery: info.battery_percent,
            modem_version,
            iccid,
        })
    }
    async fn set_time(&mut self, utc_ms: u64) -> Result<()> {
        self.update(0x1004, SetTime { utc_ms }, COMMAND_TIMEOUT)
            .await
    }
    async fn reset(&mut self, mode: ResetMode) -> Result<()> {
        self.update(0x1401, Reset { mode: mode as i32 }, COMMAND_TIMEOUT)
            .await
    }
    async fn scan_wifi(&mut self) -> Result<Vec<WifiNetwork>> {
        let body = self.request(0x1201, &[], COMMAND_TIMEOUT).await?;
        let scan: Scan = self.decode(&body)?;
        scan.items
            .into_iter()
            .map(|item| {
                Ok(WifiNetwork {
                    ssid: item.ssid,
                    rssi: i16::try_from(item.rssi).context("RPC WiFi RSSI 超出范围")?,
                })
            })
            .collect()
    }
    async fn connect_wifi(&mut self, ssid: &str, password: &str) -> Result<()> {
        ensure!(
            !ssid.is_empty() && ssid.len() <= 32,
            "SSID 必须为 1–32 字节"
        );
        ensure!(password.len() <= 64, "WiFi 密码不能超过 64 字节");
        self.update(
            0x1202,
            Connect {
                ssid: ssid.into(),
                password: password.into(),
            },
            WIFI_TIMEOUT,
        )
        .await
    }
    async fn set_sn(&mut self, sn: &str) -> Result<()> {
        self.update(0x1003, SetSn { sn: sn.into() }, COMMAND_TIMEOUT)
            .await
    }
    // Activation and upgrade deliberately use DeviceOps' explicit unsupported errors.
}

// Only fields needed by the batch initializer; prost skips other/unknown tags.
#[derive(Clone, PartialEq, Message)]
struct Info {
    #[prost(string, tag = "2")]
    sn: String,
    #[prost(string, tag = "3")]
    fw_version: String,
    #[prost(uint32, tag = "4")]
    battery_percent: u32,
    #[prost(uint64, tag = "14")]
    storage_total: u64,
    #[prost(message, optional, tag = "21")]
    lte: Option<Lte>,
}
#[derive(Clone, PartialEq, Message)]
struct Lte {
    #[prost(string, tag = "2")]
    cgmr: String,
    #[prost(string, tag = "5")]
    ccid: String,
}
fn modem_details(kind: DeviceKind, lte: Option<Lte>) -> Result<(String, String)> {
    if kind != DeviceKind::Sa02a1 {
        return Ok((String::new(), String::new()));
    }
    let lte = lte.context("SA02A1 未返回 Modem 信息")?;
    ensure!(
        !lte.cgmr.is_empty() && !lte.ccid.is_empty(),
        "SA02A1 Modem 信息缺少版本或 ICCID"
    );
    Ok((lte.cgmr, lte.ccid))
}
#[derive(Clone, PartialEq, Message)]
struct SetTime {
    #[prost(uint64, tag = "1")]
    utc_ms: u64,
}
#[derive(Clone, PartialEq, Message)]
struct SetSn {
    #[prost(string, tag = "1")]
    sn: String,
}
#[derive(Clone, PartialEq, Message)]
struct Reset {
    #[prost(int32, tag = "1")]
    mode: i32,
}
#[derive(Clone, PartialEq, Message)]
struct Connect {
    #[prost(string, tag = "1")]
    ssid: String,
    #[prost(string, tag = "2")]
    password: String,
}
#[derive(Clone, PartialEq, Message)]
struct Scan {
    #[prost(message, repeated, tag = "1")]
    items: Vec<ScanItem>,
}
#[derive(Clone, PartialEq, Message)]
struct ScanItem {
    #[prost(string, tag = "1")]
    ssid: String,
    #[prost(int32, tag = "2")]
    rssi: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct Io {
        incoming: VecDeque<Vec<u8>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }
    #[async_trait]
    impl PacketIo for Io {
        async fn write(&mut self, data: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        async fn read(&mut self) -> Result<Vec<u8>> {
            match self.incoming.pop_front() {
                Some(value) => Ok(value),
                None => std::future::pending().await,
            }
        }
    }
    fn rpc(incoming: Vec<Vec<u8>>) -> (Rpc, Arc<Mutex<Vec<Vec<u8>>>>) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        (
            Rpc {
                io: Box::new(Io {
                    incoming: incoming.into(),
                    writes: writes.clone(),
                }),
                kind: DeviceKind::Sa02a1,
                poisoned: false,
            },
            writes,
        )
    }
    #[test]
    fn sa02a1_requires_complete_modem_information() {
        assert_eq!(
            modem_details(DeviceKind::Sa01a2, None).unwrap(),
            (String::new(), String::new())
        );
        assert!(modem_details(DeviceKind::Sa02a1, None).is_err());
        assert!(
            modem_details(
                DeviceKind::Sa02a1,
                Some(Lte {
                    cgmr: "MODEM".into(),
                    ccid: String::new(),
                })
            )
            .is_err()
        );
        assert_eq!(
            modem_details(
                DeviceKind::Sa02a1,
                Some(Lte {
                    cgmr: "MODEM".into(),
                    ccid: "ICCID".into(),
                })
            )
            .unwrap(),
            ("MODEM".into(), "ICCID".into())
        );
    }

    #[tokio::test]
    async fn fragments_requests_at_twenty_bytes() {
        let (mut rpc, writes) = rpc(vec![vec![0, RESPONSE | LAST, 0, 0, 0, 0]]);
        rpc.request(0x1003, &[7; 40], COMMAND_TIMEOUT)
            .await
            .unwrap();
        let writes = writes.lock().unwrap();
        assert_eq!(
            writes.iter().map(Vec::len).collect::<Vec<_>>(),
            [20, 20, 12]
        );
        assert_eq!(
            writes.iter().map(|v| (v[0], v[1])).collect::<Vec<_>>(),
            [(0, 0), (1, 0), (2, LAST)]
        );
        let joined: Vec<_> = writes.iter().flat_map(|v| v[2..].iter().copied()).collect();
        assert_eq!(joined, request_frame(0x1003, &[7; 40]).unwrap());
        assert!(request_frame(1, &vec![0; 4603]).is_err());
        assert!(request_frame(1, &vec![0; 65536]).is_err());
    }
    #[test]
    fn reassembles_and_resets_after_errors() {
        let mut decoder = Reassembly::default();
        assert!(decoder.push(&[0, RESPONSE, 0]).unwrap().is_none());
        assert_eq!(
            decoder
                .push(&[1, RESPONSE | LAST, 2, 0, 0, 8, 9])
                .unwrap()
                .unwrap(),
            [0, 2, 0, 0, 8, 9]
        );
        for bad in [
            vec![0],
            vec![1, 3, 0, 0, 0, 0],
            vec![0, 1, 0, 0, 0, 0],
            vec![0, 7, 0, 0, 0, 0],
            vec![0, 3, 0, 1, 0, 0],
            vec![0, 3, 0, 0, 0, 0, 9],
            vec![0, 2, 0, 0, 0, 0],
        ] {
            assert!(decoder.push(&bad).is_err());
            assert!(decoder.push(&[0, 3, 0, 0, 0, 0]).unwrap().is_some());
        }
        assert!(decoder.push(&[0, RESPONSE, 0, 1, 0, 0]).unwrap().is_none());
        assert!(decoder.push(&[1, LAST, 5]).is_err());
    }
    #[test]
    fn bounds_fragment_count() {
        let mut decoder = Reassembly::default();
        for seq in 0..255 {
            assert!(decoder.push(&[seq, RESPONSE]).unwrap().is_none());
        }
        assert!(decoder.push(&[255, RESPONSE]).is_err());
    }
    #[tokio::test]
    async fn device_errors_are_not_success_and_preserve_session() {
        let (mut rpc, _) = rpc(vec![vec![0, 3, 0, 0, 0, 201], vec![0, 3, 0, 0, 0, 0]]);
        assert!(
            rpc.connect_wifi("ap", "pw")
                .await
                .unwrap_err()
                .to_string()
                .contains("201")
        );
        assert!(!rpc.poisoned);
        rpc.set_time(1_700_000_000_123).await.unwrap();
    }
    #[tokio::test]
    async fn timeout_and_corruption_prevent_further_writes() {
        for incoming in [vec![], vec![vec![1, 3, 0, 0, 0, 0]]] {
            let (mut rpc, writes) = rpc(incoming);
            assert!(rpc.request(1, &[], Duration::from_millis(1)).await.is_err());
            let count = writes.lock().unwrap().len();
            assert!(rpc.set_sn("later").await.is_err());
            assert_eq!(writes.lock().unwrap().len(), count);
            assert!(rpc.poisoned);
        }
    }
}
