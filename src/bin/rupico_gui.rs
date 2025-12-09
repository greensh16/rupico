use eframe::egui;
use rupico::micropython::{ExecResult, MicroPythonDevice, Result as MpResult};
use serialport::available_ports;

/// Simple in-memory representation of a remote file or directory for the GUI
/// file browser.
#[derive(Clone, Debug)]
struct RemoteNode {
    name: String,
    path: String,
    is_dir: bool,
    children: Vec<RemoteNode>,
}

/// A single open editor tab, optionally associated with a remote path.
#[derive(Clone, Debug)]
struct EditorTab {
    /// Remote path on the device, if this tab is tied to a device file.
    path: Option<String>,
    /// Current buffer contents.
    text: String,
    /// Whether the buffer has unsaved changes relative to the device.
    dirty: bool,
}

impl EditorTab {
    fn new_default() -> Self {
        Self {
            path: None,
            text: "print('hello from rupico GUI')\n".to_string(),
            dirty: false,
        }
    }

    fn new_with_contents(path: String, text: String) -> Self {
        Self {
            path: Some(path),
            text,
            dirty: false,
        }
    }

    fn new_empty_remote(path: String) -> Self {
        Self {
            path: Some(path),
            text: String::new(),
            dirty: false,
        }
    }

    fn display_name(&self) -> String {
        let base = match &self.path {
            Some(p) => p.rsplit('/').next().unwrap_or(p.as_str()).to_string(),
            None => "untitled".to_string(),
        };
        if self.dirty { format!("{base}*") } else { base }
    }
}

struct GuiApp {
    // Connection / device state
    available_ports: Vec<String>,
    selected_port: Option<String>,
    connection_error: Option<String>,
    device: Option<MicroPythonDevice>,

    // Editor + output
    tabs: Vec<EditorTab>,
    active_tab: usize,
    last_output: Option<ExecResult>,
    /// Short human-readable description of the last operation.
    last_status: Option<String>,

    // Device file browser
    remote_tree: Vec<RemoteNode>,
    selected_remote_path: Option<String>,
    selected_remote_is_dir: bool,
    /// Path to use when creating a new remote file.
    new_remote_path: String,
    /// Pending delete confirmation: (path, is_dir).
    confirm_delete: Option<(String, bool)>,
    /// Pending rename: (original path, is_dir).
    rename_from: Option<(String, bool)>,
    /// Buffer for the new path when renaming.
    rename_to: String,
}

impl Default for GuiApp {
    fn default() -> Self {
        let ports = available_ports()
            .map(|list| list.into_iter().map(|p| p.port_name).collect())
            .unwrap_or_default();

        Self {
            available_ports: ports,
            selected_port: None,
            connection_error: None,
            device: None,
            tabs: vec![EditorTab::new_default()],
            active_tab: 0,
            last_output: None,
            last_status: None,
            remote_tree: Vec::new(),
            selected_remote_path: None,
            selected_remote_is_dir: false,
            new_remote_path: "/main.py".to_string(),
            confirm_delete: None,
            rename_from: None,
            rename_to: String::new(),
        }
    }
}

impl GuiApp {
    fn ensure_connected(&mut self) {
        if self.device.is_some() {
            return;
        }

        let port = match self.selected_port.clone() {
            Some(p) => p,
            None => {
                self.connection_error = Some("No port selected".to_string());
                return;
            }
        };

        match MicroPythonDevice::connect(&port) {
            Ok(mut dev) => {
                if let Err(e) = dev.enter_raw_repl() {
                    self.connection_error = Some(format!("Failed to enter raw REPL: {e}"));
                    self.last_status = Some("Failed to enter raw REPL".to_string());
                    return;
                }
                self.connection_error = None;
                self.last_status = Some(format!("Connected to {port}"));
                self.device = Some(dev);
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to connect: {e}"));
                self.last_status = Some("Failed to connect".to_string());
            }
        }
    }

    fn disconnect(&mut self) {
        if let Some(mut dev) = self.device.take() {
            let _ = dev.exit_raw_repl();
        }
        self.last_status = Some("Disconnected".to_string());
    }

