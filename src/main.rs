#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod app;
mod backend;
mod blufi;
mod config;
mod model;
mod rpc;

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--diagnose") {
        return tokio::runtime::Runtime::new()?.block_on(backend::diagnose());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("BLUFI Batch · 设备批量初始化")
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "BLUFI Batch",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, backend::Backend::spawn())))),
    )
    .map_err(|error| anyhow::anyhow!("无法启动原生界面: {error}"))
}
