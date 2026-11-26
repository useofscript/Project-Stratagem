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

/// Turns whatever the user pasted into a clean origin with no trailing
/// `/`, `/v1`, or `/v1/chat/completions`. Without this, a pasted Base URL
/// produced requests to `.../v1/v1/models`, which looks like an auth failure.
fn normalize_base_url(raw: &str) -> String {
    let mut url = raw.trim().trim_end_matches('/').to_owned();
    for suffix in ["/chat/completions", "/models", "/v1"] {
        if url.to_ascii_lowercase().ends_with(suffix) {
            url.truncate(url.len() - suffix.len());
            url = url.trim_end_matches('/').to_owned();
        }
    }
    url
}

/// Short, single-line excerpt of a response body for the activity log.
fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        "(empty body)".to_owned()
    } else {
        flat.chars().take(240).collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Probe {
    /// 2xx response.
    Ok,
    /// 401/403: the credential sent on this hop was rejected.
    Rejected,
    /// Cloudflare could not reach the tunnel/origin (502, 503, 504, 52x, 530).
    Unreachable,
    /// Some other HTTP status.
    Other,
    /// No HTTP response at all (DNS, refused connection, timeout).
    NoResponse,
}

fn classify(status: reqwest::StatusCode) -> Probe {
    match status.as_u16() {
        200..=299 => Probe::Ok,
        401 | 403 => Probe::Rejected,
        502 | 503 | 504 | 520..=530 => Probe::Unreachable,
        _ => Probe::Other,
    }
}

/// Sends GET `{base}/v1/models` with one auth header and returns the
/// classification plus a human-readable detail line.
fn probe_models(client: &reqwest::blocking::Client, base: &str, header: &str, value: &str) -> (Probe, String) {
    let url = format!("{base}/v1/models");
    match client.get(&url).header(header, value).send() {
        Ok(response) => {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            let kind = classify(status);
            let detail = match kind {
                Probe::Ok => {
                    let count = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|json| {
                            json.get("data")
                                .or_else(|| json.get("models"))
                                .and_then(|list| list.as_array())
                                .map(|list| list.len())
                        });
                    match count {
                        Some(count) => format!("HTTP {status}, {count} models"),
                        None => format!("HTTP {status}"),
                    }
                }
                _ => format!("HTTP {status}: {}", excerpt(&body)),
            };
            (kind, detail)
        }
        Err(error) => (Probe::NoResponse, error.to_string()),
    }
}

fn http_client(seconds: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(seconds))
        .build()
        .map_err(|error| error.to_string())
}

/// True if a process command line runs the configured bridge script.
/// Compares with normalized slashes and case, and also accepts a bare
/// relative `bridge.mjs` (what you get from `cd folder; node bridge.mjs`).
fn command_line_runs_bridge(command_line: &str, bridge_path: &str) -> bool {
    let norm = |text: &str| text.replace('/', "\\").to_ascii_lowercase();
    let line = norm(command_line);
    let path = norm(bridge_path);
    if !path.is_empty() && line.contains(&path) {
        return true;
    }
    let file_name = Path::new(&path.replace('\\', "/"))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    !file_name.is_empty()
        && line
            .split_whitespace()
            .map(|arg| arg.trim_matches('"'))
            .any(|arg| arg == file_name || arg.ends_with(&format!("\\{file_name}")))
}

