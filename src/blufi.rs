//! Native BLUFI security v1 and the SM/SMCE application commands.
//!
//! Wire references: Espressif ESP-IDF `api-guides/ble/blufi`, v4.4.8
//! `examples/bluetooth/blufi/main/blufi_security.c`, and EspBlufiForAndroid.
//! These devices use unauthenticated DH1024/MD5/AES-CFB128, not security v2.
//! Data replies without encryption are intentionally accepted for old firmware.

use crate::model::*;
use aes::Aes128;
use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use cfb_mode::cipher::{AsyncStreamCipher, KeyIvInit};
use md5::{Digest, Md5};
use num_bigint::{BigUint, RandBigInt};
use prost::Message;
use std::time::Duration;
use tokio::time::timeout;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(30);
const NEGOTIATION: u8 = 0x01;
const SECURITY_MODE: u8 = 0x04; // Control subtype 1, NOT data subtype 1 (0x05).
const WIFI_MODE: u8 = 0x08;
const CONNECT: u8 = 0x0c;
const SCAN: u8 = 0x24;
const SSID: u8 = 0x09;
const PASSWORD: u8 = 0x0d;
const WIFI_STATUS: u8 = 0x3d;
const WIFI_LIST: u8 = 0x45;
const ERROR: u8 = 0x49;
const CUSTOM: u8 = 0x4d;
const ENCRYPTED: u8 = 1;
const CHECKSUM: u8 = 2;
const FROM_DEVICE: u8 = 4;
const REQUIRE_ACK: u8 = 8;
const FRAGMENT: u8 = 16;
const PACKET_SIZE: usize = 20;

// The same DH1024 group used by Espressif's original Android client.
const DH_PRIME: &str = concat!(
    "cf5cf5c38419a724957ff5dd323b9c45c3cdd261eb740f69aa94b8bb1a5c9640",
    "9153bd76b24222d03274e4725a5406092e9e82e9135c643cae98132b0d95f7d6",
    "5347c68afc1e677da90e51bbab5f5cf429c291b4ba39c6b2dc5e8c7231e46aa7",
    "728e87664532cdf547be20c9a3fa8342be6e34371a27c06f7dc0edddd2f86373"
);

pub async fn open(io: Box<dyn PacketIo>, kind: DeviceKind) -> Result<Box<dyn DeviceOps>> {
    ensure!(
        matches!(
            kind,
            DeviceKind::BlufiOld | DeviceKind::BlufiNew | DeviceKind::Blufi4g
        ),
        "不是 BLUFI 设备"
    );
    let mut session = Session {
        io,
        kind,
        codec: Codec::default(),
        poisoned: false,
    };
    let result = timeout(COMMAND_TIMEOUT, session.negotiate()).await;
    session.finish(result)?;
    Ok(Box::new(session))
}

struct DhExchange {
    prime: BigUint,
    private: BigUint,
    public: Vec<u8>,
}

impl DhExchange {
    fn new() -> Self {
        let prime = BigUint::parse_bytes(DH_PRIME.as_bytes(), 16).expect("constant DH prime");
        let private = rand::rngs::OsRng.gen_biguint_range(&BigUint::from(2u8), &(&prime - 1u8));
        Self::with_private(prime, private)
    }

    fn with_private(prime: BigUint, private: BigUint) -> Self {
        let public = BigUint::from(2u8).modpow(&private, &prime).to_bytes_be();
        Self {
            prime,
            private,
            public,
        }
    }

    fn parameters(&self) -> Vec<u8> {
        let prime = self.prime.to_bytes_be();
        let mut parameters = Vec::with_capacity(263);
        // mbedtls_dhm_read_params: three big-endian, u16-length-prefixed integers.
        for number in [prime.as_slice(), &[2], self.public.as_slice()] {
            parameters.extend_from_slice(&(number.len() as u16).to_be_bytes());
            parameters.extend_from_slice(number);
        }
        parameters
    }

    fn key(&self, public: &[u8]) -> Result<[u8; 16]> {
        ensure!(
            !public.is_empty() && public.len() <= 128,
            "BLUFI DH 公钥长度错误"
        );
        let peer = BigUint::from_bytes_be(public);
        ensure!(
            peer >= BigUint::from(2u8) && peer <= &self.prime - 2u8,
            "BLUFI DH 公钥超出范围"
        );
        // mbedTLS hashes the minimal unsigned big-endian secret, WITHOUT padding.
        let secret = peer.modpow(&self.private, &self.prime).to_bytes_be();
        Ok(Md5::digest(&secret).into())
    }
}

fn crc16<'a>(bytes: impl IntoIterator<Item = &'a u8>) -> u16 {
    // esp_crc16_be(0, ...): complemented CCITT, initial register 0xffff.
    let mut crc = 0xffffu16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    !crc
}

fn crypt(key: &[u8; 16], seq: u8, data: &mut [u8], decrypt: bool) {
    let mut iv = [0; 16];
    iv[0] = seq;
    if decrypt {
        cfb_mode::Decryptor::<Aes128>::new(key.into(), (&iv).into()).decrypt(data);
    } else {
        cfb_mode::Encryptor::<Aes128>::new(key.into(), (&iv).into()).encrypt(data);
    }
}

#[derive(Default)]
struct Codec {
    tx: u8,
    rx: u8,
    key: Option<[u8; 16]>,
    pending: Option<Fragments>,
}

struct Fragments {
    kind: u8,
    remaining: usize,
    data: Vec<u8>,
}

