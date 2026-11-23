#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::{self, OpenOptions},
    io::Write,
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const APP_NAME: &str = "Project Stratagem";
const BG: Color32 = Color32::from_rgb(13, 17, 24);
const PANEL: Color32 = Color32::from_rgb(22, 28, 38);
const CARD: Color32 = Color32::from_rgb(28, 36, 48);
const BORDER: Color32 = Color32::from_rgb(48, 59, 75);
const TEXT: Color32 = Color32::from_rgb(229, 236, 245);
const MUTED: Color32 = Color32::from_rgb(139, 153, 173);
const ACCENT: Color32 = Color32::from_rgb(76, 155, 255);
const GREEN: Color32 = Color32::from_rgb(71, 199, 133);
const RED: Color32 = Color32::from_rgb(232, 102, 112);
const AMBER: Color32 = Color32::from_rgb(234, 184, 91);

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    #[serde(alias = "bridge")]
    bridge_path: String,
    #[serde(alias = "node")]
    node_path: String,
    #[serde(alias = "cloudflared")]
    cloudflared_path: String,
    token_env: String,
    #[serde(alias = "target")]
    tunnel_target: String,
    #[serde(alias = "ollama")]
    ollama_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bridge_path: default_bridge_path(),
            node_path: find_executable("node"),
            cloudflared_path: find_executable("cloudflared"),
            token_env: "BRIDGE_TOKEN".to_owned(),
            tunnel_target: "http://127.0.0.1:11435".to_owned(),
            ollama_url: "http://127.0.0.1:11434".to_owned(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Settings,
    Activity,
}

struct ManagedProcess {
    child: Child,
}

struct StratagemApp {
    settings: Settings,
    token: String,
    show_token: bool,
    page: Page,
    processes: HashMap<String, ManagedProcess>,
    logs: Arc<Mutex<VecDeque<String>>>,
    log_path: PathBuf,
    logs_dir: PathBuf,
    settings_path: PathBuf,
    token_path: PathBuf,
    ollama_online: bool,
    bridge_online: bool,
    tunnel_running: bool,
    last_status_check: Instant,
    saved_message: String,
}

fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ProjectStratagem")
}

fn default_bridge_path() -> String {
    let Some(home) = dirs::home_dir() else {
        return String::new();
    };
    let candidates = [
        home.join("Downloads/ollama-gateway-setup/ollama-gateway-setup/bridge.mjs"),
        home.join("Downloads/ollama-gateway-setup/bridge.mjs"),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .unwrap_or(&candidates[0])
        .to_string_lossy()
        .into_owned()
}

fn find_executable(name: &str) -> String {
    let path_var = env::var_os("PATH").unwrap_or_default();
    let mut names = vec![name.to_owned()];
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        names.push(format!("{name}.exe"));
    }
    for dir in env::split_paths(&path_var) {
        for candidate in &names {
            let path = dir.join(candidate);
            if path.is_file() {
                return path.to_string_lossy().into_owned();
            }
        }
    }
    String::new()
}

fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, log_path: &Path, message: impl AsRef<str>) {
    let line = format!(
        "[{}] {}",
        chrono_like_time(),
        message.as_ref().trim()
    );
    if let Ok(mut entries) = logs.lock() {
        entries.push_back(line.clone());
        while entries.len() > 700 {
            entries.pop_front();
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(file, "{line}");
    }
}

fn chrono_like_time() -> String {
    // Keep the logger dependency-free; timestamps are UTC.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seconds = now % 60;
    let minutes = (now / 60) % 60;
    let hours = (now / 3600) % 24;
    format!("{hours:02}:{minutes:02}:{seconds:02} UTC")
}

fn port_is_open(port: u16) -> bool {
    let address = ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next());
    address
        .and_then(|address| TcpStream::connect_timeout(&address, Duration::from_millis(180)).ok())
        .is_some()
}

