use crate::model::{Config, DeviceRow};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn directory() -> Result<PathBuf> {
    let dirs =
        ProjectDirs::from("com", "szzy", "blufi-batch-next").context("无法确定用户配置目录")?;
    let dir = dirs.config_dir().to_path_buf();
    fs::create_dir_all(&dir).context("无法创建配置目录")?;
    Ok(dir)
}
#[derive(Serialize, Deserialize, Default)]
struct SavedSettings {
    #[serde(flatten)]
    config: Config,
    #[serde(default)]
    wifi_password_saved: bool,
}

pub struct LoadedSettings {
    pub config: Config,
    pub password_error: Option<String>,
}

fn password_entry() -> Result<keyring::v1::Entry> {
    let account = directory()?.to_string_lossy().into_owned();
    keyring::v1::Entry::new("blufi-batch-next", &account).context("无法打开系统密码库")
}

pub fn load() -> Result<LoadedSettings> {
    let path = directory()?.join("settings.json");
    let mut saved: SavedSettings = match fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).context("配置文件无效")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SavedSettings::default(),
        Err(error) => return Err(error).context("无法读取配置"),
    };
    let mut password_error = None;
    if saved.wifi_password_saved {
        if let Some(wifi) = &mut saved.config.wifi {
            match password_entry()
                .and_then(|entry| entry.get_password().context("无法读取 WiFi 密码"))
            {
                Ok(password) => wifi.password = password,
                Err(error) => password_error = Some(format!("{error:#}")),
            }
        }
    }
    Ok(LoadedSettings {
        config: saved.config,
        password_error,
    })
}