struct Received {
    message: Option<(u8, Vec<u8>)>,
    ack: Option<u8>,
}

struct Packet {
    bytes: [u8; PACKET_SIZE],
    length: usize,
}

impl std::ops::Deref for Packet {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl Codec {
    fn packet(&mut self, kind: u8, flags: u8, payload: &[u8]) -> Result<Packet> {
        let length = payload.len() + 4 + if flags & CHECKSUM != 0 { 2 } else { 0 };
        ensure!(length <= PACKET_SIZE, "BLUFI 数据包超过安全 ATT 长度");
        let mut packet = Packet {
            bytes: [0; PACKET_SIZE],
            length,
        };
        let end = 4 + payload.len();
        packet.bytes[..4].copy_from_slice(&[kind, flags, self.tx, payload.len() as u8]);
        packet.bytes[4..end].copy_from_slice(payload);
        if flags & CHECKSUM != 0 {
            let checksum = crc16(&packet.bytes[2..end]);
            packet.bytes[end..end + 2].copy_from_slice(&checksum.to_le_bytes());
        }
        if flags & ENCRYPTED != 0 {
            crypt(
                self.key.as_ref().context("BLUFI 密钥尚未协商")?,
                self.tx,
                &mut packet.bytes[4..end],
                false,
            );
        }
        self.tx = self.tx.wrapping_add(1);
        Ok(packet)
    }

    fn fragment(&mut self, kind: u8, remaining: &[u8], secure: bool) -> Result<(Packet, usize)> {
        ensure!(remaining.len() <= u16::MAX as usize, "BLUFI 消息过长");
        let flags = if secure { ENCRYPTED | CHECKSUM } else { 0 };
        let capacity = PACKET_SIZE - 4 - if secure { 2 } else { 0 };
        if remaining.len() <= capacity {
            return Ok((self.packet(kind, flags, remaining)?, remaining.len()));
        }
        let consumed = capacity - 2;
        let mut payload = [0; PACKET_SIZE - 4];
        payload[..2].copy_from_slice(&(remaining.len() as u16).to_le_bytes());
        payload[2..capacity].copy_from_slice(&remaining[..consumed]);
        Ok((
            self.packet(kind, flags | FRAGMENT, &payload[..capacity])?,
            consumed,
        ))
    }

    #[cfg(test)]
    fn encode(&mut self, kind: u8, mut data: &[u8], secure: bool) -> Result<Vec<Vec<u8>>> {
        let mut packets = Vec::new();
        loop {
            let (packet, consumed) = self.fragment(kind, data, secure)?;
            packets.push(packet.to_vec());
            data = &data[consumed..];
            if data.is_empty() {
                return Ok(packets);
            }
        }
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Received> {
        ensure!(packet.len() >= 4, "BLUFI 帧头不完整");
        let (kind, flags, seq, length) = (packet[0], packet[1], packet[2], usize::from(packet[3]));
        ensure!(
            flags & !0x1f == 0 && flags & FROM_DEVICE != 0,
            "BLUFI 帧方向或控制位错误"
        );
        ensure!(kind & 3 <= 1, "BLUFI 帧类型错误");
        ensure!(
            seq == self.rx,
            "BLUFI 接收序号错误: 期望 {}, 收到 {}",
            self.rx,
            seq
        );
        ensure!(
            packet.len() == 4 + length + if flags & CHECKSUM != 0 { 2 } else { 0 },
            "BLUFI 数据帧长度错误"
        );
        ensure!(
            kind & 3 != 0 || flags & ENCRYPTED == 0,
            "BLUFI 控制帧不应加密"
        );
        let mut payload = packet[4..4 + length].to_vec();
        if flags & ENCRYPTED != 0 {
            crypt(
                self.key.as_ref().context("BLUFI 收到未协商的加密数据")?,
                seq,
                &mut payload,
                true,
            );
        }
        if flags & CHECKSUM != 0 {
            let header = [seq, length as u8];
            let expected = u16::from_le_bytes([packet[4 + length], packet[5 + length]]);
            ensure!(
                crc16(header.iter().chain(payload.iter())) == expected,
                "BLUFI CRC 校验失败"
            );
        }
        self.rx = self.rx.wrapping_add(1);
        let ack = (flags & REQUIRE_ACK != 0).then_some(seq);
        // Link-layer ACKs may be interleaved with fragmented application data.
        if kind == 0 {
            ensure!(
                payload.len() == 1 && flags & FRAGMENT == 0,
                "BLUFI ACK 长度错误"
            );
            return Ok(Received { message: None, ack });
        }
        if flags & FRAGMENT != 0 {
            ensure!(payload.len() >= 3, "BLUFI 分片为空或缺少总长度");
            let remaining = usize::from(u16::from_le_bytes([payload[0], payload[1]]));
            let content = &payload[2..];
            ensure!(remaining > content.len(), "BLUFI 分片剩余长度错误");
            if let Some(pending) = self.pending.as_mut() {
                ensure!(
                    pending.kind == kind && pending.remaining == remaining,
                    "BLUFI 分片类型或剩余长度不连续"
                );
                pending.data.extend_from_slice(content);
                pending.remaining -= content.len();
            } else {
                let mut data = Vec::with_capacity(remaining);
                data.extend_from_slice(content);
                self.pending = Some(Fragments {
                    kind,
                    remaining: remaining - content.len(),
                    data,
                });
            }
            return Ok(Received { message: None, ack });
        }
        if let Some(mut pending) = self.pending.take() {
            ensure!(
                pending.kind == kind && pending.remaining == payload.len(),
                "BLUFI 最后分片长度或类型错误"
            );
            pending.data.extend_from_slice(&payload);
            payload = pending.data;
        }
        Ok(Received {
            message: Some((kind, payload)),
            ack,
        })
    }
}

struct Session {
    io: Box<dyn PacketIo>,
    kind: DeviceKind,
    codec: Codec,
    poisoned: bool,
}

impl Session {
    fn ready(&self) -> Result<()> {
        ensure!(!self.poisoned, "BLUFI 会话已失效，必须重新连接");
        Ok(())
    }

