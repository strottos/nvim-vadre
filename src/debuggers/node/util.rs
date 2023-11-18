use std::path::PathBuf;

use anyhow::Result;

/// Node script, indicated by receiving a 'Debugger.scriptParsed' message from Node
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Script {
    file: String,
    script_id: String,
}

impl Script {
    pub fn new(file: String, script_id: String) -> Self {
        Script { file, script_id }
    }

    pub fn get_file(&self) -> &str {
        &self.file
    }

    pub fn get_script_id(&self) -> &str {
        &self.script_id
    }
}

pub(crate) fn canonical_filename(filename: &str) -> Result<String> {
    if filename.starts_with("file:///") {
        let file_path = if cfg!(windows) {
            PathBuf::from(&filename[8..])
        } else {
            PathBuf::from(&filename[7..])
        };
        let file_path = dunce::canonicalize(file_path)?;
        Ok(file_path
            .to_str()
            .expect("can convert path to string")
            .to_string())
    } else {
        Ok(filename.to_string())
    }
}
