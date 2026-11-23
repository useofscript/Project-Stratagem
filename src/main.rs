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
const BG: Color32 = Color32::from_rgb(10, 12, 16);
const PANEL: Color32 = Color32::from_rgb(17, 20, 26);
const CARD: Color32 = Color32::from_rgb(23, 27, 34);
const BORDER: Color32 = Color32::from_rgb(42, 47, 56);
const TEXT: Color32 = Color32::from_rgb(237, 239, 242);
const MUTED: Color32 = Color32::from_rgb(143, 151, 164);
const ACCENT: Color32 = Color32::from_rgb(206, 165, 87);
const GREEN: Color32 = Color32::from_rgb(76, 198, 132);
const RED: Color32 = Color32::from_rgb(230, 98, 106);
const AMBER: Color32 = Color32::from_rgb(223, 176, 93);

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
    #[serde(default = "default_worker_url")]
    worker_url: String,
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
            worker_url: default_worker_url(),
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
    worker_api_key: String,
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
    icon_texture: egui::TextureHandle,
}

fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ProjectStratagem")
}

fn default_worker_url() -> String {
    "https://ollama-gateway.pizzadoe173.workers.dev".to_owned()
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

        let icon_rgba = image::load_from_memory(include_bytes!("../assets/stratagem-icon.png"))
            .expect("embedded Stratagem icon must be valid PNG")
            .to_rgba8();
        let icon_texture = cc.egui_ctx.load_texture(
            "stratagem-icon",
            egui::ColorImage::from_rgba_unmultiplied(
                [icon_rgba.width() as usize, icon_rgba.height() as usize],
                icon_rgba.as_raw(),
            ),
            egui::TextureOptions::LINEAR,
        );

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
            worker_api_key: "clank".to_owned(),
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
            icon_texture,
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

    fn restart_bridge(&mut self) {
        self.save_settings();
        self.save_token();

        let script = PathBuf::from(self.settings.bridge_path.trim());
        if !script.is_file() {
            self.log(format!("Cannot restart bridge: bridge.mjs was not found at {}.", script.display()));
            return;
        }

        if let Some(mut process) = self.processes.remove("bridge") {
            match process.child.try_wait() {
                Ok(Some(_)) => self.log("The bridge process launched by Stratagem has already exited."),
                Ok(None) => match process.child.kill() {
                    Ok(()) => {
                        let _ = process.child.wait();
                        self.log("Stopped the bridge process launched by Stratagem.");
                    }
                    Err(error) => self.log(format!("Could not stop the bridge process launched by Stratagem: {error}")),
                },
                Err(error) => self.log(format!("Could not inspect the bridge process launched by Stratagem: {error}")),
            }
        }

        #[cfg(windows)]
        {
            let bridge_path = script.to_string_lossy().replace('\'', "''");
            let command = r#"
$bridgePath = '__BRIDGE_PATH__'
$owners = @(Get-NetTCPConnection -LocalPort 11435 -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique)
foreach ($ownerPid in $owners) {
    $owner = Get-CimInstance Win32_Process -Filter "ProcessId = $ownerPid" -ErrorAction SilentlyContinue
    if ($owner -and $owner.Name -ieq 'node.exe' -and $owner.CommandLine -and $owner.CommandLine.IndexOf($bridgePath, [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
        Stop-Process -Id $ownerPid -Force -ErrorAction Stop
        Write-Output "STOPPED:$ownerPid"
    } elseif ($owner) {
        Write-Output "SKIPPED:$ownerPid:$($owner.Name)"
    }
}
"#.replace("__BRIDGE_PATH__", &bridge_path);

            self.log("Restarting the configured bridge. Only a Node.js process running this exact bridge.mjs path can be stopped.");
            match Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
                .arg(&command)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    if !stdout.is_empty() {
                        self.log(stdout);
                    }
                    if !output.status.success() {
                        self.log(format!("PowerShell could not restart the bridge listener: {stderr}"));
                    }
                }
                Err(error) => {
                    self.log(format!("Could not run the safe bridge-restart check: {error}"));
                    return;
                }
            }
        }

        #[cfg(not(windows))]
        {
            self.log("Automatic restart of an external bridge process is supported on Windows only.");
        }

        for _ in 0..30 {
            if !port_is_open(11435) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }

        if port_is_open(11435) {
            self.log("Port 11435 is still occupied. Stratagem did not terminate it because its process could not be confirmed as the configured bridge.mjs. Check Settings > bridge.mjs path and the Activity log.");
            return;
        }

        self.log("Port 11435 is free. Starting bridge with the saved token and environment-variable name.");
        self.start_bridge();
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

    fn test_worker_api_key(&mut self) {
        let base = self.settings.worker_url.trim().trim_end_matches('/').to_owned();
        let api_key = self.worker_api_key.trim().to_owned();
        let logs = Arc::clone(&self.logs);
        let log_path = self.log_path.clone();
        if api_key.is_empty() {
            self.log("Worker API key test skipped: enter the Clank World API key first.");
            return;
        }
        self.log(format!("Testing Cloudflare Worker client authentication at {base}/v1/models ..."));
        thread::spawn(move || {
            let client = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build() {
                Ok(client) => client,
                Err(error) => {
                    push_log(&logs, &log_path, format!("Could not create Worker test client: {error}"));
                    return;
                }
            };
            match client.get(format!("{base}/v1/models")).bearer_auth(api_key).send() {
                Ok(response) if response.status().is_success() => {
                    let status = response.status();
                    let body = response.text().unwrap_or_default();
                    let models = serde_json::from_str::<serde_json::Value>(&body).ok()
                        .and_then(|json| json.get("data").or_else(|| json.get("models"))).and_then(|list| list.as_array()).map(|list| list.len());
                    push_log(&logs, &log_path, format!("Worker API key accepted (HTTP {status}). Gateway endpoint and client authentication are working.{}", models.map(|count| format!(" Returned {count} model entries.")).unwrap_or_default()));
                }
                Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED || response.status() == reqwest::StatusCode::FORBIDDEN => {
                    push_log(&logs, &log_path, format!("Worker rejected the Clank World API key (HTTP {}). This is the Cloudflare Worker API_KEY/client credential, separate from the bridge token. Verify the API_KEY secret and the key entered in Clank World.", response.status()));
                }
                Ok(response) if response.status() == reqwest::StatusCode::BAD_GATEWAY => {
                    let body = response.text().unwrap_or_default().chars().take(300).collect::<String>();
                    push_log(&logs, &log_path, format!("Worker accepted the request but could not reach the local bridge (HTTP 502). This is an upstream/tunnel/bridge issue, not a client API-key rejection. Response: {body}"));
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().unwrap_or_default().chars().take(300).collect::<String>();
                    push_log(&logs, &log_path, format!("Worker test returned HTTP {status}; not classified as an API-key failure. Response: {body}"));
                }
                Err(error) => push_log(&logs, &log_path, format!("Could not reach the Cloudflare Worker endpoint: {error}. Check Worker URL and internet connection.")),
            }
        });
    }

    fn test_bridge_auth(&mut self) {
        let token = self.token.trim().to_owned();
        let logs = Arc::clone(&self.logs);
        let log_path = self.log_path.clone();
        self.log("Testing both supported bridge authentication headers at http://127.0.0.1:11435/v1/models ...");
        thread::spawn(move || {
            let client = match reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    push_log(&logs, &log_path, format!("Could not create bridge test client: {error}"));
                    return;
                }
            };

            let bearer = format!("Bearer {token}");
            let tests = [
                ("X-Bridge-Token", token.as_str()),
                ("Authorization", bearer.as_str()),
            ];
            let mut rejected = Vec::new();

            for (header_name, header_value) in tests {
                match client
                    .get("http://127.0.0.1:11435/v1/models")
                    .header(header_name, header_value)
                    .send()
                {
                    Ok(response) if response.status().is_success() => {
                        push_log(&logs, &log_path, format!("Bridge authentication accepted using {header_name} (HTTP {}).", response.status()));
                        return;
                    }
                    Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED || response.status() == reqwest::StatusCode::FORBIDDEN => {
                        rejected.push(format!("{header_name}: HTTP {}", response.status()));
                        push_log(&logs, &log_path, format!("Bridge rejected {header_name} (HTTP {}).", response.status()));
                    }
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().unwrap_or_default();
                        let excerpt = body.chars().take(240).collect::<String>();
                        push_log(&logs, &log_path, format!("Bridge returned HTTP {status} for {header_name}; this is not an authentication rejection. Response: {excerpt}"));
                    }
                    Err(error) => {
                        push_log(&logs, &log_path, format!("Bridge test using {header_name} could not reach the listener: {error}"));
                    }
                }
            }

            if rejected.len() == 2 {
                push_log(&logs, &log_path, "Both X-Bridge-Token and Authorization Bearer were rejected with HTTP 401/403. The bridge process is using a different token or a different token environment-variable name. Use Restart Bridge to relaunch the configured bridge.mjs with Stratagem's saved token.");
            }
        });
    }

    fn current_tunnel_url(&self) -> Option<String> {
        let contents = fs::read_to_string(self.logs_dir.join("tunnel.log")).ok()?;
        for line in contents.lines().rev() {
            if let Some(start) = line.find("https://") {
                let tail = &line[start..];
                let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                let candidate = tail[..end].trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ')'));
                if candidate.contains(".trycloudflare.com") {
                    return Some(candidate.to_owned());
                }
            }
        }
        None
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
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                ui.set_min_width(190.0);
                ui.horizontal(|ui| {
                    let color = if healthy { GREEN } else { RED };
                    ui.label(RichText::new("●").color(color).size(14.0));
                    ui.label(RichText::new(title).size(12.0).strong().color(MUTED));
                });
                ui.add_space(8.0);
                ui.label(RichText::new(value).size(22.0).strong().color(if healthy { GREEN } else { RED }));
                ui.label(RichText::new(detail).size(11.0).color(MUTED));
            });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("The local AI stack, at a glance.").color(MUTED));
        ui.add_space(14.0);

        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(28, 28, 31))
            .stroke(Stroke::new(1.0, Color32::from_rgb(74, 63, 44)))
            .inner_margin(egui::Margin::same(22))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("STRATEGEM CONTROL").size(10.0).strong().color(ACCENT));
                        ui.add_space(7.0);
                        ui.heading(RichText::new("Your local AI.\nUnder command.").size(25.0).strong());
                        ui.label(RichText::new("Start your model server, bridge, and secure tunnel in one move.").color(MUTED));
                        ui.add_space(12.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_sized([138.0, 38.0], egui::Button::new(RichText::new("▶  START ALL").strong()).fill(Color32::from_rgb(37, 110, 75))).clicked() {
                                self.start_all();
                            }
                            if ui.add_sized([110.0, 38.0], egui::Button::new(RichText::new("■  STOP ALL").strong()).fill(Color32::from_rgb(83, 43, 46))).clicked() {
                                self.stop_all();
                            }
                        });
                    });
                });
            });

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.heading("Services");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("↻ Refresh").clicked() {
                    self.last_status_check = Instant::now() - Duration::from_secs(3);
                }
            });
        });
        ui.columns(3, |columns| {
            Self::status_card(&mut columns[0], "Ollama · 11434", if self.ollama_online { "Online" } else { "Offline" }, "Model server", self.ollama_online);
            Self::status_card(&mut columns[1], "Bridge · 11435", if self.bridge_online { "Online" } else { "Offline" }, "OpenAI-compatible API", self.bridge_online);
            Self::status_card(&mut columns[2], "Cloudflare Tunnel", if self.tunnel_running { "Running" } else { "Stopped" }, "Public connection", self.tunnel_running);
        });

        ui.add_space(14.0);
        ui.columns(2, |columns| {
            egui::Frame::group(columns[0].style())
                .fill(CARD)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(egui::Margin::same(16))
                .show(&mut columns[0], |ui| {
                    ui.heading("Quick actions");
                    ui.label(RichText::new("Start or test a service individually.").color(MUTED).size(12.0));
                    ui.add_space(9.0);
                    if ui.button("Start Ollama").clicked() { self.start_ollama(); }
                    if ui.button("Start Bridge").clicked() { self.start_bridge(); }
                    if ui.button("Restart Bridge (apply saved token)").clicked() { self.restart_bridge(); }
                    if ui.button("Start Cloudflare Tunnel").clicked() { self.start_tunnel(); }
                    ui.separator();
                    if ui.button("Test Ollama API").clicked() { self.test_ollama(); }
                    if ui.button("Test Bridge Token").clicked() { self.test_bridge_auth(); }
                });

            egui::Frame::group(columns[1].style())
                .fill(CARD)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(egui::Margin::same(16))
                .show(&mut columns[1], |ui| {
                    ui.heading("Clank World · Cloudflare Gateway");
                    ui.label(RichText::new("Stable Worker endpoint · OpenAI-compatible API").color(MUTED).size(12.0));
                    ui.add_space(7.0);
                    let base_url = format!("{}/v1", self.settings.worker_url.trim().trim_end_matches('/'));
                    ui.label("Base URL");
                    ui.label(RichText::new(&base_url).monospace().color(TEXT));
                    if ui.button("Copy Clank World Base URL").clicked() {
                        ui.ctx().copy_text(base_url);
                        self.saved_message = "Cloudflare Worker Base URL copied".to_owned();
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Clank World API Key = the Cloudflare Worker API_KEY secret. The Worker forwards requests to the local bridge using BRIDGE_TOKEN.").color(AMBER).size(11.0));
                });
        });

        ui.add_space(12.0);
        egui::Frame::group(ui.style())
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(13))
            .show(ui, |ui| {
                ui.label(RichText::new("QUICK TUNNEL").strong().color(AMBER));
                ui.label("Cloudflare Quick Tunnel addresses are temporary. If it restarts, update the Worker BRIDGE_URL binding with the new tunnel URL. Clank World itself should keep using the stable workers.dev URL.");
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
                ui.label("Cloudflare Worker URL (used by Clank World)");
                ui.text_edit_singleline(&mut self.settings.worker_url);
                ui.label(RichText::new("Clank World Base URL is this address plus /v1. Do not use the temporary trycloudflare.com URL as Clank World's Base URL.").size(11.0).color(MUTED));
                ui.add_space(6.0);
                ui.label("Bridge token environment variable");
                ui.text_edit_singleline(&mut self.settings.token_env);
                ui.label(RichText::new("Must match the variable name read by bridge.mjs. Usually BRIDGE_TOKEN.").size(11.0).color(MUTED));
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
                    if ui.button("Copy token").clicked() && !self.token.trim().is_empty() {
                        ui.ctx().copy_text(self.token.trim().to_owned());
                        self.saved_message = "Bridge token copied to clipboard".to_owned();
                    }
                });
                ui.label(RichText::new("This token is for the local bridge only. It must match the Cloudflare Worker's BRIDGE_TOKEN secret; it is not the Clank World client API key.").size(11.0).color(AMBER));
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
        egui::Frame::group(ui.style())
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.heading("Clank World · Cloudflare Gateway");
                ui.label(RichText::new("Use the stable Worker URL in Clank World, not the temporary Quick Tunnel address.").color(MUTED));
                let base_url = format!("{}/v1", self.settings.worker_url.trim().trim_end_matches('/'));
                ui.label("Base URL");
                ui.label(RichText::new(&base_url).monospace());
                if ui.button("Copy Clank World Base URL").clicked() {
                    ui.ctx().copy_text(base_url);
                    self.saved_message = "Cloudflare Worker Base URL copied".to_owned();
                }
                ui.label(RichText::new("Clank World API Key = Cloudflare Worker API_KEY (currently configured as clank).").size(11.0).color(AMBER));
                ui.label("Worker API key test (same key you enter in Clank World)");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.worker_api_key).password(true).desired_width(f32::INFINITY));
                    if ui.button("Test Worker Key").clicked() { self.test_worker_api_key(); }
                });
                ui.label(RichText::new("This tests the Cloudflare Worker client credential independently from local bridge-token authentication. The test key is not saved to settings or logs.").size(11.0).color(MUTED));
                ui.label(RichText::new("Bridge Token = Cloudflare Worker BRIDGE_TOKEN. This is sent by the Worker to the local bridge in the X-Bridge-Token header.").size(11.0).color(MUTED));
                if let Some(tunnel) = self.current_tunnel_url() {
                    ui.separator();
                    ui.label("Current Quick Tunnel (Worker BRIDGE_URL)");
                    ui.label(RichText::new(&tunnel).monospace().size(11.0));
                    if ui.button("Copy current tunnel URL").clicked() {
                        ui.ctx().copy_text(tunnel);
                        self.saved_message = "Current Quick Tunnel URL copied".to_owned();
                    }
                }
                if ui.button("Test local bridge authentication").clicked() {
                    self.test_bridge_auth();
                }
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

        let mut copy_activity = false;
        egui::Frame::group(ui.style())
            .fill(Color32::from_rgb(8, 12, 18))
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_height(230.0);
                ui.set_max_height(400.0);
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
            copy_activity = ui.button("Copy activity log").clicked();
            for name in ["ollama.log", "bridge.log", "tunnel.log", "stratagem.log"] {
                if ui.button(format!("Open {name}")).clicked() {
                    Self::open_path(&self.logs_dir.join(name));
                }
            }
        });
        if copy_activity {
            if let Ok(entries) = self.logs.lock() {
                ui.ctx().copy_text(entries.iter().cloned().collect::<Vec<_>>().join("\\n"));
                self.saved_message = "Activity log copied to clipboard".to_owned();
            }
        }
    }
}