    fn stop_program(&mut self) {
        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        if let Err(e) = dev.stop_current_program() {
            self.connection_error = Some(format!("Failed to stop program: {e}"));
            self.last_status = Some("Failed to stop program".to_string());
            return;
        }
        let _ = dev.enter_raw_repl();
        self.last_status = Some("Program stopped".to_string());
        self.last_output = Some(ExecResult {
            stdout: "Program stopped\n".to_string(),
            stderr: String::new(),
        });
    }

    fn flash_active_as_main(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        let tab = &self.tabs[self.active_tab];
        if let Err(e) = dev.flash_main_script(&tab.text) {
            self.connection_error = Some(format!("Failed to flash main.py: {e}"));
            self.last_status = Some("Failed to flash main.py".to_string());
            return;
        }
        self.last_status = Some("Flashed active tab as main.py".to_string());
        // Refresh tree so /main.py appears or updates.
        self.refresh_remote_tree();
    }

    fn run_main_script(&mut self) {
        // We intentionally drop our raw-REPL connection after triggering main,
        // since a soft reboot will leave the device in a different state.
        self.ensure_connected();
        let mut dev = match self.device.take() {
            Some(d) => d,
            None => {
                self.last_status = Some("No device connected".to_string());
                return;
            }
        };

        match dev.run_main() {
            Ok(()) => {
                self.last_status = Some("Soft reboot triggered; main.py should run".to_string());
                self.last_output = Some(ExecResult {
                    stdout: "Soft reboot triggered; main.py should run on the device\n".to_string(),
                    stderr: String::new(),
                });
                // `device` remains None; user can reconnect to regain raw REPL.
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to run main.py: {e}"));
                self.last_status = Some("Failed to run main.py".to_string());
            }
        }
    }

    fn run_current_script(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        let tab = &mut self.tabs[self.active_tab];

        // If the tab is bound to a remote path, save (if dirty) and run the file.
        // Otherwise, run the buffer as an in-memory snippet.
        let result = if let Some(path) = tab.path.clone() {
            if tab.dirty {
                if let Err(e) = dev.write_text_file(&path, &tab.text) {
                    self.connection_error = Some(format!("Failed to save before run: {e}"));
                    self.last_status = Some("Run aborted: save failed".to_string());
                    return;
                }
                tab.dirty = false;
            }
            self.last_status = Some(format!("Running file {path}"));
            dev.run_file(&path)
        } else {
            self.last_status = Some("Running snippet".to_string());
            dev.run_snippet(&tab.text)
        };

        match result {
            Ok(res) => {
                if res.stderr.is_empty() {
                    self.last_status = Some("Run finished successfully".to_string());
                } else {
                    self.last_status = Some("Run finished with errors (stderr)".to_string());
                }
                self.last_output = Some(res);
            }
            Err(e) => {
                self.connection_error = Some(format!("Execution error: {e}"));
                self.last_status = Some("Run failed".to_string());
            }
        }
    }

    fn refresh_remote_tree(&mut self) {
        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        match build_remote_tree(dev, "/", 0, 4) {
            Ok(nodes) => {
                self.remote_tree = nodes;
                self.last_status = Some("Refreshed device file tree".to_string());
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to list device files: {e}"));
                self.remote_tree.clear();
                self.last_status = Some("Failed to refresh device file tree".to_string());
            }
        }
    }

    fn open_selected_file(&mut self) {
        let path = match self.selected_remote_path.clone() {
            Some(p) if !self.selected_remote_is_dir => p,
            _ => {
                self.connection_error = Some("Select a file to open".to_string());
                return;
            }
        };

        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        match dev.read_text_file(&path) {
            Ok(text) => {
                // Reuse an existing tab for this path if present, otherwise open a new tab.
                if let Some(idx) = self
                    .tabs
                    .iter()
                    .position(|t| t.path.as_deref() == Some(path.as_str()))
                {
                    let tab = &mut self.tabs[idx];
                    tab.text = text;
                    tab.dirty = false;
                    self.active_tab = idx;
                } else {
                    self.tabs
                        .push(EditorTab::new_with_contents(path.clone(), text));
                    self.active_tab = self.tabs.len() - 1;
                }
                self.connection_error = None;
                self.last_status = Some(format!("Opened {path}"));
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to open file: {e}"));
                self.last_status = Some("Failed to open file".to_string());
            }
        }
    }

