//! The connection dialog (DESIGN.md "Connection dialog"): engine, fields, TLS, color, and the
//! **Agent access** section (off / read-only / read-only role with the SQL to create it, sensitive
//! column patterns, approve-similar). Saving is `conn.save`, human-only: it needs a ticket from
//! the person's own click or Enter.

use std::path::PathBuf;

use ashlar::egui::{self, vec2};
use ashlar::ui::automation::{self, Role};
use ashlar::ui::controls::{Segmented, Switch};
use ashlar::ui::dialog::{Dialog, DialogButton};
use ashlar::ui::form::Field;
use ashlar::ui::tokens;
use quarry_core::{AgentAccess, ConnColor, ConnectionId, ConnectionSpec, Target, TlsMode};

use crate::human::HumanTicket;
use crate::sidebar::connections::Saved;
use crate::theme;

const ENGINES: &[&str] = &["PostgreSQL", "SQLite"];
const TLS: &[&str] = &["Disable", "Prefer", "Require", "Verify full"];
const COLORS: &[&str] = &["None", "Local", "Staging", "Prod"];
const AGENT: &[&str] = &["Off", "Read-only", "Read-only role"];

/// The result of a connection test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestState {
    Running,
    Ok(String),
    Failed(String),
}

/// What is being typed into the dialog. The password stays here (and then the keychain) only;
/// no `Debug`, so it never reaches a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Draft {
    pub editing: Option<ConnectionId>,
    pub name: String,
    pub engine: usize,
    pub host: String,
    pub port: String,
    pub db: String,
    pub user: String,
    pub password: String,
    pub tls: usize,
    pub ca_file: String,
    pub path: String,
    pub read_only: bool,
    pub color: usize,
    pub agent: usize,
    pub role: String,
    pub sensitive: String,
    pub approve_similar: bool,
    pub test: Option<TestState>,
    pub error: Option<String>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            editing: None,
            name: String::new(),
            engine: 0,
            host: "localhost".into(),
            port: "5432".into(),
            db: String::new(),
            user: String::new(),
            password: String::new(),
            tls: 1,
            ca_file: String::new(),
            path: String::new(),
            read_only: false,
            color: 0,
            agent: 1,
            role: "quarry_agent".into(),
            sensitive: "password, token, secret, ssn".into(),
            approve_similar: false,
            test: None,
            error: None,
        }
    }
}

impl Draft {
    /// The dialog for an existing connection (the password is never read back).
    pub fn from_saved(s: &Saved) -> Self {
        let spec = &s.spec;
        let mut d = Draft { editing: Some(spec.id.clone()), name: spec.name.clone(), ..Default::default() };
        match &spec.target {
            Target::Postgres { host, port, db, user, tls, .. } => {
                d.engine = 0;
                d.host = host.clone();
                d.port = port.to_string();
                d.db = db.clone();
                d.user = user.clone();
                d.tls = match tls {
                    TlsMode::Disable => 0,
                    TlsMode::Prefer => 1,
                    TlsMode::Require => 2,
                    TlsMode::VerifyFull { ca_file } => {
                        d.ca_file = ca_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                        3
                    }
                };
            }
            Target::Sqlite { path } => {
                d.engine = 1;
                d.path = path.display().to_string();
            }
        }
        d.read_only = spec.human_read_only;
        d.color = match spec.color {
            ConnColor::None => 0,
            ConnColor::Local => 1,
            ConnColor::Staging => 2,
            ConnColor::Prod => 3,
        };
        match &spec.agent_access {
            AgentAccess::Off => d.agent = 0,
            AgentAccess::ReadOnly => d.agent = 1,
            AgentAccess::ReadOnlyRole { role } => {
                d.agent = 2;
                d.role = role.clone();
            }
        }
        d.sensitive = spec.sensitive_patterns.join(", ");
        d.approve_similar = s.approve_similar;
        d
    }

