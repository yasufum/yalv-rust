use std::fs::File;
use std::io;
use std::process::Command;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use log::{LevelFilter, error, info, warn};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use simplelog::{ConfigBuilder, WriteLogger};

struct Vm {
    id: String,
    name: String,
    state: String,
}

enum Mode {
    Normal,
    SshInput { vm_name: String, ip: String },
}

struct App {
    vms: Vec<Vm>,
    table_state: TableState,
    mode: Mode,
    input: String,
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
        }
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
    let vms = parse_virsh_output(&stdout);
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
/// Parse an IPv4 address from `virsh domifaddr` output.
///
/// Output format:
///  Name       MAC address          Protocol     Address
/// -------------------------------------------------------
///  vnet0      52:54:00:xx:xx:xx    ipv4         192.168.122.x/24
fn parse_domifaddr_output(output: &str) -> Option<String> {
    for line in output.lines().skip(2) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 && parts[2] == "ipv4" {
            if let Some(ip) = parts[3].split('/').next() {
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// Get the IP address of a VM using `virsh domifaddr`.
///
/// Tries multiple sources in order: default (lease), arp, then agent,
/// because the default only works with libvirt-managed DHCP networks.
fn get_vm_ip(name: &str) -> Option<String> {
    info!("Looking up IP for VM '{name}'");
    let sources = ["lease", "arp", "agent"];
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
        if let Some(ip) = parse_domifaddr_output(&stdout) {
            info!("Resolved VM '{name}' -> {ip} (source: {source})");
            return Some(ip);
        }
    }
    warn!("No IPv4 address found for VM '{name}' from any source");
    None
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

    let show_all = args.iter().any(|a| a == "--all");
    let mut app = App::new(show_all);
    info!("Loaded {} VMs (show_all={})", app.vms.len(), show_all);

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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match &app.mode {
                Mode::Normal => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        info!("Quit requested");
                        return Ok(());
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
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
}

fn ui(f: &mut Frame, app: &mut App) {
    let show_input = matches!(app.mode, Mode::SshInput { .. });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if show_input {
            vec![Constraint::Min(1), Constraint::Length(3)]
        } else {
            vec![Constraint::Min(1)]
        })
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
                Cell::from(vm.state.clone()).style(state_style),
            ])
        })
        .collect();

    let header = Row::new(vec!["Id", "Name", "State"])
        .style(Style::default().bold())
        .bottom_margin(1);

    let widths = [
        Constraint::Length(6),
        Constraint::Min(20),
        Constraint::Length(15),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Virtual Machines (q: quit, j/k: navigate, Enter: console, s: ssh) "),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">> ");

    f.render_stateful_widget(table, chunks[0], &mut app.table_state);

    if let Mode::SshInput { vm_name, ip } = &app.mode {
        let prompt = Paragraph::new(format!("{}|", &app.input))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" SSH user for {vm_name} ({ip}) — Enter: connect, Esc: cancel ")),
            );
        f.render_widget(prompt, chunks[1]);
    }
}
