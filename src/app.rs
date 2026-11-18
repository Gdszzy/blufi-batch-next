use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::mpsc::TryRecvError,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{self, Color32, RichText};

use crate::{backend::Backend, config, model::*};

const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
const GREEN: Color32 = Color32::from_rgb(21, 128, 61);
const RED: Color32 = Color32::from_rgb(185, 28, 28);
const AMBER: Color32 = Color32::from_rgb(161, 98, 7);
const TEXT: Color32 = Color32::from_rgb(30, 41, 59);
const MUTED: Color32 = Color32::from_rgb(100, 116, 139);
const BORDER: Color32 = Color32::from_rgb(222, 229, 238);
const SURFACE: Color32 = Color32::from_rgb(248, 250, 252);
const CANVAS: Color32 = Color32::from_rgb(243, 246, 250);
const MAX_LOGS: usize = 2000;

struct LogEntry {
    time: String,
    message: String,
}

pub struct App {
    backend: Backend,
    config: Config,
    last_run: Option<Config>,
    adapters: Vec<String>,
    rows: Vec<DeviceRow>,
    row_indices: HashMap<String, usize>,
    deleted_rows: HashSet<String>,
    logs: VecDeque<LogEntry>,
    secrets: Vec<String>,
    running: bool,
    stopping: bool,
    connected: bool,
    started_at: Option<Instant>,
    finished_elapsed: Duration,
    selected: Option<String>,
    notice: Option<(bool, String)>,
    draft: Option<Config>,
    remembered: bool,
    password_missing: bool,
    show_logs: bool,
    show_password: bool,
    log_search: String,
    follow_logs: bool,
    font_loaded: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, backend: Backend) -> Self {
        let font_loaded = install_chinese_font(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);
        let (config, password_missing, notice) = match config::load() {
            Ok(saved) => {
                let missing = saved.password_error.is_some();
                let notice = saved
                    .password_error
                    .map(|error| (true, format!("WiFi 密码未恢复：{error}")));
                (saved.config, missing, notice)
            }
            Err(error) => (
                Config::default(),
                false,
                Some((true, format!("读取配置失败：{error:#}"))),
            ),
        };
        let mut app = Self {
            backend,
            config,
            last_run: None,
            adapters: Vec::new(),
            rows: Vec::new(),
            row_indices: HashMap::new(),
            deleted_rows: HashSet::new(),
            logs: VecDeque::new(),
            secrets: Vec::new(),
            running: false,
            stopping: false,
            connected: true,
            started_at: None,
            finished_elapsed: Duration::ZERO,
            selected: None,
            notice,
            draft: None,
            remembered: false,
            password_missing,
            show_logs: false,
            show_password: false,
            log_search: String::new(),
            follow_logs: true,
            font_loaded,
        };
        let initial = app.config.clone();
        app.remember_secrets(&initial);
        app
    }

    fn remember_secrets(&mut self, config: &Config) {
        if let Some(wifi) = &config.wifi {
            for value in [&wifi.ssid, &wifi.password] {
                if !value.is_empty() && !self.secrets.contains(value) {
                    self.secrets.push(value.clone());
                }
            }
        }
        for signal in &config.signals {
            if !signal.ssid.is_empty() && !self.secrets.contains(&signal.ssid) {
                self.secrets.push(signal.ssid.clone());
            }
        }
        // Replace longer values first, so overlapping credentials remain fully hidden.
        self.secrets
            .sort_by_key(|value| std::cmp::Reverse(value.len()));
    }

    fn redact(&self, mut text: String) -> String {
        for secret in &self.secrets {
            text = text.replace(secret, "[已隐藏]");
        }
        text
    }

    fn log(&mut self, message: String) {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if self.logs.len() == MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            time: format!(
                "{:02}:{:02}:{:02}",
                seconds / 3600 % 24,
                seconds / 60 % 60,
                seconds % 60
            ),
            message: self.redact(message),
        });
    }

    fn send(&mut self, command: Command) -> bool {
        if self.backend.commands.send(command).is_err() {
            self.connected = false;
            self.running = false;
            self.stopping = false;
            self.notice = Some((
                true,
                "后台服务已断开。请关闭程序后重新启动；未确认完成的设备不能视为成功。".into(),
            ));
            false
        } else {
            true
        }
    }

    fn events(&mut self) {
        // Keep input responsive even when a large discovery burst arrives.
        for _ in 0..512 {
            match self.backend.events.try_recv() {
                Ok(Event::Log(message)) => self.log(message),
                Ok(Event::Error(message)) => {
                    let message = self.redact(message);
                    self.log(message.clone());
                    self.notice = Some((true, message));
                }
                Ok(Event::Device(mut row)) => {
                    if self.deleted_rows.contains(&row.id) {
                        continue;
                    }
                    row.stage = self.redact(row.stage);
                    if let Some(&index) = self.row_indices.get(&row.id) {
                        self.rows[index] = row;
                    } else {
                        self.row_indices.insert(row.id.clone(), self.rows.len());
                        self.rows.push(row);
                    }
                }
                Ok(Event::Running) => {
                    if !self.running {
                        self.started_at = Some(Instant::now());
                    }
                    self.running = true;
                }
                Ok(Event::BatchStopped) => {
                    self.finish_clock();
                    self.running = false;
                    self.stopping = false;
                }
                Ok(Event::Adapters(adapters)) => self.adapters = adapters,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.connected {
                        self.finish_clock();
                        self.connected = false;
                        self.running = false;
                        self.stopping = false;
                        self.notice
                            .get_or_insert_with(|| (true, "蓝牙后台已退出，请重启".into()));
                    }
                    break;
                }
            }
        }
    }

    fn finish_clock(&mut self) {
        if let Some(start) = self.started_at.take() {
            self.finished_elapsed = start.elapsed();
        }
    }

    fn start(&mut self, config: Config) {
        self.remember_secrets(&config);
        if self.send(Command::Start(config.clone())) {
            self.deleted_rows.clear();
            self.last_run = Some(config);
            self.running = true;
            self.stopping = false;
            self.started_at = Some(Instant::now());
            self.finished_elapsed = Duration::ZERO;
            self.notice = None;
        }
    }

    fn retry(&mut self) {
        let Some(previous) = self.last_run.clone() else {
            return;
        };
        if previous != self.config {
            if let Err(error) = config::save(&previous) {
                self.notice = Some((true, format!("记住配置失败：{error:#}")));
                return;
            }
            self.config = previous;
            self.remembered = true;
            self.password_missing = false;
        }
        if self.send(Command::RetryFailed) {
            self.deleted_rows.clear();
            if !self.running {
                self.started_at = Some(Instant::now());
                self.finished_elapsed = Duration::ZERO;
            }
            self.running = true;
            self.stopping = false;
            for row in &mut self.rows {
                if matches!(row.status, RowStatus::Failed | RowStatus::Cancelled) {
                    row.status = RowStatus::Queued;
                    row.stage = "等待扫描并重新处理".into();
                }
            }
            self.notice = None;
        }
    }

    fn commit(&mut self, config: Config, start: bool) {
        self.remember_secrets(&config);
        if !self.remembered || config != self.config || self.password_missing {
            if let Err(error) = config::save(&config) {
                self.notice = Some((true, format!("记住配置失败：{error:#}")));
                self.draft = Some(config);
                return;
            }
        }
        self.config = config;
        self.remembered = true;
        self.password_missing = false;
        self.draft = None;
        self.notice = None;
        if start {
            self.start(self.config.clone());
        }
    }

    fn apply(&mut self, config: Config, start: bool) {
        if let Err(error) = config.validate() {
            self.notice = Some((true, format!("配置无效：{error}")));
            self.draft = Some(config);
        } else {
            self.commit(config, start);
        }
    }

    fn request_start(&mut self) {
        if self.password_missing {
            self.notice = Some((true, "请先在配置中恢复 WiFi 密码".into()));
            self.draft = Some(self.config.clone());
        } else {
            self.apply(self.config.clone(), true);
        }
    }

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(24, 16)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("BLUFI Batch").size(22.0).strong());
                    ui.add_space(12.0);
                    ui.label(RichText::new(filter_label(self.config.filter)).color(MUTED));
                    let (label, color) = if !self.connected {
                        ("后台离线", RED)
                    } else if self.stopping {
                        ("正在停止", AMBER)
                    } else if self.running {
                        ("运行中", GREEN)
                    } else {
                        ("就绪", MUTED)
                    };
                    badge(ui, label, color);
                    if self.running {
                        ui.label(
                            RichText::new(duration_text(
                                self.started_at
                                    .map_or(self.finished_elapsed, |start| start.elapsed()),
                            ))
                            .monospace()
                            .color(MUTED),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        {
                            let label = if self.stopping {
                                "停止中…"
                            } else if self.running {
                                "停止"
                            } else {
                                "开始"
                            };
                            if primary_button(
                                ui,
                                label,
                                if self.running { RED } else { ACCENT },
                                self.connected && !self.stopping && self.draft.is_none(),
                            )
                            .clicked()
                            {
                                if self.running {
                                    if self.send(Command::Stop) {
                                        self.stopping = true;
                                    }
                                } else {
                                    self.request_start();
                                }
                            }
                            if ui
                                .add_enabled(
                                    !self.running,
                                    egui::Button::new("配置").min_size(egui::vec2(76.0, 38.0)),
                                )
                                .on_hover_text("编辑自动记住的初始化流程")
                                .clicked()
                            {
                                self.show_password = false;
                                self.draft = Some(self.config.clone());
                            }
                            ui.menu_button("更多", |ui| {
                                if ui.button("运行日志").clicked() {
                                    self.show_logs = true;
                                    ui.close();
                                }
                            });
                        };
                    });
                });
                if self.draft.is_none() {
                    if let Some((error, message)) = &self.notice {
                        ui.add_space(12.0);
                        let mut dismiss = false;
                        egui::Frame::new()
                            .fill(tint(if *error { RED } else { GREEN }))
                            .corner_radius(8.0)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_sized(
                                        [(ui.available_width() - 42.0).max(80.0), 26.0],
                                        egui::Label::new(RichText::new(message).color(if *error {
                                            RED
                                        } else {
                                            GREEN
                                        }))
                                        .truncate(),
                                    )
                                    .on_hover_text(message);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            dismiss = ui
                                                .add(egui::Button::new("×").frame(false))
                                                .on_hover_text("关闭提示")
                                                .clicked();
                                        },
                                    );
                                });
                            });
                        if dismiss {
                            self.notice = None;
                        }
                    }
                }
                if !self.font_loaded {
                    ui.colored_label(AMBER, "Install Noto Sans CJK / Microsoft YaHei.");
                }
            });
    }

    fn reindex_rows(&mut self) {
        self.row_indices.clear();
        for (index, row) in self.rows.iter().enumerate() {
            self.row_indices.insert(row.id.clone(), index);
        }
    }

    fn settings(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.draft.take() else {
            return;
        };
        let mut done = false;
        let mut cancel = false;
        let response = dialog(ctx, "settings").show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 96.0).min(900.0).max(420.0));
            done = dialog_header(ui, "初始化配置");
            ui.add_space(10.0);
            {
                egui::ScrollArea::vertical()
                    .id_salt("settings_body")
                    .max_height((ctx.content_rect().height() - 190.0).max(240.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        ui.strong("设备与连接");
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("设备类型");
                            egui::ComboBox::from_id_salt("device_filter")
                                .width(180.0)
                                .selected_text(filter_label(draft.filter))
                                .show_ui(ui, |ui| {
                                    for filter in [
                                        DeviceFilter::Auto,
                                        DeviceFilter::Blufi,
                                        DeviceFilter::Smartcard,
                                    ] {
                                        ui.selectable_value(
                                            &mut draft.filter,
                                            filter,
                                            filter_label(filter),
                                        );
                                    }
                                });
                            ui.add_space(16.0);
                            ui.label("蓝牙适配器");
                            egui::ComboBox::from_id_salt("adapter_filter")
                                .width(220.0)
                                .selected_text(if draft.adapter.is_empty() {
                                    "自动选择"
                                } else {
                                    &draft.adapter
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut draft.adapter,
                                        String::new(),
                                        "自动选择",
                                    );
                                    for adapter in &self.adapters {
                                        ui.selectable_value(
                                            &mut draft.adapter,
                                            adapter.clone(),
                                            adapter,
                                        );
                                    }
                                });
                        });
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.columns(2, |columns| {
                            let ui = &mut columns[0];
                            ui.strong("质量检查");
                            ui.add_space(8.0);
                            optional_text(ui, "固件版本", &mut draft.version, "精确匹配");
                            optional_text(ui, "存储容量", &mut draft.storage, "例如 7.28G");
                            ui.horizontal(|ui| {
                                let mut enabled = draft.min_power.is_some();
                                let mut value = draft.min_power.unwrap_or(20);
                                let toggle = ui
                                    .allocate_ui_with_layout(
                                        egui::vec2(112.0, 34.0),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.set_min_size(egui::vec2(112.0, 34.0));
                                            ui.checkbox(&mut enabled, "最低电量")
                                        },
                                    )
                                    .inner;
                                if toggle.changed() {
                                    draft.min_power = enabled.then_some(value);
                                }
                                ui.add_enabled_ui(enabled, |ui| {
                                    let changed = ui
                                        .scope(|ui| {
                                            ui.spacing_mut().slider_width =
                                                (ui.available_width() - 80.0).max(64.0);
                                            ui.visuals_mut().selection.bg_fill = ACCENT;
                                            ui.visuals_mut().widgets.inactive.fg_stroke =
                                                egui::Stroke::new(1.5_f32, ACCENT);
                                            ui.add(
                                                egui::Slider::new(&mut value, 0..=100)
                                                    .show_value(false)
                                                    .trailing_fill(true),
                                            )
                                        })
                                        .inner
                                        .changed();
                                    let edited = ui
                                        .add_sized(
                                            [60.0, 34.0],
                                            egui::DragValue::new(&mut value)
                                                .range(0..=100)
                                                .speed(1.0)
                                                .suffix(" %"),
                                        )
                                        .changed();
                                    if changed || edited {
                                        draft.min_power = Some(value);
                                    }
                                });
                            });
                            optional_text(
                                ui,
                                "Modem 版本",
                                &mut draft.modem_version,
                                "4G / SA02A1",
                            );
                            ui.add_space(8.0);
                            ui.label("工厂固件白名单（每行一个版本）");
                            ui.add(
                                egui::TextEdit::multiline(&mut draft.factory_firmware_whitelist)
                                    .desired_rows(3)
                                    .desired_width(ui.available_width())
                                    .id_salt("factory_firmware_whitelist")
                                    .hint_text(RichText::new("4.1.0\n4.1.1").color(MUTED)),
                            );
                            ui.label(
                                RichText::new("白名单命中后，才允许执行工厂固件专属操作")
                                .small()
                                .color(MUTED),
                            );
                            let ui = &mut columns[1];
                            ui.strong("初始化动作");
                            ui.add_space(8.0);
                            egui::Grid::new("operations")
                                .num_columns(2)
                                .min_col_width(140.0)
                                .spacing([16.0, 12.0])
                                .show(ui, |ui| {
                                    ui.checkbox(&mut draft.activate, "激活设备")
                                        .on_hover_text("仅旧 WiFi BLUFI");
                                    ui.checkbox(&mut draft.sync_time, "同步时间");
                                    ui.end_row();
                                    ui.checkbox(&mut draft.factory_reset, "恢复出厂")
                                        .on_hover_text("删除配置和本地文件");
                                    ui.checkbox(&mut draft.clear_data, "格式化存储")
                                        .on_hover_text("删除全部存储数据");
                                    ui.end_row();
                                    ui.checkbox(&mut draft.clear_wifi, "清除 WiFi");
                                    ui.end_row();
                                });
                        });
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.strong("工厂固件专属操作");
                        ui.label(
                            RichText::new(
                                "仅设备固件版本命中白名单时执行；填写 Modem 版本后，SMCE 4G 校验通过会进行 SN/ICCID 绑定。",
                            )
                            .small()
                            .color(MUTED),
                        );
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut draft.sync_sn, "自动分配 SN")
                                .on_hover_text("仅白名单内 SMCE 4G 的出厂占位号会分配正式 SN");
                            ui.add_space(24.0);
                            ui.checkbox(&mut draft.upgrade, "触发固件升级")
                                .on_hover_text("仅白名单内的 SMCE 4G 会发送升级命令");
                            ui.add_space(24.0);
                            ui.checkbox(&mut draft.factory_test_check, "校验出厂测试")
                                .on_hover_text("SN 分配前查询出厂测试记录，未通过或无记录则停止");
                        });
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);
                        let mut enabled = draft.wifi.is_some();
                        if ui
                            .checkbox(&mut enabled, RichText::new("WiFi 配网").strong())
                            .changed()
                        {
                            draft.wifi = enabled.then(|| WifiConfig {
                                ssid: String::new(),
                                password: String::new(),
                                attempts: 3,
                            });
                        }
                        let mut inactive_wifi = WifiConfig {
                            ssid: String::new(),
                            password: String::new(),
                            attempts: 3,
                        };
                        let wifi = draft.wifi.as_mut().unwrap_or(&mut inactive_wifi);
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.add_space(6.0);
                            let width = (ui.available_width() - 100.0).max(180.0);
                            egui::Grid::new("wifi_settings")
                                .num_columns(2)
                                .min_col_width(76.0)
                                .spacing([12.0, 6.0])
                                .show(ui, |ui| {
                                    ui.label("网络名称");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut wifi.ssid)
                                            .min_size(egui::vec2(0.0, 32.0))
                                            .vertical_align(egui::Align::Center)
                                            .id_salt("wifi_ssid")
                                            .desired_width(width)
                                            .hint_text(RichText::new("SSID").color(MUTED)),
                                    );
                                    ui.end_row();
                                    ui.label("密码").on_hover_text("自动记入系统密码库");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut wifi.password)
                                                .min_size(egui::vec2(0.0, 32.0))
                                                .vertical_align(egui::Align::Center)
                                                .id_salt("wifi_password")
                                                .password(!self.show_password)
                                                .desired_width(width - 64.0)
                                                .hint_text(
                                                    RichText::new("开放网络可留空").color(MUTED),
                                                ),
                                        );
                                        if ui
                                            .add_sized(
                                                [54.0, 32.0],
                                                egui::Button::new(if self.show_password {
                                                    "隐藏"
                                                } else {
                                                    "显示"
                                                }),
                                            )
                                            .clicked()
                                        {
                                            self.show_password = !self.show_password;
                                        }
                                    });
                                    ui.end_row();
                                    ui.label("尝试次数");
                                    ui.add(
                                        egui::DragValue::new(&mut wifi.attempts)
                                            .range(1..=10)
                                            .suffix(" 次"),
                                    );
                                    ui.end_row();
                                });
                        });
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.strong("WiFi 信号检查");
                        ui.label(
                            RichText::new("最多 5 组；填写 SSID 的行才会参与检查，空行会忽略")
                                .small()
                                .color(MUTED),
                        );
                        let mut signal_slots = draft.signals.clone();
                        signal_slots.resize_with(5, || SignalCheck {
                            ssid: String::new(),
                            min_rssi: -75,
                        });
                        for (index, signal) in signal_slots.iter_mut().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal(|ui| {
                                    let ssid_width =
                                        (ui.available_width() - 130.0).clamp(120.0, 360.0);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut signal.ssid)
                                            .min_size(egui::vec2(0.0, 32.0))
                                            .vertical_align(egui::Align::Center)
                                            .id_salt("signal_ssid")
                                            .hint_text(
                                                RichText::new(format!("SSID {}", index + 1))
                                                    .color(MUTED),
                                            )
                                            .desired_width(ssid_width),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut signal.min_rssi)
                                            .range(-127..=0)
                                            .suffix(" dBm"),
                                    );
                                });
                            });
                        }
                        draft.signals = signal_slots;
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.strong("连接参数");
                        ui.columns(2, |columns| {
                            columns[0].add(
                                egui::Slider::new(&mut draft.min_rssi, -127..=0)
                                    .text("最低蓝牙信号")
                                    .suffix(" dBm"),
                            );
                            columns[1].add(
                                egui::Slider::new(&mut draft.concurrency, 1..=8).text("并发设备数"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("连接后等待设备初始化");
                            ui.add(
                                egui::DragValue::new(&mut draft.info_delay_seconds)
                                    .range(0..=60)
                                    .suffix(" 秒"),
                            );
                            ui.label(
                                RichText::new("等待结束后读取设备信息；0 秒表示不等待")
                                    .small()
                                    .color(MUTED),
                            );
                        });
                    });
                if let Some((true, message)) = &self.notice {
                    ui.add_space(10.0);
                    ui.colored_label(RED, message);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        done |= primary_button(ui, "完成", ACCENT, true).clicked();
                        cancel = ui
                            .add_sized([80.0, 38.0], egui::Button::new("取消"))
                            .clicked();
                    });
                });
            };
        });
        if response.is_top_modal
            && !response.any_popup_open
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            cancel = true;
        }
        if cancel {
            self.show_password = false;
            return;
        }
        if done {
            self.show_password = false;
            self.apply(draft, false);
        } else {
            self.draft = Some(draft);
        }
    }

    fn logs_panel(&mut self, ctx: &egui::Context) {
        if !self.show_logs {
            return;
        }
        let mut close = false;
        let response = dialog(ctx, "logs").show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 96.0).min(800.0).max(420.0));
            close = dialog_header(ui, "运行日志");
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.log_search)
                        .min_size(egui::vec2(0.0, 34.0))
                        .vertical_align(egui::Align::Center)
                        .id_salt("log_search")
                        .hint_text(RichText::new("搜索日志").color(MUTED))
                        .desired_width((ui.available_width() - 220.0).max(160.0)),
                );
                ui.checkbox(&mut self.follow_logs, "跟随最新");
                if ui.button("清空").clicked() {
                    self.logs.clear();
                }
            });
            ui.add_space(12.0);
            egui::Frame::new()
                .fill(SURFACE)
                .stroke(egui::Stroke::new(1.0_f32, BORDER))
                .corner_radius(8.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("log_body")
                        .max_height((ctx.content_rect().height() - 340.0).clamp(100.0, 320.0))
                        .auto_shrink([false, false])
                        .stick_to_bottom(self.follow_logs)
                        .show(ui, |ui| {
                            if self.logs.is_empty() {
                                ui.label(RichText::new("暂无日志").color(MUTED));
                            }
                            for entry in &self.logs {
                                if !contains_folded(&entry.message, &self.log_search) {
                                    continue;
                                }
                                ui.horizontal_top(|ui| {
                                    ui.label(
                                        RichText::new(&entry.time).monospace().small().color(MUTED),
                                    );
                                    ui.add(egui::Label::new(&entry.message).wrap());
                                });
                            }
                        });
                });
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui
                    .add_sized([80.0, 38.0], egui::Button::new("关闭"))
                    .clicked();
            });
        });
        if close || response.should_close() {
            self.show_logs = false;
        }
    }

    fn workspace(&mut self, ctx: &egui::Context) {
        let mut retry_all = false;
        let mut clear_success = false;
        let mut retry_device = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(CANVAS).inner_margin(16.0))
            .show(ctx, |ui| {
                let height = ui.available_height();
                ui.spacing_mut().item_spacing.x = 12.0;
                ui.columns(3, |columns| {
                    for (group, ui) in columns.iter_mut().enumerate() {
                        let matches = |row: &&DeviceRow| {
                            row.kind
                                .is_some_and(|kind| self.config.filter.accepts(kind))
                                && result_group(row.status) == group
                        };
                        let count = self.rows.iter().filter(matches).count();
                        let (title, color, empty) = [
                            ("处理中", ACCENT, "等待设备"),
                            ("成功", GREEN, "暂无完成设备"),
                            ("异常", RED, "暂无异常"),
                        ][group];
                        egui::Frame::new()
                            .fill(Color32::WHITE)
                            .stroke(egui::Stroke::new(1.0_f32, BORDER))
                            .corner_radius(10.0)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.set_min_height((height - 22.0).max(180.0));
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.spacing_mut().interact_size.y = 24.0;
                                ui.horizontal(|ui| {
                                    let (dot, _) = ui.allocate_exact_size(
                                        egui::vec2(6.0, 6.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(dot.center(), 3.0, color);
                                    ui.label(RichText::new(title).size(15.0).strong());
                                    if group == 2 {
                                        let has_retryable = self.rows.iter().any(|row| {
                                            matches!(
                                                row.status,
                                                RowStatus::Failed | RowStatus::Cancelled
                                            )
                                        });
                                        if ui
                                            .add_enabled(
                                                has_retryable
                                                    && !self.stopping
                                                    && self.connected
                                                    && self.last_run.is_some(),
                                                egui::Button::new("重试全部"),
                                            )
                                            .clicked()
                                        {
                                            retry_all = true;
                                        }
                                    } else if group == 1 {
                                        let has_clearable = self.rows.iter().any(|row| {
                                            matches!(
                                                row.status,
                                                RowStatus::Success | RowStatus::NeedsCheck
                                            )
                                        });
                                        if ui
                                            .add_enabled(
                                                has_clearable && !self.stopping && self.connected,
                                                egui::Button::new("清理成功"),
                                            )
                                            .on_hover_text("清除成功和待检查记录")
                                            .clicked()
                                        {
                                            clear_success = true;
                                        }
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(count.to_string())
                                                    .size(14.0)
                                                    .strong()
                                                    .color(color),
                                            );
                                        },
                                    );
                                });
                                ui.add_space(4.0);
                                ui.separator();
                                ui.add_space(4.0);
                                egui::ScrollArea::vertical()
                                    .id_salt(("results", group))
                                    .max_height((height - 70.0).max(80.0))
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 0.0;
                                        for (index, row) in
                                            self.rows.iter().filter(matches).enumerate()
                                        {
                                            let selected =
                                                self.selected.as_deref() == Some(&row.id);
                                            let background = if selected {
                                                tint(ACCENT)
                                            } else if index % 2 == 0 {
                                                SURFACE
                                            } else {
                                                Color32::WHITE
                                            };
                                            let line = egui::Frame::new()
                                                .fill(background)
                                                .corner_radius(3.0)
                                                .inner_margin(egui::Margin::symmetric(6, 0))
                                                .show(ui, |ui| {
                                                    ui.set_min_width(
                                                        (ui.available_width() - 1.0).max(30.0),
                                                    );
                                                    ui.spacing_mut().item_spacing =
                                                        egui::vec2(6.0, 0.0);
                                                    ui.horizontal(|ui| {
                                                        if row.status == RowStatus::Running {
                                                            ui.add(
                                                                egui::Spinner::new()
                                                                    .size(12.0)
                                                                    .color(ACCENT),
                                                            );
                                                        }
                                                        let name_width = (ui.available_width()
                                                            * 0.38)
                                                            .clamp(80.0, 116.0);
                                                        list_cell(
                                                            ui,
                                                            RichText::new(nonempty(&row.name))
                                                                .size(14.0),
                                                            name_width,
                                                        );
                                                        let text = match row.status {
                                                            RowStatus::Success => row
                                                                .info
                                                                .as_ref()
                                                                .map_or("完成", |info| {
                                                                    nonempty(&info.sn)
                                                                }),
                                                            RowStatus::Cancelled
                                                            | RowStatus::Skipped => {
                                                                row.status.label()
                                                            }
                                                            _ => &row.stage,
                                                        };
                                                        let text_color = if group == 2
                                                            || row.status == RowStatus::NeedsCheck
                                                        {
                                                            status_color(row.status)
                                                        } else {
                                                            MUTED
                                                        };
                                                        let stage_width =
                                                            (ui.available_width() - 42.0).max(24.0);
                                                        list_cell(
                                                            ui,
                                                            RichText::new(text)
                                                                .size(13.0)
                                                                .color(text_color),
                                                            stage_width,
                                                        );
                                                        ui.with_layout(
                                                            egui::Layout::right_to_left(
                                                                egui::Align::Center,
                                                            ),
                                                            |ui| {
                                                                ui.label(
                                                                    RichText::new(duration_text(
                                                                        Duration::from_millis(
                                                                            row.elapsed_ms,
                                                                        ),
                                                                    ))
                                                                    .monospace()
                                                                    .size(11.0)
                                                                    .color(MUTED),
                                                                );
                                                            },
                                                        );
                                                    });
                                                });
                                            let response = line
                                                .response
                                                .interact(egui::Sense::click())
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .on_hover_ui(|ui| {
                                                    ui.label(RichText::new(&row.name).strong());
                                                    ui.label(&row.id);
                                                    ui.add(egui::Label::new(&row.stage).wrap());
                                                    if matches!(
                                                        row.status,
                                                        RowStatus::Failed | RowStatus::Cancelled
                                                    ) {
                                                        ui.label("右键点击立即重试此设备");
                                                    }
                                                });
                                            let retryable = matches!(
                                                row.status,
                                                RowStatus::Failed | RowStatus::Cancelled
                                            );
                                            let secondary_clicked = ctx.input(|input| {
                                                input
                                                    .pointer
                                                    .button_clicked(egui::PointerButton::Secondary)
                                            });
                                            if retryable
                                                && response.hovered()
                                                && secondary_clicked
                                                && self.connected
                                                && !self.stopping
                                                && self.last_run.is_some()
                                            {
                                                retry_device = Some(row.id.clone());
                                            }
                                            if response.hovered() {
                                                ui.painter().rect_stroke(
                                                    response.rect,
                                                    3.0,
                                                    egui::Stroke::new(1.0_f32, ACCENT),
                                                    egui::StrokeKind::Inside,
                                                );
                                            }
                                            if response.clicked() {
                                                self.selected = Some(row.id.clone());
                                            }
                                            ui.add_space(2.0);
                                        }
                                        if count == 0 {
                                            ui.add_space(24.0);
                                            ui.vertical_centered(|ui| {
                                                ui.label(RichText::new(empty).small().color(MUTED));
                                            });
                                        }
                                    });
                            });
                    }
                });
            });
        if retry_all {
            self.retry();
        }
        if clear_success && self.send(Command::ForgetSuccess) {
            self.rows
                .retain(|row| !matches!(row.status, RowStatus::Success | RowStatus::NeedsCheck));
            self.reindex_rows();
        }
        if let Some(id) = retry_device {
            if self.send(Command::RetryDevice(id.clone())) {
                if let Some(index) = self.row_indices.get(&id).copied() {
                    if let Some(row) = self.rows.get_mut(index) {
                        row.status = RowStatus::Queued;
                        row.stage = "等待扫描并重新处理".into();
                    }
                }
            }
        }
    }

    fn details(&mut self, ctx: &egui::Context) {
        let Some(id) = &self.selected else {
            return;
        };
        let Some(row) = self
            .row_indices
            .get(id)
            .and_then(|index| self.rows.get(*index))
        else {
            self.selected = None;
            return;
        };
        let mut close = false;
        let mut delete = false;
        let response = dialog(ctx, "device_details").show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 96.0).min(540.0).max(360.0));
            close = dialog_header(ui, "设备详情");
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(nonempty(&row.name)).size(18.0).strong());
                badge(ui, row.status.label(), status_color(row.status));
            });
            ui.add_space(16.0);
            egui::ScrollArea::vertical()
                .id_salt("device_detail_body")
                .max_height((ctx.content_rect().height() - 300.0).max(160.0))
                .show(ui, |ui| {
                    ui.spacing_mut().interact_size.y = 24.0;
                    egui::Grid::new("detail_grid")
                        .num_columns(2)
                        .spacing([24.0, 8.0])
                        .show(ui, |ui| {
                            detail(ui, "蓝牙标识", &row.id);
                            detail(ui, "设备类型", row.kind.map_or("待识别", DeviceKind::label));
                            detail(
                                ui,
                                "蓝牙信号",
                                &row.rssi
                                    .map_or_else(|| "—".into(), |rssi| format!("{rssi} dBm")),
                            );
                            detail(
                                ui,
                                "耗时",
                                &duration_text(Duration::from_millis(row.elapsed_ms)),
                            );
                            if let Some(info) = &row.info {
                                detail(ui, "SN", nonempty(&info.sn));
                                detail(ui, "固件版本", nonempty(&info.firmware));
                                detail(
                                    ui,
                                    "存储容量",
                                    &format!(
                                        "{}（{} 字节）",
                                        crate::backend::format_storage(info.storage_total),
                                        info.storage_total
                                    ),
                                );
                                detail(ui, "电量", &format!("{}%", info.battery));
                                detail(ui, "Modem 版本", nonempty(&info.modem_version));
                                detail(ui, "ICCID", nonempty(&info.iccid));
                            }
                        });
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_space(10.0);
                    ui.label(RichText::new("执行步骤（按发生顺序）").small().color(MUTED));
                    if row.operations.is_empty() {
                        ui.label(RichText::new("尚未开始设备操作").color(MUTED));
                    } else {
                        for (index, operation) in row.operations.iter().enumerate() {
                            ui.horizontal_top(|ui| {
                                ui.label(
                                    RichText::new(format!("{:02}.", index + 1))
                                        .monospace()
                                        .color(MUTED),
                                );
                                ui.add(egui::Label::new(operation).wrap().selectable(true));
                            });
                        }
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(10.0);
                    ui.label(RichText::new("当前状态").small().color(MUTED));
                    ui.add(egui::Label::new(&row.stage).wrap().selectable(true));
                });
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui
                    .add_sized([80.0, 38.0], egui::Button::new("关闭"))
                    .clicked();
                if !matches!(row.status, RowStatus::Queued | RowStatus::Running) {
                    delete |= ui
                        .add_sized(
                            [80.0, 38.0],
                            egui::Button::new(RichText::new("删除").color(RED)),
                        )
                        .clicked();
                }
            });
        });
        if delete {
            let id = id.clone();
            self.rows.retain(|row| row.id != id);
            self.deleted_rows.insert(id);
            self.reindex_rows();
            self.selected = None;
        } else if close || response.should_close() {
            self.selected = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.events();
        self.header(ctx);
        self.workspace(ctx);
        self.settings(ctx);
        self.logs_panel(ctx);
        self.details(ctx);
        // The std channel does not wake egui; poll without busy-spinning or blocking BLE on the UI thread.
        ctx.request_repaint_after(Duration::from_millis(if self.running { 100 } else { 300 }));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.backend.commands.send(Command::Shutdown);
    }
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(
        egui::viewport::SystemTheme::Light,
    ));
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    let visuals = &mut style.visuals;
    visuals.panel_fill = Color32::WHITE;
    visuals.window_fill = Color32::WHITE;
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.faint_bg_color = SURFACE;
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.window_stroke = egui::Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = egui::CornerRadius::same(14);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(28),
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(22),
    };
    visuals.selection.bg_fill = Color32::from_rgb(219, 234, 254);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.text_cursor.stroke = egui::Stroke::new(1.5_f32, ACCENT);
    visuals.disabled_alpha = 0.45;
    visuals.handle_shape = egui::style::HandleShape::Circle;
    visuals.slider_trailing_fill = true;
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(3);
        widget.fg_stroke = egui::Stroke::new(1.0_f32, TEXT);
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER);
    visuals.widgets.noninteractive.bg_fill = BORDER;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(226, 232, 240);
    visuals.widgets.inactive.weak_bg_fill = Color32::WHITE;
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0_f32, Color32::from_rgb(203, 213, 225));
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(239, 246, 255);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(239, 246, 255);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.bg_fill = Color32::from_rgb(219, 234, 254);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(219, 234, 254);
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.open = visuals.widgets.hovered;
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 34.0;
    style.spacing.slider_width = 140.0;
    style.spacing.slider_rail_height = 4.0;
    style.spacing.menu_margin = egui::Margin::same(8);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
    ctx.set_style(style);
}