    fn save_current_file(&mut self) {
        if self.tabs.is_empty() {
            return;
        }

        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        let tab = &mut self.tabs[self.active_tab];

        let path = if let Some(p) = tab.path.clone() {
            p
        } else if let Some(sel) = self.selected_remote_path.clone() {
            if self.selected_remote_is_dir {
                self.connection_error =
                    Some("Cannot save into a directory; select a file".to_string());
                return;
            }
            sel
        } else {
            self.connection_error = Some("No remote file selected to save".to_string());
            return;
        };

        match dev.write_text_file(&path, &tab.text) {
            Ok(()) => {
                tab.path = Some(path.clone());
                tab.dirty = false;
                self.connection_error = None;
                self.last_status = Some(format!("Saved {path}"));
                // Tree might change if file size/mtime changed.
                self.refresh_remote_tree();
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to save file: {e}"));
                self.last_status = Some("Failed to save file".to_string());
            }
        }
    }

    fn create_new_file(&mut self) {
        let path = self.new_remote_path.trim().to_string();
        if path.is_empty() {
            return;
        }

        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        match dev.write_text_file(&path, "") {
            Ok(()) => {
                self.connection_error = None;
                self.last_status = Some(format!("Created {path}"));
                self.refresh_remote_tree();

                // Open a new empty tab for this file.
                self.tabs.push(EditorTab::new_empty_remote(path));
                self.active_tab = self.tabs.len() - 1;
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to create file: {e}"));
                self.last_status = Some("Failed to create file".to_string());
            }
        }
    }