    /// The connection this draft describes, under `id`, or why it is incomplete.
    pub fn to_saved(&self, id: ConnectionId) -> Result<Saved, String> {
        let name = self.name.trim();
        let target = if self.engine == 0 {
            let port: u16 = self.port.trim().parse().map_err(|_| "Port must be a number from 1 to 65535".to_string())?;
            if port == 0 {
                return Err("Port must be a number from 1 to 65535".into());
            }
            if self.host.trim().is_empty() {
                return Err("Host is required".into());
            }
            if self.user.trim().is_empty() {
                return Err("User is required".into());
            }
            let tls = match self.tls {
                0 => TlsMode::Disable,
                2 => TlsMode::Require,
                3 => TlsMode::VerifyFull { ca_file: Some(self.ca_file.trim()).filter(|s| !s.is_empty()).map(PathBuf::from) },
                _ => TlsMode::Prefer,
            };
            let db = if self.db.trim().is_empty() { self.user.trim().to_string() } else { self.db.trim().to_string() };
            Target::Postgres { host: self.host.trim().into(), port, db, user: self.user.trim().into(), tls, ssh: None }
        } else {
            if self.path.trim().is_empty() {
                return Err("Choose a SQLite file".into());
            }
            Target::Sqlite { path: PathBuf::from(self.path.trim()) }
        };
        let agent_access = match self.agent {
            0 => AgentAccess::Off,
            2 => {
                let role = self.role.trim();
                if role.is_empty() || !role.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err("Role name: letters, digits and _ only".into());
                }
                AgentAccess::ReadOnlyRole { role: role.into() }
            }
            _ => AgentAccess::ReadOnly,
        };
        let color = match self.color {
            1 => ConnColor::Local,
            2 => ConnColor::Staging,
            3 => ConnColor::Prod,
            _ => ConnColor::None,
        };
        let fallback = match &target {
            Target::Postgres { host, db, .. } => format!("{db}@{host}"),
            Target::Sqlite { path } => path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "SQLite".into()),
        };
        let spec = ConnectionSpec {
            id,
            name: if name.is_empty() { fallback } else { name.chars().take(80).collect() },
            target,
            color,
            agent_access,
            sensitive_patterns: self
                .sensitive
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(64)
                .map(|s| s.chars().take(64).collect())
                .collect(),
            human_read_only: self.read_only,
        };
        // prod connections never offer approve-similar (SECURITY.md layer C)
        let approve_similar = self.approve_similar && color != ConnColor::Prod && spec.agent_access.allows_agent();
        Ok(Saved { spec, approve_similar })
    }
}

/// The SQL that creates a read-only agent role (shown, never run by Quarry).
pub fn role_sql(role: &str) -> String {
    format!(
        "CREATE ROLE {role} LOGIN PASSWORD '…';\nGRANT USAGE ON SCHEMA public TO {role};\nGRANT SELECT ON ALL TABLES IN SCHEMA public TO {role};\nALTER DEFAULT PRIVILEGES IN SCHEMA public\n  GRANT SELECT ON TABLES TO {role};"
    )
}

/// What the person did in the dialog this frame.
pub enum Outcome {
    Cancel,
    /// Save, with the ticket of the person's own click or Enter (`conn.save`).
    Save(HumanTicket, Saved, Option<String>),
    /// Test the draft as it is (no saving).
    Test(ConnectionSpec, Option<String>),
}

/// Show the dialog. `new_id` names a connection that is not saved yet.
pub fn show(ctx: &egui::Context, d: &mut Draft, new_id: impl Fn(&str) -> ConnectionId) -> Option<Outcome> {
    let title = if d.editing.is_some() { "Edit connection" } else { "New connection" };
    let buttons = [DialogButton::cancel("Cancel"), DialogButton::new("Test"), DialogButton::new("Save").default()];
    let out = Dialog::new("quarry.dialog.connection", title, &buttons)
        .width(560.0)
        .description("Passwords are stored in the system keychain.")
        .show(ctx, |ui| body(ui, d));
    let id = || d.editing.clone().unwrap_or_else(|| new_id(&d.name));
    if let Some(ticket) = HumanTicket::from_dialog(ctx, out, 2) {
        return match d.to_saved(id()) {
            Ok(s) => Some(Outcome::Save(ticket, s, Some(d.password.clone()).filter(|p| !p.is_empty()))),
            Err(e) => {
                d.error = Some(e);
                None
            }
        };
    }
    match out {
        Some(ashlar::ui::dialog::DialogOutcome::Button(1)) => match d.to_saved(id()) {
            Ok(s) => {
                d.test = Some(TestState::Running);
                d.error = None;
                Some(Outcome::Test(s.spec, Some(d.password.clone()).filter(|p| !p.is_empty())))
            }
            Err(e) => {
                d.error = Some(e);
                None
            }
        },
        // Save while an agent drives the window: refused, the dialog stays
        Some(ashlar::ui::dialog::DialogOutcome::Button(2)) => {
            d.error = Some("Saving needs your own click".into());
            None
        }
        Some(_) => Some(Outcome::Cancel),
        None => None,
    }
}

