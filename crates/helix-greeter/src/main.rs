//! helix-greeter: Helix's login screen, a greeter for greetd
//! (helixdesktop/spec `components/greeter.md`). It runs as a layer-shell
//! client of `helix-comp` (started with `--exit-with-shell`) and exits
//! with status 0 once greetd has accepted the session to start.
//!
//! `--demo`: a stand-in for greetd in the process (any user, password
//! "demo"); the session it would start is printed, power actions are not
//! done and nothing is remembered.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

mod app;
mod atspi;
#[cfg(test)]
mod atspi_bus;
mod clock;
// Shared with helix-login-config (src/lib.rs); `crate::config` still
// names it.
use helix_greeter::config;
mod greetd;
mod login;
mod monitors;
mod osk;
mod paint;
mod picture;
mod power;
mod screen_reader;
mod secret;
mod sessions;
mod state;
mod ui;
mod users;

const USAGE: &str = "Usage: helix-greeter [--demo]\n\nHelix's login screen, run by greetd (GREETD_SOCK names its socket).\n\n  --demo   try the screen without greetd: any user, password \"demo\"\n";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();
    // The system's locale (helixdesktop/spec components/greeter.md,
    // "Language"): the environment's, else /etc/locale.conf or
    // /etc/default/locale; then its translations, month and weekday
    // names. Before any other thread starts.
    let adopted = helix_i18n::adopt_system_locale();
    helix_i18n::init();
    helix_draw::set_rtl(helix_i18n::is_rtl());
    if !adopted.is_empty() {
        tracing::info!(?adopted, "using the system locale");
    }

    let mut demo = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--demo" => demo = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprint!("helix-greeter: unknown argument {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let greetd: Box<dyn greetd::Greetd> = if demo {
        Box::new(greetd::Demo::default())
    } else {
        match greetd::Client::connect_env() {
            Ok(client) => Box::new(client),
            Err(err) => {
                tracing::error!(%err, "cannot connect to greetd");
                return ExitCode::FAILURE;
            }
        }
    };

    let config = config::Config::load();
    let (twelve_hour, source) = clock::decide(config.clock_format.as_deref(), clock::settings_clock_format());
    tracing::debug!(twelve_hour, ?source, "clock format");
    helix_draw::accent::set(config.accent());
    let state = state::State::load();
    let sessions = sessions::load(std::path::Path::new(sessions::SESSIONS_DIR));
    if sessions.is_empty() {
        tracing::warn!("no sessions installed in {}", sessions::SESSIONS_DIR);
    }
    let load_users = config.user_list;
    let greeter = app::Greeter::new(config, sessions, &state);
    match ui::run(greeter, greetd, load_users, demo) {
        ui::Outcome::Started => {
            tracing::info!("session accepted; exiting");
            ExitCode::SUCCESS
        }
        ui::Outcome::Failed(err) => {
            tracing::error!(%err, "greeter stopped");
            ExitCode::FAILURE
        }
    }
}
