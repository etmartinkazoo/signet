//! Signet: a calendar for Omarchy that shares its accounts with Sealwax.

mod editor;
mod runtime;
mod selftest;
mod theme;
mod week;
mod window;

use gtk4::{gio, glib, prelude::*};
use libadwaita as adw;
use signet_core::service::Service;
use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

/// Debug builds use `.Devel`, so they run beside an installed build.
const APP_ID: &str = if cfg!(debug_assertions) { "io.brem.Signet.Devel" } else { "io.brem.Signet" };

/// `signet --from-sealwax`: Sealwax's plugin protocol, a message on stdin as JSON.
const FROM_SEALWAX: &str = "--from-sealwax";
/// The extension of the file a message is handed to the running Signet in.
const MESSAGE_SUFFIX: &str = ".sealwax-message.json";
/// The most a message may be: Sealwax sends headers and plain text, not attachments.
const MAX_MESSAGE: u64 = 8 * 1024 * 1024;

thread_local! {
    static WINDOW: RefCell<Option<Rc<window::Window>>> = const { RefCell::new(None) };
}

/// The window, made on first use.
fn window(app: &adw::Application) -> Option<Rc<window::Window>> {
    if let Some(window) = WINDOW.with(|w| w.borrow().clone()) {
        return Some(window);
    }
    match Service::open() {
        Ok((service, warnings)) => {
            let window = window::Window::new(app, Arc::new(service), warnings);
            if let Some(dir) = std::env::var_os(selftest::ENV) {
                selftest::start(&window, dir.into());
            }
            WINDOW.with(|w| *w.borrow_mut() = Some(Rc::clone(&window)));
            Some(window)
        }
        Err(error) => {
            eprintln!("signet: {error}");
            app.quit();
            None
        }
    }
}

/// Reads the message Sealwax sends on stdin into a private file the running Signet opens, so
/// a second `signet` hands it over and exits.
fn take_message() -> Result<PathBuf, String> {
    let mut text = String::new();
    std::io::stdin()
        .take(MAX_MESSAGE)
        .read_to_string(&mut text)
        .map_err(|error| format!("couldn't read the message: {error}"))?;
    let input: signet_core::email::PluginInput =
        serde_json::from_str(&text).map_err(|error| format!("the message isn't Sealwax's plugin JSON: {error}"))?;
    if input.version != 1 {
        return Err(format!("Sealwax's plugin protocol version {} isn't supported", input.version));
    }
    let dir = glib::user_runtime_dir().join("signet");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}{MESSAGE_SUFFIX}", uuid_like()));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn uuid_like() -> String {
    signet_core::event::new_uid()
}

/// Opens messages handed over by `--from-sealwax`.
fn open_messages(app: &adw::Application, files: &[gio::File]) {
    let Some(window) = window(app) else { return };
    window.window.present();
    for file in files {
        let Some(path) = file.path().filter(|p| p.to_string_lossy().ends_with(MESSAGE_SUFFIX)) else { continue };
        let text = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        match text.ok().and_then(|text| serde_json::from_str::<signet_core::email::PluginInput>(&text).ok()) {
            Some(input) => window.create_from_email(&input),
            None => tracing::warn!("a message from Sealwax that couldn't be read"),
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SIGNET_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == FROM_SEALWAX) {
        return hand_over();
    }

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.set_accels_for_action("win.new", &["<Control>n"]);
    app.set_accels_for_action("win.refresh", &["<Control>r"]);
    app.set_accels_for_action("win.today", &["<Control>t"]);
    app.set_accels_for_action("window.close", &["<Control>w", "<Control>q"]);
    app.connect_activate(|app| {
        if let Some(window) = window(app) {
            window.window.present();
        }
    });
    app.connect_open(|app, files, _| open_messages(app, files));
    app.run_with_args(&args)
}

/// `--from-sealwax`: saves the message, starts `signet <file>` on its own (it opens the file in
/// the running Signet, or becomes Signet) and answers Sealwax at once, so Sealwax never waits
/// on the window.
fn hand_over() -> glib::ExitCode {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let started = take_message().and_then(|path| {
        let program = std::env::current_exe().map_err(|e| e.to_string())?;
        Command::new(program)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own process group: Sealwax ending a slow plugin doesn't end Signet.
            .process_group(0)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("couldn't start Signet: {error}"))
    });
    match started {
        Ok(()) => {
            // The plugin protocol's answer: a notice Sealwax shows.
            println!(r#"{{"notice": "Opened in Signet"}}"#);
            glib::ExitCode::SUCCESS
        }
        Err(error) => {
            // Sealwax shows a failing plugin's stderr.
            eprintln!("{error}");
            glib::ExitCode::FAILURE
        }
    }
}
