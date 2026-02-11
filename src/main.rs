use std::fs::File;
use std::io;
use std::process::Command;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use log::{LevelFilter, error, info, warn};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use simplelog::{ConfigBuilder, WriteLogger};
use xmlparser::{ElementEnd, Token, Tokenizer};

struct Vm {
    id: String,
    name: String,
    vcpus: String,
    memory: String,
    state: String,
}

enum Action {
    Start,
    Shutdown,
}

enum Mode {
    Normal,
    SshInput { vm_name: String, ip: String },
    Confirm { vm_name: String, action: Action },
}

struct App {
    vms: Vec<Vm>,
    table_state: TableState,
    mode: Mode,
    input: String,
    show_all: bool,
    info_cache: Option<(String, String)>, // (vm_name, info_text)
}

impl App {
    fn new(show_all: bool) -> Self {
        let vms = get_vm_list(show_all);
        let mut table_state = TableState::default();
        if !vms.is_empty() {
            table_state.select(Some(0));
        }
        Self {
            vms,
            table_state,
            mode: Mode::Normal,
            input: String::new(),
            show_all,
            info_cache: None,
        }
    }

    fn update_info_cache(&mut self) {
        let name = self.selected_vm().map(|vm| vm.name.clone());
        let needs_update = match (&self.info_cache, &name) {
            (Some((cached, _)), Some(n)) => cached != n,
            (None, Some(_)) => true,
            (_, None) => {
                self.info_cache = None;
                return;
            }
        };
        if needs_update {
            let name = name.unwrap();
            let text = get_vm_info(&name);
            self.info_cache = Some((name, text));
        }
    }

    fn refresh_vms(&mut self) {
        let selected = self.table_state.selected();
        self.vms = get_vm_list(self.show_all);
        if self.vms.is_empty() {
            self.table_state.select(None);
        } else {
            let idx = selected.unwrap_or(0).min(self.vms.len() - 1);
            self.table_state.select(Some(idx));
        }
        self.info_cache = None;
        self.update_info_cache();
    }

    fn next(&mut self) {
        if self.vms.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) if i >= self.vms.len() - 1 => 0,
            Some(i) => i + 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn selected_vm(&self) -> Option<&Vm> {
        self.table_state.selected().and_then(|i| self.vms.get(i))
    }