fn install_chinese_font(ctx: &egui::Context) -> bool {
    let mut paths = Vec::new();
    if let Some(windows) = std::env::var_os("WINDIR") {
        let fonts = PathBuf::from(windows).join("Fonts");
        for name in ["msyh.ttc", "msyh.ttf", "simhei.ttf", "simsun.ttc"] {
            paths.push(fonts.join(name));
        }
    }
    // Prefer regular static CJK faces: variable collections may default to a
    // thin master in the GUI rasterizer instead of the desktop's selected weight.
    for path in [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
        "/System/Library/Fonts/PingFang.ttc",
    ] {
        paths.push(PathBuf::from(path));
    }
    if load_chinese_font(ctx, &paths) {
        return true;
    }
    paths.clear();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        find_cjk_fonts(&home.join(".local/share/fonts"), 0, &mut paths);
        find_cjk_fonts(&home.join(".fonts"), 0, &mut paths);
    }
    find_cjk_fonts(Path::new("/usr/local/share/fonts"), 0, &mut paths);
    find_cjk_fonts(Path::new("/usr/share/fonts"), 0, &mut paths);
    load_chinese_font(ctx, &paths)
}

fn load_chinese_font(ctx: &egui::Context, paths: &[PathBuf]) -> bool {
    for path in paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "system_cjk".into(),
            egui::FontData::from_owned(bytes).into(),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("system_cjk".into());
        }
        ctx.set_fonts(fonts);
        return true;
    }
    false
}