fn text_field(ui: &mut egui::Ui, id: &str, label: &str, value: &mut String, password: bool, hint: &str) {
    Field::new(id, label).show(ui, |ui, id| {
        let r = ui.add(egui::TextEdit::singleline(value).password(password).hint_text(hint).desired_width(f32::INFINITY).margin(vec2(8.0, 5.0)));
        automation::register(ui, &r, id, label, Role::Field);
        r
    });
}

fn body(ui: &mut egui::Ui, d: &mut Draft) {
    let t = tokens::current(ui.ctx());
    ui.spacing_mut().item_spacing.y = 8.0;
    let label = |ui: &mut egui::Ui, s: &str| {
        ui.label(egui::RichText::new(s).color(t.text.primary));
    };
    egui::ScrollArea::vertical().max_height(560.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.columns(2, |c| {
            if let [a, b] = c {
                label(a, "Engine");
                Segmented::new("quarry.dialog.connection.engine", "Engine", ENGINES, &mut d.engine).show(a);
                label(b, "Color");
                Segmented::new("quarry.dialog.connection.color", "Color", COLORS, &mut d.color).show(b);
            }
        });
        text_field(ui, "quarry.dialog.connection.name", "Name", &mut d.name, false, "staging");
        if d.engine == 0 {
            ui.columns(2, |c| {
                if let [a, b] = c {
                    text_field(a, "quarry.dialog.connection.host", "Host", &mut d.host, false, "localhost");
                    text_field(b, "quarry.dialog.connection.port", "Port", &mut d.port, false, "5432");
                    text_field(a, "quarry.dialog.connection.db", "Database", &mut d.db, false, "postgres");
                    text_field(b, "quarry.dialog.connection.user", "User", &mut d.user, false, "postgres");
                    text_field(
                        a,
                        "quarry.dialog.connection.password",
                        "Password",
                        &mut d.password,
                        true,
                        if d.editing.is_some() { "unchanged" } else { "stored in the keychain" },
                    );
                    label(b, "TLS");
                    Segmented::new("quarry.dialog.connection.tls", "TLS mode", TLS, &mut d.tls).show(b);
                }
            });
            if d.tls == 3 {
                text_field(ui, "quarry.dialog.connection.ca_file", "CA certificate", &mut d.ca_file, false, "system roots");
            }
        } else {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 96.0;
                ui.allocate_ui(vec2(w.max(120.0), 54.0), |ui| {
                    text_field(ui, "quarry.dialog.connection.path", "File", &mut d.path, false, "/path/to/app.sqlite")
                });
                ui.vertical(|ui| {
                    ui.add_space(22.0);
                    let b = ui.add(egui::Button::new("Choose…").min_size(vec2(88.0, 28.0)));
                    automation::register(ui, &b, "quarry.dialog.connection.choose", "Choose…", Role::Button);
                    if b.clicked() {
                        let picked = ashlar::native::dialogs::FileDialog::new()
                            .title("Open a SQLite database")
                            .filter("SQLite", &["sqlite", "sqlite3", "db"])
                            .open_file();
                        if let Ok(p) = picked {
                            d.path = p.display().to_string();
                        }
                    }
                });
            });
        }
        Switch::new("quarry.dialog.connection.read_only", "Read-only (your own connection too)", &mut d.read_only).show(ui);

        ui.add_space(6.0);
        ui.separator();
        ui.label(theme::strong("Agent access", 14.0));
        ui.label(
            egui::RichText::new("The agent always gets its own connection. It can read; every write waits for your approval.")
                .size(12.0)
                .color(t.text.secondary),
        );
        Segmented::new("quarry.dialog.connection.agent", "Agent access", AGENT, &mut d.agent).show(ui);
        if d.agent == 2 {
            text_field(ui, "quarry.dialog.connection.role", "Role", &mut d.role, false, "quarry_agent");
            ui.label(egui::RichText::new("Run this once as an administrator:").size(12.0).color(t.text.secondary));
            let sql = role_sql(d.role.trim());
            let r = egui::Frame::new()
                .fill(t.surface.sunken)
                .stroke(egui::Stroke::new(1.0, t.border))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new(&sql).monospace().color(t.text.primary));
                })
                .response;
            automation::register(ui, &r, "quarry.dialog.connection.role_sql", &sql, Role::Document);
        }
        if d.agent != 0 {
            text_field(ui, "quarry.dialog.connection.sensitive", "Sensitive columns", &mut d.sensitive, false, "password, token");
            let prod = d.color == 3;
            let mut similar = d.approve_similar && !prod;
            ui.add_enabled_ui(!prod, |ui| {
                Switch::new("quarry.dialog.connection.approve_similar", "Allow \"approve similar\" for this session", &mut similar).show(ui);
            });
            if !prod {
                d.approve_similar = similar;
            }
        }
        match &d.test {
            Some(TestState::Running) => {
                ui.label(egui::RichText::new("Testing…").color(t.text.secondary));
            }
            Some(TestState::Ok(m)) => {
                let r = ui.label(egui::RichText::new(format!("Connected: {m}")).color(theme::OK));
                automation::register(ui, &r, "quarry.dialog.connection.test_result", m, Role::Status);
            }
            Some(TestState::Failed(m)) => {
                let r = ui.label(egui::RichText::new(m).color(theme::ERROR));
                automation::register(ui, &r, "quarry.dialog.connection.test_result", m, Role::Alert);
            }
            None => {}
        }
        if let Some(e) = &d.error {
            let r = ui.label(egui::RichText::new(e).color(theme::ERROR));
            automation::register(ui, &r, "quarry.dialog.connection.error", e, Role::Alert);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_round_trip_and_validate() {
        let mut d = Draft { name: "Staging".into(), user: "alice".into(), color: 2, agent: 2, approve_similar: true, ..Default::default() };
        let s = d.to_saved(ConnectionId::new("staging")).unwrap();
        assert!(matches!(&s.spec.target, Target::Postgres { port: 5432, db, .. } if db == "alice"));
        assert_eq!(s.spec.agent_access, AgentAccess::ReadOnlyRole { role: "quarry_agent".into() });
        assert!(s.approve_similar);
        assert_eq!(s.spec.sensitive_patterns, ["password", "token", "secret", "ssn"]);
        let back = Draft::from_saved(&s);
        assert_eq!(back.to_saved(ConnectionId::new("staging")).unwrap(), s);
        d.port = "99999".into();
        assert!(d.to_saved(ConnectionId::new("x")).is_err());
        d.port = "5432".into();
        d.role = "x; DROP".into();
        assert!(d.to_saved(ConnectionId::new("x")).is_err());
        // prod never allows approve-similar
        let p = Draft { name: "Prod".into(), user: "a".into(), color: 3, approve_similar: true, ..Default::default() };
        assert!(!p.to_saved(ConnectionId::new("p")).unwrap().approve_similar);
        let lite = Draft { engine: 1, path: "/tmp/a.sqlite".into(), agent: 0, ..Default::default() };
        let s = lite.to_saved(ConnectionId::new("a")).unwrap();
        assert_eq!(s.spec.name, "a.sqlite");
        assert_eq!(s.spec.agent_access, AgentAccess::Off);
        assert!(Draft { engine: 1, ..Default::default() }.to_saved(ConnectionId::new("a")).is_err());
        assert!(role_sql("quarry_agent").contains("GRANT SELECT ON ALL TABLES"));
    }
}