fn default_bridge_path() -> String {
    let Some(home) = dirs::home_dir() else {
        return String::new();
    };
    // Join one component at a time so Windows gets consistent backslashes.
    // A mixed path like C:\Users\me\Downloads/ollama-gateway-setup/bridge.mjs
    // never matched the command line of a manually started bridge.
    let setup = home.join("Downloads").join("ollama-gateway-setup");
    let candidates = [
        setup.join("ollama-gateway-setup").join("bridge.mjs"),
        setup.join("bridge.mjs"),
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
        // Remember a decryption failure so it shows in Activity instead of the
        // token silently coming back empty (which then breaks every request).
        let mut token_error = None;
        let token = match fs::read(&token_path) {
            Ok(bytes) => unprotect_token(&bytes).unwrap_or_else(|error| {
                token_error = Some(error);
                String::new()
            }),
            Err(_) => String::new(),
        };
        let app = Self {
            settings,
            token,
            worker_api_key: String::new(),
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
        if let Some(error) = token_error {
            app.log(format!("Saved bridge token could not be read ({error}). Enter it again in Settings and save it."));
        }
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
            self.log("Port 11435 is already responding; not starting a duplicate bridge. If you changed the token, use Restart Bridge so the running bridge picks it up.");
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
            use std::os::windows::process::CommandExt;
            // List every listener on 11435 as "PID<TAB>Name<TAB>CommandLine".
            // The decision about what is safe to stop is made in Rust, using
            // normalized path matching (see command_line_runs_bridge).
            let list = r#"
$owners = @(Get-NetTCPConnection -LocalPort 11435 -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique)
foreach ($ownerPid in $owners) {
    $owner = Get-CimInstance Win32_Process -Filter "ProcessId = $ownerPid" -ErrorAction SilentlyContinue
    if ($owner) { Write-Output ("{0}`t{1}`t{2}" -f $ownerPid, $owner.Name, $owner.CommandLine) }
}
"#;
            self.log("Restarting the configured bridge. Only a node.exe process running bridge.mjs can be stopped.");
            let output = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", list])
                .creation_flags(0x08000000)
                .output();
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    self.log(format!("Could not run the safe bridge-restart check: {error}"));
                    return;
                }
            };
            if !output.status.success() {
                self.log(format!(
                    "PowerShell could not list the bridge listener: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            let bridge_path = script.to_string_lossy().into_owned();
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let mut parts = line.splitn(3, '\t');
                let (Some(pid), Some(name), command_line) = (parts.next(), parts.next(), parts.next().unwrap_or("")) else {
                    continue;
                };
                let Ok(pid) = pid.trim().parse::<u32>() else { continue };
                if name.eq_ignore_ascii_case("node.exe") && command_line_runs_bridge(command_line, &bridge_path) {
                    let killed = Command::new("taskkill.exe")
                        .args(["/PID", &pid.to_string(), "/F"])
                        .creation_flags(0x08000000)
                        .output()
                        .map(|out| out.status.success())
                        .unwrap_or(false);
                    if killed {
                        self.log(format!("Stopped old bridge (PID {pid}): {command_line}"));
                    } else {
                        self.log(format!("Could not stop old bridge PID {pid}. Close it manually."));
                    }
                } else {
                    self.log(format!(
                        "Left PID {pid} ({name}) running on port 11435; it is not node.exe running bridge.mjs. Command line: {command_line}"
                    ));
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
        // Each Quick Tunnel gets a brand-new random URL. tunnel.log used to be
        // appended forever, so the app kept showing the *previous* (dead) URL
        // until the new one was printed. Start the log fresh for each tunnel.
        let _ = fs::write(self.logs_dir.join("tunnel.log"), "");
        self.log("A new Quick Tunnel means a NEW trycloudflare.com URL. Update the Worker's BRIDGE_URL to it, or Clank World requests will fail.");
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

    /// Local-only check: bridge on 127.0.0.1 plus the Quick Tunnel.
    fn test_bridge_auth(&mut self) {
        self.diagnose_chain(false);
    }

    /// Full check, including the Cloudflare Worker with the Clank World key.
    fn test_worker_api_key(&mut self) {
        if self.worker_api_key.trim().is_empty() {
            self.log("Enter the Clank World API key (the Worker's API_KEY secret) in Settings first.");
            return;
        }
        self.diagnose_chain(true);
    }

    /// Walks the request path hop by hop, the same way Clank World's request
    /// travels, and names the first hop that fails:
    ///
    ///   Clank World --API_KEY--> Worker --X-Bridge-Token--> Quick Tunnel --> bridge.mjs --> Ollama
    ///
    /// A 401 at Clank World can come from the Worker (wrong API_KEY) *or* be
    /// the bridge's 401 passed back through the Worker (Worker BRIDGE_TOKEN
    /// does not match the local token). Those look identical from Clank
    /// World, which is why this was so hard to pin down. This test tells
    /// them apart by also sending a deliberately wrong key and comparing.
    fn diagnose_chain(&mut self, include_worker: bool) {
        let token = self.token.trim().to_owned();
        let api_key = self.worker_api_key.trim().to_owned();
        let local = normalize_base_url(&self.settings.tunnel_target);
        let worker = normalize_base_url(&self.settings.worker_url);
        let tunnel = self.current_tunnel_url();
        let env_name = self.settings.token_env.trim().to_owned();
        let logs = Arc::clone(&self.logs);
        let log_path = self.log_path.clone();
        self.log("── Connection diagnosis started ──");
        thread::spawn(move || {
            let say = |message: String| push_log(&logs, &log_path, message);
            let client = match http_client(12) {
                Ok(client) => client,
                Err(error) => return say(format!("Could not create HTTP client: {error}")),
            };
            if token.is_empty() {
                return say("Stop: the bridge token in Settings is empty.".to_owned());
            }

            // Hop 1: local bridge, using the header the Worker sends.
            let (hop1, detail) = probe_models(&client, &local, "X-Bridge-Token", &token);
            say(format!("1. Local bridge {local} with X-Bridge-Token → {detail}"));
            match hop1 {
                Probe::Ok => {}
                Probe::NoResponse => {
                    return say("   ✗ The bridge is not running or not on that port. Start Bridge, then test again.".to_owned());
                }
                Probe::Rejected => {
                    let (bearer, _) = probe_models(&client, &local, "Authorization", &format!("Bearer {token}"));
                    if bearer == Probe::Ok {
                        return say("   ✗ The bridge accepts Authorization: Bearer but NOT X-Bridge-Token. The Worker sends X-Bridge-Token, so every Worker request fails. Make bridge.mjs accept X-Bridge-Token, or make the Worker send Authorization: Bearer.".to_owned());
                    }
                    return say(format!("   ✗ The running bridge has a DIFFERENT token than Settings. Most likely an old bridge is still running, or bridge.mjs reads a variable other than {env_name}. Click Restart Bridge, then test again."));
                }
                Probe::Unreachable | Probe::Other => {
                    return say("   ✗ The bridge answered, but not with a model list. Check that Ollama is running (Test Ollama API) and read bridge.log.".to_owned());
                }
            }
            say("   ✓ Local bridge and token are good.".to_owned());

            // Hop 2: the public Quick Tunnel straight to the bridge.
            match &tunnel {
                None => say("2. No Quick Tunnel URL found in tunnel.log (tunnel not started by Stratagem). Skipping.".to_owned()),
                Some(url) => {
                    let (hop2, detail) = probe_models(&client, url, "X-Bridge-Token", &token);
                    say(format!("2. Quick Tunnel {url} → {detail}"));
                    if hop2 != Probe::Ok {
                        return say("   ✗ The tunnel is down or still starting. Wait ~15 seconds or restart the tunnel, then copy the new URL into the Worker's BRIDGE_URL.".to_owned());
                    }
                    say("   ✓ Tunnel reaches the bridge.".to_owned());
                }
            }

            if !include_worker {
                return say("Local checks passed. Enter the Clank World key and click Test Worker Key to test the Worker too.".to_owned());
            }

            // Hop 3: the Worker, exactly like Clank World calls it.
            let bearer = format!("Bearer {api_key}");
            let (hop3, detail) = probe_models(&client, &worker, "Authorization", &bearer);
            say(format!("3. Worker {worker}/v1/models with Clank World key → {detail}"));
            match hop3 {
                Probe::Ok => say(format!(
                    "   ✓ Whole chain works. In Clank World use Base URL {worker}/v1 and this exact API key. If Clank World still fails, re-paste the key there (watch for spaces)."
                )),
                Probe::Rejected => {
                    // Who said no? Send a key that is definitely wrong. If the
                    // Worker gives the same answer, it is rejecting our key too.
                    let bogus = format!("Bearer stratagem-wrong-{}", uuid::Uuid::new_v4().simple());
                    let (_, bogus_detail) = probe_models(&client, &worker, "Authorization", &bogus);
                    if bogus_detail == detail {
                        say("   ✗ The Worker rejected the API key itself. The key you entered does not match the Worker's API_KEY secret. Re-set it with: npx wrangler secret put API_KEY (type it, don't pipe it with echo, which adds a newline).".to_owned());
                    } else {
                        say(format!(
                            "   ✗ The API key passed the Worker, but the bridge rejected the Worker's token. The Worker's BRIDGE_TOKEN secret does not match the token in Stratagem. Copy the token from Settings and run: npx wrangler secret put BRIDGE_TOKEN. (Wrong-key response was: {bogus_detail})"
                        ));
                    }
                }
                Probe::Unreachable => say(match &tunnel {
                    Some(url) => format!("   ✗ The Worker cannot reach your PC. Its BRIDGE_URL is probably an old tunnel address. Set BRIDGE_URL to {url} and redeploy the Worker."),
                    None => "   ✗ The Worker cannot reach your PC. Start the tunnel and set the Worker's BRIDGE_URL to the new trycloudflare.com address.".to_owned(),
                }),
                Probe::Other => say("   ✗ Unexpected Worker response. 404 usually means the Worker URL in Settings is wrong.".to_owned()),
                Probe::NoResponse => say("   ✗ Could not reach the Worker at all. Check the Worker URL and your internet connection.".to_owned()),
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
                        ui.label(RichText::new("STRATAGEM CONTROL").size(10.0).strong().color(ACCENT));
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
                    let base_url = format!("{}/v1", normalize_base_url(&self.settings.worker_url));
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
                let base_url = format!("{}/v1", normalize_base_url(&self.settings.worker_url));
                ui.label("Base URL");
                ui.label(RichText::new(&base_url).monospace());
                if ui.button("Copy Clank World Base URL").clicked() {
                    ui.ctx().copy_text(base_url);
                    self.saved_message = "Cloudflare Worker Base URL copied".to_owned();
                }
                ui.label(RichText::new("Clank World API Key = the Cloudflare Worker API_KEY secret.").size(11.0).color(AMBER));
                ui.label("Worker API key test (same key you enter in Clank World)");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.worker_api_key).password(true).desired_width(f32::INFINITY));
                    if ui.button("Test Worker Key").clicked() { self.test_worker_api_key(); }
                });
                ui.label(RichText::new("Tests every hop: local bridge, Quick Tunnel, then the Worker. It reports exactly which hop fails. The key is kept in memory only, never saved or logged.").size(11.0).color(MUTED));
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
                if ui.button("Test bridge + tunnel (local only)").clicked() {
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
                ui.ctx().copy_text(entries.iter().cloned().collect::<Vec<_>>().join("\n"));
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
                        ui.label(RichText::new("0.3.4 · STRATEGIST EDITION").size(9.0).color(MUTED));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_strips_pasted_suffixes() {
        let want = "https://gw.example.workers.dev";
        for raw in [
            "https://gw.example.workers.dev",
            "https://gw.example.workers.dev/",
            " https://gw.example.workers.dev/v1 ",
            "https://gw.example.workers.dev/v1/",
            "https://gw.example.workers.dev/v1/models",
            "https://gw.example.workers.dev/v1/chat/completions",
        ] {
            assert_eq!(normalize_base_url(raw), want, "input: {raw:?}");
        }
        assert_eq!(normalize_base_url("http://127.0.0.1:11435"), "http://127.0.0.1:11435");
    }

    #[test]
    fn bridge_match_handles_slashes_case_and_relative_launch() {
        let configured = r"C:\Users\Ro\Downloads/ollama-gateway-setup/bridge.mjs";
        assert!(command_line_runs_bridge(r#""C:\Program Files\nodejs\node.exe" C:\users\ro\downloads\ollama-gateway-setup\bridge.mjs"#, configured));
        assert!(command_line_runs_bridge("node bridge.mjs", configured));
        assert!(command_line_runs_bridge(r#"node ".\bridge.mjs""#, configured));
        assert!(!command_line_runs_bridge("node server.js", configured));
        assert!(!command_line_runs_bridge("node not-bridge.mjs", configured));
    }

    #[test]
    fn status_classification() {
        use reqwest::StatusCode;
        assert_eq!(classify(StatusCode::OK), Probe::Ok);
        assert_eq!(classify(StatusCode::UNAUTHORIZED), Probe::Rejected);
        assert_eq!(classify(StatusCode::FORBIDDEN), Probe::Rejected);
        assert_eq!(classify(StatusCode::BAD_GATEWAY), Probe::Unreachable);
        assert_eq!(classify(StatusCode::from_u16(530).unwrap()), Probe::Unreachable);
        assert_eq!(classify(StatusCode::NOT_FOUND), Probe::Other);
    }
}
