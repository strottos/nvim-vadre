#!/bin/bash
vadre_root=$(dirname "$(realpath $0)")
cargo build --manifest-path="${vadre_root}/Cargo.toml" && NVIM_LOG_FILE="$(pwd)/nvim_log" VADRE_LOG='trace' VADRE_LOG_FILE="$(pwd)/vadre_log" nvim --clean -u "${vadre_root}/test/vimfiles/vimrc"
