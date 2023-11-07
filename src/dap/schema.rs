#![allow(non_camel_case_types)]

use serde::{Deserialize, Serialize};

schemafy::schemafy!("./src/dap/schema.json");

impl InitializeRequestArguments {
    pub fn new(adapter_id: String) -> Self {
        InitializeRequestArguments {
            adapter_id,
            client_id: Some("nvim_vadre".to_string()),
            client_name: Some("nvim_vadre".to_string()),
            columns_start_at_1: Some(true),
            lines_start_at_1: Some(true),
            locale: Some("en_GB".to_string()),
            path_format: Some("path".to_string()),
            supports_invalidated_event: None,
            supports_memory_event: None,
            supports_memory_references: Some(true),
            supports_progress_reporting: None,
            supports_run_in_terminal_request: Some(true),
            supports_variable_type: Some(true),
            supports_variable_paging: Some(false),
            supports_args_can_be_interpreted_by_shell: Some(false),
            supports_start_debugging_request: Some(false),
        }
    }
}

impl SourceBreakpoint {
    pub fn new(line: i64) -> Self {
        SourceBreakpoint {
            column: None,
            condition: None,
            hit_condition: None,
            line,
            log_message: None,
        }
    }
}

impl Source {
    pub fn new_file(file_name: String, file_path: String) -> Self {
        Source {
            adapter_data: None,
            checksums: None,
            name: Some(file_name),
            origin: None,
            path: Some(file_path),
            presentation_hint: None,
            source_reference: None,
            sources: None,
        }
    }
}
