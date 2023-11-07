use std::collections::{BTreeSet, HashMap};

use anyhow::{anyhow, bail, Result};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DebuggerBreakpoint {
    pub enabled: bool,

    // Collection of placed breakpoints, can be multiple breakpoints placed for one in the source.
    // Here we store the breakpoint id as the key and the line number it was placed on as the
    // value.
    pub resolved: HashMap<String, i64>,
}

impl DebuggerBreakpoint {
    fn new() -> Self {
        DebuggerBreakpoint {
            enabled: false,
            resolved: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Breakpoints(HashMap<String, HashMap<i64, DebuggerBreakpoint>>);

impl Breakpoints {
    pub(crate) fn new() -> Self {
        Breakpoints(HashMap::new())
    }

    pub(crate) fn add_pending_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        self.0
            .entry(source_file_path)
            .and_modify(|line_map| {
                line_map.insert(source_line_number, DebuggerBreakpoint::new());
            })
            .or_insert({
                let mut line_map = HashMap::new();
                line_map.insert(source_line_number, DebuggerBreakpoint::new());
                line_map
            });

        Ok(())
    }

    pub(crate) fn remove_breakpoint(
        &mut self,
        source_file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        if let Some(line_map) = self.0.get_mut(&source_file_path) {
            assert!(line_map.remove(&source_line_number).is_some());
        } else {
            bail!(
                "Can't find existing breakpoints for file {}",
                source_file_path
            )
        }

        Ok(())
    }

    pub(crate) fn get_all_files(&self) -> BTreeSet<String> {
        self.0.keys().map(|x| x.clone()).collect()
    }

    pub(crate) fn get_breakpoint_for_id(&self, id: &i64) -> Option<(String, i64)> {
        for (source_file_path, line_map) in self.0.iter() {
            for (source_line_number, breakpoint) in line_map.iter() {
                if breakpoint.resolved.contains_key(&id.to_string()) {
                    return Some((source_file_path.clone(), *source_line_number));
                }
            }
        }
        None
    }

    pub(crate) fn set_breakpoint_resolved(
        &mut self,
        file_path: String,
        source_line_number: i64,
        actual_line_number: i64,
        id: String,
    ) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.resolved.insert(id, actual_line_number);

        Ok(())
    }

    pub(crate) fn set_breakpoint_enabled(
        &mut self,
        file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.enabled = true;

        Ok(())
    }

    fn set_breakpoint_disabled(
        &mut self,
        file_path: String,
        source_line_number: i64,
    ) -> Result<()> {
        let breakpoint = self
            .0
            .get_mut(&file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))?
            .get_mut(&source_line_number)
            .ok_or_else(|| {
                anyhow!(
                    "Can't find existing breakpoints for file {} and line {}",
                    file_path,
                    source_line_number
                )
            })?;
        breakpoint.enabled = false;

        Ok(())
    }

    pub(crate) fn get_all_breakpoint_line_numbers_for_file(
        &self,
        file_path: &str,
    ) -> Result<&HashMap<i64, DebuggerBreakpoint>> {
        self.0
            .get(file_path)
            .ok_or_else(|| anyhow!("Can't find existing breakpoints for file {}", file_path))
    }
}