    fn finish<T>(
        &mut self,
        result: std::result::Result<Result<T>, tokio::time::error::Elapsed>,
    ) -> Result<T> {
        match result {
            Ok(result) => result,
            Err(_) => {
                // Commands have no request ID; a late response cannot safely be reused.
                self.poisoned = true;
                bail!("BLUFI 操作超时，会话已关闭，必须重新连接")
            }
        }
    }

    async fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        self.ready()?;
        if let Err(error) = self.io.write(packet).await {
            self.poisoned = true;
            return Err(error.context("BLUFI 写入失败，会话已失效"));
        }
        Ok(())
    }

    async fn send(&mut self, kind: u8, mut payload: &[u8], secure: bool) -> Result<()> {
        self.ready()?;
        loop {
            let (packet, consumed) = self.codec.fragment(kind, payload, secure)?;
            self.write_packet(&packet).await?;
            payload = &payload[consumed..];
            if payload.is_empty() {
                return Ok(());
            }
        }
    }

    async fn receive(&mut self) -> Result<(u8, Vec<u8>)> {
        self.ready()?;
        loop {
            let packet = match self.io.read().await {
                Ok(packet) => packet,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error.context("BLUFI 读取失败，会话已失效"));
                }
            };
            let received = match self.codec.decode(&packet) {
                Ok(received) => received,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            if let Some(seq) = received.ack {
                let packet = self.codec.packet(0, 0, &[seq])?;
                self.write_packet(&packet).await?;
            }
            if let Some((kind, data)) = received.message {
                if kind == ERROR {
                    self.poisoned = true;
                    bail!(
                        "BLUFI 设备协议错误: {}",
                        data.first()
                            .map_or_else(|| "空错误帧".into(), |v| format!("0x{v:02x}"))
                    );
                }
                return Ok((kind, data));
            }
        }
    }

    async fn negotiate(&mut self) -> Result<()> {
        let exchange = DhExchange::new();
        let parameters = exchange.parameters();
        let length = (parameters.len() as u16).to_be_bytes();
        self.send(NEGOTIATION, &[0, length[0], length[1]], false)
            .await?;
        let mut body = Vec::with_capacity(parameters.len() + 1);
        body.push(1);
        body.extend_from_slice(&parameters);
        self.send(NEGOTIATION, &body, false).await?;
        let public = loop {
            let (kind, data) = self.receive().await?;
            if kind == NEGOTIATION {
                break data;
            }
        };
        self.codec.key = Some(exchange.key(&public)?);
        // Standard security-control frame. Data encrypted+CRC, controls plaintext.
        self.send(SECURITY_MODE, &[3], false).await
    }

    fn wrapped(&self) -> bool {
        self.kind != DeviceKind::Blufi4g
    }

    async fn custom_inner(&mut self, command: &[u8]) -> Result<Vec<u8>> {
        if self.wrapped() {
            let wire = wrap(command)?;
            self.send(CUSTOM, &wire, true).await?;
        } else {
            self.send(CUSTOM, command, true).await?;
        }
        loop {
            let (kind, response) = self.receive().await?;
            if kind != CUSTOM {
                continue;
            }
            let payload = match application_payload(&response, self.wrapped()) {
                Ok(payload) => payload,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            if !matches_command(payload, command, self.wrapped()) {
                continue;
            }
            return response_data(payload, command, self.wrapped()).map(<[u8]>::to_vec);
        }
    }

    async fn custom(&mut self, command: &[u8]) -> Result<Vec<u8>> {
        self.ready()?;
        let duration = if command == [4, 1] {
            UPGRADE_TIMEOUT
        } else {
            COMMAND_TIMEOUT
        };
        let result = timeout(duration, self.custom_inner(command)).await;
        self.finish(result)
    }

    async fn scan_inner(&mut self) -> Result<Vec<WifiNetwork>> {
        self.send(SCAN, &[], false).await?;
        loop {
            let (kind, data) = self.receive().await?;
            if kind == WIFI_LIST {
                return parse_wifi(&data);
            }
        }
    }

    async fn connect_inner(&mut self, ssid: &str, password: &str) -> Result<()> {
        self.send(WIFI_MODE, &[1], false).await?;
        self.send(SSID, ssid.as_bytes(), true).await?;
        self.send(PASSWORD, password.as_bytes(), true).await?;
        self.send(CONNECT, &[], false).await?;
        loop {
            let (kind, data) = self.receive().await?;
            if kind != WIFI_STATUS {
                continue;
            }
            check_wifi_status(&data)?;
            return Ok(());
        }
    }
}

fn wrap(command: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        command.len() <= u16::MAX as usize - 6,
        "BLUFI 自定义命令过长"
    );
    let mut frame = Vec::with_capacity(command.len() + 6);
    frame.extend_from_slice(&[0xbd, 0xa0]);
    frame.extend_from_slice(&(command.len() as u16).to_be_bytes());
    frame.extend_from_slice(command);
    frame.extend_from_slice(&[0x0a, 0xdb]);
    Ok(frame)
}