pub fn save(config: &Config) -> Result<PathBuf> {
    config.validate()?;
    let path = directory()?.join("settings.json");
    let temp = path.with_extension("json.tmp");
    let wifi_password_saved = config
        .wifi
        .as_ref()
        .is_some_and(|wifi| !wifi.password.is_empty());
    let previous_had_password = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SavedSettings>(&bytes).ok())
        .is_some_and(|saved| saved.wifi_password_saved);
    if wifi_password_saved {
        password_entry()?
            .set_password(&config.wifi.as_ref().unwrap().password)
            .context("无法记住 WiFi 密码，请解锁系统密码库后重试")?;
    }
    let saved = SavedSettings {
        config: config.clone(),
        wifi_password_saved,
    };
    let bytes = serde_json::to_vec_pretty(&saved)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(&temp).context("无法写入配置临时文件")?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    // Same-directory rename replaces the previous file on Unix and Windows;
    // do not unlink the old settings before committing the replacement.
    fs::rename(&temp, &path).context("无法保存配置")?;
    if previous_had_password && !wifi_password_saved {
        password_entry()?
            .delete_credential()
            .context("配置已保存，但旧 WiFi 密码未能清除")?;
    }
    Ok(path)
}
#[allow(dead_code)] // Retained for a future report entry point; the current UI exposes logs only.
pub fn export(rows: &[DeviceRow]) -> Result<PathBuf> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let dir = directory()?.join("reports");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("batch-{millis}.csv"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    use std::io::Write;
    file.write_all(&[0xef, 0xbb, 0xbf])?; // Excel UTF-8 detection.
    let mut writer = csv::Writer::from_writer(file);
    writer.write_record([
        "设备ID",
        "名称",
        "类型",
        "蓝牙RSSI",
        "结果",
        "阶段/原因",
        "SN",
        "固件",
        "容量字节",
        "电量%",
        "Modem版本",
        "ICCID",
        "耗时毫秒",
    ])?;
    for row in rows {
        let info = row.info.as_ref();
        let fields = [
            row.id.clone(),
            row.name.clone(),
            row.kind.map(|k| k.label()).unwrap_or("待识别").to_owned(),
            row.rssi.map(|v| v.to_string()).unwrap_or_default(),
            row.status.label().to_owned(),
            row.stage.clone(),
            info.map(|i| i.sn.clone()).unwrap_or_default(),
            info.map(|i| i.firmware.clone()).unwrap_or_default(),
            info.map(|i| i.storage_total.to_string())
                .unwrap_or_default(),
            info.map(|i| i.battery.to_string()).unwrap_or_default(),
            info.map(|i| i.modem_version.clone()).unwrap_or_default(),
            info.map(|i| i.iccid.clone()).unwrap_or_default(),
            row.elapsed_ms.to_string(),
        ];
        writer.write_record(fields.iter().map(|s| spreadsheet_safe(s)))?;
    }
    writer.flush()?;
    Ok(path)
}
#[allow(dead_code)]
fn spreadsheet_safe(value: &str) -> String {
    // Device-provided strings must not become executable spreadsheet formulas.
    if value
        .trim_start()
        .starts_with(['=', '+', '-', '@', '\t', '\r', '\n'])
    {
        format!("'{value}")
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WifiConfig;

    #[test]
    fn saved_profile_never_contains_wifi_password() {
        let config = Config {
            wifi: Some(WifiConfig {
                ssid: "production-network".into(),
                password: "private-secret-123".into(),
                attempts: 3,
            }),
            ..Default::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        assert!(!encoded.contains("private-secret-123"));
        let decoded: Config = serde_json::from_str(&encoded).unwrap();
        let wifi = decoded.wifi.unwrap();
        assert_eq!(wifi.ssid, "production-network");
        assert!(wifi.password.is_empty());
    }

    #[test]
    fn old_device_filters_migrate_to_new_categories() {
        assert_eq!(Config::default().filter, crate::model::DeviceFilter::Auto);
        let config: Config = serde_json::from_str(r#"{"filter":"All"}"#).unwrap();
        assert_eq!(config.filter, crate::model::DeviceFilter::Auto);
        for old_filter in ["Wifi", "Blufi4g"] {
            let config: Config =
                serde_json::from_str(&format!(r#"{{"filter":"{old_filter}"}}"#)).unwrap();
            assert_eq!(config.filter, crate::model::DeviceFilter::Blufi);
        }
        for old_filter in ["Sa01a2", "Sa02a1"] {
            let config: Config =
                serde_json::from_str(&format!(r#"{{"filter":"{old_filter}"}}"#)).unwrap();
            assert_eq!(config.filter, crate::model::DeviceFilter::Smartcard);
        }
    }

    #[test]
    fn factory_firmware_whitelist_and_test_check_are_persisted() {
        let config = Config {
            factory_test_check: true,
            ..Config::default()
        };
        let encoded = serde_json::to_string(&config).unwrap();
        let decoded: Config = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.factory_firmware_whitelist, "4.1.0\n4.1.1");
        assert!(decoded.factory_test_check);
        assert_eq!(decoded.info_delay_seconds, 5);
        assert!(encoded.contains("4.1.0\\n4.1.1"));
    }

    #[test]
    fn initialization_delay_is_configurable_from_zero_to_sixty_seconds() {
        assert_eq!(Config::default().info_delay_seconds, 5);
        assert!(
            Config {
                info_delay_seconds: 0,
                ..Config::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            Config {
                info_delay_seconds: 60,
                ..Config::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            Config {
                info_delay_seconds: 61,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn empty_wifi_signal_slots_are_valid_and_ignored() {
        let config = Config {
            signals: vec![
                crate::model::SignalCheck {
                    ssid: String::new(),
                    min_rssi: -75,
                };
                5
            ],
            ..Config::default()
        };
        assert!(config.validate().is_ok());
        assert!(config.signals.iter().all(|signal| signal.ssid.is_empty()));
    }

    #[test]
    fn device_strings_cannot_inject_spreadsheet_formulas() {
        for formula in [
            "=HYPERLINK(\"https://invalid\")",
            " +SUM(1,2)",
            "@cmd",
            "-1+2",
            "\t=1+1",
        ] {
            assert!(spreadsheet_safe(formula).starts_with('\''));
        }
        assert_eq!(spreadsheet_safe("4S1234EC1234567890"), "4S1234EC1234567890");
    }
}