fn find_cjk_fonts(directory: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            find_cjk_fonts(&entry.path(), depth + 1, output);
        } else if kind.is_file() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if (name.contains("notosanscjk")
                || name.contains("notosanssc")
                || name.contains("droidsansfallback")
                || name.starts_with("wqy-")
                || name == "msyh.ttc"
                || name == "simhei.ttf")
                && (name.ends_with(".ttf") || name.ends_with(".ttc") || name.ends_with(".otf"))
            {
                output.push(entry.path());
            }
        }
    }
}

fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>, hint: &str) {
    ui.horizontal(|ui| {
        let mut enabled = value.is_some();
        let toggle = ui
            .allocate_ui_with_layout(
                egui::vec2(112.0, 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_size(egui::vec2(112.0, 34.0));
                    ui.checkbox(&mut enabled, label)
                },
            )
            .inner;
        if toggle.changed() {
            *value = enabled.then(String::new);
        }
        let mut empty = String::new();
        let text = value.as_mut().unwrap_or(&mut empty);
        ui.add_enabled(
            enabled,
            egui::TextEdit::singleline(text)
                .min_size(egui::vec2(0.0, 34.0))
                .vertical_align(egui::Align::Center)
                .id_salt(("check", label))
                .desired_width((ui.available_width() - 4.0).max(100.0))
                .hint_text(RichText::new(hint).color(MUTED)),
        );
    });
}