fn application_payload(frame: &[u8], wrapped: bool) -> Result<&[u8]> {
    if !wrapped {
        return Ok(frame);
    }
    ensure!(
        frame.len() >= 8
            && frame[..2] == [0xbd, 0xa0]
            && frame[frame.len() - 2..] == [0x0a, 0xdb]
            && usize::from(u16::from_be_bytes([frame[2], frame[3]])) == frame.len() - 6,
        "BLUFI 应用帧封装或长度错误"
    );
    Ok(&frame[4..frame.len() - 2])
}

fn matches_command(payload: &[u8], command: &[u8], wrapped: bool) -> bool {
    if command.len() < 2 || payload.len() < 2 {
        return false;
    }
    if wrapped && command[0] == 1 {
        if command[1] == 0x13 {
            return payload[..2] == [1, 0x14];
        }
        return payload.len() == 6 && payload[..3] == [1, 0xff, command[1]];
    }
    payload[..2] == command[..2]
}

fn response_data<'a>(payload: &'a [u8], command: &[u8], wrapped: bool) -> Result<&'a [u8]> {
    ensure!(
        matches_command(payload, command, wrapped),
        "BLUFI 响应命令不匹配"
    );
    if wrapped && command[0] == 1 {
        if command[1] == 0x13 {
            return Ok(&payload[2..]);
        }
        ensure!(
            payload[3..] == [0, 0, 0],
            "BLUFI 命令失败: {:02x?}",
            &payload[3..]
        );
        return Ok(&payload[6..]);
    }
    ensure!(payload.len() >= 3, "BLUFI 响应缺少状态码");
    ensure!(payload[2] == 0, "BLUFI 命令失败: {}", payload[2]);
    Ok(&payload[3..])
}

fn parse_wifi(data: &[u8]) -> Result<Vec<WifiNetwork>> {
    let mut networks = Vec::new();
    let mut remaining = data;
    while !remaining.is_empty() {
        let length = usize::from(remaining[0]);
        ensure!(
            length >= 1 && remaining.len() > length,
            "BLUFI WiFi 列表条目长度错误"
        );
        let ssid =
            std::str::from_utf8(&remaining[2..1 + length]).context("WiFi SSID 不是 UTF-8")?;
        networks.push(WifiNetwork {
            ssid: ssid.into(),
            rssi: i16::from(remaining[1] as i8),
        });
        remaining = &remaining[1 + length..];
    }
    Ok(networks)
}

fn check_wifi_status(data: &[u8]) -> Result<()> {
    ensure!(data.len() >= 2, "BLUFI WiFi 状态帧过短");
    ensure!(
        data[0] == 1 || data[0] == 3,
        "设备不是 WiFi Station 模式: {}",
        data[0]
    );
    // Only state zero confirms connection WITH an assigned IP. 1/2/3 and
    // vendor-specific reason codes must never be interpreted as success.
    ensure!(data[1] == 0, "WiFi 连接失败，设备状态码: {}", data[1]);
    Ok(())
}

// Minimal prost projections: unknown fields are skipped, no protoc/build script.
#[derive(Clone, PartialEq, Message)]
struct InfoMessage {
    #[prost(string, tag = "2")]
    sn: String,
    #[prost(string, tag = "3")]
    version: String,
    #[prost(uint32, tag = "4")]
    power: u32,
    #[prost(uint64, tag = "9")]
    total_storage: u64,
}

#[derive(Clone, PartialEq, Message)]
struct ModemMessage {
    #[prost(string, tag = "3")]
    version: String,
    #[prost(string, tag = "4")]
    iccid: String,
}

fn decode_modem_info(data: &[u8]) -> Result<ModemMessage> {
    let modem = ModemMessage::decode(data).context("BLUFI Modem protobuf 无效")?;
    ensure!(
        !modem.version.is_empty() && !modem.iccid.is_empty(),
        "BLUFI Modem 信息缺少版本或 ICCID"
    );
    Ok(modem)
}

fn legacy_info(data: &[u8]) -> Result<DeviceInfo> {
    let json: serde_json::Value =
        serde_json::from_slice(data).context("旧 BLUFI 信息 JSON 无效")?;
    let object = json.as_object().context("旧 BLUFI 信息必须是 JSON 对象")?;
    let text = |keys: &[&str]| -> Result<String> {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("旧 BLUFI 信息缺少字符串字段 {}", keys.join("/")))
    };
    let number = |keys: &[&str]| -> Result<u64> {
        keys.iter()
            .find_map(|key| {
                object
                    .get(*key)
                    .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            })
            .ok_or_else(|| anyhow!("旧 BLUFI 信息缺少数值字段 {}", keys.join("/")))
    };
    Ok(DeviceInfo {
        sn: text(&["device_id", "sn"])?,
        firmware: text(&["version"])?,
        storage_total: number(&["storage_space", "totalStorage"])?,
        battery: number(&["device_power", "power"])?
            .try_into()
            .context("设备电量超出范围")?,
        ..DeviceInfo::default()
    })
}

