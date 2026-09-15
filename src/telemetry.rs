use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use sysinfo::{System, Disks};

#[derive(Clone)]
pub struct SystemMetrics {
    pub current_cpu_load: f32,
    pub current_ram_percent: f32,
    pub ram_info_text: String,
    pub disk_info_text: String,
}

impl SystemMetrics {
    pub fn new() -> Self {
        Self {
            current_cpu_load: 0.0,
            current_ram_percent: 0.0,
            ram_info_text: String::from("RAM: Gathering data..."),
            disk_info_text: String::from("DISK C: Gathering data..."),
        }
    }
}

pub fn spawn_telemetry_daemon(metrics_container: Arc<Mutex<SystemMetrics>>) {
    thread::spawn(move || {
        let mut sys = System::new_all();
        let mut disks = Disks::new_with_refreshed_list();
        loop {
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            disks.refresh_list();

            let cpus = sys.cpus();
            let cpu_load = if !cpus.is_empty() {
                let total_cpu: f32 = cpus.iter().map(|cpu| cpu.cpu_usage()).sum();
                total_cpu / cpus.len() as f32
            } else {
                0.0
            };

            let total_ram = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
            let used_ram = sys.used_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
            let ram_percent = (used_ram / total_ram) * 100.0;

            let ram_text = format!("RAM USAGE: {:.1}GB / {:.1}GB ({:.1}%)", used_ram, total_ram, ram_percent);

            let mut disk_text = String::from("DISK C: NOT DETECTED");
            for disk in disks.list() {
                let mount_point = disk.mount_point().to_string_lossy();
                if mount_point.contains("C:") || mount_point == "/" {
                    let total_disk = disk.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
                    let free_disk = disk.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
                    disk_text = format!("DISK {}: {:.1}GB FREE / {:.1}GB TOTAL", if cfg!(target_os = "windows") { "C" } else { "ROOT" }, free_disk, total_disk);
                    break;
                }
            }

            if let Ok(mut guard) = metrics_container.lock() {
                guard.current_cpu_load = cpu_load;
                guard.current_ram_percent = ram_percent as f32;
                guard.ram_info_text = ram_text;
                guard.disk_info_text = disk_text;
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}