fn detail(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).color(MUTED));
    ui.add(egui::Label::new(value).selectable(true).wrap());
    ui.end_row();
}

fn nonempty(value: &str) -> &str {
    if value.is_empty() { "—" } else { value }
}

fn filter_label(filter: DeviceFilter) -> &'static str {
    match filter {
        DeviceFilter::Auto => "自动选择（全部设备）",
        DeviceFilter::Blufi => "BLUFI（WiFi / 4G）",
        DeviceFilter::Smartcard => "Smartcard（SA01A2 / SA02A1）",
    }
}

fn result_group(status: RowStatus) -> usize {
    match status {
        RowStatus::Queued | RowStatus::Running => 0,
        RowStatus::Success | RowStatus::NeedsCheck => 1,
        RowStatus::Failed | RowStatus::Cancelled | RowStatus::Skipped => 2,
    }
}

fn status_color(status: RowStatus) -> Color32 {
    match status {
        RowStatus::Queued => MUTED,
        RowStatus::Running => ACCENT,
        RowStatus::Success => GREEN,
        RowStatus::Failed => RED,
        RowStatus::NeedsCheck | RowStatus::Cancelled | RowStatus::Skipped => AMBER,
    }
}
fn tint(color: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 18)
}

fn badge(ui: &mut egui::Ui, label: &str, color: Color32) {
    egui::Frame::new()
        .fill(tint(color))
        .corner_radius(16.0)
        .inner_margin(egui::Margin::symmetric(9, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(13.0).strong().color(color));
        });
}

