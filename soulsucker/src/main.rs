mod acoustid;
mod app;
mod audio;
mod cli;
mod config;
mod input;
mod matcher;
mod musicbrainz;
mod session;
mod ui;
mod util;

use anyhow::{Context, Result};
use clap::Parser;

use crate::app::App;
use crate::cli::Args;

fn main() -> Result<()> {
    let args = Args::parse();
    if args.yes && (args.artist().is_none() || args.album().is_none()) {
        anyhow::bail!("--yes needs both an artist and an album");
    }

    // The library writes warnings to stderr. While the TUI owns the terminal,
    // buffer those lines and flush them after the alternate screen is gone.
    soulseek_rs::utils::logger::set_log_level(if args.verbose {
        soulseek_rs::utils::logger::LogLevel::Info
    } else {
        soulseek_rs::utils::logger::LogLevel::Error
    });
    soulseek_rs::utils::logger::enable_buffering();

    let loaded = config::load(args.config.as_deref())?;
    let mut persisted_cfg = loaded.config.clone();
    args.apply_credential_overrides(&mut persisted_cfg);
    persisted_cfg.normalize();

    let mut cfg = persisted_cfg.clone();
    args.apply_runtime_overrides(&mut cfg);
    cfg.normalize();

    let mut notices = Vec::new();
    if let Some(warning) = loaded.warning {
        notices.push(warning);
    }
    if args.credential_override() && persisted_cfg.has_credentials() && !args.setup {
        match config::save(&persisted_cfg, &loaded.path) {
            Ok(()) => notices.push(format!(
                "Credentials from the command line saved to {}",
                loaded.path.display()
            )),
            Err(err) => notices.push(format!("Could not save credentials: {err}")),
        }
    }
    let notice = notices.join(" ");

    let mut app = App::new(
        cfg,
        persisted_cfg,
        loaded.path,
        &args,
        (!notice.is_empty()).then_some(notice),
    );

    let mut terminal = ratatui::try_init().context("could not initialize the terminal")?;
    let result = app.run(&mut terminal);
    let restore = ratatui::try_restore();
    // Only explicit verbose runs print raw library logs: peer-controlled
    // fields in those lines bypass the app's terminal sanitizer.
    if args.verbose {
        soulseek_rs::utils::logger::flush_buffered_logs();
    }
    result?;
    restore?;
    Ok(())
}