    fn delete_path_internal(&mut self, path: &str, is_dir: bool) {
        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        let res = if is_dir {
            dev.rmdir(path)
        } else {
            dev.remove(path)
        };

        match res {
            Ok(()) => {
                // Clear any tab that was bound to this path, but keep its buffer as an
                // unsaved script.
                self.last_status = Some(format!("Deleted {path}"));
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(path) {
                        tab.path = None;
                    }
                }
                if self.selected_remote_path.as_deref() == Some(path) {
                    self.selected_remote_path = None;
                }
                self.refresh_remote_tree();
            }
            Err(e) => {
                self.connection_error = Some(format!("Failed to delete {path}: {e}"));
                self.last_status = Some("Failed to delete".to_string());
            }
        }
    }

    fn rename_path_internal(&mut self, old_path: &str, new_path: &str, _is_dir: bool) {
        self.ensure_connected();
        let dev = match self.device.as_mut() {
            Some(d) => d,
            None => return,
        };

        match dev.rename(old_path, new_path) {
            Ok(()) => {
                self.last_status = Some(format!(
                    "Renamed {old} to {new}",
                    old = old_path,
                    new = new_path
                ));
                for tab in &mut self.tabs {
                    if tab.path.as_deref() == Some(old_path) {
                        tab.path = Some(new_path.to_string());
                    }
                }
                if self.selected_remote_path.as_deref() == Some(old_path) {
                    self.selected_remote_path = Some(new_path.to_string());
                }
                self.refresh_remote_tree();
            }
            Err(e) => {
                self.connection_error = Some(format!(
                    "Failed to rename {old} to {new}: {e}",
                    old = old_path,
                    new = new_path
                ));
                self.last_status = Some("Failed to rename".to_string());
            }
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.heading("Rupico GUI");

            ui.horizontal(|ui| {
                ui.label("Serial port:");

                let mut selected_index: Option<usize> = self
                    .selected_port
                    .as_ref()
                    .and_then(|p| self.available_ports.iter().position(|s| s == p));

                egui::ComboBox::from_id_source("port_combo")
                    .width(200.0)
                    .selected_text(
                        selected_index
                            .and_then(|i| self.available_ports.get(i))
                            .cloned()
                            .unwrap_or_else(|| "Select port".to_string()),
                    )
                    .show_ui(ui, |ui| {
                        for (i, name) in self.available_ports.iter().enumerate() {
                            ui.selectable_value(&mut selected_index, Some(i), name);
                        }
                    });

                if let Some(i) = selected_index {
                    self.selected_port = self.available_ports.get(i).cloned();
                }

                if ui.button("Refresh").clicked() {
                    self.available_ports = available_ports()
                        .map(|list| list.into_iter().map(|p| p.port_name).collect())
                        .unwrap_or_default();
                }

                if self.device.is_some() {
                    if ui.button("Disconnect").clicked() {
                        self.disconnect();
                    }
                } else if ui.button("Connect").clicked() {
                    self.ensure_connected();
                    if self.device.is_some() {
                        self.refresh_remote_tree();
                    }
                }
            });

            if let Some(err) = &self.connection_error {
                ui.colored_label(egui::Color32::RED, err);
            }
        });

        egui::SidePanel::left("file_panel")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.heading("Device files");

                if ui.button("Refresh tree").clicked() {
                    self.refresh_remote_tree();
                }

                ui.separator();
                ui.label("New file path:");
                ui.text_edit_singleline(&mut self.new_remote_path);
                if ui.button("Create").clicked() {
                    self.create_new_file();
                }

                ui.separator();
                let has_selection = self.selected_remote_path.is_some();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            has_selection && !self.selected_remote_is_dir,
                            egui::Button::new("Open"),
                        )
                        .clicked()
                    {
                        self.open_selected_file();
                    }
                    if ui
                        .add_enabled(has_selection, egui::Button::new("Rename"))
                        .clicked()
                    {
                        if let Some(path) = self.selected_remote_path.clone() {
                            self.rename_from = Some((path.clone(), self.selected_remote_is_dir));
                            self.rename_to = path;
                        }
                    }
                    if ui
                        .add_enabled(has_selection, egui::Button::new("Delete"))
                        .clicked()
                    {
                        if let Some(path) = self.selected_remote_path.clone() {
                            self.confirm_delete = Some((path, self.selected_remote_is_dir));
                        }
                    }
                });

                ui.separator();

                let mut selected_path = self.selected_remote_path.clone();
                let mut selected_is_dir = self.selected_remote_is_dir;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for node in &self.remote_tree {
                        show_remote_node(ui, node, &mut selected_path, &mut selected_is_dir);
                    }
                });
                self.selected_remote_path = selected_path;
                self.selected_remote_is_dir = selected_is_dir;
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Tab bar
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let label = tab.display_name();
                    if ui.selectable_label(i == self.active_tab, label).clicked() {
                        self.active_tab = i;
                    }
                }
                if ui.button("+").clicked() {
                    self.tabs.push(EditorTab::new_default());
                    self.active_tab = self.tabs.len().saturating_sub(1);
                }
            });

            ui.horizontal(|ui| {
                if ui.button("Run").clicked() {
                    self.run_current_script();
                }
                if ui.button("Stop").clicked() {
                    self.stop_program();
                }
                if ui.button("Flash main").clicked() {
                    self.flash_active_as_main();
                }
                if ui.button("Run main").clicked() {
                    self.run_main_script();
                }
                let can_save = self
                    .tabs
                    .get(self.active_tab)
                    .map(|t| t.path.is_some())
                    .unwrap_or(false)
                    || (self.selected_remote_path.is_some() && !self.selected_remote_is_dir);
                if ui
                    .add_enabled(can_save, egui::Button::new("Save to device"))
                    .clicked()
                {
                    self.save_current_file();
                }
                if ui.button("Clear output").clicked() {
                    self.last_output = None;
                }
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    if let Some(path) = &tab.path {
                        ui.label(format!("Editing: {path}"));
                    }
                }
            });

            ui.separator();

            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                ui.label("Script:");
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut tab.text)
                        .font(egui::TextStyle::Monospace)
                        .code_editor()
                        .desired_rows(12),
                );
                if resp.changed() {
                    tab.dirty = true;
                }
            }

            ui.separator();

            ui.label("Output:");
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(res) = &self.last_output {
                    if !res.stderr.is_empty() {
                        ui.colored_label(egui::Color32::RED, "Errors reported (stderr not empty)");
                    }
                    ui.heading("stdout:");
                    ui.code(&res.stdout);
                    ui.separator();
                    ui.heading("stderr:");
                    if res.stderr.is_empty() {
                        ui.code("<empty>");
                    } else {
                        ui.code(&res.stderr);
                    }
                } else {
                    ui.label("(no output yet)");
                }
            });
        });

        if let Some((path, is_dir)) = self.confirm_delete.clone() {
            egui::Window::new("Confirm delete")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    if is_dir {
                        ui.label(format!("Delete directory '{path}' from device?"));
                    } else {
                        ui.label(format!("Delete file '{path}' from device?"));
                    }
                    ui.label("This cannot be undone.");

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.confirm_delete = None;
                        }
                        if ui.button("Delete").clicked() {
                            self.delete_path_internal(&path, is_dir);
                            self.confirm_delete = None;
                        }
                    });
                });
        }

        if let Some((from, is_dir)) = self.rename_from.clone() {
            egui::Window::new("Rename")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    if is_dir {
                        ui.label(format!("Rename directory '{from}'"));
                    } else {
                        ui.label(format!("Rename file '{from}'"));
                    }
                    ui.label("New path:");
                    ui.text_edit_singleline(&mut self.rename_to);

                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.rename_from = None;
                        }
                        if ui.button("Rename").clicked() {
                            let new_path = self.rename_to.trim().to_string();
                            if !new_path.is_empty() && new_path != from {
                                self.rename_path_internal(&from, &new_path, is_dir);
                            }
                            self.rename_from = None;
                        }
                    });
                });
        }

        // Status bar at the bottom
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let port_label = self
                    .selected_port
                    .as_deref()
                    .unwrap_or("(no port selected)");
                let status_label = if self.device.is_some() {
                    "Connected"
                } else {
                    "Disconnected"
                };
                ui.label(format!("Port: {port_label}"));
                ui.separator();
                ui.label(status_label);
                ui.separator();
                if let Some(msg) = &self.last_status {
                    ui.label(msg);
                } else {
                    ui.label("Ready");
                }
            });
        });
    }
}