#[cfg(windows)]
fn protect_token(token: &str) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;
    use std::slice;
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
            },
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: token.len() as u32,
        pbData: token.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|error| format!("Windows DPAPI encryption failed: {error}"))?;
        let encrypted = slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
        Ok(encrypted)
    }
}

#[cfg(not(windows))]
fn protect_token(_token: &str) -> Result<Vec<u8>, String> {
    Err("Token encryption is supported only on Windows.".to_owned())
}

#[cfg(windows)]
fn unprotect_token(encrypted: &[u8]) -> Result<String, String> {
    use std::{ffi::c_void, slice};
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
            },
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|error| format!("Windows DPAPI decryption failed: {error}"))?;
        let bytes = slice::from_raw_parts(output.pbData, output.cbData as usize);
        let token_result = String::from_utf8(bytes.to_vec())
            .map_err(|error| format!("Stored token is not valid UTF-8: {error}"));
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
        token_result
    }
}

#[cfg(not(windows))]
fn unprotect_token(_encrypted: &[u8]) -> Result<String, String> {
    Err("Token decryption is supported only on Windows.".to_owned())
}

impl StratagemApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.visuals.panel_fill = BG;
            style.visuals.window_fill = PANEL;
            style.visuals.override_text_color = Some(TEXT);
            style.visuals.widgets.noninteractive.bg_fill = PANEL;
            style.visuals.widgets.inactive.bg_fill = CARD;
            style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(39, 51, 68);
            style.visuals.widgets.active.bg_fill = Color32::from_rgb(44, 86, 135);
            style.visuals.selection.bg_fill = ACCENT;
            style.spacing.item_spacing = Vec2::new(10.0, 9.0);
            style.spacing.button_padding = Vec2::new(12.0, 8.0);
        });

        let root = data_dir();
        let logs_dir = root.join("logs");
        let _ = fs::create_dir_all(&logs_dir);
        let settings_path = root.join("settings.json");
        let token_path = root.join("token.dpapi");
        let log_path = logs_dir.join("stratagem.log");

        let mut settings = Settings::default();
        if let Ok(raw) = fs::read_to_string(&settings_path) {
            if let Ok(stored) = serde_json::from_str::<Settings>(&raw) {
                settings = stored;
            }
        }

        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let app = Self {
            settings,
            token: fs::read(&token_path)
                .ok()
                .and_then(|bytes| unprotect_token(&bytes).ok())
                .unwrap_or_default(),
            show_token: false,
            page: Page::Dashboard,
            processes: HashMap::new(),
            logs,
            log_path,
            logs_dir,
            settings_path,
            token_path,
            ollama_online: false,
            bridge_online: false,
            tunnel_running: false,
            last_status_check: Instant::now() - Duration::from_secs(10),
            saved_message: String::new(),
        };
        app.log("Project Stratagem started. Native Rust build.");
        app
    }

    fn log(&self, message: impl AsRef<str>) {
        push_log(&self.logs, &self.log_path, message);
    }

    fn save_settings(&mut self) {
        match serde_json::to_string_pretty(&self.settings)
            .map_err(|error| error.to_string())
            .and_then(|json| fs::write(&self.settings_path, json).map_err(|error| error.to_string()))
        {
            Ok(()) => {
                self.saved_message = "Settings saved".to_owned();
                self.log("Settings saved.");
            }
            Err(error) => {
                self.saved_message = format!("Could not save settings: {error}");
                self.log(&self.saved_message);
            }
        }
    }

    fn save_token(&mut self) {
        let token = self.token.trim();
        if token.is_empty() {
            self.saved_message = "Enter a token before saving".to_owned();
            return;
        }
        match protect_token(token).and_then(|encrypted| {
            fs::write(&self.token_path, encrypted).map_err(|error| error.to_string())
        }) {
            Ok(()) => {
                self.saved_message = "Token protected with Windows DPAPI".to_owned();
                self.log("Bridge token encrypted and saved for the current Windows account.");
            }
            Err(error) => {
                self.saved_message = error.clone();
                self.log(error);
            }
        }
    }

    fn generate_token(&mut self) {
        self.token = uuid::Uuid::new_v4().simple().to_string();
        self.saved_message = "New token generated. Save it, then configure the bridge/Worker to match.".to_owned();
        self.log("Generated a new random token. It is not active until the bridge and remote secret use the same value.");
    }

    fn launch(&mut self, key: &str, program: &str, args: &[String], cwd: Option<&Path>, token: Option<(&str, &str)>) {
        if self.process_running(key) {
            self.log(format!("{key} is already running under Stratagem."));
            return;
        }

        let log_file = self.logs_dir.join(format!("{key}.log"));
        let stdout = match OpenOptions::new().create(true).append(true).open(&log_file) {
            Ok(file) => file,
            Err(error) => {
                self.log(format!("Cannot open {}: {error}", log_file.display()));
                return;
            }
        };
        let stderr = match stdout.try_clone() {
            Ok(file) => file,
            Err(error) => {
                self.log(format!("Cannot prepare process log: {error}"));
                return;
            }
        };

        let mut command = Command::new(program);
        command
            .args(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(directory) = cwd {
            command.current_dir(directory);
        }
        if let Some((name, value)) = token {
            command.env(name, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }

        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.processes.insert(key.to_owned(), ManagedProcess { child });
                self.log(format!("Started {key} (PID {pid}). Output: {}", log_file.display()));
            }
            Err(error) => self.log(format!("Could not start {key}: {error}")),
        }
    }

    fn process_running(&mut self, key: &str) -> bool {
        match self.processes.get_mut(key) {
            Some(process) => matches!(process.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn start_ollama(&mut self) {
        if port_is_open(11434) {
            self.log("Ollama is already responding on 127.0.0.1:11434; leaving the existing process alone.");
            return;
        }
        let executable = find_executable("ollama");
        if executable.is_empty() {
            self.log("Ollama executable was not found in PATH. Install Ollama or add its folder to PATH.");
            return;
        }
        self.launch("ollama", &executable, &["serve".to_owned()], None, None);
    }

    fn start_bridge(&mut self) {
        if port_is_open(11435) {
            self.log("Port 11435 is already responding; not starting a duplicate bridge.");
            return;
        }
        let script = PathBuf::from(self.settings.bridge_path.trim());
        let node = if self.settings.node_path.trim().is_empty() {
            find_executable("node")
        } else {
            self.settings.node_path.trim().to_owned()
        };
        let env_name = if self.settings.token_env.trim().is_empty() {
            "BRIDGE_TOKEN".to_owned()
        } else {
            self.settings.token_env.trim().to_owned()
        };
        let token = self.token.trim().to_owned();
        if !script.is_file() {
            self.log(format!("bridge.mjs was not found at {}. Set its path in Settings.", script.display()));
            return;
        }
        if node.is_empty() || !Path::new(&node).is_file() {
            self.log("Node.js was not found. Install it or select node.exe in Settings.");
            return;
        }
        if token.is_empty() {
            self.log("Bridge token is empty. Enter or generate one in Settings, save it, then start the bridge.");
            return;
        }
        self.launch(
            "bridge",
            &node,
            &[script.to_string_lossy().into_owned()],
            script.parent(),
            Some((&env_name, &token)),
        );
    }

    fn start_tunnel(&mut self) {
        if self.process_running("tunnel") {
            self.log("Cloudflare Quick Tunnel is already running under Stratagem.");
            return;
        }
        let executable = if self.settings.cloudflared_path.trim().is_empty() {
            find_executable("cloudflared")
        } else {
            self.settings.cloudflared_path.trim().to_owned()
        };
        if executable.is_empty() || !Path::new(&executable).is_file() {
            self.log("cloudflared.exe was not found. Install cloudflared or select it in Settings.");
            return;
        }
        let target = self.settings.tunnel_target.trim().to_owned();
        if target.is_empty() {
            self.log("Tunnel target URL is blank. Set it in Settings.");
            return;
        }
        self.launch(
            "tunnel",
            &executable,
            &["tunnel".to_owned(), "--url".to_owned(), target.clone()],
            None,
            None,
        );
        self.log(format!("Quick Tunnel requested for {target}. Its temporary public URL is written to tunnel.log."));
    }

    fn start_all(&mut self) {
        self.save_settings();
        self.start_ollama();
        self.start_bridge();
        self.start_tunnel();
    }

    fn stop_all(&mut self) {
        for key in ["tunnel", "bridge", "ollama"] {
            if let Some(mut process) = self.processes.remove(key) {
                match process.child.try_wait() {
                    Ok(Some(_)) => self.log(format!("{key} has already exited.")),
                    Ok(None) => match process.child.kill() {
                        Ok(()) => {
                            let _ = process.child.wait();
                            self.log(format!("Stopped {key}."));
                        }
                        Err(error) => self.log(format!("Could not stop {key}: {error}")),
                    },
                    Err(error) => self.log(format!("Could not inspect {key}: {error}")),
                }
            }
        }
        self.log("Only processes launched by this app were stopped. Existing Ollama/bridge services were left untouched.");
    }

    fn test_ollama(&mut self) {
        let base = self.settings.ollama_url.trim().trim_end_matches('/').to_owned();
        let logs = Arc::clone(&self.logs);
        let log_path = self.log_path.clone();
        self.log(format!("Testing Ollama API at {base}/api/tags ..."));
        thread::spawn(move || {
            let result = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(6))
                .build()
                .map_err(|error| error.to_string())
                .and_then(|client| {
                    client
                        .get(format!("{base}/api/tags"))
                        .send()
                        .and_then(|response| response.error_for_status())
                        .map_err(|error| error.to_string())
                })
                .and_then(|response| response.json::<serde_json::Value>().map_err(|error| error.to_string()));
            match result {
                Ok(body) => {
                    let names = body
                        .get("models")
                        .and_then(|models| models.as_array())
                        .map(|models| {
                            models
                                .iter()
                                .filter_map(|model| model.get("name").and_then(|name| name.as_str()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    let summary = if names.is_empty() { "API responded; no models listed".to_owned() } else { format!("API connected. Models: {names}") };
                    push_log(&logs, &log_path, summary);
                }
                Err(error) => push_log(&logs, &log_path, format!("Ollama API test failed: {error}")),
            }
        });
    }

    fn update_status(&mut self) {
        if self.last_status_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.ollama_online = port_is_open(11434);
        self.bridge_online = port_is_open(11435);
        self.tunnel_running = self.process_running("tunnel");
        self.last_status_check = Instant::now();
    }

    fn open_path(path: &Path) {
        #[cfg(windows)]
        {
            let _ = Command::new("explorer.exe").arg(path).spawn();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("xdg-open").arg(path).spawn();
        }
    }

    fn browse_file(current: &str, filter: (&str, &[&str])) -> Option<String> {
        let mut dialog = rfd::FileDialog::new().add_filter(filter.0, filter.1);
        if let Some(parent) = Path::new(current).parent().filter(|path| path.exists()) {
            dialog = dialog.set_directory(parent);
        }
        dialog.pick_file().map(|path| path.to_string_lossy().into_owned())
    }

    fn status_card(ui: &mut egui::Ui, title: &str, value: &str, detail: &str, healthy: bool) {
        egui::Frame::group(ui.style())
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_min_width(180.0);
                ui.label(RichText::new(title.to_uppercase()).size(11.0).strong().color(MUTED));
                ui.add_space(7.0);
                let color = if healthy { GREEN } else { RED };
                ui.label(RichText::new(value).size(20.0).strong().color(color));
                ui.add_space(3.0);
                ui.label(RichText::new(detail).size(11.0).color(MUTED));
            });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Control center").size(24.0).strong());
        ui.label(RichText::new("Manage the local AI gateway stack from one Windows application.").color(MUTED));
        ui.add_space(15.0);

        ui.columns(3, |columns| {
            Self::status_card(
                &mut columns[0],
                "Ollama · 11434",
                if self.ollama_online { "ONLINE" } else { "OFFLINE" },
                "Local model server",
                self.ollama_online,
            );
            Self::status_card(
                &mut columns[1],
                "Node bridge · 11435",
                if self.bridge_online { "ONLINE" } else { "OFFLINE" },
                "Gateway bridge endpoint",
                self.bridge_online,
            );
            Self::status_card(
                &mut columns[2],
                "Cloudflare Tunnel",
                if self.tunnel_running { "RUNNING" } else { "STOPPED" },
                "Quick Tunnel process",
                self.tunnel_running,
            );
        });

        ui.add_space(18.0);
        ui.heading("Services");
        ui.label(RichText::new("Start the stack in order, or manage each service separately.").color(MUTED));
        ui.add_space(7.0);

        let mut start_all = false;
        let mut stop_all = false;
        let mut start_ollama = false;
        let mut start_bridge = false;
        let mut start_tunnel = false;
        let mut test_ollama = false;

        ui.horizontal_wrapped(|ui| {
            start_all = ui.add_sized([142.0, 38.0], egui::Button::new(RichText::new("START ALL").strong()).fill(Color32::from_rgb(30, 125, 83))).clicked();
            stop_all = ui.add_sized([118.0, 38.0], egui::Button::new(RichText::new("STOP ALL").strong()).fill(Color32::from_rgb(145, 54, 64))).clicked();
            test_ollama = ui.add_sized([142.0, 38.0], egui::Button::new("TEST OLLAMA API")).clicked();
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            start_ollama = ui.button("Start Ollama").clicked();
            start_bridge = ui.button("Start Bridge").clicked();
            start_tunnel = ui.button("Start Tunnel").clicked();
        });

        if start_all { self.start_all(); }
        if stop_all { self.stop_all(); }
        if start_ollama { self.start_ollama(); }
        if start_bridge { self.start_bridge(); }
        if start_tunnel { self.start_tunnel(); }
        if test_ollama { self.test_ollama(); }

        ui.add_space(18.0);
        egui::Frame::group(ui.style())
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.label(RichText::new("IMPORTANT").strong().color(AMBER));
                ui.add_space(4.0);
                ui.label("A Quick Tunnel creates a temporary public URL. Stratagem starts cloudflared and records its output, but it does not edit your Cloudflare Worker URL or remote secret. Those values must match your current bridge configuration.");
            });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Configuration").size(24.0).strong());
        ui.label(RichText::new("Configure the local executables, gateway URLs, and bridge authentication.").color(MUTED));
        ui.add_space(12.0);

        egui::Frame::group(ui.style())
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.heading("Runtime paths");
                ui.add_space(6.0);

                ui.label("bridge.mjs");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.bridge_path).desired_width(f32::INFINITY));
                    if ui.button("Browse").clicked() {
                        if let Some(path) = Self::browse_file(&self.settings.bridge_path, ("JavaScript modules", &["mjs", "js"])) {
                            self.settings.bridge_path = path;
                        }
                    }
                });

                ui.add_space(7.0);
                ui.label("Node.js executable");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.node_path).desired_width(f32::INFINITY));
                    if ui.button("Browse").clicked() {
                        if let Some(path) = Self::browse_file(&self.settings.node_path, ("Windows executable", &["exe"])) {
                            self.settings.node_path = path;
                        }
                    }
                });

                ui.add_space(7.0);
                ui.label("cloudflared.exe");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.settings.cloudflared_path).desired_width(f32::INFINITY));
                    if ui.button("Browse").clicked() {
                        if let Some(path) = Self::browse_file(&self.settings.cloudflared_path, ("Windows executable", &["exe"])) {
                            self.settings.cloudflared_path = path;
                        }
                    }
                });
            });

        ui.add_space(12.0);
        egui::Frame::group(ui.style())
            .fill(CARD)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.heading("Connection settings");
                ui.add_space(6.0);
                ui.label("Ollama API base URL");
                ui.text_edit_singleline(&mut self.settings.ollama_url);
                ui.add_space(6.0);
                ui.label("Tunnel local target");
                ui.text_edit_singleline(&mut self.settings.tunnel_target);
                ui.add_space(6.0);
                ui.label("Bridge token environment variable");
                ui.text_edit_singleline(&mut self.settings.token_env);
                ui.add_space(6.0);
                ui.label("Bridge token");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(!self.show_token)
                            .desired_width(f32::INFINITY),
                    );
                    if ui.button(if self.show_token { "Hide" } else { "Show" }).clicked() {
                        self.show_token = !self.show_token;
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Generate token").clicked() {
                        self.generate_token();
                    }
                    if ui.button("Save encrypted token").clicked() {
                        self.save_token();
                    }
                });
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.add_sized([150.0, 36.0], egui::Button::new(RichText::new("SAVE SETTINGS").strong()).fill(ACCENT)).clicked() {
                self.save_settings();
            }
            if !self.saved_message.is_empty() {
                ui.label(RichText::new(&self.saved_message).color(MUTED));
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Token storage uses Windows DPAPI and is tied to this Windows user account.").size(11.0).color(MUTED));
    }

    fn activity_page(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Activity").size(24.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Open log folder").clicked() {
                    Self::open_path(&self.logs_dir);
                }
            });
        });
        ui.label(RichText::new("Application events are shown here. Process output is saved to individual log files.").color(MUTED));
        ui.add_space(10.0);

        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(8, 12, 18))
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    if let Ok(entries) = self.logs.lock() {
                        for entry in entries.iter() {
                            ui.label(RichText::new(entry).monospace().size(12.0).color(Color32::from_rgb(190, 204, 222)));
                        }
                    }
                });
            });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            for name in ["ollama.log", "bridge.log", "tunnel.log", "stratagem.log"] {
                if ui.button(format!("Open {name}")).clicked() {
                    Self::open_path(&self.logs_dir.join(name));
                }
            }
        });
    }
}