    fn previous(&mut self) {
        if self.vms.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(0) => self.vms.len() - 1,
            Some(i) => i - 1,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

}

fn get_vm_list(show_all: bool) -> Vec<Vm> {
    info!("Running virsh list (show_all={})", show_all);
    let mut cmd = Command::new("virsh");
    cmd.arg("list");
    if show_all {
        cmd.arg("--all");
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            error!("Failed to run virsh: {e}");
            eprintln!("Failed to run virsh: {e}");
            std::process::exit(1);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("virsh failed: {stderr}");
        eprintln!("virsh failed: {stderr}");
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut vms = parse_virsh_output(&stdout);
    for vm in &mut vms {
        if let Some((vcpus, memory)) = get_vm_resources(&vm.name) {
            vm.vcpus = vcpus;
            vm.memory = memory;
        }
    }
    info!("Parsed {} VMs from virsh output", vms.len());
    vms
}

/// Parse the tabular output of `virsh list --all`.
///
/// Example input:
/// ```text
///  Id   Name       State
/// --------------------------
///  1    vm1        running
///  -    vm2        shut off
/// ```
/// Parse IPv4 addresses from `virsh domifaddr` output.
///
/// Output format:
///  Name       MAC address          Protocol     Address
/// -------------------------------------------------------
///  vnet0      52:54:00:xx:xx:xx    ipv4         192.168.122.x/24
fn parse_domifaddr_output(output: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for line in output.lines().skip(2) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 && parts[2] == "ipv4" {
            if let Some(ip) = parts[3].split('/').next() {
                let ip = ip.to_string();
                if !ips.iter().any(|v| v == &ip) {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

/// Get the IP address of a VM using `virsh domifaddr`.
///
/// Tries multiple sources in order: default (lease), arp, then agent,
/// because the default only works with libvirt-managed DHCP networks.
fn get_vm_ip(name: &str) -> Option<String> {
    get_vm_ips(name).into_iter().next()
}

fn get_vm_ips(name: &str) -> Vec<String> {
    info!("Looking up IP for VM '{name}'");
    let sources = ["lease", "arp", "agent"];
    let mut ips = Vec::new();
    for source in sources {
        info!("Trying domifaddr --source {source} for VM '{name}'");
        let output = Command::new("virsh")
            .args(["domifaddr", name, "--source", source])
            .output();
        let output = match output {
            Ok(o) if o.status.success() => o,
            Ok(_) => {
                warn!("virsh domifaddr --source {source} failed for VM '{name}'");
                continue;
            }
            Err(e) => {
                warn!("Failed to run virsh domifaddr --source {source}: {e}");
                continue;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        for ip in parse_domifaddr_output(&stdout) {
            if !ips.iter().any(|v| v == &ip) {
                info!("Resolved VM '{name}' -> {ip} (source: {source})");
                ips.push(ip);
            }
        }
    }
    if ips.is_empty() {
        warn!("No IPv4 address found for VM '{name}' from any source");
    }
    ips
}

/// Get VM details from `virsh dumpxml`.
fn get_vm_info(name: &str) -> String {
    let ip_text = {
        let ips = get_vm_ips(name);
        if ips.is_empty() {
            "N/A".to_string()
        } else {
            ips.join(", ")
        }
    };
    format!("IPs: {ip_text}\n{}", get_dumpxml_summary(name))
}

fn get_dumpxml_summary(name: &str) -> String {
    let output = Command::new("virsh").args(["dumpxml", name]).output();
    match output {
        Ok(o) if o.status.success() => {
            let raw_xml = String::from_utf8_lossy(&o.stdout);
            summarize_dumpxml(&raw_xml).unwrap_or_else(|_| format!("(unable to parse dumpxml for '{name}')"))
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            format!("(dumpxml failed for '{name}': {stderr})")
        }
        Err(e) => format!("(unable to run dumpxml for '{name}': {e})"),
    }
}

fn get_vm_resources(name: &str) -> Option<(String, String)> {
    let output = Command::new("virsh").args(["dumpxml", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw_xml = String::from_utf8_lossy(&output.stdout);
    parse_dumpxml_resources(&raw_xml).ok().map(|(vcpu, memory)| {
        (
            vcpu.unwrap_or_else(|| "N/A".to_string()),
            memory.unwrap_or_else(|| "N/A".to_string()),
        )
    })
}

fn parse_dumpxml_resources(
    xml: &str,
) -> Result<(Option<String>, Option<String>), xmlparser::Error> {
    let mut stack: Vec<String> = Vec::new();
    let mut vcpu: Option<String> = None;
    let mut memory: Option<String> = None;
    let mut memory_unit: Option<String> = None;

    for token in Tokenizer::from(xml) {
        let token = token?;
        match token {
            Token::ElementStart { local, .. } => {
                stack.push(local.as_str().to_string());
            }
            Token::Attribute { local, value, .. } => {
                if matches!(stack.last().map(String::as_str), Some("memory"))
                    && local.as_str() == "unit"
                {
                    memory_unit = Some(value.as_str().to_string());
                }
            }
            Token::Text { text } => {
                let value = text.as_str().trim();
                if !value.is_empty()
                    && vcpu.is_none()
                    && matches!(stack.last().map(String::as_str), Some("vcpu"))
                {
                    vcpu = Some(value.to_string());
                } else if !value.is_empty()
                    && memory.is_none()
                    && matches!(stack.last().map(String::as_str), Some("memory"))
                {
                    memory = Some(value.to_string());
                }
            }
            Token::ElementEnd { end, .. } => match end {
                ElementEnd::Open => {}
                ElementEnd::Empty | ElementEnd::Close(_, _) => {
                    let _ = stack.pop();
                }
            },
            _ => {}
        }
    }

    let memory_mib = memory
        .as_deref()
        .and_then(|v| convert_memory_to_mib(v, memory_unit.as_deref()));
    Ok((vcpu, memory_mib))
}

fn convert_memory_to_mib(value: &str, unit: Option<&str>) -> Option<String> {
    let amount = value.parse::<f64>().ok()?;
    let unit = unit.unwrap_or("KiB").to_ascii_lowercase();
    let mib = match unit.as_str() {
        "kib" => amount / 1024.0,
        "mib" => amount,
        "gib" => amount * 1024.0,
        "b" | "byte" | "bytes" => amount / (1024.0 * 1024.0),
        _ => return None,
    };
    let formatted = if (mib.fract()).abs() < 0.01 {
        format!("{mib:.0}")
    } else {
        format!("{mib:.1}")
    };
    Some(format!("{formatted} MiB"))
}

fn parse_virsh_output(output: &str) -> Vec<Vm> {
    let mut vms = Vec::new();
    for line in output.lines().skip(2) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.chars().all(|c| c == '-') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 3 {
            vms.push(Vm {
                id: parts[0].to_string(),
                name: parts[1].to_string(),
                vcpus: "N/A".to_string(),
                memory: "N/A".to_string(),
                state: parts[2..].join(" "),
            });
        }
    }
    vms
}

fn print_help() {
    println!("yalv-rust - Yet Another Libvirt Viewer");
    println!();
    println!("USAGE:");
    println!("    yalv-rust [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("        --all     Show all VMs (including inactive)");
    println!("    -h, --help    Show this help message and exit");
    println!();
    println!("KEYBINDINGS:");
    println!("    j / Down      Move selection down");
    println!("    k / Up        Move selection up");
    println!("    Enter         Open console (running VMs only)");
    println!("    s             SSH into VM (running VMs only)");
    println!("    u             Start VM (shut off VMs only)");
    println!("    d             Shut down VM (running VMs only)");
    println!("    A             Toggle between all / running VMs");
    println!("    q / Esc       Quit");
}

const LOG_FILE: &str = "yalv-rust.log";

fn init_logger() {
    let config = ConfigBuilder::new()
        .set_time_format_rfc3339()
        .build();
    if let Ok(file) = File::create(LOG_FILE) {
        let _ = WriteLogger::init(LevelFilter::Debug, config, file);
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    init_logger();
    info!("yalv-rust started with args: {:?}", args);

    let mut app = App::new(true);
    app.update_info_cache();
    info!("Loaded {} VMs (show_all=true)", app.vms.len());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

fn run_ssh(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    vm_name: &str,
    ip: &str,
    user: &str,
) -> io::Result<()> {
    let dest = format!("{user}@{ip}");
    info!("SSH into VM '{vm_name}' as {dest}");
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    let status = Command::new("ssh").arg(&dest).status();
    enable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    match &status {
        Ok(s) => info!("SSH to '{vm_name}' exited with {s}"),
        Err(e) => error!("Failed to run ssh: {e}"),
    }
    if let Err(e) = status {
        eprintln!("Failed to run ssh: {e}");
    }
    Ok(())
}

fn summarize_dumpxml(xml: &str) -> Result<String, xmlparser::Error> {
    #[derive(Default)]
    struct DiskInfo {
        is_disk: bool,
        target: Option<String>,
        source: Option<String>,
    }
    #[derive(Default)]
    struct InterfaceInfo {
        fields: Vec<String>,
    }

    let mut stack: Vec<String> = Vec::new();
    let mut emulator: Option<String> = None;
    let mut networks: Vec<String> = Vec::new();
    let mut interfaces: Vec<String> = Vec::new();
    let mut disks: Vec<String> = Vec::new();
    let mut current_disk: Option<DiskInfo> = None;
    let mut current_interface: Option<InterfaceInfo> = None;

    for token in Tokenizer::from(xml) {
        let token = token?;
        match token {
            Token::ElementStart { local, .. } => {
                let name = local.as_str().to_string();
                if name == "disk" {
                    current_disk = Some(DiskInfo::default());
                } else if name == "interface" {
                    current_interface = Some(InterfaceInfo::default());
                }
                stack.push(name);
            }
            Token::Attribute { local, value, .. } => {
                if let Some(elem) = stack.last().map(String::as_str) {
                    if let Some(disk) = current_disk.as_mut() {
                        if elem == "disk" && local.as_str() == "device" && value.as_str() == "disk" {
                            disk.is_disk = true;
                        } else if elem == "target" && local.as_str() == "dev" {
                            disk.target = Some(value.as_str().to_string());
                        } else if elem == "source"
                            && matches!(
                                local.as_str(),
                                "file" | "dev" | "name" | "volume" | "path"
                            )
                            && disk.source.is_none()
                        {
                            disk.source = Some(value.as_str().to_string());
                        }
                    }
                    if let Some(interface) = current_interface.as_mut() {
                        let skip_field = elem == "address"
                            && matches!(
                                local.as_str(),
                                "type" | "domain" | "bus" | "slot" | "function"
                            );
                        if skip_field {
                            continue;
                        }
                        let field = if elem == "interface" {
                            format!("{}={}", local.as_str(), value.as_str())
                        } else {
                            format!("{}.{}={}", elem, local.as_str(), value.as_str())
                        };
                        if !interface.fields.iter().any(|f| f == &field) {
                            interface.fields.push(field);
                        }
                    }
                    if elem == "source"
                        && matches!(stack.iter().rev().nth(1).map(String::as_str), Some("interface"))
                        && matches!(local.as_str(), "network" | "bridge" | "dev")
                    {
                        let source = value.as_str();
                        if !networks.iter().any(|n| n == source) {
                            networks.push(source.to_string());
                        }
                    }
                }
            }
            Token::Text { text } => {
                let value = text.as_str().trim();
                if value.is_empty() {
                    continue;
                }
                if let Some(elem) = stack.last().map(String::as_str) {
                    if elem == "emulator" && emulator.is_none() {
                        emulator = Some(value.to_string());
                    }
                }
                if let Some(interface) = current_interface.as_mut() {
                    if let Some(elem) = stack.last().map(String::as_str) {
                        let field = format!("{elem}={value}");
                        if !interface.fields.iter().any(|f| f == &field) {
                            interface.fields.push(field);
                        }
                    }
                }
            }
            Token::ElementEnd { end, .. } => match end {
                ElementEnd::Open => {}
                ElementEnd::Empty => {
                    if let Some(closed) = stack.pop() {
                        if closed == "interface" {
                            if let Some(interface) = current_interface.take() {
                                if interface.fields.is_empty() {
                                    interfaces.push("N/A".to_string());
                                } else {
                                    interfaces.push(interface.fields.join(", "));
                                }
                            }
                        } else if closed == "disk" {
                            if let Some(disk) = current_disk.take() {
                                if disk.is_disk {
                                    let target = disk.target.unwrap_or_else(|| "unknown".to_string());
                                    let source = disk.source.unwrap_or_else(|| "unknown".to_string());
                                    disks.push(format!("{target}: {source}"));
                                }
                            }
                        }
                    }
                }
                ElementEnd::Close(_, _) => {
                    if let Some(closed) = stack.pop() {
                        if closed == "interface" {
                            if let Some(interface) = current_interface.take() {
                                if interface.fields.is_empty() {
                                    interfaces.push("N/A".to_string());
                                } else {
                                    interfaces.push(interface.fields.join(", "));
                                }
                            }
                        } else if closed == "disk" {
                            if let Some(disk) = current_disk.take() {
                                if disk.is_disk {
                                    let target = disk.target.unwrap_or_else(|| "unknown".to_string());
                                    let source = disk.source.unwrap_or_else(|| "unknown".to_string());
                                    disks.push(format!("{target}: {source}"));
                                }
                            }
                        }
                    }
                }
            },
            _ => {}
        }
    }

    let emulator_text = emulator.unwrap_or_else(|| "N/A".to_string());
    let network_text = if networks.is_empty() {
        "N/A".to_string()
    } else {
        networks.join(", ")
    };
    let interface_text = if interfaces.is_empty() {
        "N/A".to_string()
    } else {
        interfaces.join(", ")
    };
    let disk_text = if disks.is_empty() {
        "N/A".to_string()
    } else {
        disks.join(", ")
    };

    Ok(format!(
        "Network: {network_text}\nInterfaces: {interface_text}\nEmulator: {emulator_text}\nDisks: {disk_text}"
    ))
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if !event::poll(REFRESH_INTERVAL)? {
            app.refresh_vms();
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &app.mode {
                Mode::Normal => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        info!("Quit requested");
                        return Ok(());
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.next();
                        app.update_info_cache();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.previous();
                        app.update_info_cache();
                    }
                    KeyCode::Enter => {
                        if let Some(vm) = app.selected_vm() {
                            if vm.state == "running" {
                                let name = vm.name.clone();
                                info!("Opening console for VM '{name}'");
                                disable_raw_mode()?;
                                crossterm::execute!(
                                    terminal.backend_mut(),
                                    LeaveAlternateScreen
                                )?;
                                let status = Command::new("virsh")
                                    .args(["console", &name])
                                    .status();
                                enable_raw_mode()?;
                                crossterm::execute!(
                                    terminal.backend_mut(),
                                    EnterAlternateScreen
                                )?;
                                terminal.clear()?;
                                match &status {
                                    Ok(s) => info!("Console for '{name}' exited with {s}"),
                                    Err(e) => error!("Failed to run virsh console: {e}"),
                                }
                                if let Err(e) = status {
                                    eprintln!("Failed to run virsh console: {e}");
                                }
                            }
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(vm) = app.selected_vm() {
                            if vm.state == "running" {
                                let name = vm.name.clone();
                                if let Some(ip) = get_vm_ip(&name) {
                                    info!("Prompting username for SSH to '{name}' ({ip})");
                                    app.input.clear();
                                    app.mode = Mode::SshInput { vm_name: name, ip };
                                }
                            }
                        }
                    }
                    KeyCode::Char('u') => {
                        if let Some(vm) = app.selected_vm() {
                            if vm.state == "shut off" {
                                let name = vm.name.clone();
                                info!("Confirming start for VM '{name}'");
                                app.mode = Mode::Confirm { vm_name: name, action: Action::Start };
                            }
                        }
                    }
                    KeyCode::Char('A') => {
                        app.show_all = !app.show_all;
                        info!("Toggled show_all to {}", app.show_all);
                        app.refresh_vms();
                    }
                    KeyCode::Char('d') => {
                        if let Some(vm) = app.selected_vm() {
                            if vm.state == "running" {
                                let name = vm.name.clone();
                                info!("Confirming shutdown for VM '{name}'");
                                app.mode = Mode::Confirm { vm_name: name, action: Action::Shutdown };
                            }
                        }
                    }
                    _ => {}
                },
                Mode::Confirm { vm_name, action } => match key.code {
                    KeyCode::Char('y') => {
                        let vm_name = vm_name.clone();
                        let action = match action {
                            Action::Start => "start",
                            Action::Shutdown => "shutdown",
                        };
                        info!("Confirmed: virsh {action} '{vm_name}'");
                        app.mode = Mode::Normal;
                        let output = Command::new("virsh")
                            .args([action, &vm_name])
                            .output();
                        match &output {
                            Ok(o) if o.status.success() => {
                                info!("virsh {action} '{vm_name}' succeeded");
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                error!("virsh {action} '{vm_name}' failed: {stderr}");
                            }
                            Err(e) => {
                                error!("Failed to run virsh {action}: {e}");
                            }
                        }
                        app.refresh_vms();
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {
                        info!("Cancelled action for VM '{vm_name}'");
                        app.mode = Mode::Normal;
                    }
                    _ => {}
                },
                Mode::SshInput { vm_name, ip } => match key.code {
                    KeyCode::Enter => {
                        let user = app.input.trim().to_string();
                        if !user.is_empty() {
                            let vm_name = vm_name.clone();
                            let ip = ip.clone();
                            app.mode = Mode::Normal;
                            app.input.clear();
                            run_ssh(terminal, &vm_name, &ip, &user)?;
                        }
                    }
                    KeyCode::Esc => {
                        info!("SSH input cancelled");
                        app.mode = Mode::Normal;
                        app.input.clear();
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => {
                        app.input.push(c);
                    }
                    _ => {}
                },
            }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let show_prompt = matches!(app.mode, Mode::SshInput { .. } | Mode::Confirm { .. });
    let has_info = app.info_cache.is_some();
    let mut constraints = vec![Constraint::Min(1)];
    if has_info {
        constraints.push(Constraint::Length(10));
    }
    if show_prompt {
        constraints.push(Constraint::Length(3));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let rows: Vec<Row> = app
        .vms
        .iter()
        .map(|vm| {
            let state_style = match vm.state.as_str() {
                "running" => Style::default().fg(Color::Green),
                "shut off" => Style::default().fg(Color::Red),
                "paused" => Style::default().fg(Color::Yellow),
                _ => Style::default(),
            };
            Row::new(vec![
                Cell::from(vm.id.clone()),
                Cell::from(vm.name.clone()),
                Cell::from(vm.vcpus.clone()),
                Cell::from(vm.memory.clone()),
                Cell::from(vm.state.clone()).style(state_style),
            ])
        })
        .collect();

    let header = Row::new(vec!["Id", "Name", "VCPUs", "Memory", "State"])
        .style(Style::default().bold())
        .bottom_margin(1);

    let widths = [
        Constraint::Length(6),
        Constraint::Min(12),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(15),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Virtual Machines [{}] (q: quit, j/k: navigate, Enter: console, s: ssh, u: start, d: shutdown, A: toggle all) ",
                    if app.show_all { "all" } else { "running" }
                )),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">> ");

    f.render_stateful_widget(table, chunks[0], &mut app.table_state);

    let mut next_chunk = 1;

    if let Some((vm_name, info_text)) = &app.info_cache {
        let info = Paragraph::new(info_text.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Info: {vm_name} ")),
            );
        f.render_widget(info, chunks[next_chunk]);
        next_chunk += 1;
    }

    match &app.mode {
        Mode::SshInput { vm_name, ip } => {
            let prompt = Paragraph::new(format!("{}|", &app.input))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" SSH user for {vm_name} ({ip}) — Enter: connect, Esc: cancel ")),
                );
            f.render_widget(prompt, chunks[next_chunk]);
        }
        Mode::Confirm { vm_name, action } => {
            let action_label = match action {
                Action::Start => "Start",
                Action::Shutdown => "Shut down",
            };
            let prompt = Paragraph::new("y / n")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {action_label} VM '{vm_name}'? ")),
                );
            f.render_widget(prompt, chunks[next_chunk]);
        }
        Mode::Normal => {}
    }
}
