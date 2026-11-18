use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    BlufiOld,
    BlufiNew,
    Blufi4g,
    Sa01a2,
    Sa02a1,
}
impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::BlufiOld => "WiFi · 旧 BLUFI",
            Self::BlufiNew => "WiFi · 新 BLUFI",
            Self::Blufi4g => "4G · BLUFI",
            Self::Sa01a2 => "SA01A2",
            Self::Sa02a1 => "SA02A1",
        }
    }
    pub fn has_modem(self) -> bool {
        matches!(self, Self::Blufi4g | Self::Sa02a1)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeviceFilter {
    #[default]
    #[serde(alias = "All")]
    Auto,
    #[serde(alias = "Wifi", alias = "Blufi4g")]
    Blufi,
    #[serde(alias = "Sa01a2", alias = "Sa02a1")]
    Smartcard,
}
impl DeviceFilter {
    pub fn accepts(self, kind: DeviceKind) -> bool {
        match self {
            Self::Auto => true,
            Self::Blufi => matches!(
                kind,
                DeviceKind::BlufiOld | DeviceKind::BlufiNew | DeviceKind::Blufi4g
            ),
            Self::Smartcard => matches!(kind, DeviceKind::Sa01a2 | DeviceKind::Sa02a1),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiConfig {
    pub ssid: String,
    #[serde(skip)]
    pub password: String,
    pub attempts: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalCheck {
    pub ssid: String,
    pub min_rssi: i16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub filter: DeviceFilter,
    pub adapter: String,
    pub concurrency: usize,
    pub min_rssi: i16,
    pub info_delay_seconds: u32,
    pub version: Option<String>,
    pub storage: Option<String>,
    pub min_power: Option<u32>,
    pub modem_version: Option<String>,
    pub factory_firmware_whitelist: String,
    pub activate: bool,
    pub sync_time: bool,
    pub factory_reset: bool,
    pub clear_data: bool,
    pub clear_wifi: bool,
    pub sync_sn: bool,
    pub upgrade: bool,
    pub factory_test_check: bool,
    pub wifi: Option<WifiConfig>,
    pub signals: Vec<SignalCheck>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            filter: DeviceFilter::Auto,
            adapter: String::new(),
            concurrency: 2,
            min_rssi: -85,
            info_delay_seconds: 5,
            version: None,
            storage: None,
            min_power: Some(20),
            modem_version: None,
            factory_firmware_whitelist: "4.1.0\n4.1.1".into(),
            activate: false,
            sync_time: true,
            factory_reset: false,
            clear_data: false,
            clear_wifi: false,
            sync_sn: false,
            upgrade: false,
            factory_test_check: false,
            wifi: None,
            signals: Vec::new(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !(1..=8).contains(&self.concurrency) {
            bail!("并发数必须为 1–8");
        }
        if !(-127..=0).contains(&self.min_rssi) {
            bail!("蓝牙 RSSI 必须为 -127–0 dBm");
        }
        if self.info_delay_seconds > 60 {
            bail!("设备初始化等待时间必须为 0–60 秒");
        }
        if self.min_power.is_some_and(|v| v > 100) {
            bail!("最低电量必须为 0–100%");
        }
        for (name, value) in [
            ("固件版本", &self.version),
            ("存储容量", &self.storage),
            ("Modem 版本", &self.modem_version),
        ] {
            if value.as_ref().is_some_and(|v| v.trim().is_empty()) {
                bail!("{name}不能为空");
            }
        }
        if let Some(wifi) = &self.wifi {
            if wifi.ssid.is_empty() || wifi.ssid.len() > 32 {
                bail!("SSID 必须为 1–32 个 UTF-8 字节");
            }
            if wifi.password.len() > 64 {
                bail!("WiFi 密码不能超过 64 字节");
            }
            if !(1..=10).contains(&wifi.attempts) {
                bail!("配网尝试次数必须为 1–10");
            }
        }
        if self.signals.len() > 5 {
            bail!("最多支持五组 WiFi 信号检查");
        }
        for check in &self.signals {
            if check.ssid.is_empty() {
                continue;
            }
            if check.ssid.len() > 32 || !(-127..=0).contains(&check.min_rssi) {
                bail!("WiFi 信号检查的 SSID 或 RSSI 无效");
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub sn: String,
    pub firmware: String,
    pub storage_total: u64,
    pub battery: u32,
    pub modem_version: String,
    pub iccid: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub rssi: i16,
}
#[derive(Debug, Clone, Copy)]
pub enum ResetMode {
    Factory = 0,
    Wifi = 1,
    Format = 2,
}
#[async_trait]
pub trait PacketIo: Send {
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn read(&mut self) -> Result<Vec<u8>>;
}
#[async_trait]
pub trait DeviceOps: Send {
    async fn info(&mut self) -> Result<DeviceInfo>;
    async fn activate(&mut self) -> Result<()> {
        bail!("当前设备协议不支持激活");
    }
    async fn set_time(&mut self, utc_ms: u64) -> Result<()>;
    async fn reset(&mut self, mode: ResetMode) -> Result<()>;
    async fn scan_wifi(&mut self) -> Result<Vec<WifiNetwork>>;
    async fn connect_wifi(&mut self, ssid: &str, password: &str) -> Result<()>;
    async fn set_sn(&mut self, sn: &str) -> Result<()>;
    async fn upgrade(&mut self) -> Result<()> {
        bail!("当前设备协议不支持升级触发");
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowStatus {
    Queued,
    Running,
    Success,
    Failed,
    NeedsCheck,
    Cancelled,
    Skipped,
}
impl RowStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队",
            Self::Running => "处理中",
            Self::Success => "成功",
            Self::Failed => "失败",
            Self::NeedsCheck => "待检查",
            Self::Cancelled => "已停止",
            Self::Skipped => "已跳过",
        }
    }
}
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub kind: Option<DeviceKind>,
    pub rssi: Option<i16>,
    pub status: RowStatus,
    pub stage: String,
    pub operations: Vec<String>,
    pub info: Option<DeviceInfo>,
    pub elapsed_ms: u64,
}
#[derive(Debug)]
pub enum Command {
    Start(Config),
    Stop,
    RetryFailed,
    RetryDevice(String),
    ForgetSuccess,
    Shutdown,
}
#[derive(Debug)]
pub enum Event {
    Log(String),
    Error(String),
    Device(DeviceRow),
    BatchStopped,
    Running,
    Adapters(Vec<String>),
}
