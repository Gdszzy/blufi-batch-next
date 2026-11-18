use crate::model::*;
use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
        ValueNotification, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures::{Stream, StreamExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::Pin,
    sync::mpsc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    task::JoinSet,
    time::{interval, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const BLUFI: Uuid = Uuid::from_u128(0x0000ffff_0000_1000_8000_00805f9b34fb);
const SA01: Uuid = Uuid::from_u128(0x99eeb53e_ec41_4472_ac6c_20cc08390000);
const SA02: Uuid = Uuid::from_u128(0x32b4f7d5_20ce_4c14_9b73_d83e26061800);
const BLE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Backend {
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Backend {
    pub fn spawn() -> Self {
        let (commands, rx) = mpsc::channel();
        let (tx, events) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("blufi-ble".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = tx.send(Event::Error(format!("无法启动蓝牙工作线程: {error}")));
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(run(rx, tx.clone())) {
                    let _ = tx.send(Event::Error(format!("蓝牙后端错误: {error:#}")));
                    let _ = tx.send(Event::BatchStopped);
                }
            })
            .expect("无法创建蓝牙线程");
        Self {
            commands,
            events,
            worker: Some(worker),
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct BleIo {
    peripheral: Peripheral,
    write_char: Characteristic,
    notify_uuid: Uuid,
    write_type: WriteType,
    notifications: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
}
#[async_trait]
impl PacketIo for BleIo {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        if data.len() > 20 {
            bail!("发送帧超过安全 ATT 载荷 20 字节");
        }
        timeout(
            BLE_TIMEOUT,
            self.peripheral
                .write(&self.write_char, data, self.write_type),
        )
        .await
        .context("蓝牙写入超时")?
        .context("蓝牙写入失败")
    }
    async fn read(&mut self) -> Result<Vec<u8>> {
        while let Some(notification) = self.notifications.next().await {
            if notification.uuid == self.notify_uuid {
                return Ok(notification.value);
            }
        }
        bail!("蓝牙通知流已断开")
    }
}
fn kind_for_services(services: impl Iterator<Item = Uuid>, name: &str) -> Option<DeviceKind> {
    let mask = services.fold(0_u8, |mask, service| {
        mask | if service == SA02 {
            4
        } else if service == SA01 {
            2
        } else if service == BLUFI {
            1
        } else {
            0
        }
    });
    if mask & 4 != 0 {
        Some(DeviceKind::Sa02a1)
    } else if mask & 2 != 0 {
        Some(DeviceKind::Sa01a2)
    } else if mask & 1 != 0 {
        Some(if name.starts_with("SMCE") {
            DeviceKind::Blufi4g
        } else if name.chars().count() == 10 {
            DeviceKind::BlufiNew
        } else {
            DeviceKind::BlufiOld
        })
    } else {
        None
    }
}

fn project_scan_filter(filter: DeviceFilter) -> ScanFilter {
    // WinRT filters individual advertisement packets. In particular, our combined
    // service filter drops valid devices on Windows, and even a single UUID can
    // discard a separate scan response containing the name. Receive both packets
    // so btleplug can merge their properties, then enforce UUID/name/model rules
    // in advertised_project_kind before anything enters the processing queue.
    if cfg!(target_os = "windows") {
        return ScanFilter::default();
    }
    let services = match filter {
        DeviceFilter::Auto => vec![BLUFI, SA01, SA02],
        DeviceFilter::Blufi => vec![BLUFI],
        DeviceFilter::Smartcard => vec![SA01, SA02],
    };
    ScanFilter { services }
}

fn advertised_project_kind(
    services: &[Uuid],
    name: &str,
    filter: DeviceFilter,
) -> Option<DeviceKind> {
    let kind = kind_for_services(services.iter().copied(), name)?;
    // BLUFI's UUID is shared by other Espressif products. Our legacy products
    // advertise SMCD/SMCE; RPC's vendor-specific UUIDs identify the newer models.
    if matches!(
        kind,
        DeviceKind::BlufiOld | DeviceKind::BlufiNew | DeviceKind::Blufi4g
    ) && !name.starts_with("SMCD")
        && !name.starts_with("SMCE")
    {
        return None;
    }
    filter.accepts(kind).then_some(kind)
}
async fn open(peripheral: &Peripheral, name: &str) -> Result<(DeviceKind, Box<dyn DeviceOps>)> {
    timeout(BLE_TIMEOUT, peripheral.connect())
        .await
        .context("蓝牙连接超时")??;
    timeout(BLE_TIMEOUT, peripheral.discover_services())
        .await
        .context("服务发现超时")??;
    let kind = kind_for_services(peripheral.services().iter().map(|s| s.uuid), name)
        .context("未发现受支持的 GATT 服务")?;
    let (service, rx, tx) = match kind {
        DeviceKind::Sa01a2 => (
            SA01,
            Uuid::from_u128(SA01.as_u128() + 1),
            Uuid::from_u128(SA01.as_u128() + 2),
        ),
        DeviceKind::Sa02a1 => (
            SA02,
            Uuid::from_u128(SA02.as_u128() + 1),
            Uuid::from_u128(SA02.as_u128() + 2),
        ),
        _ => (
            BLUFI,
            Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb),
            Uuid::from_u128(0x0000ff02_0000_1000_8000_00805f9b34fb),
        ),
    };
    let chars = peripheral.characteristics();
    let rx = chars
        .iter()
        .find(|c| c.service_uuid == service && c.uuid == rx)
        .context("缺少 RX 特征")?
        .clone();
    let tx = chars
        .iter()
        .find(|c| c.service_uuid == service && c.uuid == tx)
        .context("缺少 TX 特征")?
        .clone();
    let can_write = |c: &Characteristic| {
        c.properties
            .intersects(CharPropFlags::WRITE | CharPropFlags::WRITE_WITHOUT_RESPONSE)
    };
    let can_notify = |c: &Characteristic| {
        c.properties
            .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
    };
    let (write_char, notify_char) = if can_write(&rx) && can_notify(&tx) {
        (rx, tx)
    } else if matches!(kind, DeviceKind::Sa01a2 | DeviceKind::Sa02a1)
        && can_write(&tx)
        && can_notify(&rx)
    {
        (tx, rx)
    } else {
        bail!("GATT 特征属性不支持写入/通知");
    };
    let write_type = if write_char.properties.contains(CharPropFlags::WRITE) {
        WriteType::WithResponse
    } else {
        WriteType::WithoutResponse
    };
    let notifications = timeout(BLE_TIMEOUT, peripheral.notifications())
        .await
        .context("打开通知流超时")??;
    timeout(BLE_TIMEOUT, peripheral.subscribe(&notify_char))
        .await
        .context("订阅通知超时")??;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let io = Box::new(BleIo {
        peripheral: peripheral.clone(),
        write_char,
        notify_uuid: notify_char.uuid,
        write_type,
        notifications,
    });
    let session = match kind {
        DeviceKind::Sa01a2 | DeviceKind::Sa02a1 => crate::rpc::open(io, kind).await?,
        _ => crate::blufi::open(io, kind).await?,
    };
    Ok((kind, session))
}
async fn disconnect(peripheral: &Peripheral) {
    let _ = timeout(Duration::from_secs(5), peripheral.disconnect()).await;
}
async fn peripheral_is_disconnected(peripheral: &Peripheral) -> bool {
    matches!(
        timeout(Duration::from_secs(2), peripheral.is_connected()).await,
        Ok(Ok(false))
    )
}
fn error_indicates_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_lowercase();
        message.contains("断开")
            || message.contains("disconnect")
            || message.contains("not connected")
    })
}
async fn adapters(manager: &Manager, tx: &mpsc::Sender<Event>) -> Result<Vec<Adapter>> {
    let list = timeout(BLE_TIMEOUT, manager.adapters())
        .await
        .context("枚举蓝牙适配器超时")??;
    let mut names = Vec::with_capacity(list.len());
    for adapter in &list {
        names.push(adapter.adapter_info().await?);
    }
    let _ = tx.send(Event::Adapters(names));
    Ok(list)
}
async fn start_scan(
    manager: &Manager,
    config: &Config,
    tx: &mpsc::Sender<Event>,
) -> Result<Adapter> {
    config.validate()?;
    let list = adapters(manager, tx).await?;
    let mut selected = None;
    for adapter in list {
        if config.adapter.is_empty() || adapter.adapter_info().await? == config.adapter {
            selected = Some(adapter);
            break;
        }
    }
    let adapter =
        selected.context("没有可用的蓝牙适配器；请接入 BLE 适配器、开启蓝牙并确认系统权限")?;
    timeout(
        BLE_TIMEOUT,
        adapter.start_scan(project_scan_filter(config.filter)),
    )
    .await
    .context("开始扫描超时")??;
    Ok(adapter)
}