fn list_cell(ui: &mut egui::Ui, text: RichText, width: f32) -> egui::Response {
    ui.allocate_ui_with_layout(
        egui::vec2(width, 24.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(egui::vec2(width, 24.0));
            ui.add(egui::Label::new(text).truncate())
        },
    )
    .inner
}

fn primary_button(ui: &mut egui::Ui, label: &str, color: Color32, enabled: bool) -> egui::Response {
    ui.scope(|ui| {
        let darker = |amount: u8| {
            Color32::from_rgb(
                color.r().saturating_sub(amount),
                color.g().saturating_sub(amount),
                color.b().saturating_sub(amount),
            )
        };
        let widgets = &mut ui.visuals_mut().widgets;
        widgets.noninteractive.weak_bg_fill = color;
        widgets.inactive.weak_bg_fill = color;
        widgets.hovered.weak_bg_fill = darker(12);
        widgets.active.weak_bg_fill = darker(24);
        for widget in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            widget.bg_stroke = egui::Stroke::NONE;
            widget.corner_radius = egui::CornerRadius::same(8);
        }
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                .min_size(egui::vec2(96.0, 38.0)),
        )
    })
    .inner
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn dialog(ctx: &egui::Context, id: &'static str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(id))
        .backdrop_color(Color32::from_black_alpha(44))
        .frame(egui::Frame::popup(&ctx.style()).inner_margin(24.0))
}

fn dialog_header(ui: &mut egui::Ui, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(20.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = ui
                .add(
                    egui::Button::new(RichText::new("×").size(20.0).color(MUTED))
                        .frame(false)
                        .min_size(egui::vec2(28.0, 28.0)),
                )
                .on_hover_text("关闭")
                .clicked();
        });
    });
    close
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn contains_folded(value: &str, query: &str) -> bool {
    query.is_empty()
        || value
            .as_bytes()
            .windows(query.len())
            .any(|window| window.eq_ignore_ascii_case(query.as_bytes()))
}