impl eframe::App for StratagemApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_status();
        ui.ctx().request_repaint_after(Duration::from_millis(800));

        ui.vertical(|ui| {
            egui::Frame::default()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(egui::Margin::symmetric(18, 13))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new("PROJECT STRATAGEM").size(19.0).strong().color(TEXT));
                            ui.label(RichText::new("LOCAL AI GATEWAY CONTROL").size(10.0).strong().color(MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.selectable_label(self.page == Page::Activity, "Activity").clicked() {
                                self.page = Page::Activity;
                            }
                            if ui.selectable_label(self.page == Page::Settings, "Settings").clicked() {
                                self.page = Page::Settings;
                            }
                            if ui.selectable_label(self.page == Page::Dashboard, "Dashboard").clicked() {
                                self.page = Page::Dashboard;
                            }
                        });
                    });
                });

            ui.add_space(14.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                match self.page {
                    Page::Dashboard => self.dashboard(ui),
                    Page::Settings => self.settings_page(ui),
                    Page::Activity => self.activity_page(ui),
                }
                ui.add_space(18.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Project Stratagem · Native Rust build").size(10.0).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Open data folder").clicked() {
                            Self::open_path(&data_dir());
                        }
                    });
                });
            });
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 780.0])
            .with_min_inner_size([940.0, 680.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(StratagemApp::new(cc)))),
    )
}