#[async_trait]
impl DeviceOps for Session {
    async fn info(&mut self) -> Result<DeviceInfo> {
        let mut info = if self.kind == DeviceKind::BlufiOld {
            legacy_info(&self.custom(&[1, 0x13]).await?)?
        } else {
            let message = InfoMessage::decode(self.custom(&[2, 1]).await?.as_slice())
                .context("BLUFI Info protobuf 无效")?;
            DeviceInfo {
                sn: message.sn,
                firmware: message.version,
                battery: message.power,
                storage_total: message.total_storage,
                ..DeviceInfo::default()
            }
        };
        ensure!(
            !info.sn.is_empty() && !info.firmware.is_empty(),
            "BLUFI 设备信息缺少 SN 或固件版本"
        );
        ensure!(info.battery <= 100, "BLUFI 设备电量无效: {}", info.battery);
        if self.kind == DeviceKind::Blufi4g {
            let modem = decode_modem_info(&self.custom(&[3, 1]).await?)?;
            info.modem_version = modem.version;
            info.iccid = modem.iccid;
        }
        Ok(info)
    }

    async fn activate(&mut self) -> Result<()> {
        ensure!(
            self.kind == DeviceKind::BlufiOld,
            "新 BLUFI 设备已激活，不支持激活命令"
        );
        self.custom(&[1, 0x0b]).await?;
        Ok(())
    }

    async fn set_time(&mut self, utc_ms: u64) -> Result<()> {
        let mut command = [0u8; 10];
        command[..2].copy_from_slice(&[1, 0x1e]);
        command[2..].copy_from_slice(&utc_ms.to_be_bytes());
        self.custom(&command).await?;
        Ok(())
    }

    async fn reset(&mut self, mode: ResetMode) -> Result<()> {
        let command = [1, 0x0c, mode as u8, 0];
        let length = if self.wrapped() { 4 } else { 3 };
        self.custom(&command[..length]).await?;
        Ok(())
    }

    async fn scan_wifi(&mut self) -> Result<Vec<WifiNetwork>> {
        self.ready()?;
        let result = timeout(COMMAND_TIMEOUT, self.scan_inner()).await;
        self.finish(result)
    }

    async fn connect_wifi(&mut self, ssid: &str, password: &str) -> Result<()> {
        self.ready()?;
        ensure!(
            !ssid.is_empty() && ssid.len() <= 32 && password.len() <= 64,
            "SSID 或 WiFi 密码长度无效"
        );
        let result = timeout(COMMAND_TIMEOUT, self.connect_inner(ssid, password)).await;
        self.finish(result)
    }

    async fn set_sn(&mut self, sn: &str) -> Result<()> {
        ensure!(
            self.kind == DeviceKind::Blufi4g,
            "当前 BLUFI 型号不支持写入 SN"
        );
        ensure!(
            sn.len() == 18 && sn.is_ascii(),
            "SN 必须为 18 个 ASCII 字符"
        );
        let mut command = Vec::with_capacity(20);
        command.extend_from_slice(&[0xff, 3]);
        command.extend_from_slice(sn.as_bytes());
        self.custom(&command).await?;
        Ok(())
    }

