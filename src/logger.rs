use std::{path::Path, sync::Arc};

use anyhow::Result;
use tracing_subscriber::{
    fmt::writer::BoxMakeWriter, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry,
};

pub(crate) fn setup_logging(log_file: Option<&Path>, filter: Option<&str>) -> Result<()> {
    let log_file = match log_file {
        Some(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Some(std::fs::File::create(path)?)
        }
        None => None,
    };
    let filter = filter.map_or(EnvFilter::default(), EnvFilter::new);
    let writer = match log_file {
        Some(file) => BoxMakeWriter::new(Arc::new(file)),
        None => BoxMakeWriter::new(std::io::stderr),
    };

    let vadre_fmt_layer = tracing_subscriber::fmt::layer().with_writer(writer);

    Registry::default()
        .with(filter)
        .with(vadre_fmt_layer)
        .init();

    Ok(())
}