impl eframe::App for StratagemApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_status();
        ui.ctx().request_repaint_after(Duration::from_millis(800));
        let full_width = ui.available_width();
        let sidebar_width = 204.0;
        let main_width = (full_width - sidebar_width - 18.0 - 72.0).max(360.0);

        let full_height = ui.available_height();
        ui.painter().rect_filled(ui.max_rect(), 0.0, Color32::from_rgb(7, 9, 12));
        ui.horizontal(|ui| {
            ui.set_min_height(full_height);
            egui::Frame::default()
                .fill(Color32::from_rgb(13, 15, 19))
                .inner_margin(egui::Margin::symmetric(14, 16))
                .show(ui, |ui| {
                    ui.set_min_height(full_height - 32.0);
                    ui.set_width(sidebar_width);
                    ui.vertical(|ui| {
                        ui.set_min_height(full_height - 32.0);
                        ui.horizontal(|ui| {
                            ui.image((self.icon_texture.id(), Vec2::splat(48.0)));
                            ui.vertical(|ui| {
                                ui.label(RichText::new("STRATAGEM").size(18.0).strong().color(TEXT));
                                ui.label(RichText::new("LOCAL AI CONTROL").size(9.0).strong().color(ACCENT));
                            });
                        });
                        ui.add_space(24.0);
                        ui.label(RichText::new("WORKSPACE").size(10.0).strong().color(MUTED));
                        ui.add_space(6.0);
                        for (page, label) in [
                            (Page::Dashboard, "◈  Overview"),
                            (Page::Settings, "⚙  Settings"),
                            (Page::Activity, "≡  Activity logs"),
                        ] {
                            let selected = self.page == page;
                            egui::Frame::default()
                                .fill(if selected { Color32::from_rgb(43, 36, 26) } else { Color32::TRANSPARENT })
                                .inner_margin(egui::Margin::symmetric(5, 3))
                                .show(ui, |ui| {
                                    if ui.add_sized(
                                        [sidebar_width - 34.0, 36.0],
                                        egui::Button::new(RichText::new(label).size(14.0).strong())
                                            .fill(if selected { Color32::from_rgb(43, 36, 26) } else { CARD }),
                                    ).clicked() {
                                        self.page = page;
                                    }
                                });
                        }
                        ui.add_space(22.0);
                        ui.label(RichText::new("SERVICES").size(10.0).strong().color(MUTED));
                        ui.add_space(8.0);
                        for (label, online) in [
                            ("Ollama", self.ollama_online),
                            ("Bridge", self.bridge_online),
                            ("Tunnel", self.tunnel_running),
                        ] {
                            let dot = if online { GREEN } else { RED };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("●").color(dot));
                                ui.label(RichText::new(label).color(TEXT).size(12.0));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    ui.label(RichText::new(if online { "READY" } else { "OFF" }).size(9.0).color(if online { GREEN } else { MUTED }));
                                });
                            });
                        }
                        ui.add_space(20.0);
                        ui.separator();
                        ui.label(RichText::new("0.3.3 · STRATEGIST EDITION").size(9.0).color(MUTED));
                        ui.label(RichText::new("Local-first. Your hardware.").size(10.0).color(MUTED));
                    });
                });

            ui.separator();
            ui.vertical(|ui| {
                ui.set_min_height(full_height);
                ui.set_width(main_width);
                egui::Frame::default()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(22, 16))
                    .show(ui, |ui| {
                        ui.set_min_height(full_height - 32.0);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                let (title, subtitle) = match self.page {
                                    Page::Dashboard => ("Command center", "System overview and connection controls"),
                                    Page::Settings => ("Settings", "Configure local services and authentication"),
                                    Page::Activity => ("Activity", "Service output and diagnostic history"),
                                };
                                ui.label(RichText::new(title).size(23.0).strong().color(TEXT));
                                ui.label(RichText::new(subtitle).size(11.0).color(MUTED));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Open data folder").clicked() {
                                    Self::open_path(&data_dir());
                                }
                            });
                        });
                        ui.add_space(15.0);
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            match self.page {
                                Page::Dashboard => self.dashboard(ui),
                                Page::Settings => self.settings_page(ui),
                                Page::Activity => self.activity_page(ui),
                            }
                            ui.add_space(18.0);
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("PROJECT STRATAGEM · WINDOWS DESKTOP").size(10.0).color(MUTED));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Open log folder").clicked() {
                                        Self::open_path(&self.logs_dir);
                                    }
                                });
                            });
                        });
                    });
            });
        });
    }
}

fn app_icon() -> egui::IconData {
    let rgba = image::load_from_memory(include_bytes!("../assets/stratagem-icon.png"))
        .expect("embedded Stratagem icon must be valid PNG")
        .to_rgba8();
    egui::IconData {
        rgba: rgba.as_raw().to_vec(),
        width: rgba.width(),
        height: rgba.height(),
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 860.0])
            .with_min_inner_size([980.0, 700.0])
            .with_icon(app_icon()),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(StratagemApp::new(cc)))),
    )
}