    async fn upgrade(&mut self) -> Result<()> {
        ensure!(
            self.kind == DeviceKind::Blufi4g,
            "当前 BLUFI 型号不支持升级触发"
        );
        self.custom(&[4, 1]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn incoming(codec: &mut Codec, kind: u8, body: &[u8], secure: bool) -> Vec<Vec<u8>> {
        codec
            .encode(kind, body, secure)
            .unwrap()
            .into_iter()
            .map(|mut p| {
                p[1] |= FROM_DEVICE;
                p
            })
            .collect()
    }

    #[test]
    fn standard_security_control_and_crc_vector() {
        assert_eq!(
            Codec::default().encode(SECURITY_MODE, &[3], false).unwrap(),
            vec![vec![4, 0, 0, 1, 3]]
        );
        assert_eq!(crc16(b"123456789"), 0xd64e);
    }

    #[test]
    fn actual_shipped_wasm_security_and_encrypted_fragment_vectors() {
        // Captured from blufi-web/src/blufi.wasm through its real BlufiCore(20),
        // using Node crypto as the DH peer. Freeze the negotiated MD5 key so
        // this regression is deterministic despite the WASM's random DH key.
        let key = [
            127, 187, 136, 242, 86, 181, 76, 40, 244, 132, 208, 255, 155, 232, 76, 196,
        ];
        let mut codec = Codec {
            tx: 20,
            key: Some(key),
            ..Codec::default()
        };
        assert_eq!(
            codec.encode(SECURITY_MODE, &[3], false).unwrap(),
            vec![vec![4, 0, 20, 1, 3]]
        );
        let body: Vec<u8> = (0..45).collect();
        let expected = vec![
            vec![
                77, 19, 21, 14, 40, 175, 79, 232, 16, 16, 99, 147, 24, 83, 93, 28, 206, 29, 205,
                109,
            ],
            vec![
                77, 19, 22, 14, 155, 139, 186, 125, 35, 13, 221, 136, 213, 63, 250, 83, 102, 226,
                188, 134,
            ],
            vec![
                77, 19, 23, 14, 186, 251, 219, 178, 187, 177, 68, 108, 247, 197, 65, 198, 65, 163,
                166, 195,
            ],
            vec![
                77, 3, 24, 9, 184, 157, 185, 175, 154, 252, 65, 186, 70, 71, 115,
            ],
        ];
        assert_eq!(codec.encode(CUSTOM, &body, true).unwrap(), expected);
        let mut receiver = Codec {
            rx: 21,
            key: Some(key),
            ..Codec::default()
        };
        for mut packet in expected {
            packet[1] |= FROM_DEVICE;
            let result = receiver.decode(&packet).unwrap();
            if let Some(message) = result.message {
                assert_eq!(message, (CUSTOM, body.clone()));
            }
        }
    }

    #[test]
    fn aes_cfb128_known_answer_and_sequence_iv() {
        let mut bytes = [0u8; 32];
        crypt(&[0; 16], 0, &mut bytes, false);
        assert_eq!(
            bytes,
            [
                0x66, 0xe9, 0x4b, 0xd4, 0xef, 0x8a, 0x2c, 0x3b, 0x88, 0x4c, 0xfa, 0x59, 0xca, 0x34,
                0x2b, 0x2e, 0xf7, 0x95, 0xbd, 0x4a, 0x52, 0xe2, 0x9e, 0xd7, 0x13, 0xd3, 0x13, 0xfa,
                0x20, 0xe9, 0x8d, 0xbc
            ]
        );
        let encrypted = bytes;
        crypt(&[0; 16], 0, &mut bytes, true);
        assert_eq!(bytes, [0; 32]);
        crypt(&[0; 16], 1, &mut bytes, false);
        assert_ne!(bytes, encrypted);
    }

    #[test]
    fn dh_parameters_and_minimal_secret_hash_match_firmware() {
        let prime = BigUint::parse_bytes(DH_PRIME.as_bytes(), 16).unwrap();
        let a = DhExchange::with_private(prime.clone(), BigUint::from(2u8));
        let b = DhExchange::with_private(prime, BigUint::from(3u8));
        assert_eq!(a.key(&b.public).unwrap(), b.key(&a.public).unwrap());
        assert_eq!(a.key(&[2]).unwrap(), <[u8; 16]>::from(Md5::digest([4])));
        assert_ne!(a.key(&[2]).unwrap(), <[u8; 16]>::from(Md5::digest([0, 4])));
        let parameters = a.parameters();
        assert_eq!(&parameters[..2], &[0, 128]);
        assert_eq!(&parameters[130..], &[0, 1, 2, 0, 1, 4]);
        assert!(a.key(&[0]).is_err());
        assert!(a.key(&[1]).is_err());
        assert!(a.key(&a.prime.to_bytes_be()).is_err());
    }

    #[test]
    fn encrypted_fragments_are_safe_and_plaintext_replies_are_compatible() {
        let body: Vec<u8> = (0..=255).cycle().take(600).collect();
        for secure in [false, true] {
            let mut sender = Codec {
                key: Some([7; 16]),
                ..Codec::default()
            };
            let packets = incoming(&mut sender, CUSTOM, &body, secure);
            assert!(packets.iter().all(|p| p.len() <= 20));
            let mut receiver = Codec {
                key: Some([7; 16]),
                ..Codec::default()
            };
            for (index, packet) in packets.iter().enumerate() {
                let received = receiver.decode(packet).unwrap();
                if index + 1 == packets.len() {
                    assert_eq!(received.message, Some((CUSTOM, body.clone())));
                } else {
                    assert!(received.message.is_none());
                }
            }
        }
    }

    #[test]
    fn rejects_corruption_sequence_direction_and_bad_fragment_lengths() {
        let mut sender = Codec {
            key: Some([9; 16]),
            ..Codec::default()
        };
        let packet = incoming(&mut sender, CUSTOM, &[2, 1, 0], true).remove(0);
        let mut broken = packet.clone();
        broken[4] ^= 1;
        assert!(
            Codec {
                key: Some([9; 16]),
                ..Codec::default()
            }
            .decode(&broken)
            .is_err()
        );
        let mut broken = packet.clone();
        broken[2] = 8;
        assert!(Codec::default().decode(&broken).is_err());
        let mut broken = packet.clone();
        broken[1] &= !FROM_DEVICE;
        assert!(Codec::default().decode(&broken).is_err());
        assert!(Codec::default().decode(&[CUSTOM, 4, 0, 2, 1]).is_err());
        let mut receiver = Codec::default();
        assert!(
            receiver
                .decode(&[CUSTOM, 20, 0, 3, 4, 0, 1])
                .unwrap()
                .message
                .is_none()
        );
        assert!(receiver.decode(&[CUSTOM, 4, 1, 1, 2]).is_err());
    }

    #[test]
    fn ack_interleaving_and_sequence_wrap() {
        let mut receiver = Codec {
            rx: 255,
            ..Codec::default()
        };
        let received = receiver.decode(&[CUSTOM, 28, 255, 3, 3, 0, 10]).unwrap();
        assert_eq!(received.ack, Some(255));
        assert!(received.message.is_none());
        assert!(receiver.decode(&[0, 4, 0, 1, 5]).unwrap().message.is_none());
        assert_eq!(
            receiver.decode(&[CUSTOM, 4, 1, 2, 11, 12]).unwrap().message,
            Some((CUSTOM, vec![10, 11, 12]))
        );
        let mut sender = Codec {
            tx: 255,
            ..Codec::default()
        };
        assert_eq!(sender.packet(SCAN, 0, &[]).unwrap()[2], 255);
        assert_eq!(sender.packet(SCAN, 0, &[]).unwrap()[2], 0);
    }

    #[test]
    fn legacy_ack_matches_subcommand_and_checks_every_result_byte() {
        let command = [1, 0x0c, 2, 0];
        let payload = [1, 0xff, 0x0c, 0, 0, 0];
        assert!(matches_command(&payload, &command, true));
        assert!(!matches_command(&[1, 0xff, 0x1e, 0, 0, 0], &command, true));
        assert!(!matches_command(&[1, 0xff, 0x0c, 0], &command, true));
        for index in 3..6 {
            let mut failed = payload;
            failed[index] = 1;
            assert!(response_data(&failed, &command, true).is_err());
        }
        let frame = wrap(&payload).unwrap();
        assert_eq!(application_payload(&frame, true).unwrap(), payload);
        let mut broken = frame;
        broken[3] += 1;
        assert!(application_payload(&broken, true).is_err());
        assert!(response_data(&[2, 1, 1], &[2, 1], false).is_err());
        assert!(response_data(&[2, 1], &[2, 1], false).is_err());
        assert_eq!(
            response_data(&[1, 0x14, b'{', b'}'], &[1, 0x13], true).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn legacy_json_variants_and_protobuf_projection() {
        for data in [
            r#"{"device_id":"SN","version":"1.0","device_power":88,"storage_space":8589934592}"#,
            r#"{"sn":"SN","version":"1.0","device_power":"88","storage_space":"8589934592"}"#,
        ] {
            let info = legacy_info(data.as_bytes()).unwrap();
            assert_eq!(info.sn, "SN");
            assert_eq!(info.battery, 88);
            assert_eq!(info.storage_total, 8589934592);
        }
        assert!(legacy_info(b"{}").is_err());
        let message =
            InfoMessage::decode(&[0x12, 2, b'S', b'N', 0x1a, 1, b'V', 0x20, 88, 0x48, 128, 8][..])
                .unwrap();
        assert_eq!(message.sn, "SN");
        assert_eq!(message.total_storage, 1024);
        let modem = decode_modem_info(&[0x1a, 1, b'V', 0x22, 2, b'I', b'D'][..]).unwrap();
        assert_eq!(modem.version, "V");
        assert_eq!(modem.iccid, "ID");
        assert!(decode_modem_info(&ModemMessage::default().encode_to_vec()).is_err());
        assert!(
            decode_modem_info(
                &ModemMessage {
                    version: "MODEM".into(),
                    iccid: String::new(),
                }
                .encode_to_vec()
            )
            .is_err()
        );
    }

    #[test]
    fn scan_and_connect_require_valid_real_results() {
        assert_eq!(
            parse_wifi(&[4, 206, b'A', b'P', b'1']).unwrap(),
            vec![WifiNetwork {
                ssid: "AP1".into(),
                rssi: -50
            }]
        );
        assert!(parse_wifi(&[0]).is_err());
        assert!(parse_wifi(&[4, 206, b'A']).is_err());
        assert!(check_wifi_status(&[1, 0]).is_ok());
        assert!(check_wifi_status(&[3, 0, 0]).is_ok());
        for status in [1, 2, 3, 15, 201, 202, 203] {
            assert!(check_wifi_status(&[1, status]).is_err());
        }
        assert!(check_wifi_status(&[2, 0]).is_err());
        assert!(check_wifi_status(&[1]).is_err());
    }

    struct SilentIo(Arc<AtomicUsize>);
    #[async_trait]
    impl PacketIo for SilentIo {
        async fn write(&mut self, _: &[u8]) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn read(&mut self) -> Result<Vec<u8>> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn timeout_poisons_session_and_prevents_any_subsequent_write() {
        let writes = Arc::new(AtomicUsize::new(0));
        let mut session = Session {
            io: Box::new(SilentIo(writes.clone())),
            kind: DeviceKind::Blufi4g,
            codec: Codec {
                key: Some([1; 16]),
                ..Codec::default()
            },
            poisoned: false,
        };
        let result = timeout(Duration::from_millis(1), session.custom_inner(&[2, 1])).await;
        assert!(session.finish(result).is_err());
        assert!(session.poisoned);
        let count = writes.load(Ordering::SeqCst);
        assert_eq!(count, 1);
        assert!(session.set_time(1).await.is_err());
        assert!(session.scan_wifi().await.is_err());
        assert!(session.connect_wifi("AP", "password").await.is_err());
        assert_eq!(writes.load(Ordering::SeqCst), count);
    }

    struct FirmwarePeer {
        kind: DeviceKind,
        codec: Codec,
        replies: std::collections::VecDeque<Vec<u8>>,
        parameter_length: usize,
        secured: bool,
    }

    impl FirmwarePeer {
        fn respond(&mut self, kind: u8, payload: &[u8], secure: bool) {
            self.replies
                .extend(incoming(&mut self.codec, kind, payload, secure));
        }
    }

    #[async_trait]
    impl PacketIo for FirmwarePeer {
        async fn write(&mut self, data: &[u8]) -> Result<()> {
            assert!(data.len() <= 20);
            // Mirror the client direction solely to reuse the independently
            // sequenced receiver; direction does not participate in the CRC.
            let mut mirrored = data.to_vec();
            mirrored[1] |= FROM_DEVICE;
            let Some((kind, body)) = self.codec.decode(&mirrored)?.message else {
                return Ok(());
            };
            match kind {
                NEGOTIATION if body[0] == 0 => {
                    self.parameter_length = usize::from(u16::from_be_bytes([body[1], body[2]]));
                }
                NEGOTIATION => {
                    assert_eq!(body[0], 1);
                    assert_eq!(body.len() - 1, self.parameter_length);
                    let mut cursor = &body[1..];
                    let mut integers = Vec::new();
                    for _ in 0..3 {
                        let length = usize::from(u16::from_be_bytes([cursor[0], cursor[1]]));
                        integers.push(cursor[2..2 + length].to_vec());
                        cursor = &cursor[2 + length..];
                    }
                    assert!(cursor.is_empty());
                    assert_eq!(integers[1], [2]);
                    let peer = DhExchange::with_private(
                        BigUint::from_bytes_be(&integers[0]),
                        BigUint::from(7u8),
                    );
                    self.respond(NEGOTIATION, &peer.public, false);
                    self.codec.key = Some(peer.key(&integers[2])?);
                }
                SECURITY_MODE => {
                    assert_eq!(body, [3]);
                    assert_eq!(data[1], 0);
                    assert!(self.codec.key.is_some());
                    self.secured = true;
                }
                CUSTOM => {
                    assert!(self.secured);
                    assert_eq!(data[1] & 3, 3);
                    let wrapped = self.kind != DeviceKind::Blufi4g;
                    let command = application_payload(&body, wrapped)?;
                    let mut response = match &command[..2] {
                        [1, 0x13] => {
                            let mut result = vec![1, 0x14];
                            result.extend_from_slice(br#"{"sn":"SN","version":"V","device_power":88,"storage_space":1024}"#);
                            result
                        }
                        [2, 1] => {
                            let mut result = vec![2, 1, 0];
                            result.extend_from_slice(
                                &InfoMessage {
                                    sn: "SN".into(),
                                    version: "V".into(),
                                    power: 88,
                                    total_storage: 1024,
                                }
                                .encode_to_vec(),
                            );
                            result
                        }
                        [3, 1] => {
                            let mut result = vec![3, 1, 0];
                            result.extend_from_slice(
                                &ModemMessage {
                                    version: "MODEM".into(),
                                    iccid: "ICCID".into(),
                                }
                                .encode_to_vec(),
                            );
                            result
                        }
                        [1, subcommand] if wrapped => vec![1, 0xff, *subcommand, 0, 0, 0],
                        _ => vec![command[0], command[1], 0],
                    };
                    if wrapped {
                        response = wrap(&response)?;
                    }
                    // Exercise both old unencrypted firmware and encrypted SMCE.
                    self.respond(CUSTOM, &response, self.kind == DeviceKind::Blufi4g);
                }
                SCAN => self.respond(WIFI_LIST, &[3, 206, b'A', b'P'], true),
                CONNECT => self.respond(WIFI_STATUS, &[1, 0, 0], true),
                WIFI_MODE => assert_eq!(body, [1]),
                SSID | PASSWORD => assert!(self.secured),
                _ => bail!("unexpected host packet 0x{kind:02x}"),
            }
            Ok(())
        }

        async fn read(&mut self) -> Result<Vec<u8>> {
            self.replies
                .pop_front()
                .context("client read before firmware response")
        }
    }

    #[tokio::test]
    async fn native_handshake_and_all_supported_device_operations_interoperate() {
        for kind in [
            DeviceKind::BlufiOld,
            DeviceKind::BlufiNew,
            DeviceKind::Blufi4g,
        ] {
            let peer = FirmwarePeer {
                kind,
                codec: Codec::default(),
                replies: Default::default(),
                parameter_length: 0,
                secured: false,
            };
            let mut device = open(Box::new(peer), kind).await.unwrap();
            let info = device.info().await.unwrap();
            assert_eq!(info.sn, "SN");
            assert_eq!(info.firmware, "V");
            assert_eq!(info.storage_total, 1024);
            assert_eq!(info.battery, 88);
            if kind == DeviceKind::Blufi4g {
                assert_eq!(info.modem_version, "MODEM");
                assert_eq!(info.iccid, "ICCID");
                device.set_sn("4S1000EC1234567890").await.unwrap();
                device.upgrade().await.unwrap();
            } else {
                assert!(device.set_sn("4S1000EC1234567890").await.is_err());
                assert!(device.upgrade().await.is_err());
            }
            if kind == DeviceKind::BlufiOld {
                device.activate().await.unwrap();
            } else {
                assert!(device.activate().await.is_err());
            }
            device.set_time(0x0102030405060708).await.unwrap();
            for mode in [ResetMode::Factory, ResetMode::Wifi, ResetMode::Format] {
                device.reset(mode).await.unwrap();
            }
            assert_eq!(
                device.scan_wifi().await.unwrap(),
                vec![WifiNetwork {
                    ssid: "AP".into(),
                    rssi: -50
                }]
            );
            device
                .connect_wifi("AP", "a-password-long-enough-to-fragment")
                .await
                .unwrap();
        }
    }

    struct ErrorIo;
    #[async_trait]
    impl PacketIo for ErrorIo {
        async fn write(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn read(&mut self) -> Result<Vec<u8>> {
            Ok(vec![ERROR, 4, 0, 1, 9])
        }
    }

    #[tokio::test]
    async fn firmware_format_error_is_not_success() {
        let mut session = Session {
            io: Box::new(ErrorIo),
            kind: DeviceKind::BlufiOld,
            codec: Codec::default(),
            poisoned: false,
        };
        let error = session.receive().await.unwrap_err();
        assert!(error.to_string().contains("0x09"));
        assert!(session.poisoned);
    }
}