fn join_remote_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{}", name)
    } else if base.ends_with('/') {
        format!("{}{}", base, name)
    } else {
        format!("{}/{}", base, name)
    }
}

fn build_remote_tree(
    dev: &mut MicroPythonDevice,
    path: &str,
    depth: usize,
    max_depth: usize,
) -> MpResult<Vec<RemoteNode>> {
    let mut nodes = Vec::new();
    let entries = dev.list_dir(path)?;

    for e in entries {
        let full = join_remote_path(path, &e.name);
        let children = if e.is_dir && depth < max_depth {
            build_remote_tree(dev, &full, depth + 1, max_depth)?
        } else {
            Vec::new()
        };
        nodes.push(RemoteNode {
            name: e.name,
            path: full,
            is_dir: e.is_dir,
            children,
        });
    }

    Ok(nodes)
}

fn show_remote_node(
    ui: &mut egui::Ui,
    node: &RemoteNode,
    selected_path: &mut Option<String>,
    selected_is_dir: &mut bool,
) {
    if node.is_dir {
        let resp = egui::CollapsingHeader::new(&node.name)
            .id_source(&node.path)
            .show(ui, |ui| {
                for child in &node.children {
                    show_remote_node(ui, child, selected_path, selected_is_dir);
                }
            });
        if resp.header_response.clicked() {
            *selected_path = Some(node.path.clone());
            *selected_is_dir = true;
        }
    } else {
        let is_selected = selected_path
            .as_deref()
            .map(|p| p == node.path.as_str())
            .unwrap_or(false);
        if ui
            .selectable_label(is_selected, &node.name)
            .on_hover_text(&node.path)
            .clicked()
        {
            *selected_path = Some(node.path.clone());
            *selected_is_dir = false;
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Rupico GUI",
        options,
        Box::new(|_cc| Box::new(GuiApp::default()) as Box<dyn eframe::App>),
    )
}