async fn run(rx: mpsc::Receiver<Command>, tx: mpsc::Sender<Event>) -> Result<()> {
    let manager = Manager::new()
        .await
        .context("无法连接系统蓝牙服务；Linux 需要 BlueZ / system D-Bus")?;
    if let Err(error) = adapters(&manager, &tx).await {
        let _ = tx.send(Event::Error(format!("枚举适配器失败: {error:#}")));
    }
    let mut adapter: Option<Adapter> = None;
    let mut config = Config::default();
    let mut last_config = None;
    let mut seen: HashMap<String, RowStatus> = HashMap::new();
    let mut pending_retry_ids: HashSet<String> = HashSet::new();
    let mut forget_success_on_next_start = false;
    let mut queue: VecDeque<(Peripheral, DeviceRow)> = VecDeque::new();
    let mut jobs: JoinSet<DeviceRow> = JoinSet::new();
    let mut cancel = CancellationToken::new();
    let mut stopping = false;
    let mut shutting_down = false;
    let mut tick = interval(Duration::from_millis(150));
    let mut last_scan = Instant::now() - Duration::from_secs(2);
    loop {
        tick.tick().await;
        loop {
            let command = match rx.try_recv() {
                Ok(c) => c,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => Command::Shutdown,
            };
            match command {
                Command::Start(next) => {
                    if adapter.is_some() || !jobs.is_empty() || stopping {
                        continue;
                    }
                    match start_scan(&manager, &next, &tx).await {
                        Ok(value) => {
                            config = next;
                            last_config = Some(config.clone());
                            // Explicit batches reprocess failures/excluded devices but retain
                            // success records unless the user cleared them during the prior run.
                            if forget_success_on_next_start {
                                seen.clear();
                                forget_success_on_next_start = false;
                            } else {
                                seen.retain(|_, status| {
                                    matches!(status, RowStatus::Success | RowStatus::NeedsCheck)
                                });
                            }
                            pending_retry_ids.clear();
                            cancel = CancellationToken::new();
                            adapter = Some(value);
                            let _ = tx.send(Event::Running);
                            let _ = tx.send(Event::Log(
                                "批次已启动；持续发现新设备，已完成设备不会自动重复处理".into(),
                            ));
                        }
                        Err(error) => {
                            let _ = tx.send(Event::Error(format!("无法开始批次: {error:#}")));
                            let _ = tx.send(Event::BatchStopped);
                        }
                    }
                }
                Command::RetryFailed => {
                    seen.retain(|_, status| {
                        !matches!(status, RowStatus::Failed | RowStatus::Cancelled)
                    });
                    last_scan = Instant::now() - Duration::from_secs(1);
                    if adapter.is_none() && jobs.is_empty() && !stopping {
                        if let Some(previous) = last_config.clone() {
                            match start_scan(&manager, &previous, &tx).await {
                                Ok(value) => {
                                    config = previous;
                                    adapter = Some(value);
                                    cancel = CancellationToken::new();
                                    let _ = tx.send(Event::Running);
                                }
                                Err(error) => {
                                    let _ =
                                        tx.send(Event::Error(format!("重试启动失败: {error:#}")));
                                    let _ = tx.send(Event::BatchStopped);
                                }
                            }
                        } else {
                            let _ = tx.send(Event::BatchStopped);
                        }
                    }
                }
                Command::RetryDevice(id) => {
                    if !matches!(
                        seen.get(&id),
                        Some(RowStatus::Failed | RowStatus::Cancelled)
                    ) {
                        let _ = tx.send(Event::Error("设备状态已变化，无法单独重试".into()));
                        continue;
                    }
                    if adapter.is_none() && jobs.is_empty() && !stopping {
                        let Some(previous) = last_config.clone() else {
                            let _ = tx.send(Event::Error("没有可用于重试的上一轮配置".into()));
                            let _ = tx.send(Event::BatchStopped);
                            continue;
                        };
                        match start_scan(&manager, &previous, &tx).await {
                            Ok(value) => {
                                config = previous;
                                adapter = Some(value);
                                cancel = CancellationToken::new();
                                let _ = tx.send(Event::Running);
                            }
                            Err(error) => {
                                let _ =
                                    tx.send(Event::Error(format!("单设备重试启动失败: {error:#}")));
                                let _ = tx.send(Event::BatchStopped);
                                continue;
                            }
                        }
                    }
                    pending_retry_ids.insert(id.clone());
                    seen.remove(&id);
                    last_scan = Instant::now() - Duration::from_secs(1);
                }
                Command::ForgetSuccess => {
                    if adapter.is_some() || !jobs.is_empty() || stopping {
                        // Keep successful devices deduplicated for this live scan. Forget
                        // their records at the next explicit batch start instead.
                        forget_success_on_next_start = true;
                    } else {
                        seen.retain(|_, status| {
                            !matches!(status, RowStatus::Success | RowStatus::NeedsCheck)
                        });
                    }
                }
                Command::Stop | Command::Shutdown => {
                    shutting_down |= matches!(command, Command::Shutdown);
                    stopping = true;
                    pending_retry_ids.clear();
                    cancel.cancel();
                    if let Some(value) = adapter.take() {
                        let _ = timeout(BLE_TIMEOUT, value.stop_scan()).await;
                    }
                    for (_, mut row) in queue.drain(..) {
                        row.status = RowStatus::Cancelled;
                        row.stage = "未执行；批次已停止".into();
                        seen.insert(row.id.clone(), row.status);
                        let _ = tx.send(Event::Device(row));
                    }
                    break;
                }
            }
        }
        while let Some(result) = jobs.try_join_next() {
            match result {
                Ok(row) => {
                    seen.insert(row.id.clone(), row.status);
                    let _ = tx.send(Event::Device(row));
                }
                Err(error) => {
                    let _ = tx.send(Event::Error(format!("设备工作任务异常: {error}")));
                }
            }
        }
        if stopping && jobs.is_empty() {
            let _ = tx.send(Event::BatchStopped);
            stopping = false;
            if shutting_down {
                break;
            }
        }
        if let Some(active_adapter) = &adapter {
            if last_scan.elapsed() >= Duration::from_secs(1) {
                last_scan = Instant::now();
                match timeout(BLE_TIMEOUT, active_adapter.peripherals()).await {
                    Ok(Ok(peripherals)) => {
                        for peripheral in peripherals {
                            let id = peripheral.id().to_string();
                            let retry_requested = pending_retry_ids.contains(&id);
                            if seen.contains_key(&id) && !retry_requested {
                                continue;
                            }
                            let properties = match timeout(
                                Duration::from_secs(3),
                                peripheral.properties(),
                            )
                            .await
                            {
                                Ok(Ok(Some(p))) => p,
                                _ => continue,
                            };
                            let name = properties.local_name.unwrap_or_default();
                            let Some(kind) =
                                advertised_project_kind(&properties.services, &name, config.filter)
                            else {
                                continue;
                            };
                            if properties.rssi.is_none_or(|rssi| rssi < config.min_rssi) {
                                continue;
                            }
                            let row = DeviceRow {
                                id: id.clone(),
                                name: if name.is_empty() {
                                    "未命名设备".into()
                                } else {
                                    name
                                },
                                kind: Some(kind),
                                rssi: properties.rssi,
                                status: RowStatus::Queued,
                                stage: "等待可用连接".into(),
                                operations: Vec::new(),
                                info: None,
                                elapsed_ms: 0,
                            };
                            pending_retry_ids.remove(&id);
                            seen.insert(id, RowStatus::Queued);
                            let _ = tx.send(Event::Device(row.clone()));
                            queue.push_back((peripheral, row));
                        }
                    }
                    Ok(Err(error)) => {
                        let _ = tx.send(Event::Error(format!("扫描失败，已停止批次: {error}")));
                        cancel.cancel();
                        stopping = true;
                    }
                    Err(_) => {
                        let _ = tx.send(Event::Error("读取扫描结果超时，已停止批次".into()));
                        cancel.cancel();
                        stopping = true;
                    }
                }
            }
            while !stopping && jobs.len() < config.concurrency {
                let Some((peripheral, row)) = queue.pop_front() else {
                    break;
                };
                let config = config.clone();
                let tx = tx.clone();
                let cancel = cancel.clone();
                jobs.spawn(async move { worker(peripheral, row, config, tx, cancel).await });
            }
        }
        if stopping {
            if let Some(value) = adapter.take() {
                let _ = timeout(BLE_TIMEOUT, value.stop_scan()).await;
            }
            for (_, mut row) in queue.drain(..) {
                row.status = RowStatus::Cancelled;
                row.stage = "扫描中断，未执行".into();
                seen.insert(row.id.clone(), row.status);
                let _ = tx.send(Event::Device(row));
            }
        }
    }
    Ok(())
}

