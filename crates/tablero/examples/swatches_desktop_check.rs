//! Local-only desktop review driver: producer snapshots emit once, so later
//! content must come from reload replay rather than a fresh producer update.
use std::{error::Error, path::PathBuf};
use tablero::{
    config::Config,
    producer::from_fn,
    sni::SniHostProducer,
    widget::{ActiveWindow, DeviceKind, Msg, Volume, Workspaces},
};

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("config path"));
    let monitors: Vec<String> = args.collect();
    let producer = from_fn("one-shot-review-snapshots", move |tx| async move {
        tx.send(Msg::Workspaces(Workspaces::with_monitors(
            monitors
                .iter()
                .enumerate()
                .map(|(i, name)| (i as i32 + 1, name.clone())),
            monitors
                .iter()
                .enumerate()
                .map(|(i, name)| (name.clone(), i as i32 + 1)),
            1,
        )))?;
        for monitor in monitors {
            tx.send(Msg::ActiveWindow {
                window: Some(ActiveWindow::new(
                    "review",
                    format!("Swatches {monitor}: restored title"),
                )),
                monitor,
            })?;
        }
        tx.send(Msg::Volume(Some(Volume::new(
            0.73,
            false,
            DeviceKind::Headphones,
        ))))?;
        Ok(())
    });
    tablero::run_with_producers(
        Config::load_from_path(&path)?,
        vec![producer, Box::new(SniHostProducer::new())],
        Some(path),
    )
}
