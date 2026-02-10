use std::io;
use std::process::Command;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};

struct Vm {
    id: String,
    name: String,
    state: String,
}

struct App {
    vms: Vec<Vm>,
    table_state: TableState,
}

impl App {
    fn new(show_all: bool) -> Self {
        let vms = get_vm_list(show_all);
        let mut table_state = TableState::default();
        if !vms.is_empty() {
            table_state.select(Some(0));
        }
        Self { vms, table_state }
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
    let mut cmd = Command::new("virsh");
    cmd.arg("list");
    if show_all {
        cmd.arg("--all");
    }
    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to run virsh: {e}");
            std::process::exit(1);
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("virsh failed: {stderr}");
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_virsh_output(&stdout)
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
    println!("    q / Esc       Quit");
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let show_all = args.iter().any(|a| a == "--all");
    let mut app = App::new(show_all);

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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Enter => {
                        if let Some(vm) = app.selected_vm() {
                            if vm.state == "running" {
                                let name = vm.name.clone();
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
                                if let Err(e) = status {
                                    eprintln!("Failed to run virsh console: {e}");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
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
                .title(" Virtual Machines (q: quit, j/k: navigate, Enter: console) "),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol(">> ");

    f.render_stateful_widget(table, f.area(), &mut app.table_state);
}