struct Progress {
    row: DeviceRow,
    tx: mpsc::Sender<Event>,
    started: Instant,
}
impl Progress {
    fn stage(&mut self, stage: &str) {
        self.row.stage = stage.into();
        self.row.operations.push(stage.into());
        self.row.elapsed_ms = self.started.elapsed().as_millis() as u64;
        let _ = self.tx.send(Event::Device(self.row.clone()));
    }
    fn status(&mut self, message: &str) {
        self.row.stage = message.into();
        self.row.elapsed_ms = self.started.elapsed().as_millis() as u64;
        let _ = self.tx.send(Event::Device(self.row.clone()));
    }
    fn note(&self, message: &str) {
        let _ = self
            .tx
            .send(Event::Log(format!("{}: {message}", self.row.id)));
    }
}
fn sanitized(mut text: String, config: &Config) -> String {
    if let Some(wifi) = &config.wifi {
        for value in [&wifi.password, &wifi.ssid] {
            if !value.is_empty() {
                text = text.replace(value, "[已隐藏]");
            }
        }
    }
    text
}
async fn worker(
    peripheral: Peripheral,
    row: DeviceRow,
    config: Config,
    tx: mpsc::Sender<Event>,
    cancel: CancellationToken,
) -> DeviceRow {
    let mut progress = Progress {
        row,
        tx,
        started: Instant::now(),
    };
    progress.row.status = RowStatus::Running;
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("用户停止；已发送操作不可回滚，请重试前检查设备状态")),
        result = provision(&peripheral, &config, &mut progress) => result,
    };
    // The peripheral handle outlives every operation, even cancellation/timeout.
    // Always disconnect before releasing the bounded worker slot.
    disconnect(&peripheral).await;
    progress.row.elapsed_ms = progress.started.elapsed().as_millis() as u64;
    match result {
        Ok(())
            if matches!(
                progress.row.status,
                RowStatus::Skipped | RowStatus::NeedsCheck
            ) => {}
        Ok(()) => {
            progress.row.status = RowStatus::Success;
            progress.row.stage = "全部选定步骤已确认完成".into();
        }
        Err(error) => {
            progress.row.status = if cancel.is_cancelled() {
                RowStatus::Cancelled
            } else {
                RowStatus::Failed
            };
            progress.row.stage = sanitized(format!("{}: {error:#}", progress.row.stage), &config);
        }
    }
    progress.row
}
async fn provision(
    peripheral: &Peripheral,
    config: &Config,
    progress: &mut Progress,
) -> Result<()> {
    progress.stage("连接设备 / 识别 GATT 协议");
    let (kind, mut device) = open(peripheral, &progress.row.name).await?;
    progress.row.kind = Some(kind);
    if !config.filter.accepts(kind) {
        progress.row.status = RowStatus::Skipped;
        progress.stage("协议不在当前设备筛选范围内；未执行初始化");
        return Ok(());
    }
    if config.info_delay_seconds > 0 {
        progress.stage(&format!(
            "等待设备初始化（{} 秒）",
            config.info_delay_seconds
        ));
        tokio::time::sleep(Duration::from_secs(u64::from(config.info_delay_seconds))).await;
    }
    progress.stage("读取设备 / Modem 信息");
    let mut info = device.info().await?;
    progress.row.info = Some(info.clone());
    progress.stage("检查版本、容量与电量");
    check_info(&info, config, kind)?;
    let factory_firmware = is_factory_firmware(&info.firmware, config);
    if config.factory_test_check {
        if kind == DeviceKind::Blufi4g && factory_firmware {
            progress.stage("校验设备出厂测试记录");
            verify_factory_test(&info.sn).await?;
            progress.note("出厂测试记录校验通过");
        } else if kind == DeviceKind::Blufi4g {
            progress.note("固件不在工厂固件白名单中，跳过出厂测试校验");
        } else {
            progress.note("当前设备类型不支持出厂测试校验，跳过");
        }
    }
    if config.sync_sn
        && kind == DeviceKind::Blufi4g
        && factory_firmware
        && info.sn.starts_with("4S1000EC")
    {
        progress.stage("向服务器分配并写入正式 SN");
        let client = http_client()?;
        let sn = client
            .get(format!(
                "https://erp.gdszzy.com/api/dual-mode/getSn/{}",
                info.sn
            ))
            .send()
            .await
            .context("SN 分配请求失败")?
            .error_for_status()
            .context("SN 分配服务器拒绝请求")?
            .text()
            .await?;
        if !valid_legacy_sn(&sn) || sn.starts_with("4S1000EC") {
            bail!("服务器返回的正式 SN 无效；未写入设备");
        }
        device.set_sn(&sn).await?;
        let confirmed = device.info().await?;
        if confirmed.sn != sn {
            bail!("SN 写入后读回不一致；停止后续操作");
        }
        info = confirmed;
        progress.row.info = Some(info.clone());
    } else if config.sync_sn {
        if kind == DeviceKind::Blufi4g && !factory_firmware {
            progress.note("固件不在工厂固件白名单中，跳过自动分配 SN");
        } else {
            progress.note("仅白名单内 SMCE 4G 的出厂占位 SN 自动分配；保留现有 SN");
        }
    }
    if kind == DeviceKind::Blufi4g && factory_firmware && config.modem_version.is_some() {
        if info.iccid.is_empty() {
            bail!("设备未返回 ICCID，无法完成绑定上报");
        }
        progress.stage("上报 SN / ICCID 绑定");
        http_client()?
            .post("https://erp.gdszzy.com/api/dual-mode/setInfo")
            .json(&serde_json::json!({"sn": info.sn, "ccid": info.iccid}))
            .send()
            .await
            .context("ICCID 上报请求失败")?
            .error_for_status()
            .context("ICCID 上报服务器拒绝请求")?;
    } else if kind == DeviceKind::Blufi4g && !factory_firmware && config.modem_version.is_some() {
        progress.note("固件不在工厂固件白名单中，跳过 SN/ICCID 绑定");
    }
    if config.activate {
        if kind == DeviceKind::BlufiOld {
            progress.stage("激活设备");
            device.activate().await?;
        } else {
            progress.note("当前型号无需旧 BLUFI 激活，不适用");
        }
    }
    if config.sync_time {
        progress.stage("同步 UTC 时间");
        device
            .set_time(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
            .await?;
    }
    for (enabled, mode, label) in [
        (
            config.factory_reset,
            ResetMode::Factory,
            "恢复出厂（删除配置与文件）",
        ),
        (config.clear_data, ResetMode::Format, "格式化设备存储"),
        (config.clear_wifi, ResetMode::Wifi, "清除已保存 WiFi"),
    ] {
        if enabled {
            progress.stage(label);
            device.reset(mode).await?;
        }
    }
    let signal_checks: Vec<_> = config
        .signals
        .iter()
        .filter(|check| !check.ssid.is_empty())
        .cloned()
        .collect();
    if !signal_checks.is_empty() {
        progress.stage("扫描并检查 WiFi 信号");
        let networks = device.scan_wifi().await?;
        check_signals(&networks, &signal_checks)?;
    }
    if let Some(wifi) = &config.wifi {
        for attempt in 1..=wifi.attempts {
            progress.stage(&format!("连接并保存 WiFi · {attempt}/{}", wifi.attempts));
            match device.connect_wifi(&wifi.ssid, &wifi.password).await {
                Ok(()) => break,
                Err(error) if attempt == wifi.attempts => {
                    return Err(error).context("配网尝试次数已用尽");
                }
                Err(_) => {
                    progress.note("配网失败；断开旧会话后重试，不重放已完成的初始化步骤");
                    drop(device);
                    disconnect(peripheral).await;
                    let (reopened_kind, reopened) = open(peripheral, &progress.row.name)
                        .await
                        .context("配网重试重新连接失败")?;
                    if reopened_kind != kind {
                        bail!("重连后设备协议改变，停止配网");
                    }
                    device = reopened;
                }
            }
        }
    }
    if config.upgrade {
        if kind == DeviceKind::Blufi4g && factory_firmware {
            progress.stage("触发 4G 固件升级并等待设备确认");
            if let Err(error) = device.upgrade().await {
                if peripheral_is_disconnected(peripheral).await
                    || error_indicates_disconnect(&error)
                {
                    progress.row.status = RowStatus::NeedsCheck;
                    progress.status("设备已断开，检查灯光是否为绿色");
                    progress.note("升级等待期间设备断开；请现场检查设备灯光，不自动判定升级成功");
                    return Ok(());
                }
                return Err(error).context("等待固件升级确认失败");
            }
        } else if kind == DeviceKind::Blufi4g {
            progress.note("固件不在工厂固件白名单中，跳过升级");
        } else {
            progress.note("当前型号没有已定义的升级命令，不适用");
        }
    }
    Ok(())
}
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("无法创建 HTTPS 客户端")
}
fn parse_factory_test_result(body: &str) -> Result<bool> {
    serde_json::from_str(body.trim()).context("出厂测试接口返回值不是 true / false")
}
async fn verify_factory_test(sn: &str) -> Result<()> {
    let body = http_client()?
        .get("https://erp.gdszzy.com/api/dual-mode/testLog/pass")
        .query(&[("sn", sn)])
        .send()
        .await
        .context("出厂测试记录查询请求失败")?
        .error_for_status()
        .context("出厂测试记录查询接口拒绝请求")?
        .text()
        .await
        .context("无法读取出厂测试接口响应")?;
    ensure!(
        parse_factory_test_result(&body)?,
        "出厂测试未通过或未查询到出厂记录"
    );
    Ok(())
}
fn valid_legacy_sn(sn: &str) -> bool {
    let bytes = sn.as_bytes();
    bytes.len() == 18
        && &bytes[..2] == b"4S"
        && bytes[2..6].iter().all(u8::is_ascii_digit)
        && &bytes[6..8] == b"EC"
        && bytes[8..].iter().all(u8::is_ascii_digit)
}
pub fn format_storage(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2}{}", units[unit])
}
fn is_factory_firmware(firmware: &str, config: &Config) -> bool {
    config
        .factory_firmware_whitelist
        .lines()
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .any(|version| firmware_version_matches(firmware, version))
}
fn firmware_version_matches(device_version: &str, whitelist_version: &str) -> bool {
    let device_version = device_version.trim();
    let whitelist_version = whitelist_version.trim();
    device_version == whitelist_version
        || device_version
            .strip_prefix(whitelist_version)
            .is_some_and(|suffix| suffix.starts_with('.'))
}
fn check_info(info: &DeviceInfo, config: &Config, kind: DeviceKind) -> Result<()> {
    if info.sn.is_empty() {
        bail!("设备未返回 SN");
    }
    if matches!(
        kind,
        DeviceKind::BlufiOld | DeviceKind::BlufiNew | DeviceKind::Blufi4g
    ) && !valid_legacy_sn(&info.sn)
    {
        bail!("设备 SN 格式错误");
    }
    if let Some(expected) = &config.version {
        if &info.firmware != expected {
            bail!("固件版本不匹配：实际 {}，要求 {expected}", info.firmware);
        }
    }
    if let Some(expected) = &config.storage {
        let actual = format_storage(info.storage_total);
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            bail!("容量不匹配：实际 {actual}，要求 {expected}");
        }
    }
    if let Some(min) = config.min_power {
        if info.battery < min {
            bail!("电量 {}% 低于 {min}%", info.battery);
        }
    }
    if kind.has_modem() {
        if info.modem_version.is_empty() || info.iccid.is_empty() {
            bail!("未能完整获取 Modem 信息（版本 / ICCID）");
        }
        if let Some(expected) = &config.modem_version {
            if &info.modem_version != expected {
                bail!(
                    "Modem 版本不匹配：实际 {}，要求 {expected}",
                    info.modem_version
                );
            }
        }
    }
    Ok(())
}
fn check_signals(networks: &[WifiNetwork], checks: &[SignalCheck]) -> Result<()> {
    for (index, check) in checks.iter().enumerate() {
        let strongest = networks
            .iter()
            .filter(|network| network.ssid == check.ssid)
            .map(|network| network.rssi)
            .max()
            .with_context(|| format!("未扫描到第 {} 组目标 WiFi", index + 1))?;
        if strongest < check.min_rssi {
            bail!(
                "第 {} 组 WiFi 信号 {strongest} dBm 低于要求 {} dBm",
                index + 1,
                check.min_rssi
            );
        }
    }
    Ok(())
}
pub async fn diagnose() -> Result<()> {
    println!(
        "BLUFI Batch {} · {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    );
    let manager = timeout(BLE_TIMEOUT, Manager::new())
        .await
        .context("连接系统蓝牙服务超时")?
        .context("无法连接系统蓝牙服务")?;
    let adapters = timeout(BLE_TIMEOUT, manager.adapters())
        .await
        .context("枚举蓝牙适配器超时")?
        .context("无法枚举蓝牙适配器")?;
    if adapters.is_empty() {
        bail!("未发现 BLE 适配器；应用仍可打开配置界面，初始化需要可用蓝牙硬件");
    }
    for adapter in adapters {
        let info = timeout(BLE_TIMEOUT, adapter.adapter_info())
            .await
            .context("读取适配器信息超时")?
            .context("无法读取适配器信息")?;
        println!("适配器: {info}");
        let scan_filter = project_scan_filter(DeviceFilter::Auto);
        println!(
            "扫描策略: {}；扫描 6 秒，仅统计广播，不连接设备",
            if scan_filter.services.is_empty() {
                "系统不限制服务 UUID，应用层筛选项目设备"
            } else {
                "系统按项目服务 UUID 筛选，应用层核对设备名称"
            }
        );
        timeout(BLE_TIMEOUT, adapter.start_scan(scan_filter))
            .await
            .context("开始扫描超时")?
            .context("无法开始扫描")?;
        tokio::time::sleep(Duration::from_secs(6)).await;
        // Both WinRT and BlueZ retain discovered properties after stopping. Stop
        // before reading them so a later enumeration/property error cannot leave
        // a diagnostic scan running. The snapshot can include OS-cached devices.
        timeout(BLE_TIMEOUT, adapter.stop_scan())
            .await
            .context("停止诊断扫描超时")?
            .context("无法停止诊断扫描")?;
        let devices = timeout(BLE_TIMEOUT, adapter.peripherals())
            .await
            .context("读取扫描结果超时")?
            .context("无法读取扫描结果")?;
        let total = devices.len();
        let mut service_matches = 0;
        let mut project_matches = 0;
        let mut unavailable_properties = 0;
        for device in devices {
            let properties = match timeout(Duration::from_secs(3), device.properties()).await {
                Ok(Ok(Some(properties))) => properties,
                _ => {
                    unavailable_properties += 1;
                    continue;
                }
            };
            let name = properties.local_name.as_deref().unwrap_or_default();
            if kind_for_services(properties.services.iter().copied(), name).is_some() {
                service_matches += 1;
            }
            if advertised_project_kind(&properties.services, name, DeviceFilter::Auto).is_some() {
                project_matches += 1;
            }
        }
        println!(
            "扫描结束：系统设备记录 {total} 台，匹配项目服务 {service_matches} 台，符合项目准入 {project_matches} 台"
        );
        println!(
            "属性不可读 {unavailable_properties} 台（失败、超时或尚无属性）；统计可能包含系统缓存，不应用批次 RSSI 门槛"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_advertisements_do_not_enter_the_batch() {
        assert_eq!(
            advertised_project_kind(&[BLUFI], "ESP32-other-product", DeviceFilter::Blufi),
            None
        );
        assert_eq!(
            advertised_project_kind(&[], "SMCD123456", DeviceFilter::Blufi),
            None
        );
        assert_eq!(
            advertised_project_kind(&[Uuid::nil()], "SMCE123456", DeviceFilter::Blufi),
            None
        );
        assert_eq!(
            advertised_project_kind(&[SA01], "SMARTCARD", DeviceFilter::Smartcard),
            Some(DeviceKind::Sa01a2)
        );
    }

    #[test]
    fn native_scan_filter_preserves_platform_discovery_requirements() {
        if cfg!(target_os = "windows") {
            for filter in [
                DeviceFilter::Auto,
                DeviceFilter::Blufi,
                DeviceFilter::Smartcard,
            ] {
                assert!(project_scan_filter(filter).services.is_empty());
            }
            return;
        }
        assert_eq!(
            project_scan_filter(DeviceFilter::Auto).services,
            vec![BLUFI, SA01, SA02]
        );
        assert_eq!(
            project_scan_filter(DeviceFilter::Blufi).services,
            vec![BLUFI]
        );
        assert_eq!(
            project_scan_filter(DeviceFilter::Smartcard).services,
            vec![SA01, SA02]
        );
    }

    #[test]
    fn blufi_requires_name_until_advertisement_properties_are_complete() {
        assert_eq!(
            advertised_project_kind(&[BLUFI], "", DeviceFilter::Auto),
            None
        );
        assert_eq!(
            advertised_project_kind(&[BLUFI], "SMCD123456", DeviceFilter::Auto),
            Some(DeviceKind::BlufiNew)
        );
        assert_eq!(
            advertised_project_kind(&[SA01], "", DeviceFilter::Auto),
            Some(DeviceKind::Sa01a2)
        );
        assert_eq!(
            advertised_project_kind(&[SA02], "", DeviceFilter::Auto),
            Some(DeviceKind::Sa02a1)
        );
    }

    #[test]
    fn project_type_selection_is_applied_before_queueing() {
        assert_eq!(
            advertised_project_kind(&[SA02], "SMCD123456", DeviceFilter::Auto),
            Some(DeviceKind::Sa02a1)
        );
        assert_eq!(
            advertised_project_kind(&[SA02], "SMCD123456", DeviceFilter::Blufi),
            None
        );
        assert_eq!(
            advertised_project_kind(&[BLUFI], "SMCE123456", DeviceFilter::Smartcard),
            None
        );
        assert_eq!(
            advertised_project_kind(&[BLUFI], "SMCD123456", DeviceFilter::Smartcard),
            None
        );
        assert_eq!(
            advertised_project_kind(&[BLUFI, SA02], "SMCD123456", DeviceFilter::Smartcard),
            Some(DeviceKind::Sa02a1)
        );
        assert!(DeviceFilter::Auto.accepts(DeviceKind::BlufiOld));
        assert!(DeviceFilter::Auto.accepts(DeviceKind::BlufiNew));
        assert!(DeviceFilter::Auto.accepts(DeviceKind::Blufi4g));
        assert!(DeviceFilter::Auto.accepts(DeviceKind::Sa01a2));
        assert!(DeviceFilter::Auto.accepts(DeviceKind::Sa02a1));
        assert!(DeviceFilter::Blufi.accepts(DeviceKind::BlufiOld));
        assert!(DeviceFilter::Blufi.accepts(DeviceKind::BlufiNew));
        assert!(DeviceFilter::Blufi.accepts(DeviceKind::Blufi4g));
        assert!(DeviceFilter::Smartcard.accepts(DeviceKind::Sa01a2));
        assert!(DeviceFilter::Smartcard.accepts(DeviceKind::Sa02a1));
    }

    #[test]
    fn gatt_service_overrides_advertised_name() {
        assert_eq!(
            kind_for_services([BLUFI, SA02].into_iter(), "SMCD123456"),
            Some(DeviceKind::Sa02a1)
        );
        assert_eq!(
            kind_for_services([SA01].into_iter(), "SMCE123456"),
            Some(DeviceKind::Sa01a2)
        );
        assert_eq!(
            kind_for_services([BLUFI].into_iter(), "SMCE123456"),
            Some(DeviceKind::Blufi4g)
        );
        assert_eq!(
            kind_for_services([BLUFI].into_iter(), "SMCD123456"),
            Some(DeviceKind::BlufiNew)
        );
        assert_eq!(
            kind_for_services([Uuid::nil()].into_iter(), "SMCD123456"),
            None
        );
    }
    #[test]
    fn factory_test_endpoint_accepts_only_json_booleans() {
        assert_eq!(parse_factory_test_result(" true\n").unwrap(), true);
        assert_eq!(parse_factory_test_result("false").unwrap(), false);
        assert!(parse_factory_test_result("\"true\"").is_err());
        assert!(parse_factory_test_result("unknown").is_err());
    }

    #[test]
    fn upgrade_disconnect_errors_are_recognized_without_hiding_protocol_errors() {
        assert!(error_indicates_disconnect(&anyhow!("蓝牙通知流已断开")));
        assert!(error_indicates_disconnect(&anyhow!("device disconnected")));
        assert!(!error_indicates_disconnect(&anyhow!(
            "设备拒绝升级，错误码 2"
        )));
    }

    #[test]
    fn progress_records_device_steps_in_order() {
        let (tx, _rx) = mpsc::channel();
        let mut progress = Progress {
            row: DeviceRow {
                id: "device-1".into(),
                name: "device".into(),
                kind: None,
                rssi: None,
                status: RowStatus::Running,
                stage: String::new(),
                operations: Vec::new(),
                info: None,
                elapsed_ms: 0,
            },
            tx,
            started: Instant::now(),
        };
        progress.stage("读取设备信息");
        progress.stage("同步 UTC 时间");
        assert_eq!(progress.row.operations, ["读取设备信息", "同步 UTC 时间"]);
    }

    #[test]
    fn factory_firmware_matching_accepts_dotted_build_suffixes_without_partial_components() {
        let config = Config::default();
        assert!(is_factory_firmware("4.1.0", &config));
        assert!(is_factory_firmware("4.1.0.15", &config));
        assert!(is_factory_firmware(" 4.1.1.3 ", &config));
        assert!(!is_factory_firmware("4.1.10", &config));
        assert!(!is_factory_firmware("4.1.01.15", &config));
        assert!(!is_factory_firmware("4.1.2", &config));
        assert!(!is_factory_firmware(
            "4.1.0",
            &Config {
                factory_firmware_whitelist: String::new(),
                ..Config::default()
            }
        ));
    }

    #[test]
    fn checks_stop_before_mutation_for_wrong_version_or_low_battery() {
        let info = DeviceInfo {
            sn: "4S1234EC1234567890".into(),
            firmware: "4.0.30".into(),
            battery: 19,
            storage_total: 8 * 1024 * 1024 * 1024,
            modem_version: "MODEM".into(),
            iccid: "ICCID".into(),
            ..Default::default()
        };
        assert!(check_info(&info, &Config::default(), DeviceKind::Blufi4g).is_err());
        let config = Config {
            min_power: Some(19),
            version: Some("4.0.30".into()),
            storage: Some("8.00G".into()),
            ..Default::default()
        };
        assert!(check_info(&info, &config, DeviceKind::Blufi4g).is_ok());
        let mut missing_modem = info.clone();
        missing_modem.modem_version.clear();
        assert!(check_info(&missing_modem, &config, DeviceKind::Blufi4g).is_err());
        assert!(
            check_info(
                &info,
                &Config {
                    version: Some("4.0.31".into()),
                    ..config
                },
                DeviceKind::Blufi4g
            )
            .is_err()
        );
    }
    #[test]
    fn signal_check_uses_strongest_duplicate_ap_and_inclusive_threshold() {
        let networks = vec![
            WifiNetwork {
                ssid: "test".into(),
                rssi: -90,
            },
            WifiNetwork {
                ssid: "test".into(),
                rssi: -60,
            },
        ];
        assert!(
            check_signals(
                &networks,
                &[SignalCheck {
                    ssid: "test".into(),
                    min_rssi: -60
                }]
            )
            .is_ok()
        );
        assert!(
            check_signals(
                &networks,
                &[SignalCheck {
                    ssid: "missing".into(),
                    min_rssi: -80
                }]
            )
            .is_err()
        );
        assert!(
            check_signals(
                &networks,
                &[SignalCheck {
                    ssid: "test".into(),
                    min_rssi: -59
                }]
            )
            .is_err()
        );
    }
}
